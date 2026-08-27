//! Provider-facing wire adapters.
//!
//! OpenAI Chat Completions is the internal interchange format. These helpers
//! translate only at the provider boundary and deliberately discard signed or
//! provider-private reasoning artifacts when a conversation crosses vendors.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

pub(crate) fn chat_to_anthropic_request(chat: &Value) -> Value {
    let Some(source) = chat.as_object() else {
        return json!({ "messages": [], "max_tokens": 4096 });
    };

    let mut request = Map::new();
    for key in ["model", "temperature", "top_p", "top_k", "stream"] {
        if let Some(value) = source.get(key) {
            request.insert(key.to_string(), value.clone());
        }
    }
    let max_tokens = source
        .get("max_tokens")
        .or_else(|| source.get("max_completion_tokens"))
        .cloned()
        .unwrap_or_else(|| json!(4096));
    request.insert("max_tokens".to_string(), max_tokens);
    if let Some(stop) = source.get("stop") {
        request.insert(
            "stop_sequences".to_string(),
            match stop {
                Value::String(_) => Value::Array(vec![stop.clone()]),
                _ => stop.clone(),
            },
        );
    }

    if let Some(tools) = source.get("tools").and_then(Value::as_array) {
        let tools = tools
            .iter()
            .filter_map(openai_tool_to_anthropic)
            .collect::<Vec<_>>();
        if !tools.is_empty() {
            request.insert("tools".to_string(), Value::Array(tools));
        }
    }
    if let Some(choice) = source
        .get("tool_choice")
        .and_then(openai_tool_choice_to_anthropic)
    {
        request.insert("tool_choice".to_string(), choice);
    }

    let mut system = Vec::new();
    let mut messages = Vec::new();
    for message in source
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(object) = message.as_object() else {
            continue;
        };
        let role = object.get("role").and_then(Value::as_str).unwrap_or("user");
        match role {
            "system" | "developer" => {
                if let Some(content) = object.get("content") {
                    system.extend(openai_content_to_anthropic(content));
                }
            }
            "assistant" => {
                let mut content = object
                    .get("content")
                    .map(openai_content_to_anthropic)
                    .unwrap_or_default();
                if let Some(tool_calls) = object.get("tool_calls").and_then(Value::as_array) {
                    content.extend(tool_calls.iter().filter_map(openai_tool_call_to_anthropic));
                }
                push_message(&mut messages, "assistant", content);
            }
            "tool" | "function" => {
                let id = object
                    .get("tool_call_id")
                    .or_else(|| object.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("tool_call");
                let content = object
                    .get("content")
                    .map(openai_content_to_anthropic)
                    .unwrap_or_default();
                push_message(
                    &mut messages,
                    "user",
                    vec![json!({
                        "type": "tool_result",
                        "tool_use_id": id,
                        "content": content
                    })],
                );
            }
            _ => {
                let content = object
                    .get("content")
                    .map(openai_content_to_anthropic)
                    .unwrap_or_default();
                push_message(&mut messages, "user", content);
            }
        }
    }
    if !system.is_empty() {
        request.insert("system".to_string(), Value::Array(system));
    }
    request.insert("messages".to_string(), Value::Array(messages));
    Value::Object(request)
}

pub(crate) fn anthropic_to_chat_response(
    anthropic: Value,
    requested_model: Option<&str>,
    created: i64,
) -> Value {
    let id = anthropic
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("message");
    let model = requested_model
        .or_else(|| anthropic.get("model").and_then(Value::as_str))
        .unwrap_or("unknown");
    let mut text = Vec::new();
    let mut tool_calls = Vec::new();
    for part in anthropic
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(value) = part.get("text").and_then(Value::as_str) {
                    text.push(value);
                }
            }
            Some("tool_use") => {
                let Some(call_id) = part.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let Some(name) = part.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let arguments = part.get("input").cloned().unwrap_or_else(|| json!({}));
                tool_calls.push(json!({
                    "id": call_id,
                    "type": "function",
                    "function": { "name": name, "arguments": arguments.to_string() }
                }));
            }
            // Thinking blocks can carry provider-signed state. Never move
            // them into another provider's conversation history.
            Some("thinking" | "redacted_thinking") => {}
            _ => {}
        }
    }

    let mut message = json!({
        "role": "assistant",
        "content": text.join("")
    });
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    let finish_reason = match anthropic.get("stop_reason").and_then(Value::as_str) {
        Some("max_tokens") => "length",
        Some("tool_use") => "tool_calls",
        _ => "stop",
    };
    let cache = AnthropicCacheTokens::from_usage(anthropic.get("usage"));
    let input_tokens = anthropic
        .get("usage")
        .and_then(|usage| usage.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = anthropic
        .get("usage")
        .and_then(|usage| usage.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason
        }],
        "usage": cache.chat_usage(input_tokens, output_tokens)
    })
}

/// Anthropic reports cache reads and cache creation outside `input_tokens`,
/// while OpenAI's `prompt_tokens` already contains its cached share. Carrying
/// the split through the chat wire keeps cache-aware spend possible without
/// teaching every consumer both conventions.
#[derive(Debug, Clone, Copy, Default)]
struct AnthropicCacheTokens {
    read: u64,
    write: u64,
}

impl AnthropicCacheTokens {
    fn from_usage(usage: Option<&Value>) -> Self {
        let field = |name: &str| {
            usage
                .and_then(|usage| usage.get(name))
                .and_then(Value::as_u64)
                .unwrap_or(0)
        };
        Self {
            read: field("cache_read_input_tokens"),
            write: field("cache_creation_input_tokens"),
        }
    }

    /// OpenAI-shaped usage whose `prompt_tokens` is the full billable prompt.
    fn chat_usage(self, input_tokens: u64, output_tokens: u64) -> Value {
        let prompt_tokens = input_tokens
            .saturating_add(self.read)
            .saturating_add(self.write);
        json!({
            "prompt_tokens": prompt_tokens,
            "completion_tokens": output_tokens,
            "total_tokens": prompt_tokens.saturating_add(output_tokens),
            "prompt_tokens_details": {
                "cached_tokens": self.read,
                "cache_creation_tokens": self.write,
            },
        })
    }
}

pub(crate) fn sanitize_copilot_chat_request(request: &mut Value) {
    sanitize_cross_provider_chat_request(request);
    let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for message in messages {
        let Some(content) = message.get_mut("content") else {
            continue;
        };
        let Value::Array(parts) = content else {
            continue;
        };
        *parts = parts
            .iter()
            .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                Some("text" | "image_url") => Some(part.clone()),
                Some("input_text" | "output_text") => part
                    .get("text")
                    .and_then(Value::as_str)
                    .map(|text| json!({ "type": "text", "text": text })),
                Some("reasoning" | "thinking" | "redacted_thinking") => None,
                _ => serialized_content_part(part)
                    .map(|text| json!({ "type": "text", "text": text })),
            })
            .collect();
    }
}

pub(crate) fn sanitize_cross_provider_chat_request(request: &mut Value) {
    strip_provider_artifacts(request);
    let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for message in messages {
        let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        parts.retain(|part| {
            !matches!(
                part.get("type").and_then(Value::as_str),
                Some("reasoning" | "thinking" | "redacted_thinking")
            )
        });
    }
}

pub(crate) fn is_claude_model(model: Option<&str>) -> bool {
    model.is_some_and(|model| model.to_ascii_lowercase().contains("claude"))
}

#[derive(Default)]
pub(crate) struct AnthropicStreamTranslator {
    id: Option<String>,
    model: Option<String>,
    created: i64,
    role_emitted: bool,
    tool_indices: HashMap<usize, usize>,
    next_tool_index: usize,
    input_tokens: u64,
    cache_tokens: AnthropicCacheTokens,
}

impl AnthropicStreamTranslator {
    pub(crate) fn new(model: Option<String>, created: i64) -> Self {
        Self {
            model,
            created,
            ..Self::default()
        }
    }

    pub(crate) fn translate(&mut self, event: &Value) -> Vec<Value> {
        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
        let mut output = Vec::new();
        if event_type == "message_start" {
            if let Some(message) = event.get("message") {
                self.id = message
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if self.model.is_none() {
                    self.model = message
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
                self.input_tokens = message
                    .get("usage")
                    .and_then(|usage| usage.get("input_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                self.cache_tokens = AnthropicCacheTokens::from_usage(message.get("usage"));
            }
            output.push(self.role_chunk());
            self.role_emitted = true;
            return output;
        }
        if !self.role_emitted && matches!(event_type, "content_block_start" | "content_block_delta")
        {
            output.push(self.role_chunk());
            self.role_emitted = true;
        }

        match event_type {
            "content_block_start" => {
                let block = event.get("content_block").unwrap_or(&Value::Null);
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            output.push(self.delta_chunk(json!({ "content": text }), None, None));
                        }
                    }
                    Some("tool_use") => {
                        let block_index =
                            event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                        let tool_index = self.next_tool_index;
                        self.next_tool_index += 1;
                        self.tool_indices.insert(block_index, tool_index);
                        output.push(self.delta_chunk(
                            json!({
                                "tool_calls": [{
                                    "index": tool_index,
                                    "id": block.get("id").and_then(Value::as_str).unwrap_or("tool_call"),
                                    "type": "function",
                                    "function": {
                                        "name": block.get("name").and_then(Value::as_str).unwrap_or("tool"),
                                        "arguments": ""
                                    }
                                }]
                            }),
                            None,
                            None,
                        ));
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let delta = event.get("delta").unwrap_or(&Value::Null);
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            output.push(self.delta_chunk(json!({ "content": text }), None, None));
                        }
                    }
                    Some("input_json_delta") => {
                        let block_index =
                            event.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                        let tool_index = self.tool_indices.get(&block_index).copied().unwrap_or(0);
                        let arguments = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        output.push(self.delta_chunk(
                            json!({
                                "tool_calls": [{
                                    "index": tool_index,
                                    "function": { "arguments": arguments }
                                }]
                            }),
                            None,
                            None,
                        ));
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                let finish = match event
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    Some("max_tokens") => "length",
                    Some("tool_use") => "tool_calls",
                    _ => "stop",
                };
                let usage = event.get("usage").map(|usage| {
                    let output = usage
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    self.cache_tokens.chat_usage(self.input_tokens, output)
                });
                output.push(self.delta_chunk(json!({}), Some(finish), usage));
            }
            _ => {}
        }
        output
    }

    fn role_chunk(&self) -> Value {
        self.delta_chunk(json!({ "role": "assistant" }), None, None)
    }

    fn delta_chunk(
        &self,
        delta: Value,
        finish_reason: Option<&str>,
        usage: Option<Value>,
    ) -> Value {
        let mut chunk = json!({
            "id": self.id.as_deref().unwrap_or("message"),
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model.as_deref().unwrap_or("unknown"),
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": finish_reason
            }]
        });
        if let Some(usage) = usage {
            chunk["usage"] = usage;
        }
        chunk
    }
}

fn openai_tool_to_anthropic(tool: &Value) -> Option<Value> {
    let function = tool.get("function").unwrap_or(tool);
    let name = function.get("name").and_then(Value::as_str)?;
    let mut converted = json!({
        "name": name,
        "input_schema": function
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object", "properties": {} }))
    });
    if let Some(description) = function.get("description").and_then(Value::as_str) {
        converted["description"] = Value::String(description.to_string());
    }
    Some(converted)
}

fn openai_tool_choice_to_anthropic(choice: &Value) -> Option<Value> {
    match choice {
        Value::String(value) => match value.as_str() {
            "auto" => Some(json!({ "type": "auto" })),
            "required" => Some(json!({ "type": "any" })),
            "none" => Some(json!({ "type": "none" })),
            _ => None,
        },
        Value::Object(object) => object
            .get("function")
            .and_then(|function| function.get("name"))
            .or_else(|| object.get("name"))
            .and_then(Value::as_str)
            .map(|name| json!({ "type": "tool", "name": name })),
        _ => None,
    }
}

fn openai_tool_call_to_anthropic(call: &Value) -> Option<Value> {
    let id = call.get("id").and_then(Value::as_str)?;
    let function = call.get("function")?;
    let name = function.get("name").and_then(Value::as_str)?;
    let input = function
        .get("arguments")
        .and_then(Value::as_str)
        .and_then(|arguments| serde_json::from_str::<Value>(arguments).ok())
        .unwrap_or_else(|| json!({}));
    Some(json!({ "type": "tool_use", "id": id, "name": name, "input": input }))
}

fn openai_content_to_anthropic(content: &Value) -> Vec<Value> {
    match content {
        Value::String(text) => (!text.is_empty())
            .then(|| json!({ "type": "text", "text": text }))
            .into_iter()
            .collect(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(openai_content_part_to_anthropic)
            .collect(),
        Value::Null => Vec::new(),
        other => vec![json!({ "type": "text", "text": other.to_string() })],
    }
}

fn openai_content_part_to_anthropic(part: &Value) -> Option<Value> {
    let kind = part.get("type").and_then(Value::as_str);
    match kind {
        Some("text" | "input_text" | "output_text") => part
            .get("text")
            .and_then(Value::as_str)
            .map(|text| json!({ "type": "text", "text": text })),
        Some("image_url") => {
            let image_url = part.get("image_url")?;
            let url = image_url
                .as_str()
                .or_else(|| image_url.get("url").and_then(Value::as_str))?;
            Some(json!({
                "type": "image",
                "source": { "type": "url", "url": url }
            }))
        }
        Some("reasoning" | "thinking" | "redacted_thinking") => None,
        _ => part
            .get("text")
            .and_then(Value::as_str)
            .map(|text| json!({ "type": "text", "text": text })),
    }
}

fn push_message(messages: &mut Vec<Value>, role: &str, mut content: Vec<Value>) {
    if content.is_empty() {
        content.push(json!({ "type": "text", "text": "" }));
    }
    if let Some(last) = messages.last_mut()
        && last.get("role").and_then(Value::as_str) == Some(role)
        && let Some(existing) = last.get_mut("content").and_then(Value::as_array_mut)
    {
        existing.extend(content);
        return;
    }
    messages.push(json!({ "role": role, "content": content }));
}

fn strip_provider_artifacts(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for key in [
                "reasoning",
                "reasoning_content",
                "thinking",
                "signature",
                "encrypted_content",
                "cache_control",
            ] {
                object.remove(key);
            }
            for child in object.values_mut() {
                strip_provider_artifacts(child);
            }
        }
        Value::Array(values) => {
            for child in values {
                strip_provider_artifacts(child);
            }
        }
        _ => {}
    }
}

fn serialized_content_part(part: &Value) -> Option<String> {
    part.get("text")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            part.get("content").map(|content| match content {
                Value::String(content) => content.clone(),
                _ => content.to_string(),
            })
        })
        .or_else(|| (!part.is_null()).then(|| part.to_string()))
        .filter(|text| !text.is_empty())
}

/// Claude Code's identity line. Anthropic accepts subscription OAuth tokens
/// only when the request presents as Claude Code, which means the first system
/// block must be this exact string.
pub(crate) const CLAUDE_CODE_IDENTITY: &str =
    "You are Claude Code, Anthropic's official CLI for Claude.";

/// Ensure an Anthropic Messages request leads with the Claude Code identity
/// system block, as required for borrowed subscription OAuth. Idempotent.
pub(crate) fn ensure_claude_code_identity(anthropic_request: &mut Value) {
    let identity = json!({ "type": "text", "text": CLAUDE_CODE_IDENTITY });
    let Some(object) = anthropic_request.as_object_mut() else {
        return;
    };
    match object.get_mut("system") {
        Some(Value::Array(blocks)) => {
            let already = blocks.first().and_then(|block| block.get("text"))
                == Some(&json!(CLAUDE_CODE_IDENTITY));
            if !already {
                blocks.insert(0, identity);
            }
        }
        Some(Value::String(text)) => {
            let existing = text.clone();
            let mut blocks = vec![identity];
            if !existing.is_empty() && existing != CLAUDE_CODE_IDENTITY {
                blocks.push(json!({ "type": "text", "text": existing }));
            }
            object.insert("system".to_string(), Value::Array(blocks));
        }
        _ => {
            object.insert("system".to_string(), json!([identity]));
        }
    }
}

// ---------------------------------------------------------------------------
// Codex backend (ChatGPT subscription). The backend speaks the Responses API
// and is stream-only. These translate our internal Chat Completions shape into
// a Responses request, and fold the Responses SSE reply back into a single
// Chat Completions object the rest of the proxy already knows how to relay.

const CODEX_DEFAULT_INSTRUCTIONS: &str =
    "You are a coding agent running in llmproxy, routed to the ChatGPT Codex backend.";

/// Build a Codex `/responses` request body from an internal chat request.
pub(crate) fn chat_to_codex_responses_request(chat: &Value) -> Value {
    let source = chat.as_object().cloned().unwrap_or_default();

    let mut instructions = String::new();
    let mut input = Vec::new();
    for message in source
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        // The first system/developer message becomes the Responses instructions
        // (the cacheable prefix); later ones fold in as developer input items.
        if (role == "system" || role == "developer") && instructions.is_empty() {
            instructions = flatten_text(message.get("content"));
            continue;
        }
        // Assistant tool calls become function_call items; tool results become
        // function_call_output items; everything else is a message item.
        if role == "assistant"
            && let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array)
        {
            let text = flatten_text(message.get("content"));
            if !text.is_empty() {
                input.push(message_item("assistant", "output_text", &text));
            }
            for call in tool_calls {
                if let Some(item) = chat_tool_call_to_function_call(call) {
                    input.push(item);
                }
            }
            continue;
        }
        if role == "tool" {
            let call_id = message
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            input.push(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": flatten_text(message.get("content")),
            }));
            continue;
        }
        let part_type = if role == "assistant" {
            "output_text"
        } else {
            "input_text"
        };
        let mapped_role = if role == "assistant" {
            "assistant"
        } else {
            "user"
        };
        input.push(message_item(
            mapped_role,
            part_type,
            &flatten_text(message.get("content")),
        ));
    }

    if instructions.trim().is_empty() {
        instructions = CODEX_DEFAULT_INSTRUCTIONS.to_string();
    }
    if input.is_empty() {
        input.push(message_item("user", "input_text", "..."));
    }

    let mut request = Map::new();
    if let Some(model) = source.get("model") {
        request.insert("model".to_string(), model.clone());
    }
    request.insert("instructions".to_string(), json!(instructions));
    request.insert("input".to_string(), Value::Array(input));
    request.insert("stream".to_string(), json!(true));
    request.insert("store".to_string(), json!(false));
    request.insert(
        "reasoning".to_string(),
        json!({ "effort": "medium", "summary": "auto" }),
    );
    request.insert(
        "include".to_string(),
        json!(["reasoning.encrypted_content"]),
    );

    if let Some(tools) = source.get("tools").and_then(Value::as_array) {
        let tools = tools
            .iter()
            .filter_map(chat_tool_to_codex_tool)
            .collect::<Vec<_>>();
        if !tools.is_empty() {
            request.insert("tools".to_string(), Value::Array(tools));
        }
    }
    if let Some(choice) = source.get("tool_choice") {
        request.insert("tool_choice".to_string(), choice.clone());
    }

    Value::Object(request)
}

fn message_item(role: &str, part_type: &str, text: &str) -> Value {
    json!({
        "type": "message",
        "role": role,
        "content": [{ "type": part_type, "text": text }],
    })
}

fn chat_tool_call_to_function_call(call: &Value) -> Option<Value> {
    let function = call.get("function")?;
    let name = function.get("name").and_then(Value::as_str)?;
    let arguments = function
        .get("arguments")
        .map(|arguments| match arguments {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .unwrap_or_else(|| "{}".to_string());
    let call_id = call
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("call_0")
        .to_string();
    Some(json!({
        "type": "function_call",
        "call_id": call_id,
        "name": name,
        "arguments": arguments,
    }))
}

fn chat_tool_to_codex_tool(tool: &Value) -> Option<Value> {
    let function = tool.get("function").or(Some(tool))?;
    let name = function.get("name").and_then(Value::as_str)?;
    let mut codex = Map::new();
    codex.insert("type".to_string(), json!("function"));
    codex.insert("name".to_string(), json!(name));
    if let Some(description) = function.get("description") {
        codex.insert("description".to_string(), description.clone());
    }
    let parameters = function
        .get("parameters")
        .cloned()
        .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
    codex.insert("parameters".to_string(), parameters);
    Some(Value::Object(codex))
}

/// Fold a full Codex Responses SSE body into one Chat Completions object.
pub(crate) fn codex_sse_to_chat_completion(sse_body: &str, model: &str) -> Value {
    let mut text = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut usage: Option<Value> = None;
    let mut response_id = String::new();

    for event in codex_sse_events(sse_body) {
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "response.output_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    text.push_str(delta);
                }
            }
            "response.output_item.done" => {
                if let Some(item) = event.get("item") {
                    if item.get("type").and_then(Value::as_str) == Some("function_call") {
                        let index = tool_calls.len();
                        tool_calls.push(function_call_item_to_chat(item, index));
                    } else if item.get("type").and_then(Value::as_str) == Some("message")
                        && text.is_empty()
                    {
                        text.push_str(&flatten_text(item.get("content")));
                    }
                }
            }
            "response.completed" | "response.incomplete" => {
                if let Some(response) = event.get("response") {
                    if response_id.is_empty()
                        && let Some(id) = response.get("id").and_then(Value::as_str)
                    {
                        response_id = id.to_string();
                    }
                    if let Some(u) = response.get("usage").filter(|value| !value.is_null()) {
                        usage = Some(codex_usage_to_chat(u));
                    }
                }
            }
            _ => {}
        }
    }

    let finish_reason = if tool_calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    };
    let mut message = Map::new();
    message.insert("role".to_string(), json!("assistant"));
    message.insert(
        "content".to_string(),
        if text.is_empty() && !tool_calls.is_empty() {
            Value::Null
        } else {
            json!(text)
        },
    );
    if !tool_calls.is_empty() {
        message.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }

    let id = if response_id.is_empty() {
        "chatcmpl-codex".to_string()
    } else {
        response_id
    };
    let mut completion = json!({
        "id": id,
        "object": "chat.completion",
        "created": 0,
        "model": model,
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": finish_reason,
        }],
    });
    if let Some(usage) = usage {
        completion["usage"] = usage;
    }
    completion
}

fn function_call_item_to_chat(item: &Value, index: usize) -> Value {
    let call_id = item
        .get("call_id")
        .and_then(Value::as_str)
        .or_else(|| item.get("id").and_then(Value::as_str))
        .unwrap_or("call_0");
    let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
    let arguments = item
        .get("arguments")
        .map(|arguments| match arguments {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .unwrap_or_else(|| "{}".to_string());
    json!({
        "index": index,
        "id": call_id,
        "type": "function",
        "function": { "name": name, "arguments": arguments },
    })
}

fn codex_usage_to_chat(usage: &Value) -> Value {
    let input = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    // Responses already counts cached tokens inside `input_tokens`, so the
    // detail is carried through rather than added on.
    let cached = usage
        .get("input_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    json!({
        "prompt_tokens": input,
        "completion_tokens": output,
        "total_tokens": input + output,
        "prompt_tokens_details": { "cached_tokens": cached },
    })
}

/// Parse the `data:` JSON payloads out of a Codex SSE body.
fn codex_sse_events(sse_body: &str) -> Vec<Value> {
    let mut events = Vec::new();
    for line in sse_body.lines() {
        let line = line.trim_start();
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(data) {
            events.push(value);
        }
    }
    events
}

/// Flatten Responses/chat content (string or content-part array) to plain text.
fn flatten_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| part.as_str().map(str::to_string))
            })
            .collect::<Vec<_>>()
            .join(""),
        Some(other) if !other.is_null() => other.to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
#[path = "backend_adapters_tests.rs"]
mod tests;
