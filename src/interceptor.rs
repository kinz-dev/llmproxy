use std::collections::{BTreeMap, HashMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use axum::Json;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{error, info};

use crate::AppState;
use crate::INTEGRATED_AUTO_MODEL;
use crate::UpstreamConfig;
use crate::UpstreamRequestMeta;
use crate::UpstreamResponseKind;
use crate::first_chat_choice_message;
use crate::normalize_chat_completion_request;
use crate::request_id_from_headers;
use crate::rewrite_request_roles;
use crate::stats::{
    RouteClass, TokenUsage, log_request_value, log_response_text, log_response_value, proxy_stats,
};
use crate::text_from_value;
use crate::upstream_error_response;
use crate::upstream_json;
use crate::{ProviderState, proxy_integrated_chat};

const PASS_TOOL_NAME: &str = "PASS";
const ROUTE_DEFAULT_TOOL_NAME: &str = "ROUTE_DEFAULT";
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

/// Base routing policy used instead of [`DEFAULT_INSTRUCTIONS`] when
/// integrated mode is on and the operator has not overridden the prompt.
/// The concrete provider routes and their hints are appended dynamically.
const DEFAULT_INTEGRATED_INSTRUCTIONS: &str = r#"You are the routing tier for agent requests. Give every request exactly one outcome: answer it locally, or hand it to the provider route that owns its kind of work.

Answer locally only when ALL of these hold:
- The request is explicit and unambiguous, and everything needed is already in this conversation.
- The task is deterministic or routine: formatting, extraction, classification, validation, a simple transformation of supplied content, or a direct operational command.
- It needs at most one tool call, and the tool and its exact arguments are obvious without discovery.
- One short response completes it, and you are certain it is correct.

Everything else is routed, never answered locally: research, planning, investigation, design, implementation, debugging, comparison, recommendation, judgment, and anything with financial, legal, medical, security, or operational consequences. Call exactly one provider route from the list below, choosing the route whose hint matches the request; a route hinted as the default owns everything no other route clearly claims. Call ROUTE_DEFAULT with {} only when no route fits.

Complex online research, current-information investigation, and source-backed multi-step analysis must use the matching dedicated research route at max or xhigh reasoning effort when one is configured. Never answer that work locally or send it to a weaker general route.

Never ask clarifying questions, explain your decision, or answer partially. Emit only one provider route call, the ROUTE_DEFAULT call, or exactly the assistant message or single tool call that completes the request."#;

/// The intercept-mode policy: the operator override, or the built-in default
/// when the override is empty or untouched.
pub(crate) fn intercept_instructions(interceptor: &InterceptorConfig) -> &str {
    if interceptor.instructions.trim().is_empty() {
        DEFAULT_INSTRUCTIONS
    } else {
        &interceptor.instructions
    }
}

/// The integrated-mode policy: its own override, or the built-in integrated
/// default. Intercept-mode overrides never leak into integrated mode.
pub(crate) fn integrated_instructions(integrated: &IntegratedConfig) -> &str {
    if integrated.instructions.trim().is_empty() {
        DEFAULT_INTEGRATED_INSTRUCTIONS
    } else {
        &integrated.instructions
    }
}

pub(crate) fn intercept_instructions_are_default(interceptor: &InterceptorConfig) -> bool {
    intercept_instructions(interceptor) == DEFAULT_INSTRUCTIONS
}

pub(crate) fn integrated_instructions_are_default(integrated: &IntegratedConfig) -> bool {
    integrated_instructions(integrated) == DEFAULT_INTEGRATED_INSTRUCTIONS
}

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

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub(crate) struct IntegratedConfig {
    pub(crate) enabled: bool,
    pub(crate) sticky_ttl_secs: u64,
    pub(crate) sticky_capacity: usize,
    /// Integrated-mode routing policy override; empty uses the built-in
    /// integrated default.
    pub(crate) instructions: String,
    pub(crate) targets: BTreeMap<String, IntegratedTarget>,
}

impl Default for IntegratedConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sticky_ttl_secs: 1800,
            sticky_capacity: 1024,
            instructions: String::new(),
            targets: BTreeMap::new(),
        }
    }
}

fn integrated_routing_requested(config: &IntegratedConfig, client_model: Option<&str>) -> bool {
    config.enabled && client_model == Some(INTEGRATED_AUTO_MODEL)
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct IntegratedTarget {
    pub(crate) provider: String,
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) hint: String,
    #[serde(default)]
    pub(crate) reasoning_effort: Option<crate::ReasoningEffort>,
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
    pub(crate) fn as_str(self) -> &'static str {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CompletionDecision {
    Local,
    DefaultRoute,
    Redirect(String),
    InvalidControl,
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

pub(crate) fn validate_integrated_config(
    config: &IntegratedConfig,
    interceptor: &InterceptorConfig,
    providers: &HashMap<String, UpstreamConfig>,
) -> Result<()> {
    if !config.enabled {
        return Ok(());
    }
    if config.targets.is_empty() {
        bail!("integrated mode requires at least one [integrated.targets.*] entry");
    }
    if config.sticky_ttl_secs == 0 {
        bail!("integrated.sticky_ttl_secs must be greater than zero");
    }
    if config.sticky_capacity == 0 {
        bail!("integrated.sticky_capacity must be greater than zero");
    }
    for (name, provider) in providers {
        // Borrowed-OAuth providers carry the local CLI's login instead of a
        // proxy-owned API key, so they are exempt from the key requirement.
        let borrows_oauth = provider.auth != crate::ProviderAuth::None;
        if name != &interceptor.provider
            && crate::stats::route_class_for_base_url(&provider.base_url) == RouteClass::External
            && provider.api_key.is_none()
            && !borrows_oauth
        {
            bail!(
                "integrated external provider {name:?} requires proxy-owned api_key or api_key_env (or auth = \"codex-oauth\"/\"claude-oauth\")"
            );
        }
    }
    for (name, target) in &config.targets {
        if !valid_target_name(name) || is_default_route_name(name) {
            bail!(
                "integrated target {name:?} must be a non-reserved 1-64 character tool name containing only letters, digits, _, or -"
            );
        }
        let Some(provider) = providers.get(&target.provider) else {
            bail!(
                "integrated target {name:?} references unknown provider {:?}",
                target.provider
            );
        };
        if target.provider == interceptor.provider {
            bail!("integrated target {name:?} cannot route back to the interceptor provider");
        }
        if target.model.trim().is_empty() {
            bail!("integrated target {name:?} has an empty model");
        }
        if target.hint.trim().is_empty() || target.hint.lines().count() != 1 {
            bail!("integrated target {name:?} hint must be one non-empty line");
        }
        if provider.kind == crate::ProviderKind::Copilot && target.reasoning_effort.is_some() {
            bail!("integrated target {name:?} cannot set reasoning_effort for kind \"copilot\"");
        }
        if provider.kind == crate::ProviderKind::Anthropic
            && matches!(
                target.reasoning_effort,
                Some(crate::ReasoningEffort::None | crate::ReasoningEffort::Minimal)
            )
        {
            bail!(
                "integrated target {name:?} reasoning_effort must be low, medium, high, xhigh, or max for Anthropic"
            );
        }
    }
    Ok(())
}

fn valid_target_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

#[derive(Clone)]
struct StickyRoute {
    target: String,
    expires_at: Instant,
}

#[derive(Default)]
struct StickyRoutes {
    entries: Mutex<(HashMap<u64, StickyRoute>, VecDeque<u64>)>,
}

impl StickyRoutes {
    fn lookup(&self, key: u64) -> Option<String> {
        let now = Instant::now();
        let mut routes = self.entries.lock().expect("sticky route store poisoned");
        let (entries, _) = &mut *routes;
        match entries.get(&key) {
            Some(route) if route.expires_at > now => Some(route.target.clone()),
            Some(_) => {
                entries.remove(&key);
                None
            }
            None => None,
        }
    }

    fn insert(&self, key: u64, target: String, ttl_secs: u64, capacity: usize) {
        let mut routes = self.entries.lock().expect("sticky route store poisoned");
        let (entries, order) = &mut *routes;
        let now = Instant::now();
        entries.retain(|_, route| route.expires_at > now);
        entries.remove(&key);
        order.retain(|existing| *existing != key);
        while entries.len() >= capacity {
            let Some(oldest) = order.pop_front() else {
                break;
            };
            entries.remove(&oldest);
        }
        entries.insert(
            key,
            StickyRoute {
                target,
                expires_at: now + Duration::from_secs(ttl_secs),
            },
        );
        order.push_back(key);
    }

    fn remove(&self, key: u64) {
        self.entries
            .lock()
            .expect("sticky route store poisoned")
            .0
            .remove(&key);
    }
}

fn sticky_routes() -> &'static StickyRoutes {
    static ROUTES: OnceLock<StickyRoutes> = OnceLock::new();
    ROUTES.get_or_init(StickyRoutes::default)
}

fn conversation_key(headers: &HeaderMap, chat_request: &Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    for name in [
        "session-id",
        "thread-id",
        "x-session-id",
        "x-copilot-session-id",
        "x-codex-thread-id",
    ] {
        if let Some(value) = headers.get(name).and_then(|value| value.to_str().ok()) {
            name.hash(&mut hasher);
            value.hash(&mut hasher);
            return hasher.finish();
        }
    }
    if let Some(first_user) = chat_request
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .and_then(|message| message.get("content"))
    {
        text_from_value(first_user).hash(&mut hasher);
    } else {
        chat_request.to_string().hash(&mut hasher);
    }
    hasher.finish()
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
    let integrated =
        integrated_routing_requested(&state.config.integrated, client_model.as_deref());
    if !config.enabled && !integrated {
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

    let conversation_key =
        integrated.then(|| conversation_key(incoming_headers, &local_chat_request));
    if let Some(key) = conversation_key
        && let Some(target) = sticky_routes().lookup(key)
        && state.config.integrated.targets.contains_key(&target)
    {
        info!(
            request_id,
            protocol = protocol.as_str(),
            target,
            route = "sticky_redirect",
            "integrated router reused conversation route"
        );
        return execute_integrated_redirect(
            state,
            &target,
            local_chat_request,
            protocol,
            client_model,
            client_streaming,
            Some(key),
        )
        .await;
    }

    let redirect_request = local_chat_request.clone();

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
            error!(
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
    let estimated_input_tokens = crate::estimate_tokens(&local_request) as u64;

    log_request_value(RouteClass::Local, &provider.name, &local_request);
    let started_at = std::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_millis(config.timeout_ms),
        upstream_json(provider, &local_headers, &meta, Some(&local_request)),
    )
    .await;

    let completion = match result {
        Ok(Ok(completion)) => completion,
        Ok(Err(error)) => {
            log_response_text(RouteClass::Local, &provider.name, format!("error: {error}"));
            crate::stats::error_log().record(&provider.name, "interceptor", error.to_string());
            error!(
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
            log_response_text(RouteClass::Local, &provider.name, message.as_str());
            error!(
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
    let local_elapsed = started_at.elapsed();
    let local_usage = TokenUsage::from_response(
        completion.get("usage"),
        estimated_input_tokens,
        crate::estimate_tokens(&completion) as u64,
    );
    proxy_stats().record_model_usage(&provider.name, &config.model, local_elapsed, local_usage);
    log_response_value(RouteClass::Local, &provider.name, &completion);

    if first_chat_choice_message(&completion).is_none() {
        let message = "local interceptor returned no assistant message".to_string();
        error!(
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
        error!(
            request_id = %meta.request_id,
            protocol = protocol.as_str(),
            provider = %provider.name,
            fallback = config.fallback_on_error,
            "{message}"
        );
        return fallback_or_error(config, StatusCode::BAD_GATEWAY, &meta.request_id, message);
    }

    let no_targets = BTreeMap::new();
    let decision_targets = if integrated {
        &state.config.integrated.targets
    } else {
        &no_targets
    };
    match classify_completion_with_targets(&completion, decision_targets) {
        CompletionDecision::DefaultRoute => {
            info!(
                request_id = %meta.request_id,
                protocol = protocol.as_str(),
                provider = %provider.name,
                elapsed_ms = local_elapsed.as_millis(),
                route = "external",
                "local interceptor selected route"
            );
            proxy_stats().record_intercept_pass();
            None
        }
        CompletionDecision::Redirect(target) => {
            info!(
                request_id = %meta.request_id,
                protocol = protocol.as_str(),
                provider = %provider.name,
                target,
                elapsed_ms = local_elapsed.as_millis(),
                route = "redirect",
                "local interceptor selected integrated route"
            );
            execute_integrated_redirect(
                state,
                &target,
                redirect_request,
                protocol,
                client_model,
                client_streaming,
                conversation_key,
            )
            .await
        }
        CompletionDecision::InvalidControl => {
            let message = "local interceptor mixed routing control with other output".to_string();
            error!(
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
                elapsed_ms = local_elapsed.as_millis(),
                route = "local",
                "local interceptor selected route"
            );
            proxy_stats().record_intercept_local(
                &provider.name,
                protocol.as_str(),
                local_elapsed,
                local_usage,
            );
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

async fn execute_integrated_redirect(
    state: &AppState,
    target_name: &str,
    chat_request: Value,
    protocol: InterceptorProtocol,
    client_model: Option<String>,
    client_streaming: bool,
    conversation_key: Option<u64>,
) -> Option<Response> {
    let target = state.config.integrated.targets.get(target_name)?;
    let provider: &ProviderState = state.providers.get(&target.provider)?;
    info!(
        target = target_name,
        provider = target.provider,
        model = target.model,
        reasoning_effort = target.reasoning_effort.map(crate::ReasoningEffort::as_str),
        protocol = protocol.as_str(),
        "sending integrated route"
    );
    let response = proxy_integrated_chat(
        provider,
        chat_request,
        target,
        protocol,
        client_model,
        client_streaming,
        state.config.as_ref(),
    )
    .await;
    if response.status().is_success() {
        proxy_stats().record_intercept_redirect(target_name);
        if let Some(key) = conversation_key {
            sticky_routes().insert(
                key,
                target_name.to_string(),
                state.config.integrated.sticky_ttl_secs,
                state.config.integrated.sticky_capacity,
            );
        }
        Some(response)
    } else {
        if let Some(key) = conversation_key {
            sticky_routes().remove(key);
        }
        error!(
            target = target_name,
            provider = target.provider,
            model = target.model,
            reasoning_effort = target.reasoning_effort.map(crate::ReasoningEffort::as_str),
            status = %response.status(),
            "integrated redirect failed; preserving the first upstream result"
        );
        crate::stats::error_log().record(
            &target.provider,
            "integrated redirect",
            format!("{target_name} failed with HTTP {}", response.status()),
        );
        // The request reached a generation endpoint, so a fall-through could
        // duplicate work or spend. Surface the original failure instead.
        proxy_stats().record_intercept_error();
        Some(response)
    }
}

fn fallback_or_error(
    config: &InterceptorConfig,
    status: StatusCode,
    request_id: &str,
    message: String,
) -> Option<Response> {
    crate::stats::error_log().record(&config.provider, "interceptor", message.as_str());
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
    let integrated = integrated_routing_requested(&proxy_config.integrated, client_model);
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
    if let Some(name) = tools
        .iter()
        .filter_map(tool_name)
        .find(|name| is_default_route_name(name))
    {
        return Err(format!("request already defines a tool named {name}"));
    }
    if integrated {
        for name in proxy_config.integrated.targets.keys() {
            if tools
                .iter()
                .any(|tool| tool_name(tool) == Some(name.as_str()))
            {
                return Err(format!("request already defines a tool named {name}"));
            }
        }
    }
    tools.push(default_route_tool(integrated));
    if integrated {
        tools.extend(
            proxy_config
                .integrated
                .targets
                .iter()
                .map(|(name, target)| redirect_tool(name, target)),
        );
    }

    let mut dynamic_instructions = format!(
        "{}\n\nCurrent proxy protocol: {}. Original requested model: {}.",
        if integrated {
            integrated_instructions(&proxy_config.integrated)
        } else {
            intercept_instructions(config)
        }
        .trim(),
        protocol.as_str(),
        client_model.unwrap_or("unspecified"),
    );
    if integrated {
        dynamic_instructions.push_str("\n\nIntegrated provider routes:");
        for (name, target) in &proxy_config.integrated.targets {
            dynamic_instructions.push_str(&format!(
                "\nCall {name} when: {}. Destination: {}/{} with {} reasoning effort.",
                target.hint.trim(),
                target.provider,
                target.model,
                target
                    .reasoning_effort
                    .map(crate::ReasoningEffort::as_str)
                    .unwrap_or("provider-default"),
            ));
        }
        dynamic_instructions.push_str(
            "\nCall exactly one provider route, or ROUTE_DEFAULT with {} when no route fits.",
        );
    }
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

fn default_route_tool(integrated: bool) -> Value {
    let (name, description) = if integrated {
        (
            ROUTE_DEFAULT_TOOL_NAME,
            "Continue this complete original request through normal model alias and default-provider routing. Call this function alone only when no configured provider route fits.",
        )
    } else {
        (
            PASS_TOOL_NAME,
            "Route this complete original request to the external model. Call this function alone when the task should not be handled by the local model.",
        )
    };
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": {
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }
        }
    })
}

fn redirect_tool(name: &str, target: &IntegratedTarget) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": format!(
                "Route the complete original request to provider {:?} model {:?} with {} reasoning effort: {}",
                target.provider,
                target.model,
                target
                    .reasoning_effort
                    .map(crate::ReasoningEffort::as_str)
                    .unwrap_or("provider-default"),
                target.hint.trim()
            ),
            "parameters": {
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }
        }
    })
}

fn tool_name(tool: &Value) -> Option<&str> {
    tool.get("function")
        .and_then(|function| function.get("name"))
        .or_else(|| tool.get("name"))
        .and_then(Value::as_str)
}

#[cfg(test)]
pub(super) fn is_pass_tool(tool: &Value) -> bool {
    tool_name(tool) == Some(PASS_TOOL_NAME)
}

fn is_default_route_name(name: &str) -> bool {
    name == PASS_TOOL_NAME || name == ROUTE_DEFAULT_TOOL_NAME
}

#[cfg(test)]
pub(super) fn classify_completion(completion: &Value) -> CompletionDecision {
    classify_completion_with_targets(completion, &BTreeMap::new())
}

fn classify_completion_with_targets(
    completion: &Value,
    targets: &BTreeMap<String, IntegratedTarget>,
) -> CompletionDecision {
    let Some(choices) = completion.get("choices").and_then(Value::as_array) else {
        return CompletionDecision::Local;
    };
    let contains_control = choices.iter().any(|choice| {
        let Some(message) = choice.get("message") else {
            return false;
        };
        message
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|calls| {
                calls
                    .iter()
                    .any(|call| control_decision(tool_name(call), targets).is_some())
            })
            || message.get("content").is_some_and(|content| {
                control_decision(Some(text_from_value(content).trim()), targets).is_some()
            })
    });
    if !contains_control {
        return CompletionDecision::Local;
    }
    if choices.len() != 1 {
        return CompletionDecision::InvalidControl;
    }

    let Some(message) = choices[0].get("message") else {
        return CompletionDecision::InvalidControl;
    };
    let content = message
        .get("content")
        .map(text_from_value)
        .unwrap_or_default();
    let tool_calls = message.get("tool_calls").and_then(Value::as_array);
    match tool_calls {
        Some(calls) if calls.len() == 1 && content.trim().is_empty() => {
            control_tool_call_decision(&calls[0], targets)
                .unwrap_or(CompletionDecision::InvalidControl)
        }
        None => control_decision(Some(content.trim()), targets)
            .unwrap_or(CompletionDecision::InvalidControl),
        Some(calls) if calls.is_empty() => control_decision(Some(content.trim()), targets)
            .unwrap_or(CompletionDecision::InvalidControl),
        _ => CompletionDecision::InvalidControl,
    }
}

fn control_tool_call_decision(
    call: &Value,
    targets: &BTreeMap<String, IntegratedTarget>,
) -> Option<CompletionDecision> {
    has_zero_tool_arguments(call)
        .then(|| control_decision(tool_name(call), targets))
        .flatten()
}

fn has_zero_tool_arguments(call: &Value) -> bool {
    let arguments = call
        .get("function")
        .and_then(|function| function.get("arguments"))
        .or_else(|| call.get("arguments"));
    match arguments {
        None | Some(Value::Null) => true,
        Some(Value::Object(arguments)) => arguments.is_empty(),
        Some(Value::String(arguments)) if arguments.trim().is_empty() => true,
        Some(Value::String(arguments)) => serde_json::from_str::<Value>(arguments)
            .is_ok_and(|arguments| arguments.as_object().is_some_and(serde_json::Map::is_empty)),
        _ => false,
    }
}

fn control_decision(
    name: Option<&str>,
    targets: &BTreeMap<String, IntegratedTarget>,
) -> Option<CompletionDecision> {
    let name = name?.trim();
    if is_default_route_name(name) {
        return Some(CompletionDecision::DefaultRoute);
    }
    let canonical = name.replace('-', "_").to_ascii_uppercase();
    targets
        .contains_key(&canonical)
        .then_some(CompletionDecision::Redirect(canonical))
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

/// Which routing policy a route test or revision exercises. Intercept tests
/// force integrated off; integrated tests force it on with the configured
/// targets, so routes can be exercised even while the saved config keeps
/// integrated mode disabled (launchers enable it per session).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteTestMode {
    Intercept,
    Integrated,
}

impl RouteTestMode {
    pub(crate) fn parse(mode: Option<&str>) -> Result<Self, String> {
        match mode.map(str::trim).filter(|mode| !mode.is_empty()) {
            None | Some("intercept") => Ok(Self::Intercept),
            Some("integrated") => Ok(Self::Integrated),
            Some(other) => Err(format!(
                "unknown mode {other:?}; expected \"intercept\" or \"integrated\""
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Intercept => "intercept",
            Self::Integrated => "integrated",
        }
    }
}

/// Run one user prompt through the interceptor's routing decision and report
/// where it would go, without forwarding anywhere or touching stats/logs.
/// `instructions_override` tests unsaved editor text; empty means the
/// built-in default policy for the tested mode. Used by the admin
/// system-prompt tab.
pub(crate) async fn test_route(
    state: &AppState,
    prompt: &str,
    instructions_override: Option<&str>,
    mode: RouteTestMode,
) -> Value {
    let mut proxy_config = (*state.config).clone();
    match mode {
        RouteTestMode::Intercept => {
            proxy_config.integrated.enabled = false;
            if let Some(text) = instructions_override {
                proxy_config.interceptor.instructions = text.to_string();
            }
        }
        RouteTestMode::Integrated => {
            if proxy_config.integrated.targets.is_empty() {
                return json!({
                    "decision": "error",
                    "mode": mode.as_str(),
                    "detail": "integrated mode has no provider targets; add [integrated.targets.*] in Configure first",
                });
            }
            proxy_config.integrated.enabled = true;
            if let Some(text) = instructions_override {
                proxy_config.integrated.instructions = text.to_string();
            }
        }
    }
    let config = proxy_config.interceptor.clone();
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
        (mode == RouteTestMode::Integrated).then_some(INTEGRATED_AUTO_MODEL),
        &proxy_config.role_map,
        &proxy_config,
    ) {
        Ok(request) => request,
        Err(message) => {
            return json!({ "decision": "error", "mode": mode.as_str(), "detail": message });
        }
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
            error!(
                request_id = %meta.request_id,
                provider = %provider.name,
                mode = mode.as_str(),
                timeout_ms = config.timeout_ms,
                "routing test timed out"
            );
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

    let targets = match mode {
        RouteTestMode::Intercept => &BTreeMap::new(),
        RouteTestMode::Integrated => &proxy_config.integrated.targets,
    };
    let (decision, target, note) = match classify_completion_with_targets(&completion, targets) {
        CompletionDecision::Local => (
            "local",
            None,
            "the local model would answer this request itself".to_string(),
        ),
        CompletionDecision::DefaultRoute => match mode {
            RouteTestMode::Intercept => (
                "external",
                None,
                format!("{PASS_TOOL_NAME} — forwards untouched to normal external routing"),
            ),
            RouteTestMode::Integrated => (
                "default_route",
                None,
                format!(
                    "{ROUTE_DEFAULT_TOOL_NAME} — continues through model aliases to the default provider"
                ),
            ),
        },
        CompletionDecision::Redirect(target_name) => {
            let route = proxy_config.integrated.targets.get(&target_name);
            let note = route.map_or_else(
                || format!("redirect to {target_name}"),
                |route| {
                    format!(
                        "redirect to {target_name} ({}/{} at {} reasoning)",
                        route.provider,
                        route.model,
                        route
                            .reasoning_effort
                            .map(crate::ReasoningEffort::as_str)
                            .unwrap_or("provider-default"),
                    )
                },
            );
            ("redirect", Some(target_name), note)
        }
        CompletionDecision::InvalidControl => (
            "invalid",
            None,
            "the local model mixed routing control with other output; live traffic follows fallback_on_error".to_string(),
        ),
    };
    let used_default_instructions = match mode {
        RouteTestMode::Intercept => intercept_instructions_are_default(&proxy_config.interceptor),
        RouteTestMode::Integrated => integrated_instructions_are_default(&proxy_config.integrated),
    };
    json!({
        "decision": decision,
        "mode": mode.as_str(),
        "target": target,
        "note": note,
        "detail": detail,
        "elapsed_ms": elapsed_ms,
        "provider": provider.name,
        "model": config.model,
        "used_default_instructions": used_default_instructions,
    })
}

/// The meta-prompt for the admin tab's policy reviser. It confines the local
/// model to one job: emitting a complete replacement routing policy.
const REVISER_INSTRUCTIONS: &str = r#"You maintain the routing policy ("system prompt") of llmproxy's local-first interceptor. That policy tells a small local model when to answer an agent request itself and when to route it onward. Intercept mode uses PASS for fall-through. Integrated mode uses ROUTE_DEFAULT for normal routing and configured route functions for explicit provider/model routes.

The user supplies the current policy and a requested change. Reply with ONLY the complete revised policy text, ready to be used verbatim as the new system prompt. No preamble, no explanation, no code fences, no surrounding quotes.

You only edit this routing policy. If the request is unrelated to editing the policy, or asks you to do anything else, return the current policy text unchanged."#;

/// Ask the interceptor's local model to rewrite the routing instructions.
/// Returns the revised text for the admin editor; nothing is persisted here.
pub(crate) async fn revise_instructions(
    state: &AppState,
    current: &str,
    request: &str,
    mode: RouteTestMode,
) -> Value {
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
        match mode {
            RouteTestMode::Intercept => DEFAULT_INSTRUCTIONS,
            RouteTestMode::Integrated => DEFAULT_INTEGRATED_INSTRUCTIONS,
        }
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
            error!(
                request_id = %meta.request_id,
                provider = %provider.name,
                timeout_ms,
                "routing policy revision timed out"
            );
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
