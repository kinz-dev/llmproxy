//! In-memory proxy statistics for the admin interface.
//!
//! One global [`ProxyStats`] instance collects counters, latency sample
//! windows, and approximate token savings. Everything resets on restart.

use std::collections::VecDeque;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::response::Response;
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::is_event_stream_content_type;

/// Whether a response was produced by a local model (loopback/LAN provider or
/// the interceptor) or forwarded to an external provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteClass {
    Local,
    External,
}

impl RouteClass {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::External => "external",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemapKind {
    Alias,
    Auto,
}

const LATENCY_WINDOW_CAP: usize = 2048;

#[derive(Default)]
struct LatencyWindow {
    samples: Mutex<VecDeque<u32>>,
}

impl LatencyWindow {
    fn record(&self, elapsed: Duration) {
        let ms = elapsed.as_millis().min(u128::from(u32::MAX)) as u32;
        let mut samples = self.samples.lock().expect("latency window poisoned");
        if samples.len() == LATENCY_WINDOW_CAP {
            samples.pop_front();
        }
        samples.push_back(ms);
    }

    /// Nearest-rank p50/p99 over the sample window, with the sample count.
    fn percentiles(&self) -> (Option<u32>, Option<u32>, usize) {
        let samples = self.samples.lock().expect("latency window poisoned");
        if samples.is_empty() {
            return (None, None, 0);
        }
        let mut sorted = samples.iter().copied().collect::<Vec<_>>();
        sorted.sort_unstable();
        let rank = |q: f64| {
            let index = ((q * sorted.len() as f64).ceil() as usize).max(1) - 1;
            sorted[index.min(sorted.len() - 1)]
        };
        (Some(rank(0.50)), Some(rank(0.99)), sorted.len())
    }
}

#[derive(Default)]
pub(crate) struct ProxyStats {
    started_at: OnceLock<Instant>,
    requests_total: AtomicU64,
    remap_alias: AtomicU64,
    remap_auto: AtomicU64,
    intercept_attempts: AtomicU64,
    intercept_bypass: AtomicU64,
    intercept_local: AtomicU64,
    intercept_pass: AtomicU64,
    intercept_fallback: AtomicU64,
    intercept_errors: AtomicU64,
    served_local: AtomicU64,
    served_external: AtomicU64,
    tokens_saved_prompt: AtomicU64,
    tokens_saved_completion: AtomicU64,
    tokens_saved_estimated: AtomicU64,
    latency_local: LatencyWindow,
    latency_external: LatencyWindow,
}

pub(crate) fn proxy_stats() -> &'static ProxyStats {
    static STATS: OnceLock<ProxyStats> = OnceLock::new();
    STATS.get_or_init(ProxyStats::default)
}

impl ProxyStats {
    /// Anchor the uptime clock; the first caller wins.
    pub(crate) fn touch_start(&self) {
        let _ = self.started_at.get_or_init(Instant::now);
    }

    pub(crate) fn record_request(&self) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_remap(&self, kind: RemapKind) {
        match kind {
            RemapKind::Alias => self.remap_alias.fetch_add(1, Ordering::Relaxed),
            RemapKind::Auto => self.remap_auto.fetch_add(1, Ordering::Relaxed),
        };
    }

    pub(crate) fn record_intercept_attempt(&self) {
        self.intercept_attempts.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_intercept_bypass(&self) {
        self.intercept_bypass.fetch_add(1, Ordering::Relaxed);
    }

    /// The interceptor answered locally: counts as a local serve and records
    /// the local completion latency plus exact usage when present.
    pub(crate) fn record_intercept_local(&self, elapsed: Duration, usage: Option<&Value>) {
        self.intercept_local.fetch_add(1, Ordering::Relaxed);
        self.served_local.fetch_add(1, Ordering::Relaxed);
        self.latency_local.record(elapsed);
        if let Some((prompt, completion)) = usage.and_then(usage_tokens) {
            self.record_tokens_saved(prompt, completion, false);
        }
    }

    pub(crate) fn record_intercept_pass(&self) {
        self.intercept_pass.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_intercept_fallback(&self) {
        self.intercept_fallback.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_intercept_error(&self) {
        self.intercept_errors.fetch_add(1, Ordering::Relaxed);
    }

    fn record_served(&self, class: RouteClass, elapsed: Duration) {
        match class {
            RouteClass::Local => {
                self.served_local.fetch_add(1, Ordering::Relaxed);
                self.latency_local.record(elapsed);
            }
            RouteClass::External => {
                self.served_external.fetch_add(1, Ordering::Relaxed);
                self.latency_external.record(elapsed);
            }
        }
    }

    fn record_tokens_saved(&self, prompt: u64, completion: u64, estimated: bool) {
        self.tokens_saved_prompt
            .fetch_add(prompt, Ordering::Relaxed);
        self.tokens_saved_completion
            .fetch_add(completion, Ordering::Relaxed);
        if estimated {
            self.tokens_saved_estimated
                .fetch_add(prompt + completion, Ordering::Relaxed);
        }
    }

    pub(crate) fn reset(&self) {
        for counter in [
            &self.requests_total,
            &self.remap_alias,
            &self.remap_auto,
            &self.intercept_attempts,
            &self.intercept_bypass,
            &self.intercept_local,
            &self.intercept_pass,
            &self.intercept_fallback,
            &self.intercept_errors,
            &self.served_local,
            &self.served_external,
            &self.tokens_saved_prompt,
            &self.tokens_saved_completion,
            &self.tokens_saved_estimated,
        ] {
            counter.store(0, Ordering::Relaxed);
        }
        for window in [&self.latency_local, &self.latency_external] {
            window
                .samples
                .lock()
                .expect("latency window poisoned")
                .clear();
        }
    }

    /// Stable snake_case keys: the admin UI treats this shape as a contract.
    pub(crate) fn snapshot_json(&self) -> Value {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        let latency = |window: &LatencyWindow| {
            let (p50, p99, samples) = window.percentiles();
            json!({ "p50_ms": p50, "p99_ms": p99, "samples": samples })
        };
        let saved_prompt = load(&self.tokens_saved_prompt);
        let saved_completion = load(&self.tokens_saved_completion);
        json!({
            "uptime_secs": self.started_at.get().map(Instant::elapsed).unwrap_or_default().as_secs(),
            "requests": {
                "total": load(&self.requests_total),
                "remap_alias": load(&self.remap_alias),
                "remap_auto": load(&self.remap_auto),
                "remapped": load(&self.remap_alias) + load(&self.remap_auto),
            },
            "intercept": {
                "attempts": load(&self.intercept_attempts),
                "bypass": load(&self.intercept_bypass),
                "local": load(&self.intercept_local),
                "pass": load(&self.intercept_pass),
                "fallback": load(&self.intercept_fallback),
                "errors": load(&self.intercept_errors),
            },
            "served": {
                "local": load(&self.served_local),
                "external": load(&self.served_external),
            },
            "latency": {
                "local": latency(&self.latency_local),
                "external": latency(&self.latency_external),
            },
            "tokens_saved": {
                "prompt": saved_prompt,
                "completion": saved_completion,
                "total": saved_prompt + saved_completion,
                "estimated": load(&self.tokens_saved_estimated),
                "approximate": true,
            },
        })
    }
}

/// Classify a provider by its base URL host. Loopback, RFC1918/link-local,
/// unique-local IPv6, and obviously-LAN hostnames count as local.
pub(crate) fn route_class_for_base_url(base_url: &str) -> RouteClass {
    if base_url_is_local(base_url) {
        RouteClass::Local
    } else {
        RouteClass::External
    }
}

fn base_url_is_local(base_url: &str) -> bool {
    let rest = base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or("")
    } else {
        authority
            .rsplit_once(':')
            .map_or(authority, |(host, port)| {
                // Only treat the suffix as a port if it is numeric; otherwise the
                // colon belonged to an unbracketed IPv6 literal.
                if port.chars().all(|c| c.is_ascii_digit()) {
                    host
                } else {
                    authority
                }
            })
    };
    if host.is_empty() {
        return false;
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(v4) => {
                v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
            }
            IpAddr::V6(v6) => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    || (v6.segments()[0] & 0xfe00) == 0xfc00 // unique local fc00::/7
                    || (v6.segments()[0] & 0xffc0) == 0xfe80 // link local fe80::/10
            }
        };
    }

    let host = host.to_ascii_lowercase();
    host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".lan")
        || host.ends_with(".internal")
        || host.ends_with(".home.arpa")
        || !host.contains('.') // bare intranet hostname
}

/// Wrap a successful proxied response so that, when its body finishes
/// streaming, the request is counted with its full duration. Local-class
/// bodies are additionally sniffed for `usage` to tally saved tokens.
pub(crate) fn track_response(class: RouteClass, started: Instant, response: Response) -> Response {
    if !response.status().is_success() {
        log_response_text(class, format!("HTTP {}", response.status()));
        return response;
    }
    let (parts, body) = response.into_parts();
    let is_sse = parts
        .headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_event_stream_content_type);
    let mut sniffer = (class == RouteClass::Local).then(|| UsageSniffer::new(is_sse));

    let mut upstream = body.into_data_stream();
    let tracked = async_stream::stream! {
        let mut logged_first_chunk = false;
        while let Some(chunk) = upstream.next().await {
            if let Ok(bytes) = &chunk {
                if let Some(sniffer) = sniffer.as_mut() {
                    sniffer.feed(bytes);
                }
                if !logged_first_chunk && !bytes.is_empty() {
                    logged_first_chunk = true;
                    if traffic_log().enabled() {
                        let head = &bytes[..bytes.len().min(TRAFFIC_PREVIEW_CHARS * 2)];
                        log_response_text(class, String::from_utf8_lossy(head).into_owned());
                    }
                }
            }
            yield chunk;
        }
        proxy_stats().record_served(class, started.elapsed());
        if let Some(sniffer) = sniffer.take() {
            let (prompt, completion, estimated) = sniffer.finish();
            if prompt > 0 || completion > 0 {
                proxy_stats().record_tokens_saved(prompt, completion, estimated);
            }
        }
    };
    Response::from_parts(parts, Body::from_stream(tracked))
}

const SNIFFER_TAIL_CAP: usize = 16 * 1024;

/// Keeps the tail of a response body and extracts the last complete `usage`
/// object once the body ends. Falls back to a size-derived estimate.
struct UsageSniffer {
    tail: Vec<u8>,
    total_bytes: u64,
    is_sse: bool,
}

impl UsageSniffer {
    fn new(is_sse: bool) -> Self {
        Self {
            tail: Vec::new(),
            total_bytes: 0,
            is_sse,
        }
    }

    fn feed(&mut self, chunk: &[u8]) {
        self.total_bytes += chunk.len() as u64;
        if chunk.len() >= SNIFFER_TAIL_CAP {
            self.tail.clear();
            self.tail
                .extend_from_slice(&chunk[chunk.len() - SNIFFER_TAIL_CAP..]);
            return;
        }
        let overflow = (self.tail.len() + chunk.len()).saturating_sub(SNIFFER_TAIL_CAP);
        if overflow > 0 {
            self.tail.drain(..overflow);
        }
        self.tail.extend_from_slice(chunk);
    }

    /// (prompt, completion, estimated)
    fn finish(self) -> (u64, u64, bool) {
        if let Some((prompt, completion)) = extract_last_usage(&self.tail) {
            return (prompt, completion, false);
        }
        // No visible usage: assume ~4 bytes/token of payload, discounted for
        // SSE event framing. Prompt tokens are unknown and left at zero.
        let divisor = if self.is_sse { 6 } else { 4 };
        (0, self.total_bytes / divisor, true)
    }
}

/// Scan backwards for the last `"usage"` key whose value is an object with
/// token counts. Streams emit `"usage":null` chunks, so earlier matches are
/// tried until one parses.
fn extract_last_usage(haystack: &[u8]) -> Option<(u64, u64)> {
    const KEY: &[u8] = b"\"usage\"";
    let mut end = haystack.len();
    for _ in 0..16 {
        let start = rfind(&haystack[..end], KEY)?;
        if let Some(usage) = parse_usage_value(&haystack[start + KEY.len()..])
            && let Some(tokens) = usage_tokens(&usage)
        {
            return Some(tokens);
        }
        end = start;
    }
    None
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
}

/// Parse the JSON object following a `"usage"` key (after `:` and whitespace).
fn parse_usage_value(bytes: &[u8]) -> Option<Value> {
    let mut index = 0;
    while index < bytes.len() && matches!(bytes[index], b':' | b' ' | b'\t' | b'\r' | b'\n') {
        index += 1;
    }
    if bytes.get(index) != Some(&b'{') {
        return None;
    }
    let start = index;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, &byte) in bytes[start..].iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_slice(&bytes[start..start + offset + 1]).ok();
                }
            }
            _ => {}
        }
    }
    None
}

/// (prompt, completion) from OpenAI- or Anthropic-style usage objects.
fn usage_tokens(usage: &Value) -> Option<(u64, u64)> {
    let field = |names: [&str; 2]| {
        names
            .iter()
            .find_map(|name| usage.get(name).and_then(Value::as_u64))
    };
    let prompt = field(["prompt_tokens", "input_tokens"]);
    let completion = field(["completion_tokens", "output_tokens"]);
    if prompt.is_none() && completion.is_none() {
        return None;
    }
    Some((prompt.unwrap_or(0), completion.unwrap_or(0)))
}

// ---------------------------------------------------------------------------
// Traffic log: a bounded, expiring feed of request/response previews for the
// admin Monitor tab. Recording is a no-op unless the admin interface enabled
// it at startup, so the proxy hot path pays nothing when admin is off.

const TRAFFIC_LOG_CAP: usize = 512;
const TRAFFIC_LOG_TTL: Duration = Duration::from_secs(10 * 60);
const TRAFFIC_PREVIEW_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LogDirection {
    Request,
    Response,
}

impl LogDirection {
    fn as_str(self) -> &'static str {
        match self {
            Self::Request => "req",
            Self::Response => "rsp",
        }
    }
}

struct TrafficLogEntry {
    id: u64,
    epoch_ms: u64,
    recorded_at: Instant,
    class: RouteClass,
    direction: LogDirection,
    preview: String,
}

#[derive(Default)]
pub(crate) struct TrafficLog {
    enabled: AtomicBool,
    next_id: AtomicU64,
    entries: Mutex<VecDeque<TrafficLogEntry>>,
}

pub(crate) fn traffic_log() -> &'static TrafficLog {
    static LOG: OnceLock<TrafficLog> = OnceLock::new();
    LOG.get_or_init(TrafficLog::default)
}

impl TrafficLog {
    /// Called once at startup when the admin interface is enabled.
    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub(crate) fn record(&self, class: RouteClass, direction: LogDirection, preview: String) {
        if !self.enabled() {
            return;
        }
        let entry = TrafficLogEntry {
            id: self.next_id.fetch_add(1, Ordering::Relaxed) + 1,
            epoch_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_millis() as u64)
                .unwrap_or_default(),
            recorded_at: Instant::now(),
            class,
            direction,
            preview,
        };
        let mut entries = self.entries.lock().expect("traffic log poisoned");
        Self::prune(&mut entries, TRAFFIC_LOG_TTL);
        if entries.len() == TRAFFIC_LOG_CAP {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    fn prune(entries: &mut VecDeque<TrafficLogEntry>, ttl: Duration) {
        while entries
            .front()
            .is_some_and(|entry| entry.recorded_at.elapsed() >= ttl)
        {
            entries.pop_front();
        }
    }

    pub(crate) fn clear(&self) {
        self.entries.lock().expect("traffic log poisoned").clear();
    }

    /// Entries newer than `after` (an id from a previous call), oldest first.
    pub(crate) fn snapshot_after(&self, after: u64) -> Value {
        let mut entries = self.entries.lock().expect("traffic log poisoned");
        Self::prune(&mut entries, TRAFFIC_LOG_TTL);
        let latest_id = entries.back().map(|entry| entry.id).unwrap_or(after);
        let items = entries
            .iter()
            .filter(|entry| entry.id > after)
            .map(|entry| {
                json!({
                    "id": entry.id,
                    "ts_ms": entry.epoch_ms,
                    "class": entry.class.as_str(),
                    "dir": entry.direction.as_str(),
                    "text": entry.preview,
                })
            })
            .collect::<Vec<_>>();
        json!({
            "entries": items,
            "latest_id": latest_id,
            "ttl_secs": TRAFFIC_LOG_TTL.as_secs(),
        })
    }
}

/// Log the JSON body of an outgoing request (bounded preview).
pub(crate) fn log_request_value(class: RouteClass, body: &Value) {
    if !traffic_log().enabled() {
        return;
    }
    traffic_log().record(class, LogDirection::Request, json_preview(body));
}

/// Log a buffered JSON response (bounded preview).
pub(crate) fn log_response_value(class: RouteClass, body: &Value) {
    if !traffic_log().enabled() {
        return;
    }
    traffic_log().record(class, LogDirection::Response, json_preview(body));
}

/// Log a response that is only known as text (errors, status lines).
pub(crate) fn log_response_text(class: RouteClass, text: impl Into<String>) {
    if !traffic_log().enabled() {
        return;
    }
    traffic_log().record(class, LogDirection::Response, flatten_preview(text.into()));
}

/// Serialize a JSON value into a bounded single-line preview without
/// allocating the full serialization of large bodies.
fn json_preview(value: &Value) -> String {
    // A little headroom over the char budget: multi-byte chars and the
    // truncation cut both happen at finish time.
    let byte_cap = TRAFFIC_PREVIEW_CHARS * 2;
    let mut writer = BoundedWriter {
        buf: Vec::with_capacity(byte_cap),
        cap: byte_cap,
    };
    // The writer aborts serialization once the cap is reached.
    let _ = serde_json::to_writer(&mut writer, value);
    flatten_preview(String::from_utf8_lossy(&writer.buf).into_owned())
}

/// Collapse to one line and truncate to the preview budget.
fn flatten_preview(text: String) -> String {
    let mut flattened = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>();
    let mut count = 0;
    for (offset, _) in flattened.char_indices() {
        count += 1;
        if count > TRAFFIC_PREVIEW_CHARS {
            flattened.truncate(offset);
            flattened.push('…');
            break;
        }
    }
    flattened
}

struct BoundedWriter {
    buf: Vec<u8>,
    cap: usize,
}

impl std::io::Write for BoundedWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let remaining = self.cap.saturating_sub(self.buf.len());
        if remaining == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "preview budget reached",
            ));
        }
        self.buf
            .extend_from_slice(&data[..remaining.min(data.len())]);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[path = "stats_tests.rs"]
mod tests;
