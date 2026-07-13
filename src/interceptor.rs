use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Result, bail};
use axum::Json;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::AppState;
use crate::UpstreamConfig;
use crate::UpstreamRequestMeta;
use crate::UpstreamResponseKind;
use crate::first_chat_choice_message;
use crate::normalize_chat_completion_request;
use crate::request_id_from_headers;
use crate::rewrite_request_roles;
use crate::stats::{
    RouteClass, log_request_value, log_response_text, log_response_value, proxy_stats,
};
use crate::text_from_value;
use crate::upstream_error_response;
use crate::upstream_json;

const PASS_TOOL_NAME: &str = "PASS";
const LOCAL_ROUTE_HEADER: &str = "x-llmproxy-route";
const CODEX_TURN_METADATA_KEY: &str = "x-codex-turn-metadata";
const CODEX_REQUEST_KIND_KEY: &str = "request_kind";
const CODEX_AGENT_TURN_KIND: &str = "turn";
const CODEX_COMPACTION_KIND: &str = "compaction";
const COMPACTION_TRIGGER_TYPE: &str = "compaction_trigger";
const DEFAULT_INSTRUCTIONS: &str = r#"You are the local fast path for agent requests. Decide: handle this request locally, or call PASS to route it to the main model.

Handle it locally only when ALL of these hold:
- The request is explicit and unambiguous, and everything needed is already in this conversation.
- The task is deterministic or routine: formatting, extraction, classification, validation, a simple transformation of supplied content, or a direct operational command.
- It needs at most one tool call, and the tool and its exact arguments are obvious without discovery.
- One short response completes it, and you are certain it is correct.

Otherwise call PASS with {} — including whenever the task needs research, current information, planning, debugging, comparison, recommendation, judgment, or could have financial, legal, medical, security, or operational consequences. When unsure, PASS.

Never ask clarifying questions, explain your decision, or answer partially. Emit only the PASS call, or exactly the assistant message or single tool call that completes the request."#;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub(crate) struct InterceptorConfig {
    pub(crate) enabled: bool,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) timeout_ms: u64,
    pub(crate) fallback_on_error: bool,
    pub(crate) instructions: String,
    /// Sampling temperature for the local triage request.
    pub(crate) temperature: f64,
    /// Completion cap for the local triage request; 0 leaves the client's
    /// own limits untouched. Capped completions fall back to external
    /// routing instead of serving a clipped answer.
    pub(crate) max_tokens: u64,
    pub(crate) parallel_tool_calls: bool,
}

impl Default for InterceptorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: "local".to_string(),
            model: String::new(),
            timeout_ms: 60_000,
            fallback_on_error: true,
            instructions: DEFAULT_INSTRUCTIONS.to_string(),
            temperature: 0.0,
            max_tokens: 512,
            parallel_tool_calls: false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum InterceptorProtocol {
    ChatCompletions,
    Responses,
    AnthropicMessages,
}

impl InterceptorProtocol {
    fn as_str(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
            Self::AnthropicMessages => "anthropic_messages",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InterceptionPolicy {
    Eligible,
    Bypass { request_kind: String },
}

impl InterceptionPolicy {
    pub(crate) fn for_responses(headers: &HeaderMap, request: &Value) -> Self {
        let request_kind = codex_request_kind(headers, request);
        if let Some(request_kind) = request_kind
            && request_kind != CODEX_AGENT_TURN_KIND
        {
            return Self::Bypass { request_kind };
        }
        if has_compaction_trigger(request) {
            return Self::Bypass {
                request_kind: CODEX_COMPACTION_KIND.to_string(),
            };
        }
        Self::Eligible
    }

    fn bypass_kind(&self) -> Option<&str> {
        match self {
            Self::Eligible => None,
            Self::Bypass { request_kind } => Some(request_kind),
        }
    }
}

fn codex_request_kind(headers: &HeaderMap, request: &Value) -> Option<String> {
    request
        .get("client_metadata")
        .and_then(|metadata| metadata.get(CODEX_TURN_METADATA_KEY))
        .and_then(request_kind_from_metadata)
        .or_else(|| {
            request
                .get("client_metadata")
                .and_then(|metadata| metadata.get(CODEX_REQUEST_KIND_KEY))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            headers
                .get(CODEX_TURN_METADATA_KEY)
                .and_then(|value| value.to_str().ok())
                .and_then(|metadata| serde_json::from_str::<Value>(metadata).ok())
                .and_then(|metadata| request_kind_from_metadata(&metadata))
        })
}

fn request_kind_from_metadata(metadata: &Value) -> Option<String> {
    match metadata {
        Value::String(metadata) => serde_json::from_str::<Value>(metadata)
            .ok()
            .and_then(|metadata| request_kind_from_metadata(&metadata)),
        Value::Object(metadata) => metadata
            .get(CODEX_REQUEST_KIND_KEY)
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    }
}

fn has_compaction_trigger(request: &Value) -> bool {
    request
        .get("input")
        .and_then(Value::as_array)
        .is_some_and(|input| {
            input.iter().any(|item| {
                item.get("type").and_then(Value::as_str) == Some(COMPACTION_TRIGGER_TYPE)
            })
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CompletionDecision {
    Local,
    Pass,
    InvalidPass,
}

pub(crate) fn validate_config(
    config: &InterceptorConfig,
    providers: &HashMap<String, UpstreamConfig>,
) -> Result<()> {
    if !config.enabled {
        return Ok(());
    }
    if config.provider.trim().is_empty() {
        bail!("interceptor.provider cannot be empty when the interceptor is enabled");
    }
    if !providers.contains_key(&config.provider) {
        bail!(
            "interceptor references unknown provider {:?}",
            config.provider
        );
    }
    if config.model.trim().is_empty() {
        bail!("interceptor.model cannot be empty when the interceptor is enabled");
    }
    if config.timeout_ms == 0 {
        bail!("interceptor.timeout_ms must be greater than zero");
    }
    if config.instructions.trim().is_empty() {
        bail!("interceptor.instructions cannot be empty when the interceptor is enabled");
    }
    if !config.temperature.is_finite() || !(0.0..=2.0).contains(&config.temperature) {
        bail!("interceptor.temperature must be between 0.0 and 2.0");
    }
    Ok(())
}

pub(crate) async fn maybe_intercept(
    state: &AppState,
    incoming_headers: &HeaderMap,
    local_chat_request: Value,
    protocol: InterceptorProtocol,
    policy: InterceptionPolicy,
    client_model: Option<String>,
    client_streaming: bool,
) -> Option<Response> {
    let config = &state.config.interceptor;
    if !config.enabled {
        return None;
    }

    let request_id = request_id_from_headers(incoming_headers);
    if let Some(request_kind) = policy.bypass_kind() {
        info!(
            request_id,
            protocol = protocol.as_str(),
            request_kind,
            route = "external",
            "local interceptor bypassed control request"
        );
        proxy_stats().record_intercept_bypass();
        return None;
    }
    proxy_stats().record_intercept_attempt();

    let local_request = match build_local_request(
        local_chat_request,
        config,
        protocol,
        client_model.as_deref(),
        &state.config.role_map,
        state.config.as_ref(),
    ) {
        Ok(request) => request,
        Err(message) => {
            warn!(
                request_id,
                protocol = protocol.as_str(),
                error = %message,
                "local interceptor could not prepare request"
            );
            return fallback_or_error(config, StatusCode::BAD_GATEWAY, &request_id, message);
        }
    };

    let provider = state
        .providers
        .get(&config.provider)
        .expect("validated interceptor provider missing from runtime state");
    let meta = UpstreamRequestMeta::new(
        &provider.name,
        incoming_headers,
        Method::POST,
        "chat/completions",
        UpstreamResponseKind::Json,
        false,
    );
    let local_headers = HeaderMap::new();

    log_request_value(RouteClass::Local, &local_request);
    let started_at = std::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_millis(config.timeout_ms),
        upstream_json(provider, &local_headers, &meta, Some(&local_request)),
    )
    .await;

    let completion = match result {
        Ok(Ok(completion)) => completion,
        Ok(Err(error)) => {
            log_response_text(RouteClass::Local, format!("error: {error}"));
            warn!(
                request_id = %meta.request_id,
                protocol = protocol.as_str(),
                provider = %provider.name,
                elapsed_ms = started_at.elapsed().as_millis(),
                error = %error,
                fallback = config.fallback_on_error,
                "local interceptor request failed"
            );
            return if config.fallback_on_error {
                proxy_stats().record_intercept_fallback();
                None
            } else {
                proxy_stats().record_intercept_error();
                Some(upstream_error_response(&error))
            };
        }
        Err(_) => {
            let message = format!("local interceptor timed out after {}ms", config.timeout_ms);
            log_response_text(RouteClass::Local, message.as_str());
            warn!(
                request_id = %meta.request_id,
                protocol = protocol.as_str(),
                provider = %provider.name,
                elapsed_ms = started_at.elapsed().as_millis(),
                fallback = config.fallback_on_error,
                "{message}"
            );
            return fallback_or_error(
                config,
                StatusCode::GATEWAY_TIMEOUT,
                &meta.request_id,
                message,
            );
        }
    };
    log_response_value(RouteClass::Local, &completion);

    if first_chat_choice_message(&completion).is_none() {
        let message = "local interceptor returned no assistant message".to_string();
        warn!(
            request_id = %meta.request_id,
            protocol = protocol.as_str(),
            provider = %provider.name,
            fallback = config.fallback_on_error,
            "{message}"
        );
        return fallback_or_error(config, StatusCode::BAD_GATEWAY, &meta.request_id, message);
    }

    if completion_truncated(&completion) {
        // A clipped answer must never reach the agent; the cap doubles as a
        // "this was not a short local task" signal.
        let message = format!(
            "local completion hit the interceptor max_tokens cap ({})",
            config.max_tokens
        );
        warn!(
            request_id = %meta.request_id,
            protocol = protocol.as_str(),
            provider = %provider.name,
            fallback = config.fallback_on_error,
            "{message}"
        );
        return fallback_or_error(config, StatusCode::BAD_GATEWAY, &meta.request_id, message);
    }

    match classify_completion(&completion) {
        CompletionDecision::Pass => {
            info!(
                request_id = %meta.request_id,
                protocol = protocol.as_str(),
                provider = %provider.name,
                elapsed_ms = started_at.elapsed().as_millis(),
                route = "external",
                "local interceptor selected route"
            );
            proxy_stats().record_intercept_pass();
            None
        }
        CompletionDecision::InvalidPass => {
            let message = "local interceptor mixed PASS with other output".to_string();
            warn!(
                request_id = %meta.request_id,
                protocol = protocol.as_str(),
                provider = %provider.name,
                fallback = config.fallback_on_error,
                "{message}"
            );
            fallback_or_error(config, StatusCode::BAD_GATEWAY, &meta.request_id, message)
        }
        CompletionDecision::Local => {
            info!(
                request_id = %meta.request_id,
                protocol = protocol.as_str(),
                provider = %provider.name,
                elapsed_ms = started_at.elapsed().as_millis(),
                route = "local",
                "local interceptor selected route"
            );
            proxy_stats().record_intercept_local(started_at.elapsed(), completion.get("usage"));
            Some(render_local_completion(
                protocol,
                completion,
                client_model,
                client_streaming,
                &meta.request_id,
            ))
        }
    }
}

fn fallback_or_error(
    config: &InterceptorConfig,
    status: StatusCode,
    request_id: &str,
    message: String,
) -> Option<Response> {
    if config.fallback_on_error {
        proxy_stats().record_intercept_fallback();
        None
    } else {
        proxy_stats().record_intercept_error();
        Some(interceptor_error_response(status, request_id, message))
    }
}

fn interceptor_error_response(status: StatusCode, request_id: &str, message: String) -> Response {
    let mut response = (
        status,
        Json(json!({
            "error": {
                "message": message,
                "type": "interceptor_error",
                "request_id": request_id
            }
        })),
    )
        .into_response();
    if let Ok(value) = HeaderValue::from_str(request_id) {
        response
            .headers_mut()
            .insert(HeaderName::from_static("x-request-id"), value);
    }
    response
}

pub(super) fn build_local_request(
    mut request: Value,
    config: &InterceptorConfig,
    protocol: InterceptorProtocol,
    client_model: Option<&str>,
    role_map: &HashMap<String, String>,
    proxy_config: &crate::Config,
) -> std::result::Result<Value, String> {
    rewrite_request_roles(&mut request, role_map);
    let Some(object) = request.as_object_mut() else {
        return Err("local interceptor request is not a JSON object".to_string());
    };

    object.insert("model".to_string(), Value::String(config.model.clone()));
    object.insert("stream".to_string(), Value::Bool(false));
    object.remove("stream_options");
    object.insert("tool_choice".to_string(), Value::String("auto".to_string()));
    object.insert("temperature".to_string(), json!(config.temperature));
    object.insert(
        "parallel_tool_calls".to_string(),
        Value::Bool(config.parallel_tool_calls),
    );
    if config.max_tokens > 0 {
        object.insert("max_tokens".to_string(), json!(config.max_tokens));
        // A second, larger client-supplied cap would conflict with ours on
        // some backends.
        object.remove("max_completion_tokens");
    }

    let tools = object
        .entry("tools".to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| "local interceptor tools field is not an array".to_string())?;
    if tools.iter().any(is_pass_tool) {
        return Err("request already defines a tool named PASS".to_string());
    }
    tools.push(pass_tool());

    let dynamic_instructions = format!(
        "{}\n\nCurrent proxy protocol: {}. Original requested model: {}.",
        config.instructions.trim(),
        protocol.as_str(),
        client_model.unwrap_or("unspecified"),
    );
    let messages = object
        .entry("messages".to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| "local interceptor messages field is not an array".to_string())?;
    messages.push(json!({
        "role": "system",
        "content": dynamic_instructions
    }));

    normalize_chat_completion_request(&mut request, proxy_config);
    Ok(request)
}

fn pass_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": PASS_TOOL_NAME,
            "description": "Route this complete original request to the external model. Call this function alone when the task should not be handled by the local model.",
            "parameters": {
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }
        }
    })
}

pub(super) fn is_pass_tool(tool: &Value) -> bool {
    tool.get("function")
        .and_then(|function| function.get("name"))
        .or_else(|| tool.get("name"))
        .and_then(Value::as_str)
        == Some(PASS_TOOL_NAME)
}

pub(super) fn classify_completion(completion: &Value) -> CompletionDecision {
    let Some(choices) = completion.get("choices").and_then(Value::as_array) else {
        return CompletionDecision::Local;
    };
    let contains_pass = choices.iter().any(|choice| {
        let Some(message) = choice.get("message") else {
            return false;
        };
        message
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|calls| calls.iter().any(is_pass_tool_call))
            || message
                .get("content")
                .is_some_and(|content| text_from_value(content).trim() == PASS_TOOL_NAME)
    });
    if !contains_pass {
        return CompletionDecision::Local;
    }
    if choices.len() != 1 {
        return CompletionDecision::InvalidPass;
    }

    let Some(message) = choices[0].get("message") else {
        return CompletionDecision::InvalidPass;
    };
    let content = message
        .get("content")
        .map(text_from_value)
        .unwrap_or_default();
    let tool_calls = message.get("tool_calls").and_then(Value::as_array);
    match tool_calls {
        Some(calls)
            if calls.len() == 1 && is_pass_tool_call(&calls[0]) && content.trim().is_empty() =>
        {
            CompletionDecision::Pass
        }
        None if content.trim() == PASS_TOOL_NAME => CompletionDecision::Pass,
        Some(calls) if calls.is_empty() && content.trim() == PASS_TOOL_NAME => {
            CompletionDecision::Pass
        }
        _ => CompletionDecision::InvalidPass,
    }
}

/// True when any choice stopped for length: the completion is clipped and
/// must not be served as a local answer.
pub(super) fn completion_truncated(completion: &Value) -> bool {
    completion
        .get("choices")
        .and_then(Value::as_array)
        .is_some_and(|choices| {
            choices
                .iter()
                .any(|choice| choice.get("finish_reason").and_then(Value::as_str) == Some("length"))
        })
}

fn is_pass_tool_call(call: &Value) -> bool {
    call.get("function")
        .and_then(|function| function.get("name"))
        .or_else(|| call.get("name"))
        .and_then(Value::as_str)
        == Some(PASS_TOOL_NAME)
}

/// Run one user prompt through the interceptor's routing decision and report
/// where it would go, without forwarding anywhere or touching stats/logs.
/// `instructions_override` tests unsaved editor text; empty means the
/// built-in default policy. Used by the admin system-prompt tab.
pub(crate) async fn test_route(
    state: &AppState,
    prompt: &str,
    instructions_override: Option<&str>,
) -> Value {
    let mut config = state.config.interceptor.clone();
    if let Some(text) = instructions_override {
        config.instructions = if text.trim().is_empty() {
            DEFAULT_INSTRUCTIONS.to_string()
        } else {
            text.to_string()
        };
    }
    let fallback_note = if config.fallback_on_error {
        "live traffic would fall back to external routing on this failure"
    } else {
        "live traffic would return this error to the client (fallback_on_error = false)"
    };

    if config.provider.trim().is_empty() || config.model.trim().is_empty() {
        return json!({
            "decision": "error",
            "detail": "configure the interceptor provider and model first",
        });
    }
    let Some(provider) = state.providers.get(&config.provider) else {
        return json!({
            "decision": "error",
            "detail": format!("interceptor provider {:?} is not defined", config.provider),
        });
    };

    let chat_request = json!({
        "messages": [{ "role": "user", "content": prompt }]
    });
    let local_request = match build_local_request(
        chat_request,
        &config,
        InterceptorProtocol::ChatCompletions,
        None,
        &state.config.role_map,
        state.config.as_ref(),
    ) {
        Ok(request) => request,
        Err(message) => return json!({ "decision": "error", "detail": message }),
    };

    let meta = UpstreamRequestMeta::new(
        &provider.name,
        &HeaderMap::new(),
        Method::POST,
        "chat/completions",
        UpstreamResponseKind::Json,
        false,
    );
    let started_at = std::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_millis(config.timeout_ms),
        upstream_json(provider, &HeaderMap::new(), &meta, Some(&local_request)),
    )
    .await;
    let elapsed_ms = started_at.elapsed().as_millis() as u64;

    let completion = match result {
        Ok(Ok(completion)) => completion,
        Ok(Err(error)) => {
            return json!({
                "decision": "error",
                "detail": bounded_detail(&error.to_string()),
                "note": fallback_note,
                "elapsed_ms": elapsed_ms,
            });
        }
        Err(_) => {
            return json!({
                "decision": "error",
                "detail": format!("local model timed out after {}ms", config.timeout_ms),
                "note": fallback_note,
                "elapsed_ms": elapsed_ms,
            });
        }
    };

    let Some(message) = first_chat_choice_message(&completion) else {
        return json!({
            "decision": "error",
            "detail": "local model returned no assistant message",
            "note": fallback_note,
            "elapsed_ms": elapsed_ms,
        });
    };
    if completion_truncated(&completion) {
        return json!({
            "decision": "error",
            "detail": format!(
                "local completion hit the max_tokens cap ({}); the answer would be clipped",
                config.max_tokens
            ),
            "note": fallback_note,
            "elapsed_ms": elapsed_ms,
        });
    }
    let text = message
        .get("content")
        .map(text_from_value)
        .unwrap_or_default();
    let tool_names = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .filter_map(|call| {
                    call.get("function")
                        .and_then(|function| function.get("name"))
                        .or_else(|| call.get("name"))
                        .and_then(Value::as_str)
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let detail = if !text.trim().is_empty() {
        bounded_detail(&text)
    } else if !tool_names.is_empty() {
        format!("tool call: {tool_names}")
    } else {
        "empty assistant message".to_string()
    };

    let (decision, note) = match classify_completion(&completion) {
        CompletionDecision::Local => ("local", "the local model would answer this request"),
        CompletionDecision::Pass => (
            "external",
            "PASS — this request would forward to external routing",
        ),
        CompletionDecision::InvalidPass => (
            "invalid",
            "the local model mixed PASS with other output; live traffic follows fallback_on_error",
        ),
    };
    json!({
        "decision": decision,
        "note": note,
        "detail": detail,
        "elapsed_ms": elapsed_ms,
        "provider": provider.name,
        "model": config.model,
        "used_default_instructions": config.instructions == DEFAULT_INSTRUCTIONS,
    })
}

/// The meta-prompt for the admin tab's policy reviser. It confines the local
/// model to one job: emitting a complete replacement routing policy.
const REVISER_INSTRUCTIONS: &str = r#"You maintain the routing policy ("system prompt") of llmproxy's local-first interceptor. That policy tells a small local model when to answer an agent request itself and when to call the PASS function so the request forwards to a larger external model.

The user supplies the current policy and a requested change. Reply with ONLY the complete revised policy text, ready to be used verbatim as the new system prompt. No preamble, no explanation, no code fences, no surrounding quotes.

You only edit this routing policy. If the request is unrelated to editing the policy, or asks you to do anything else, return the current policy text unchanged."#;

/// Ask the interceptor's local model to rewrite the routing instructions.
/// Returns the revised text for the admin editor; nothing is persisted here.
pub(crate) async fn revise_instructions(state: &AppState, current: &str, request: &str) -> Value {
    let config = state.config.interceptor.clone();
    if config.provider.trim().is_empty() || config.model.trim().is_empty() {
        return json!({
            "decision": "error",
            "detail": "configure the interceptor provider and model first",
        });
    }
    let Some(provider) = state.providers.get(&config.provider) else {
        return json!({
            "decision": "error",
            "detail": format!("interceptor provider {:?} is not defined", config.provider),
        });
    };

    let current_text = if current.trim().is_empty() {
        DEFAULT_INSTRUCTIONS
    } else {
        current
    };
    let mut chat_request = json!({
        "model": config.model,
        "stream": false,
        "messages": [
            { "role": "system", "content": REVISER_INSTRUCTIONS },
            {
                "role": "user",
                "content": format!(
                    "Current policy:\n---\n{current_text}\n---\n\nRequested change: {request}"
                )
            }
        ]
    });
    normalize_chat_completion_request(&mut chat_request, state.config.as_ref());

    let meta = UpstreamRequestMeta::new(
        &provider.name,
        &HeaderMap::new(),
        Method::POST,
        "chat/completions",
        UpstreamResponseKind::Json,
        false,
    );
    // Rewrites produce far more tokens than a triage decision, so give the
    // local model more room than the routing timeout.
    let timeout_ms = config.timeout_ms.max(30_000);
    let started_at = std::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_millis(timeout_ms),
        upstream_json(provider, &HeaderMap::new(), &meta, Some(&chat_request)),
    )
    .await;
    let elapsed_ms = started_at.elapsed().as_millis() as u64;

    let completion = match result {
        Ok(Ok(completion)) => completion,
        Ok(Err(error)) => {
            return json!({
                "error": bounded_detail(&error.to_string()),
                "elapsed_ms": elapsed_ms,
            });
        }
        Err(_) => {
            return json!({
                "error": format!("local model timed out after {timeout_ms}ms"),
                "elapsed_ms": elapsed_ms,
            });
        }
    };

    let revised = first_chat_choice_message(&completion)
        .and_then(|message| message.get("content"))
        .map(text_from_value)
        .map(|text| strip_code_fence(&text).to_string())
        .unwrap_or_default();
    if revised.trim().is_empty() {
        return json!({
            "error": "local model returned no revision text",
            "elapsed_ms": elapsed_ms,
        });
    }
    json!({
        "instructions": revised.trim(),
        "elapsed_ms": elapsed_ms,
        "provider": provider.name,
        "model": config.model,
    })
}

/// Remove one wrapping ``` fence if the model added it despite instructions.
fn strip_code_fence(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let Some(newline) = rest.find('\n') else {
        return trimmed;
    };
    let body = &rest[newline + 1..];
    body.trim_end()
        .strip_suffix("```")
        .map(str::trim)
        .unwrap_or_else(|| body.trim())
}

fn bounded_detail(text: &str) -> String {
    const DETAIL_CHARS: usize = 400;
    let mut detail = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(DETAIL_CHARS + 1)
        .collect::<String>();
    if detail.chars().count() > DETAIL_CHARS {
        detail.pop();
        detail.push('…');
    }
    detail
}

#[path = "interceptor_render.rs"]
mod render;
use render::render_local_completion;

#[cfg(test)]
#[path = "interceptor_tests.rs"]
mod tests;
