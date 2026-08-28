//! In-band routing verbs.
//!
//! Agent CLIs give the user no way to set a per-message header, so the only
//! channel back into routing is the message text itself. A small closed set of
//! prefixes at the very start of the newest user message is recognised, acted
//! on, and stripped before anything is forwarded.
//!
//! Two properties matter and are easy to get wrong:
//!
//! - **Both copies.** Detection happens on the caller's native body and the
//!   local triage copy is built from the stripped result, so a prefix reaches
//!   neither the provider nor the triage model.
//! - **Stale prefixes.** Earlier turns keep their prefixes in the conversation
//!   the client replays. Those are stripped too, or the triage model reads an
//!   old directive as if it were current.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::interceptor::IntegratedTarget;

/// Fixed verbs. Target names are recognised in addition to these, so a target
/// may not be named after one of them.
pub(crate) const RESERVED_VERBS: [&str; 7] = [
    "triage", "unstick", "local", "default", "remember", "forget", "recall",
];

/// Longest token treated as a possible verb. Target names are capped at 64
/// characters, so nothing longer can match.
const MAX_VERB_LEN: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RoutingVerb {
    /// Ignore the sticky entry and re-run triage for this turn.
    Triage,
    /// Drop the sticky entry entirely, then re-run triage.
    Unstick,
    /// Answer this turn locally; routing tools are withheld.
    Local,
    /// Take the catch-all route without paying for triage.
    Default,
    /// Pin this configured route directly: deterministic and triage-free.
    Target(String),
    /// Record the rest of this message in the project's shared memory.
    Remember,
    /// Clear the project's shared memory.
    Forget,
    /// Show what the project's shared memory currently holds.
    Recall,
}

impl RoutingVerb {
    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::Triage => "triage",
            Self::Unstick => "unstick",
            Self::Local => "local",
            Self::Default => "default",
            Self::Remember => "remember",
            Self::Forget => "forget",
            Self::Recall => "recall",
            Self::Target(name) => name,
        }
    }

    fn from_token(token: &str, targets: &BTreeMap<String, IntegratedTarget>) -> Option<Self> {
        // Target names are matched exactly because they are already
        // constrained to tool-name-safe characters; the fixed verbs are
        // matched case-insensitively because a user types them by hand.
        if targets.contains_key(token) {
            return Some(Self::Target(token.to_string()));
        }
        match token.to_ascii_lowercase().as_str() {
            "triage" => Some(Self::Triage),
            "unstick" => Some(Self::Unstick),
            "local" => Some(Self::Local),
            "default" => Some(Self::Default),
            "remember" => Some(Self::Remember),
            "forget" => Some(Self::Forget),
            "recall" => Some(Self::Recall),
            _ => None,
        }
    }
}

/// Recognise a verb on the newest user message and strip every recognised
/// prefix from the whole conversation.
///
/// Returns `None` when the newest user message carries no verb, in which case
/// stale prefixes in earlier turns have still been removed.
pub(crate) fn detect_and_strip(
    body: &mut Value,
    targets: &BTreeMap<String, IntegratedTarget>,
) -> Option<RoutingVerb> {
    let items = user_message_indexes(body)?;
    let (&newest, earlier) = items.split_last()?;
    for &index in earlier {
        // Stale directives are removed but never acted on.
        strip_at(body, index, targets);
    }
    strip_at(body, newest, targets)
}

/// Indexes into whichever array holds the conversation: `messages` for Chat
/// Completions and Anthropic Messages, `input` for Responses.
fn user_message_indexes(body: &Value) -> Option<Vec<usize>> {
    let list = conversation_array(body)?;
    Some(
        list.iter()
            .enumerate()
            .filter(|(_, item)| item.get("role").and_then(Value::as_str) == Some("user"))
            .map(|(index, _)| index)
            .collect(),
    )
}

fn conversation_array(body: &Value) -> Option<&Vec<Value>> {
    body.get("messages")
        .and_then(Value::as_array)
        .or_else(|| body.get("input").and_then(Value::as_array))
}

fn conversation_array_mut(body: &mut Value) -> Option<&mut Vec<Value>> {
    if body.get("messages").and_then(Value::as_array).is_some() {
        return body.get_mut("messages").and_then(Value::as_array_mut);
    }
    body.get_mut("input").and_then(Value::as_array_mut)
}

fn strip_at(
    body: &mut Value,
    index: usize,
    targets: &BTreeMap<String, IntegratedTarget>,
) -> Option<RoutingVerb> {
    let list = conversation_array_mut(body)?;
    let content = list.get_mut(index)?.get_mut("content")?;
    let text = leading_text_mut(content)?;
    let (verb, rewritten) = strip_prefix(text, targets)?;
    *text = rewritten;
    verb
}

/// The first text-carrying slot of a message body, whether it is a bare string
/// or the first text part of a multimodal array.
fn leading_text_mut(content: &mut Value) -> Option<&mut String> {
    match content {
        Value::String(text) => Some(text),
        Value::Array(parts) => parts.iter_mut().find_map(|part| match part {
            Value::String(text) => Some(text),
            Value::Object(fields) => {
                let is_text = fields
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind == "text" || kind == "input_text");
                if !is_text {
                    return None;
                }
                match fields.get_mut("text") {
                    Some(Value::String(text)) => Some(text),
                    _ => None,
                }
            }
            _ => None,
        }),
        _ => None,
    }
}

/// Returns `(verb, rewritten_text)` when the text starts with a recognised
/// prefix or its escape, and `None` when it should be left exactly as it is.
///
/// The escape is a leading backslash: `\triage: ...` sends a literal
/// `triage: ...` and consumes only the backslash.
fn strip_prefix(
    text: &str,
    targets: &BTreeMap<String, IntegratedTarget>,
) -> Option<(Option<RoutingVerb>, String)> {
    let leading_ws = text.len() - text.trim_start().len();
    let (indent, rest) = text.split_at(leading_ws);
    if let Some(escaped) = rest.strip_prefix('\\') {
        // Only consume the escape when it actually protects a verb; otherwise
        // a message starting with a backslash is left untouched.
        return parse_token(escaped, targets).map(|_| (None, format!("{indent}{escaped}")));
    }
    let (verb, remainder) = parse_token(rest, targets)?;
    Some((Some(verb), format!("{indent}{remainder}")))
}

/// Match `token:` at the very start, returning the verb and the text after it.
fn parse_token<'a>(
    rest: &'a str,
    targets: &BTreeMap<String, IntegratedTarget>,
) -> Option<(RoutingVerb, &'a str)> {
    let colon = rest.find(':')?;
    let token = &rest[..colon];
    if token.is_empty() || token.len() > MAX_VERB_LEN {
        return None;
    }
    if !token
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return None;
    }
    let verb = RoutingVerb::from_token(token, targets)?;
    Some((verb, rest[colon + 1..].trim_start()))
}

#[cfg(test)]
#[path = "routing_verbs_tests.rs"]
mod tests;
