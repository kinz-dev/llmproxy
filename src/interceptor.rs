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
use crate::privacy;
use crate::request_id_from_headers;
use crate::rewrite_request_roles;
use crate::stats::{
    RouteClass, RoutingDecisionAudit, TokenUsage, log_request_value, log_response_text,
    log_response_value, proxy_stats, route_class_for_base_url, routing_decision_log,
    usage_token_fields,
};
use crate::text_from_value;
use crate::upstream_error_response;
use crate::upstream_json;
use crate::{ProviderState, proxy_integrated_chat};

const PASS_TOOL_NAME: &str = "PASS";
const ROUTE_DEFAULT_TOOL_NAME: &str = "ROUTE_DEFAULT";
const REJECT_TOOL_NAME: &str = "REJECT";
const GENERIC_REJECTION_REASON: &str = "other sensitive information";
const SENSITIVE_REJECTION_INSTRUCTIONS: &str = r#"Sensitive-data rejection is enabled and takes precedence over every execution or routing choice. Estimate the likelihood that the original request contains an actual credential, secret, personally identifiable information, or other sensitive information. At 75% likelihood or higher, call REJECT alone. Select only the matching safe reason category; never copy, quote, transform, or summarize the sensitive value. Below 75%, continue with the normal routing policy. Do not answer the request, call a caller tool, or select any provider route when the rejection threshold is met."#;
const LOCAL_ROUTE_HEADER: &str = "x-llmproxy-route";
const CODEX_TURN_METADATA_KEY: &str = "x-codex-turn-metadata";
const CODEX_REQUEST_KIND_KEY: &str = "request_kind";
const CODEX_AGENT_TURN_KIND: &str = "turn";
const CODEX_COMPACTION_KIND: &str = "compaction";
const COMPACTION_TRIGGER_TYPE: &str = "compaction_trigger";
const DEFAULT_INSTRUCTIONS: &str = r#"You are the triage and deterministic-execution provider for agent requests. Estimate whether you can complete the request correctly and deterministically, or must call PASS for normal provider routing.

Complete the request yourself only when the likelihood of correct deterministic execution is greater than 80%, based on ALL of these signals:
- The request is explicit and unambiguous, and everything needed is already in this conversation.
- The task is deterministic or routine: formatting, extraction, classification, validation, a simple transformation of supplied content, or a direct operational command.
- It needs at most one tool call, and the tool and its exact arguments are obvious without discovery.
- One short response completes it without research, planning, comparison, or judgment.

Otherwise call PASS with a brief reason naming the observable condition that reduced deterministic-execution likelihood — including whenever the task needs research, current information, planning, debugging, comparison, recommendation, judgment, or could have financial, legal, medical, security, or operational consequences. Treat exactly 80% as insufficient. When unsure, PASS.

Never ask clarifying questions or answer partially. The reason is concise audit metadata, not hidden reasoning, and must state the estimated likelihood plus the observable condition that matched. It must appear only in the routing tool arguments. Emit only the PASS call, or exactly the assistant message or single tool call that completes the request."#;

/// Base routing policy used instead of [`DEFAULT_INSTRUCTIONS`] when
/// integrated mode is on and the operator has not overridden the prompt.
/// The concrete provider routes and their hints are appended dynamically.
const DEFAULT_INTEGRATED_INSTRUCTIONS: &str = r#"You are the provider-routing tier for agent requests. You route only: never answer the request yourself and never call a caller-supplied tool.

Estimate the ownership likelihood for every available provider route. Call exactly one route when its likelihood is greater than 80%. Route hints define ownership: a deterministic route may claim explicit routine work, a specialized route may claim its stated domain, and a hint marked as the default may claim work that no more-specific route owns. Treat exactly 80% as insufficient. Call PASS when no configured route exceeds the threshold.

Complex online research, current-information investigation, and source-backed multi-step analysis must use the matching dedicated research route at max or xhigh reasoning effort when one is configured. Never execute that work in triage or send it to a weaker general route.

Never ask clarifying questions or answer partially. Include a brief reason stating the estimated likelihood and observable route hint or threshold condition that matched; do not provide hidden reasoning. Emit exactly one configured provider route call or PASS."#;

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
fn rule_route_enabled(config: &crate::Config, route: &str) -> bool {
    if let Some(target) = config.integrated.targets.get(route) {
        return config.provider_enabled(&target.provider);
    }
    route
        .split_once('/')
        .is_none_or(|(provider, _)| config.provider_enabled(provider))
}

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
    /// Fail-closed private-network screening before any execution or route.
    pub(crate) reject_sensitive: bool,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) timeout_ms: u64,
    pub(crate) fallback_on_error: bool,
    pub(crate) instructions: String,
    /// Sampling temperature for the triage request.
    pub(crate) temperature: f64,
    /// Completion cap for the triage request; 0 leaves the client's own
    /// limits untouched. Capped completions fall back to normal provider
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

/// What a matching routing rule decided.
pub(crate) enum RuleOutcome {
    /// Normal provider routing, with no triage request at all.
    Pass,
    /// A concrete destination, named for the audit.
    Route(String, IntegratedTarget),
}

/// The first rule whose conditions all hold, if any. Rules are ordered, and
/// the first match wins, so a specific rule belongs above a general one.
pub(crate) fn match_routing_rule(
    state: &AppState,
    chat_request: &Value,
    client_model: Option<&str>,
) -> Option<(String, RuleOutcome)> {
    let estimated_tokens = crate::estimate_tokens(chat_request).tokens;
    let has_tools = chat_request
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    let text = rule_match_text(chat_request);

    for rule in &state.config.routing_rules {
        if !rule.matches(estimated_tokens, has_tools, &text, client_model) {
            continue;
        }
        if !rule_route_enabled(&state.config, &rule.route) {
            continue;
        }
        let outcome = if rule.route.eq_ignore_ascii_case(crate::ROUTING_RULE_PASS) {
            RuleOutcome::Pass
        } else if let Some(target) = state.config.integrated.targets.get(&rule.route) {
            RuleOutcome::Route(rule.route.clone(), target.clone())
        } else if let Some((provider, model)) = rule.route.split_once('/') {
            RuleOutcome::Route(
                rule.route.clone(),
                IntegratedTarget {
                    provider: provider.to_string(),
                    model: model.to_string(),
                    hint: format!("routing rule {:?}", rule.name),
                    reasoning_effort: None,
                },
            )
        } else {
            // validate_config rejects unknown routes, so this only happens if
            // config changed underneath us. Fall through to triage.
            continue;
        };
        return Some((rule.name.clone(), outcome));
    }
    None
}

/// The user-authored text a rule matches against: the last user turn, which is
/// the request the agent is actually making. Earlier turns are context.
fn rule_match_text(chat_request: &Value) -> String {
    chat_request
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .map(|message| crate::text_from_value(message.get("content").unwrap_or(&Value::Null)))
        .unwrap_or_default()
        .to_lowercase()
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
            reject_sensitive: false,
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
    Reject,
    DefaultRoute,
    Redirect(String),
    InvalidControl,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompletionClassification {
    decision: CompletionDecision,
    reason: Option<String>,
}

pub(crate) enum InterceptionOutcome {
    Continue(Box<RoutingAuditDraft>),
    Respond(Response),
}

struct ClientResponseContext {
    protocol: InterceptorProtocol,
    model: Option<String>,
    streaming: bool,
}

impl ClientResponseContext {
    fn new(protocol: InterceptorProtocol, model: Option<String>, streaming: bool) -> Self {
        Self {
            protocol,
            model,
            streaming,
        }
    }
}

pub(crate) struct RoutingAuditDraft {
    audit: RoutingDecisionAudit,
}

impl RoutingAuditDraft {
    fn new(
        state: &AppState,
        request_id: String,
        protocol: InterceptorProtocol,
        request: &Value,
        requested_model: Option<String>,
        integrated: bool,
    ) -> Self {
        let mode = if integrated
            || (!state.config.interceptor.enabled && state.config.integrated.enabled)
        {
            "integrated"
        } else {
            "intercept"
        };
        let policy = match mode {
            "integrated" if integrated_instructions_are_default(&state.config.integrated) => {
                "built-in"
            }
            "intercept" if intercept_instructions_are_default(&state.config.interceptor) => {
                "built-in"
            }
            _ => "custom",
        };
        Self {
            audit: RoutingDecisionAudit {
                request_id,
                mode,
                protocol: protocol.as_str(),
                requested_model,
                prompt: request_prompt_preview(request),
                decision: "pending",
                reason: String::new(),
                policy,
                triage_provider: None,
                triage_model: None,
                triage_elapsed_ms: None,
                target: None,
                target_hint: None,
                provider: None,
                model: None,
                route_class: None,
                reasoning_effort: None,
                error: None,
            },
        }
    }

    fn set_decision(&mut self, decision: &'static str, reason: impl Into<String>) {
        self.audit.decision = decision;
        self.audit.reason = reason.into();
    }

    fn set_triage(&mut self, provider: &ProviderState, model: &str, elapsed: Duration) {
        self.audit.triage_provider = Some(provider.name.clone());
        self.audit.triage_model = Some(model.to_string());
        self.audit.triage_elapsed_ms = Some(elapsed.as_millis().min(u128::from(u64::MAX)) as u64);
    }

    fn set_target(&mut self, name: &str, target: &IntegratedTarget) {
        self.audit.target = Some(name.to_string());
        self.audit.target_hint = Some(target.hint.clone());
        self.audit.reasoning_effort = target
            .reasoning_effort
            .map(crate::ReasoningEffort::as_str)
            .map(str::to_string);
    }

    fn set_error(&mut self, error: impl Into<String>) {
        self.audit.error = Some(error.into());
    }

    fn redact_prompt(&mut self) {
        self.audit.prompt = "[redacted: sensitive prompt]".to_string();
    }

    pub(crate) fn record_destination(mut self, provider: &ProviderState, model: &str) {
        self.audit.provider = Some(provider.name.clone());
        self.audit.model = Some(model.to_string());
        self.audit.route_class = Some(route_class_for_base_url(&provider.config.base_url));
        if self.audit.reasoning_effort.is_none() {
            self.audit.reasoning_effort = provider
                .config
                .reasoning_effort
                .map(crate::ReasoningEffort::as_str)
                .map(str::to_string);
        }
        routing_decision_log().record(self.audit);
    }

    /// A peer node served the request; it never reached a provider here.
    pub(crate) fn record_peer_destination(mut self, peer: &str, url: &str, model: &str) {
        self.audit.provider = Some(format!("peer:{peer}"));
        self.audit.model = Some(model.to_string());
        self.audit.route_class = Some(route_class_for_base_url(url));
        routing_decision_log().record(self.audit);
    }

    pub(crate) fn record_route_error(mut self, error: impl Into<String>) {
        self.set_error(error);
        if self.audit.reason.is_empty() {
            self.audit.reason =
                "Normal routing failed before a provider/model destination was resolved."
                    .to_string();
        }
        routing_decision_log().record(self.audit);
    }

    fn record(self) {
        routing_decision_log().record(self.audit);
    }
}

fn request_prompt_preview(request: &Value) -> String {
    request
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| {
            messages
                .iter()
                .rev()
                .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        })
        .and_then(|message| message.get("content"))
        .map(text_from_value)
        .unwrap_or_else(|| "(no user text)".to_string())
}

pub(crate) fn validate_config(
    config: &InterceptorConfig,
    providers: &HashMap<String, UpstreamConfig>,
) -> Result<()> {
    if !config.enabled && !config.reject_sensitive {
        return Ok(());
    }
    if config.provider.trim().is_empty() {
        bail!("interceptor.provider cannot be empty when the interceptor is enabled");
    }
    let Some(provider) = providers.get(&config.provider) else {
        bail!(
            "interceptor references unknown provider {:?}",
            config.provider
        );
    };
    if config.reject_sensitive
        && crate::stats::route_class_for_base_url(&provider.base_url) == RouteClass::External
    {
        bail!(
            "interceptor.reject_sensitive requires a loopback or private-network triage provider; provider {:?} is remote and would receive the prompt before REJECT",
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
    let mut canonical_names = std::collections::HashSet::new();
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
                "integrated remote provider {name:?} requires proxy-owned api_key or api_key_env (or auth = \"codex-oauth\"/\"claude-oauth\")"
            );
        }
    }
    for (name, target) in &config.targets {
        if !valid_target_name(name)
            || is_default_route_name(name)
            || canonical_target_name(name) == REJECT_TOOL_NAME
        {
            bail!(
                "integrated target {name:?} must be a non-reserved 1-64 character tool name containing only letters, digits, _, or -"
            );
        }
        if !canonical_names.insert(canonical_target_name(name)) {
            bail!("integrated target {name:?} duplicates another target after normalization");
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

pub(crate) fn key_or(key: Option<u64>) -> u64 {
    key.unwrap_or(0)
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
) -> InterceptionOutcome {
    let config = &state.config.interceptor;
    let integrated =
        integrated_routing_requested(&state.config.integrated, client_model.as_deref());
    let request_id = request_id_from_headers(incoming_headers);
    let mut audit = RoutingAuditDraft::new(
        state,
        request_id.clone(),
        protocol,
        &local_chat_request,
        client_model.clone(),
        integrated,
    );
    if !config.enabled && !integrated && !config.reject_sensitive {
        audit.set_decision(
            "bypass",
            "An explicit model bypassed integrated triage; intercept mode is inactive.",
        );
        return InterceptionOutcome::Continue(Box::new(audit));
    }

    if let Some(request_kind) = policy.bypass_kind() {
        info!(
            request_id,
            protocol = protocol.as_str(),
            request_kind,
            route = "normal_provider",
            "triage bypassed control request"
        );
        proxy_stats().record_intercept_bypass();
        audit.set_decision(
            "bypass",
            format!("Native control request kind {request_kind:?} requires provider passthrough."),
        );
        return InterceptionOutcome::Continue(Box::new(audit));
    }
    proxy_stats().record_intercept_attempt();

    let conversation_key =
        integrated.then(|| conversation_key(incoming_headers, &local_chat_request));
    if !config.reject_sensitive
        && let Some(key) = conversation_key
        && let Some(target) = sticky_routes().lookup(key)
        && state.config.active_targets().contains_key(&target)
    {
        let target_config = state
            .config
            .integrated
            .targets
            .get(&target)
            .expect("validated sticky route target missing");
        audit.set_decision(
            "sticky",
            "The conversation already has a successful integrated route within the sticky TTL.",
        );
        audit.set_target(&target, target_config);
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
            ClientResponseContext::new(protocol, client_model, client_streaming),
            Some(key),
            audit,
        )
        .await;
    }

    // Deterministic rules run before any triage request. Screening is the one
    // thing they must never skip: when reject_sensitive is on, the prompt has
    // to be classified before anything decides where it goes.
    if !config.reject_sensitive
        && let Some((rule_name, outcome)) =
            match_routing_rule(state, &local_chat_request, client_model.as_deref())
    {
        proxy_stats().record_rule_match();
        match outcome {
            RuleOutcome::Pass => {
                info!(
                    request_id,
                    protocol = protocol.as_str(),
                    rule = rule_name,
                    route = "normal_provider",
                    "routing rule passed the request through without triage"
                );
                proxy_stats().record_intercept_pass();
                audit.set_decision(
                    "rule",
                    format!("Routing rule {rule_name:?} selected normal provider routing."),
                );
                return InterceptionOutcome::Continue(Box::new(audit));
            }
            RuleOutcome::Route(target_name, target) => {
                info!(
                    request_id,
                    protocol = protocol.as_str(),
                    rule = rule_name,
                    target = target_name,
                    provider = target.provider,
                    model = target.model,
                    route = "rule_redirect",
                    "routing rule selected a destination without triage"
                );
                audit.set_decision(
                    "rule",
                    format!("Routing rule {rule_name:?} selected {target_name}."),
                );
                audit.set_target(&target_name, &target);
                return execute_target_redirect(
                    state,
                    &target_name,
                    &target,
                    local_chat_request,
                    ClientResponseContext::new(protocol, client_model, client_streaming),
                    conversation_key,
                    audit,
                )
                .await;
            }
        }
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
                "triage could not prepare request"
            );
            return fallback_or_error(
                config,
                StatusCode::BAD_GATEWAY,
                &request_id,
                message,
                audit,
                ClientResponseContext::new(protocol, client_model, client_streaming),
            );
        }
    };

    let provider = state
        .providers
        .get(&config.provider)
        .expect("validated interceptor provider missing from runtime state");
    let triage_class = route_class_for_base_url(&provider.config.base_url);
    let meta = UpstreamRequestMeta::new(
        &provider.name,
        incoming_headers,
        Method::POST,
        "chat/completions",
        UpstreamResponseKind::Json,
        false,
    );
    let meta = if config.reject_sensitive {
        meta.without_body_logging()
    } else {
        meta
    };
    let local_headers = HeaderMap::new();
    let input_estimate = crate::estimate_tokens(&local_request);
    audit.audit.triage_provider = Some(provider.name.clone());
    audit.audit.triage_model = Some(config.model.clone());

    if !config.reject_sensitive {
        log_request_value(triage_class, &provider.name, &local_request);
    }
    let started_at = std::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_millis(config.timeout_ms),
        upstream_json(provider, &local_headers, &meta, Some(&local_request)),
    )
    .await;

    let completion = match result {
        Ok(Ok(completion)) => completion,
        Ok(Err(error)) => {
            audit.audit.triage_elapsed_ms =
                Some(started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64);
            log_response_text(triage_class, &provider.name, format!("error: {error}"));
            crate::stats::error_log().record(&provider.name, "interceptor", error.to_string());
            error!(
                request_id = %meta.request_id,
                protocol = protocol.as_str(),
                provider = %provider.name,
                elapsed_ms = started_at.elapsed().as_millis(),
                error = %error,
                fallback = config.fallback_on_error,
                "triage provider request failed"
            );
            audit.set_error(error.to_string());
            if config.reject_sensitive {
                return fallback_or_error(
                    config,
                    StatusCode::BAD_GATEWAY,
                    &meta.request_id,
                    error.to_string(),
                    audit,
                    ClientResponseContext::new(protocol, client_model, client_streaming),
                );
            }
            return if config.fallback_on_error {
                proxy_stats().record_intercept_fallback();
                audit.set_decision(
                    "fallback",
                    "The triage provider request failed and fallback_on_error selected normal routing.",
                );
                InterceptionOutcome::Continue(Box::new(audit))
            } else {
                proxy_stats().record_intercept_error();
                audit.set_decision(
                    "error",
                    "The triage provider request failed and fallback_on_error is disabled.",
                );
                audit.record();
                InterceptionOutcome::Respond(upstream_error_response(&error))
            };
        }
        Err(_) => {
            audit.audit.triage_elapsed_ms =
                Some(started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64);
            let message = format!("triage provider timed out after {}ms", config.timeout_ms);
            log_response_text(triage_class, &provider.name, message.as_str());
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
                audit,
                ClientResponseContext::new(protocol, client_model, client_streaming),
            );
        }
    };
    let local_elapsed = started_at.elapsed();
    let output_estimate = crate::estimate_tokens(&completion);
    let (reported_input, reported_output) = completion
        .get("usage")
        .map(usage_token_fields)
        .unwrap_or((None, None));
    let local_usage = TokenUsage::from_response(
        completion.get("usage"),
        (input_estimate.tokens, input_estimate.source),
        (output_estimate.tokens, output_estimate.source),
    );
    // A triage call that returned a real usage object trains the estimator just
    // like proxied traffic does: the screened request and the completion are the
    // actual texts behind the reported counts.
    proxy_stats().calibrate_from_usage(
        reported_input,
        reported_output,
        &input_estimate,
        Some(&crate::text_from_value(&local_request)),
        &crate::text_from_value(&completion),
    );
    audit.set_triage(provider, &config.model, local_elapsed);
    // Triage is a real billable call, not overhead that disappears from the
    // ledger: intercept mode adds one of these to every request it screens.
    proxy_stats().record_model_usage(
        &provider.name,
        &config.model,
        local_elapsed,
        local_usage,
        crate::request_pricing(&state.config, &provider.name, &config.model),
    );
    if !config.reject_sensitive {
        log_response_value(triage_class, &provider.name, &completion);
    }

    if first_chat_choice_message(&completion).is_none() {
        let message = "triage provider returned no assistant message".to_string();
        error!(
            request_id = %meta.request_id,
            protocol = protocol.as_str(),
            provider = %provider.name,
            fallback = config.fallback_on_error,
            "{message}"
        );
        return fallback_or_error(
            config,
            StatusCode::BAD_GATEWAY,
            &meta.request_id,
            message,
            audit,
            ClientResponseContext::new(protocol, client_model, client_streaming),
        );
    }

    if completion_truncated(&completion) {
        // A clipped answer must never reach the agent; the cap also signals
        // that this was not a short deterministic task.
        let message = format!(
            "triage completion hit the interceptor max_tokens cap ({})",
            config.max_tokens
        );
        error!(
            request_id = %meta.request_id,
            protocol = protocol.as_str(),
            provider = %provider.name,
            fallback = config.fallback_on_error,
            "{message}"
        );
        return fallback_or_error(
            config,
            StatusCode::BAD_GATEWAY,
            &meta.request_id,
            message,
            audit,
            ClientResponseContext::new(protocol, client_model, client_streaming),
        );
    }

    let no_targets = BTreeMap::new();
    let active_targets = state.config.active_targets();
    let decision_targets = if integrated {
        &*active_targets
    } else {
        &no_targets
    };
    let classification = enforce_router_only(
        classify_completion_details(&completion, decision_targets, config.reject_sensitive),
        integrated,
    );
    let reason = classification.reason.clone().unwrap_or_else(|| {
        match classification.decision {
            CompletionDecision::Local =>
                "The triage model emitted a direct answer or caller tool call and no routing control."
                    .to_string(),
            CompletionDecision::Reject => GENERIC_REJECTION_REASON.to_string(),
            CompletionDecision::DefaultRoute =>
                "The triage model emitted a legacy routing control without an audit reason."
                    .to_string(),
            CompletionDecision::Redirect(ref target) => state
                .config
                .integrated
                .targets
                .get(target)
                .map(|route| format!("The triage model selected the route hinted for: {}", route.hint))
                .unwrap_or_else(|| "The triage model selected an integrated route.".to_string()),
            CompletionDecision::InvalidControl =>
                "The triage model mixed routing control with other output or invalid arguments."
                    .to_string(),
        }
    });
    match classification.decision {
        CompletionDecision::Reject => {
            audit.set_decision("reject", reason.clone());
            audit.redact_prompt();
            proxy_stats().record_intercept_reject();
            audit.record();
            InterceptionOutcome::Respond(render_rejection(
                protocol,
                &reason,
                client_model,
                client_streaming,
                &meta.request_id,
            ))
        }
        CompletionDecision::DefaultRoute => {
            audit.set_decision(if integrated { "default_route" } else { "pass" }, reason);
            info!(
                request_id = %meta.request_id,
                protocol = protocol.as_str(),
                provider = %provider.name,
                elapsed_ms = local_elapsed.as_millis(),
                route = "normal_provider",
                "triage selected normal provider routing"
            );
            proxy_stats().record_intercept_pass();
            InterceptionOutcome::Continue(Box::new(audit))
        }
        CompletionDecision::Redirect(target) => {
            let target_config = state
                .config
                .integrated
                .targets
                .get(&target)
                .expect("classified integrated route missing from config");
            audit.set_decision("redirect", reason);
            audit.set_target(&target, target_config);
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
                ClientResponseContext::new(protocol, client_model, client_streaming),
                conversation_key,
                audit,
            )
            .await
        }
        CompletionDecision::InvalidControl => {
            audit.set_decision("invalid", reason);
            let message = "triage provider mixed routing control with other output".to_string();
            error!(
                request_id = %meta.request_id,
                protocol = protocol.as_str(),
                provider = %provider.name,
                fallback = config.fallback_on_error,
                "{message}"
            );
            fallback_or_error(
                config,
                StatusCode::BAD_GATEWAY,
                &meta.request_id,
                message,
                audit,
                ClientResponseContext::new(protocol, client_model, client_streaming),
            )
        }
        CompletionDecision::Local => {
            audit.set_decision("direct", reason);
            info!(
                request_id = %meta.request_id,
                protocol = protocol.as_str(),
                provider = %provider.name,
                elapsed_ms = local_elapsed.as_millis(),
                route = "direct",
                "triage provider answered directly"
            );
            proxy_stats().record_intercept_local(
                route_class_for_base_url(&provider.config.base_url),
                &provider.name,
                protocol.as_str(),
                local_elapsed,
                local_usage,
            );
            audit.record_destination(provider, &config.model);
            InterceptionOutcome::Respond(render_local_completion(
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
    client: ClientResponseContext,
    conversation_key: Option<u64>,
    audit: RoutingAuditDraft,
) -> InterceptionOutcome {
    let target = state
        .config
        .integrated
        .targets
        .get(target_name)
        .expect("validated integrated target missing");
    execute_target_redirect(
        state,
        target_name,
        target,
        chat_request,
        client,
        conversation_key,
        audit,
    )
    .await
}

/// Send a request to one concrete destination. Integrated targets come from
/// config; routing rules can also synthesize a `provider/model` destination
/// that was never declared as a target.
fn execute_target_redirect<'a>(
    state: &'a AppState,
    target_name: &'a str,
    target: &'a IntegratedTarget,
    chat_request: Value,
    client: ClientResponseContext,
    conversation_key: Option<u64>,
    audit: RoutingAuditDraft,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = InterceptionOutcome> + Send + 'a>> {
    Box::pin(execute_target_redirect_impl(
        state,
        target_name,
        target,
        chat_request,
        client,
        conversation_key,
        audit,
    ))
}

async fn execute_target_redirect_impl(
    state: &AppState,
    target_name: &str,
    target: &IntegratedTarget,
    chat_request: Value,
    client: ClientResponseContext,
    conversation_key: Option<u64>,
    mut audit: RoutingAuditDraft,
) -> InterceptionOutcome {
    let provider: &ProviderState = state
        .providers
        .get(&target.provider)
        .expect("validated integrated provider missing");
    // The destination of an integrated redirect is an explicit decision, so it
    // is also where the confidentiality boundary is allowed to apply its
    // declared private replacement. Sensitive or unclassifiable content never
    // reaches a public target, and a rule cannot route around that.
    if state.config.privacy.enabled() {
        let mut hop = privacy::RequestPrivacy::from_body(&chat_request).into_hop();
        let enforcement = privacy::check_hop(state, &mut hop, provider, &chat_request).await;
        if !enforcement.permitted {
            let disposition = privacy::private_redirect_disposition(state, provider, &chat_request);
            match disposition {
                privacy::RedirectDisposition::Block(reason) => {
                    let request_id = audit.audit.request_id.clone();
                    audit.set_decision("privacy_block", reason.clone());
                    audit.redact_prompt();
                    proxy_stats().record_privacy_block();
                    privacy::record_hop_denial(
                        state,
                        &request_id,
                        client.protocol.as_str(),
                        &chat_request,
                        &enforcement,
                    );
                    if let Some(sticky) = privacy::sticky_from(&enforcement) {
                        privacy::remember_conversation(key_or(conversation_key), &sticky);
                    }
                    audit.record();
                    return InterceptionOutcome::Respond(render_rejection(
                        client.protocol,
                        &reason,
                        client.model,
                        client.streaming,
                        &request_id,
                    ));
                }
                privacy::RedirectDisposition::Reroute(target_override) => {
                    info!(
                        target = target_name,
                        provider = target_override.provider,
                        model = target_override.model,
                        "privacy boundary replaced a public integrated route with the approved private target"
                    );
                    return execute_target_redirect(
                        state,
                        target_name,
                        &target_override,
                        chat_request,
                        client,
                        conversation_key,
                        audit,
                    )
                    .await;
                }
                privacy::RedirectDisposition::Proceed => {}
            }
        } else if !enforcement.decision.is_clear()
            && let Some(key) = conversation_key
        {
            privacy::remember_conversation(key, &enforcement.decision);
        }
    }
    info!(
        target = target_name,
        provider = target.provider,
        model = target.model,
        reasoning_effort = target.reasoning_effort.map(crate::ReasoningEffort::as_str),
        protocol = client.protocol.as_str(),
        "sending integrated route"
    );
    // An integrated destination is an audited routing decision, so it never
    // fails over for availability: the request goes to exactly the destination
    // the decision named, gated by the confidentiality boundary above.
    let permit = match crate::sharing::admit_audited(state, &provider.name, client.protocol).await {
        Ok(permit) => permit,
        Err(response) => {
            audit.record_route_error("no execution slot was available for the integrated target");
            return InterceptionOutcome::Respond(*response);
        }
    };
    let response = crate::sharing::hold(
        proxy_integrated_chat(
            provider,
            chat_request,
            target,
            client.protocol,
            client.model.clone(),
            client.streaming,
            state.config.as_ref(),
        )
        .await,
        permit,
    );
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
        audit.record_destination(provider, &target.model);
        InterceptionOutcome::Respond(response)
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
        audit.set_error(format!("HTTP {}", response.status()));
        audit.record_destination(provider, &target.model);
        InterceptionOutcome::Respond(response)
    }
}

fn fallback_or_error(
    config: &InterceptorConfig,
    status: StatusCode,
    request_id: &str,
    message: String,
    mut audit: RoutingAuditDraft,
    client: ClientResponseContext,
) -> InterceptionOutcome {
    crate::stats::error_log().record(&config.provider, "interceptor", message.as_str());
    audit.set_error(message.clone());
    if config.reject_sensitive {
        proxy_stats().record_intercept_reject();
        audit.redact_prompt();
        audit.set_decision(
            "reject",
            "Sensitive-data screening failed; fail-closed policy prevented provider routing.",
        );
        audit.record();
        return InterceptionOutcome::Respond(render_rejection(
            client.protocol,
            "sensitive information that could not be screened safely",
            client.model,
            client.streaming,
            request_id,
        ));
    }
    if config.fallback_on_error {
        proxy_stats().record_intercept_fallback();
        if audit.audit.decision == "pending" {
            audit.set_decision(
                "fallback",
                "The triage provider could not produce a valid decision and fallback_on_error selected normal routing.",
            );
        }
        InterceptionOutcome::Continue(Box::new(audit))
    } else {
        proxy_stats().record_intercept_error();
        if audit.audit.decision == "pending" {
            audit.set_decision(
                "error",
                "The triage provider could not produce a valid decision and fallback_on_error is disabled.",
            );
        }
        audit.record();
        InterceptionOutcome::Respond(interceptor_error_response(status, request_id, message))
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
    let targets = proxy_config.active_targets();
    rewrite_request_roles(&mut request, role_map);
    let Some(object) = request.as_object_mut() else {
        return Err("triage request is not a JSON object".to_string());
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
        .ok_or_else(|| "triage request tools field is not an array".to_string())?;
    if let Some(name) = tools
        .iter()
        .filter_map(tool_name)
        .find(|name| is_default_route_name(name) || canonical_target_name(name) == REJECT_TOOL_NAME)
    {
        return Err(format!("request already defines a tool named {name}"));
    }
    if integrated {
        for name in targets.keys() {
            let canonical_name = canonical_target_name(name);
            if tools
                .iter()
                .filter_map(tool_name)
                .any(|tool| canonical_target_name(tool) == canonical_name)
            {
                return Err(format!("request already defines a tool named {name}"));
            }
        }
    }
    if config.reject_sensitive {
        tools.push(reject_tool());
    }
    tools.push(default_route_tool(integrated));
    if integrated {
        tools.extend(
            targets
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
        for (name, target) in targets.iter() {
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
            "\nCall exactly one provider route when its ownership likelihood exceeds 80%; otherwise call PASS. Include the estimated likelihood and a brief observable audit reason in the call.",
        );
    }
    if config.reject_sensitive {
        dynamic_instructions.push_str("\n\nMandatory pre-routing rule:\n");
        dynamic_instructions.push_str(SENSITIVE_REJECTION_INSTRUCTIONS);
        if !config.enabled && !integrated {
            dynamic_instructions.push_str(
                "\nThis request is rejection-screening only. Below the rejection threshold, call PASS with a brief reason. Never answer directly.",
            );
        }
    }
    let messages = object
        .entry("messages".to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| "triage request messages field is not an array".to_string())?;
    messages.push(json!({
        "role": "system",
        "content": dynamic_instructions
    }));

    normalize_chat_completion_request(&mut request, proxy_config);
    Ok(request)
}

fn reject_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": REJECT_TOOL_NAME,
            "description": "Stop before any direct answer or provider route because the original request contains sensitive data. Never include the sensitive value.",
            "parameters": {
                "type": "object",
                "properties": {
                    "reason": {
                        "type": "string",
                        "enum": [
                            "credential or secret",
                            "personally identifiable information",
                            GENERIC_REJECTION_REASON
                        ],
                        "description": "A safe category only. Never repeat the detected value."
                    }
                },
                "required": ["reason"],
                "additionalProperties": false
            }
        }
    })
}

fn default_route_tool(integrated: bool) -> Value {
    let (name, description) = if integrated {
        (
            PASS_TOOL_NAME,
            "Continue this complete original request through normal model-alias and default-provider routing. Call this function alone when no configured provider route exceeds the routing threshold.",
        )
    } else {
        (
            PASS_TOOL_NAME,
            "Continue this complete original request through normal provider routing. Call this function alone when deterministic execution confidence is not greater than 80%.",
        )
    };
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": {
                "type": "object",
                "properties": {
                    "reason": {
                        "type": "string",
                        "maxLength": 240,
                        "description": "Briefly state the estimated likelihood and observable policy condition or route hint that matched. Do not provide chain-of-thought."
                    }
                },
                "required": ["reason"],
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
                "properties": {
                    "reason": {
                        "type": "string",
                        "maxLength": 240,
                        "description": "Briefly state the estimated likelihood and observable route hint that matched. Do not provide chain-of-thought."
                    }
                },
                "required": ["reason"],
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

fn is_default_route_name(name: &str) -> bool {
    matches!(
        canonical_target_name(name).as_str(),
        PASS_TOOL_NAME | ROUTE_DEFAULT_TOOL_NAME
    )
}

fn canonical_target_name(name: &str) -> String {
    name.trim().replace('-', "_").to_ascii_uppercase()
}

fn classify_completion_details(
    completion: &Value,
    targets: &BTreeMap<String, IntegratedTarget>,
    reject_sensitive: bool,
) -> CompletionClassification {
    let Some(choices) = completion.get("choices").and_then(Value::as_array) else {
        return classification(CompletionDecision::Local, None);
    };
    let contains_control = choices.iter().any(|choice| {
        let Some(message) = choice.get("message") else {
            return false;
        };
        message
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|calls| {
                calls.iter().any(|call| {
                    control_decision(tool_name(call), targets, reject_sensitive).is_some()
                })
            })
            || message.get("content").is_some_and(|content| {
                control_decision(
                    Some(text_from_value(content).trim()),
                    targets,
                    reject_sensitive,
                )
                .is_some()
            })
    });
    if !contains_control {
        return classification(CompletionDecision::Local, None);
    }
    if choices.len() != 1 {
        return classification(CompletionDecision::InvalidControl, None);
    }

    let Some(message) = choices[0].get("message") else {
        return classification(CompletionDecision::InvalidControl, None);
    };
    let content = message
        .get("content")
        .map(text_from_value)
        .unwrap_or_default();
    let tool_calls = message.get("tool_calls").and_then(Value::as_array);
    match tool_calls {
        Some(calls) if calls.len() == 1 && content.trim().is_empty() => {
            control_tool_call_classification(&calls[0], targets, reject_sensitive)
                .unwrap_or_else(|| classification(CompletionDecision::InvalidControl, None))
        }
        None => control_decision(Some(content.trim()), targets, reject_sensitive)
            .map(|decision| classification(decision, None))
            .unwrap_or_else(|| classification(CompletionDecision::InvalidControl, None)),
        Some(calls) if calls.is_empty() => {
            control_decision(Some(content.trim()), targets, reject_sensitive)
                .map(|decision| classification(decision, None))
                .unwrap_or_else(|| classification(CompletionDecision::InvalidControl, None))
        }
        _ => classification(CompletionDecision::InvalidControl, None),
    }
}

fn classification(
    decision: CompletionDecision,
    reason: Option<String>,
) -> CompletionClassification {
    let reason = if decision == CompletionDecision::Reject {
        Some(
            reason
                .as_deref()
                .and_then(safe_rejection_reason)
                .unwrap_or(GENERIC_REJECTION_REASON)
                .to_string(),
        )
    } else {
        reason
    };
    CompletionClassification { decision, reason }
}

fn enforce_router_only(
    result: CompletionClassification,
    integrated: bool,
) -> CompletionClassification {
    if integrated && result.decision == CompletionDecision::Local {
        return classification(
            CompletionDecision::InvalidControl,
            Some(
                "Integrated triage returned an answer instead of selecting a provider route or PASS."
                    .to_string(),
            ),
        );
    }
    result
}

fn control_tool_call_classification(
    call: &Value,
    targets: &BTreeMap<String, IntegratedTarget>,
    reject_sensitive: bool,
) -> Option<CompletionClassification> {
    let decision = control_decision(tool_name(call), targets, reject_sensitive)?;
    if decision == CompletionDecision::Reject {
        let reason = control_tool_reason(call).flatten();
        return Some(classification(decision, reason));
    }
    let reason = control_tool_reason(call)?;
    Some(classification(decision, reason))
}

fn safe_rejection_reason(reason: &str) -> Option<&'static str> {
    match reason.trim() {
        "credential or secret" => Some("credential or secret"),
        "personally identifiable information" => Some("personally identifiable information"),
        GENERIC_REJECTION_REASON => Some(GENERIC_REJECTION_REASON),
        _ => None,
    }
}

fn control_tool_reason(call: &Value) -> Option<Option<String>> {
    let arguments = call
        .get("function")
        .and_then(|function| function.get("arguments"))
        .or_else(|| call.get("arguments"));
    match arguments {
        None | Some(Value::Null) => Some(None),
        Some(Value::Object(arguments)) => reason_from_argument_object(arguments),
        Some(Value::String(arguments)) if arguments.trim().is_empty() => Some(None),
        Some(Value::String(arguments)) => {
            serde_json::from_str::<Value>(arguments)
                .ok()
                .and_then(|arguments| match arguments {
                    Value::Null => Some(None),
                    Value::Object(arguments) => reason_from_argument_object(&arguments),
                    _ => None,
                })
        }
        _ => None,
    }
}

fn reason_from_argument_object(
    arguments: &serde_json::Map<String, Value>,
) -> Option<Option<String>> {
    if arguments.is_empty() {
        return Some(None);
    }
    if arguments.len() != 1 {
        return None;
    }
    arguments
        .get("reason")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
        .map(|reason| Some(reason.to_string()))
}

fn control_decision(
    name: Option<&str>,
    targets: &BTreeMap<String, IntegratedTarget>,
    reject_sensitive: bool,
) -> Option<CompletionDecision> {
    let name = name?.trim();
    if reject_sensitive && name == REJECT_TOOL_NAME {
        return Some(CompletionDecision::Reject);
    }
    if is_default_route_name(name) {
        return Some(CompletionDecision::DefaultRoute);
    }
    let canonical = canonical_target_name(name);
    targets
        .keys()
        .find(|target| canonical_target_name(target) == canonical)
        .cloned()
        .map(CompletionDecision::Redirect)
}

/// True when any choice stopped for length: the completion is clipped and
/// must not be served as a direct answer.
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
/// targets, so either policy can be evaluated without changing the live
/// instance mode.
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
    let fallback_note = if config.reject_sensitive {
        "live traffic would fail closed without contacting an execution provider"
    } else if config.fallback_on_error {
        "live traffic would fall back to normal provider routing on this failure"
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
    let meta = if config.reject_sensitive {
        meta.without_body_logging()
    } else {
        meta
    };
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
                "detail": format!("triage provider timed out after {}ms", config.timeout_ms),
                "note": fallback_note,
                "elapsed_ms": elapsed_ms,
            });
        }
    };

    let Some(message) = first_chat_choice_message(&completion) else {
        return json!({
            "decision": "error",
            "detail": "triage provider returned no assistant message",
            "note": fallback_note,
            "elapsed_ms": elapsed_ms,
        });
    };
    if completion_truncated(&completion) {
        return json!({
            "decision": "error",
            "detail": format!(
                "triage completion hit the max_tokens cap ({}); the answer would be clipped",
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
    let classification = enforce_router_only(
        classify_completion_details(&completion, targets, config.reject_sensitive),
        mode == RouteTestMode::Integrated,
    );
    let audit_reason = classification.reason.clone();
    let (decision, target, note) = match classification.decision {
        CompletionDecision::Local => (
            "direct",
            None,
            "the triage provider would answer this request itself".to_string(),
        ),
        CompletionDecision::Reject => (
            "reject",
            None,
            "REJECT — returns a no-action response without contacting an execution provider"
                .to_string(),
        ),
        CompletionDecision::DefaultRoute => match mode {
            RouteTestMode::Intercept => (
                "external",
                None,
                format!("{PASS_TOOL_NAME} — forwards untouched to normal provider routing"),
            ),
            RouteTestMode::Integrated => (
                "default_route",
                None,
                format!("{PASS_TOOL_NAME} — continues through model aliases to the default provider"),
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
            "the triage provider mixed routing control with other output; live traffic follows fallback_on_error".to_string(),
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
        "reason": audit_reason,
        "detail": detail,
        "elapsed_ms": elapsed_ms,
        "provider": provider.name,
        "model": config.model,
        "used_default_instructions": used_default_instructions,
    })
}

/// The meta-prompt for the admin tab's policy reviser. It confines the triage
/// provider to one job: emitting a complete replacement routing policy.
const REVISER_INSTRUCTIONS: &str = r#"You maintain the routing policy ("system prompt") of llmproxy's triage provider. Intercept mode may complete high-confidence deterministic requests and uses PASS for normal routing. Integrated mode is router-only: it selects configured provider/model routes and uses PASS when no route exceeds the routing threshold. Sensitive rejection, when enabled, takes precedence.

The user supplies the current policy and a requested change. Reply with ONLY the complete revised policy text, ready to be used verbatim as the new system prompt. No preamble, no explanation, no code fences, no surrounding quotes.

You only edit this routing policy. If the request is unrelated to editing the policy, or asks you to do anything else, return the current policy text unchanged."#;

/// Ask the triage provider to rewrite the routing instructions.
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
    // Give the triage provider more room than the routing timeout.
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
                "error": format!("triage provider timed out after {timeout_ms}ms"),
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
            "error": "triage provider returned no revision text",
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
pub(crate) mod render;
use render::{render_local_completion, render_rejection};

#[cfg(test)]
#[path = "interceptor_tests.rs"]
mod tests;
