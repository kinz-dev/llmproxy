//! Redact and route.
//!
//! `reject_sensitive` is binary: detect and refuse. That turns "there is a
//! credential in this file" into "you cannot use a good external model on this
//! code at all". Substituting stable placeholders, routing normally, and
//! rehydrating on the way back converts that into "you can".
//!
//! Two rules keep it honest:
//!
//! - **Only substitute what can be put back.** A value that would need JSON
//!   escaping cannot be spliced blindly into a response body, so it is
//!   redacted but never rehydrated; the placeholder stays visible instead of
//!   corrupting the stream.
//! - **Stable placeholders.** The same secret always becomes the same
//!   placeholder within a request, so a model can still reason about two
//!   occurrences being the same value.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct RedactConfig {
    /// Replace detected secrets with placeholders instead of refusing the
    /// request. Independent of `reject_sensitive`, which refuses outright.
    pub(crate) enabled: bool,
    /// Put the original values back into the response. Turning this off leaves
    /// the placeholders visible, which is the safer setting when responses are
    /// stored or shared onward.
    pub(crate) rehydrate: bool,
    /// Extra literal strings to redact — project-specific hostnames, internal
    /// identifiers, anything the detectors cannot know about.
    pub(crate) extra_literals: Vec<String>,
}

impl Default for RedactConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            rehydrate: true,
            extra_literals: Vec::new(),
        }
    }
}

impl RedactConfig {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if self.extra_literals.iter().any(|literal| literal.len() < 4) {
            return Err(
                "interceptor.redact.extra_literals entries must be at least 4 characters; \
                 shorter ones would match everywhere"
                    .to_string(),
            );
        }
        Ok(())
    }
}

/// What a detected span is, used only to name the placeholder so a model can
/// tell a key from an address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SecretKind {
    Key,
    Token,
    Jwt,
    PrivateKey,
    Email,
    Literal,
}

impl SecretKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Key => "KEY",
            Self::Token => "TOKEN",
            Self::Jwt => "JWT",
            Self::PrivateKey => "PRIVATE_KEY",
            Self::Email => "EMAIL",
            Self::Literal => "LITERAL",
        }
    }
}

/// The substitutions made for one request, and how to undo them.
#[derive(Debug, Clone, Default)]
pub(crate) struct Redaction {
    /// Placeholder to original. Only entries that are safe to splice back into
    /// a response body are present.
    restore: Vec<(String, String)>,
    /// Every placeholder issued, including the ones that will never be put
    /// back, so the count reported is the count made.
    issued: usize,
}

impl Redaction {
    pub(crate) fn is_empty(&self) -> bool {
        self.issued == 0
    }

    pub(crate) fn issued(&self) -> usize {
        self.issued
    }

    pub(crate) fn restorable(&self) -> usize {
        self.restore.len()
    }

    /// Longest placeholder, which is how much of a chunk boundary a streaming
    /// rehydrator has to hold back.
    fn max_placeholder_len(&self) -> usize {
        self.restore
            .iter()
            .map(|(placeholder, _)| placeholder.len())
            .max()
            .unwrap_or(0)
    }
}

/// Rewrite every user-visible string in a request, returning the substitutions.
///
/// System and assistant turns are rewritten too: a secret pasted into a system
/// prompt is exactly as exposed as one in a user message.
pub(crate) fn redact_request(config: &RedactConfig, body: &mut Value) -> Redaction {
    let mut builder = Builder::new(config);
    let field = if body.get("messages").is_some() {
        "messages"
    } else {
        "input"
    };
    if let Some(list) = body.get_mut(field).and_then(Value::as_array_mut) {
        for item in list {
            redact_value(item, &mut builder);
        }
    }
    for field in ["system", "instructions"] {
        if let Some(value) = body.get_mut(field) {
            redact_value(value, &mut builder);
        }
    }
    builder.finish()
}

fn redact_value(value: &mut Value, builder: &mut Builder<'_>) {
    match value {
        Value::String(text) => {
            if let Some(rewritten) = builder.rewrite(text) {
                *text = rewritten;
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_value(item, builder);
            }
        }
        Value::Object(fields) => {
            for (_, item) in fields.iter_mut() {
                redact_value(item, builder);
            }
        }
        _ => {}
    }
}

struct Builder<'a> {
    config: &'a RedactConfig,
    /// Original value to the placeholder already issued for it, so the same
    /// secret is always the same placeholder.
    assigned: HashMap<String, String>,
    restore: Vec<(String, String)>,
    counter: usize,
}

impl<'a> Builder<'a> {
    fn new(config: &'a RedactConfig) -> Self {
        Self {
            config,
            assigned: HashMap::new(),
            restore: Vec::new(),
            counter: 0,
        }
    }

    fn rewrite(&mut self, text: &str) -> Option<String> {
        let spans = detect(text, &self.config.extra_literals);
        if spans.is_empty() {
            return None;
        }
        let mut output = String::with_capacity(text.len());
        let mut cursor = 0;
        for span in spans {
            if span.start < cursor {
                continue;
            }
            output.push_str(&text[cursor..span.start]);
            let original = &text[span.start..span.end];
            output.push_str(&self.placeholder(original, span.kind));
            cursor = span.end;
        }
        output.push_str(&text[cursor..]);
        Some(output)
    }

    fn placeholder(&mut self, original: &str, kind: SecretKind) -> String {
        if let Some(existing) = self.assigned.get(original) {
            return existing.clone();
        }
        self.counter += 1;
        let placeholder = format!("LLMPROXY_REDACTED_{}_{}", kind.as_str(), self.counter);
        self.assigned
            .insert(original.to_string(), placeholder.clone());
        if self.config.rehydrate && is_splice_safe(original) {
            self.restore
                .push((placeholder.clone(), original.to_string()));
        }
        placeholder
    }

    fn finish(self) -> Redaction {
        Redaction {
            issued: self.counter,
            restore: self.restore,
        }
    }
}

/// Whether a value can be put back into a response body without knowing
/// whether the placeholder sits inside a JSON string.
///
/// Anything needing escaping — quotes, backslashes, newlines — would corrupt
/// either the JSON or the prose depending on where it landed, so it is
/// redacted and left redacted.
fn is_splice_safe(original: &str) -> bool {
    original
        .chars()
        .all(|character| character != '"' && character != '\\' && !character.is_control())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    start: usize,
    end: usize,
    kind: SecretKind,
}

/// Vendor prefixes that make a token unambiguous. A prefix match plus a run of
/// token characters is a far lower false-positive rate than entropy scoring,
/// and a false positive here silently damages a prompt.
const KEY_PREFIXES: &[&str] = &[
    "sk-ant-",
    "sk-proj-",
    "sk-live-",
    "sk_live_",
    "rk_live_",
    "sk-",
    "github_pat_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "xapp-",
    "npm_",
    "dop_v1_",
    "AKIA",
    "ASIA",
    "AIza",
];

/// Assignment keys whose value is a secret whatever it looks like.
const SECRET_KEYS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "client_secret",
    "api_key",
    "apikey",
    "access_key",
    "access_token",
    "auth_token",
    "private_key",
];

const MIN_KEY_BODY: usize = 12;
const MIN_BEARER_LEN: usize = 20;
const MIN_ASSIGNED_LEN: usize = 8;

fn detect(text: &str, extra_literals: &[String]) -> Vec<Span> {
    let mut spans = Vec::new();
    detect_private_keys(text, &mut spans);
    detect_prefixed_keys(text, &mut spans);
    detect_jwts(text, &mut spans);
    detect_bearer_tokens(text, &mut spans);
    detect_assignments(text, &mut spans);
    detect_emails(text, &mut spans);
    for literal in extra_literals {
        let mut from = 0;
        while let Some(offset) = text[from..].find(literal.as_str()) {
            let start = from + offset;
            spans.push(Span {
                start,
                end: start + literal.len(),
                kind: SecretKind::Literal,
            });
            from = start + literal.len();
        }
    }
    // Longest match at each position wins, and overlaps are dropped, so a
    // token inside a private key block is not redacted twice.
    spans.sort_by(|left, right| {
        left.start
            .cmp(&right.start)
            .then_with(|| right.end.cmp(&left.end))
    });
    let mut merged: Vec<Span> = Vec::new();
    for span in spans {
        if merged.last().is_some_and(|last| span.start < last.end) {
            continue;
        }
        merged.push(span);
    }
    merged
}

fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')
}

fn token_run(bytes: &[u8], from: usize) -> usize {
    let mut end = from;
    while end < bytes.len() && is_token_byte(bytes[end]) {
        end += 1;
    }
    end
}

fn detect_prefixed_keys(text: &str, spans: &mut Vec<Span>) {
    let bytes = text.as_bytes();
    for prefix in KEY_PREFIXES {
        let mut from = 0;
        while let Some(offset) = text[from..].find(prefix) {
            let start = from + offset;
            // A prefix in the middle of a longer word is not a key.
            let preceded = start > 0 && is_token_byte(bytes[start - 1]);
            let end = token_run(bytes, start + prefix.len());
            if !preceded && end - (start + prefix.len()) >= MIN_KEY_BODY {
                spans.push(Span {
                    start,
                    end,
                    kind: SecretKind::Key,
                });
            }
            from = start + prefix.len();
        }
    }
}

fn detect_jwts(text: &str, spans: &mut Vec<Span>) {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(offset) = text[from..].find("eyJ") {
        let start = from + offset;
        from = start + 3;
        if start > 0 && is_token_byte(bytes[start - 1]) {
            continue;
        }
        let mut end = start;
        let mut segments = 0;
        loop {
            let segment_end = token_run(bytes, end);
            if segment_end == end {
                break;
            }
            segments += 1;
            end = segment_end;
            if end < bytes.len() && bytes[end] == b'.' {
                end += 1;
                continue;
            }
            break;
        }
        if segments >= 3 {
            spans.push(Span {
                start,
                end,
                kind: SecretKind::Jwt,
            });
        }
    }
}

fn detect_bearer_tokens(text: &str, spans: &mut Vec<Span>) {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(offset) = text[from..].find("Bearer ") {
        let start = from + offset + "Bearer ".len();
        let end = token_run(bytes, start);
        if end - start >= MIN_BEARER_LEN {
            spans.push(Span {
                start,
                end,
                kind: SecretKind::Token,
            });
        }
        from = start;
    }
}

fn detect_private_keys(text: &str, spans: &mut Vec<Span>) {
    const BEGIN: &str = "-----BEGIN";
    const END: &str = "-----END";
    let mut from = 0;
    while let Some(offset) = text[from..].find(BEGIN) {
        let start = from + offset;
        let Some(header_end) = text[start..].find("KEY-----") else {
            break;
        };
        let Some(end_offset) = text[start..].find(END) else {
            break;
        };
        let Some(tail) = text[start + end_offset..].find("-----\n").or_else(|| {
            text[start + end_offset..]
                .rfind("-----")
                .map(|index| index + "-----".len() - "-----\n".len())
        }) else {
            break;
        };
        let _ = header_end;
        let end = (start + end_offset + tail + "-----\n".len()).min(text.len());
        spans.push(Span {
            start,
            end,
            kind: SecretKind::PrivateKey,
        });
        from = end;
    }
}

fn detect_assignments(text: &str, spans: &mut Vec<Span>) {
    let lowered = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    for key in SECRET_KEYS {
        let mut from = 0;
        while let Some(offset) = lowered[from..].find(key) {
            let start = from + offset;
            from = start + key.len();
            if start > 0 && is_token_byte(bytes[start - 1]) {
                continue;
            }
            let mut cursor = start + key.len();
            // Allow `key = `, `key: `, `key":"`, and quoted key names.
            while cursor < bytes.len() && matches!(bytes[cursor], b'"' | b'\'' | b' ' | b'\t') {
                cursor += 1;
            }
            if cursor >= bytes.len() || !matches!(bytes[cursor], b'=' | b':') {
                continue;
            }
            cursor += 1;
            while cursor < bytes.len() && matches!(bytes[cursor], b' ' | b'\t') {
                cursor += 1;
            }
            let quoted = cursor < bytes.len() && matches!(bytes[cursor], b'"' | b'\'');
            if quoted {
                let quote = bytes[cursor];
                let value_start = cursor + 1;
                let Some(length) = bytes[value_start..].iter().position(|byte| *byte == quote)
                else {
                    continue;
                };
                if length >= MIN_ASSIGNED_LEN {
                    spans.push(Span {
                        start: value_start,
                        end: value_start + length,
                        kind: SecretKind::Token,
                    });
                }
            } else {
                let end = token_run(bytes, cursor);
                if end - cursor >= MIN_ASSIGNED_LEN {
                    spans.push(Span {
                        start: cursor,
                        end,
                        kind: SecretKind::Token,
                    });
                }
            }
        }
    }
}

fn detect_emails(text: &str, spans: &mut Vec<Span>) {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(offset) = text[from..].find('@') {
        let at = from + offset;
        from = at + 1;
        let local_ok =
            |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+');
        let domain_ok = |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-');
        let mut start = at;
        while start > 0 && local_ok(bytes[start - 1]) {
            start -= 1;
        }
        let mut end = at + 1;
        while end < bytes.len() && domain_ok(bytes[end]) {
            end += 1;
        }
        // Trim a trailing dot so "write to a@b.com." keeps its sentence.
        while end > at + 1 && bytes[end - 1] == b'.' {
            end -= 1;
        }
        let domain = &text[at + 1..end];
        if start < at && domain.contains('.') && domain.len() >= 4 {
            spans.push(Span {
                start,
                end,
                kind: SecretKind::Email,
            });
        }
    }
}

/// Puts originals back into a response body as it streams.
///
/// A placeholder can straddle a chunk boundary, so the tail of each chunk is
/// held back until enough bytes have arrived to rule out a partial match.
pub(crate) struct Rehydrator {
    restore: Vec<(String, String)>,
    hold: usize,
    carry: String,
}

impl Rehydrator {
    pub(crate) fn new(redaction: &Redaction) -> Option<Self> {
        if redaction.restore.is_empty() {
            return None;
        }
        Some(Self {
            hold: redaction.max_placeholder_len().saturating_sub(1),
            restore: redaction.restore.clone(),
            carry: String::new(),
        })
    }

    /// Feed one chunk; returns the text safe to emit now.
    pub(crate) fn push(&mut self, chunk: &str) -> String {
        self.carry.push_str(chunk);
        let pending = std::mem::take(&mut self.carry);
        let mut replaced = self.replace_all(pending);
        // Keep back enough of the tail that a split placeholder can still be
        // matched once the rest of it arrives.
        let split = floor_char_boundary(&replaced, replaced.len().saturating_sub(self.hold));
        self.carry = replaced.split_off(split);
        replaced
    }

    /// Flush whatever is still held back.
    pub(crate) fn finish(&mut self) -> String {
        let pending = std::mem::take(&mut self.carry);
        self.replace_all(pending)
    }

    fn replace_all(&self, mut text: String) -> String {
        for (placeholder, original) in &self.restore {
            if text.contains(placeholder.as_str()) {
                text = text.replace(placeholder.as_str(), original);
            }
        }
        text
    }
}

/// Wrap a response so placeholders become their originals as the body streams.
///
/// The body length changes, so `content-length` is dropped: the response is
/// re-framed as a stream rather than left claiming a size it no longer has.
pub(crate) fn rehydrate_response(
    response: axum::response::Response,
    redaction: &Redaction,
) -> axum::response::Response {
    let Some(mut rehydrator) = Rehydrator::new(redaction) else {
        return response;
    };
    let (mut parts, body) = response.into_parts();
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    let mut upstream = body.into_data_stream();
    let stream = async_stream::stream! {
        // Placeholders are ASCII, but the surrounding body is not, so bytes
        // are only handed to the text rehydrator once they form valid UTF-8.
        let mut pending: Vec<u8> = Vec::new();
        while let Some(chunk) = futures_util::StreamExt::next(&mut upstream).await {
            match chunk {
                Ok(bytes) => {
                    pending.extend_from_slice(&bytes);
                    let valid = match std::str::from_utf8(&pending) {
                        Ok(text) => text.len(),
                        Err(error) => error.valid_up_to(),
                    };
                    let text = String::from_utf8_lossy(&pending[..valid]).into_owned();
                    pending.drain(..valid);
                    let restored = rehydrator.push(&text);
                    if !restored.is_empty() {
                        yield Ok(bytes::Bytes::from(restored));
                    }
                }
                Err(error) => yield Err(error),
            }
        }
        let tail = rehydrator.finish();
        if !tail.is_empty() {
            yield Ok(bytes::Bytes::from(tail));
        }
    };
    axum::response::Response::from_parts(parts, axum::body::Body::from_stream(stream))
}

/// `str::floor_char_boundary` is unstable, and slicing a multi-byte character
/// in half would panic.
fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
#[path = "redact_tests.rs"]
mod tests;
