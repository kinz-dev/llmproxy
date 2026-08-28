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
use crate::route_cache::CachedDecision;
use crate::routing_verbs::RoutingVerb;
use crate::stats::{
    RouteClass, RoutingDecisionAudit, TokenUsage, log_request_value, log_response_text,
    log_response_value, proxy_stats, route_class_for_base_url, routing_decision_log,
};
use crate::text_from_value;
use crate::upstream_error_response;
use crate::upstream_json;
use crate::{ProviderState, proxy_integrated_chat};

const PASS_TOOL_NAME: &str = "PASS";
const ROUTE_DEFAULT_TOOL_NAME: &str = "ROUTE_DEFAULT";
const REJECT_TOOL_NAME: &str = "REJECT";
const GENERIC_REJECTION_REASON: &str = "other sensitive information";
const SENSITIVE_REJECTION_INSTRUCTIONS: &str = r#"Sensitive-data rejection is enabled and takes precedence over every local answer and routing choice. If the original request contains an actual credential, secret, personally identifiable information, or other sensitive information, call REJECT alone. Select only the matching safe reason category; never copy, quote, transform, or summarize the sensitive value. Do not answer the request, call a caller tool, or select any provider route."#;
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

Otherwise call PASS with a brief reason naming the policy condition that matched — including whenever the task needs research, current information, planning, debugging, comparison, recommendation, judgment, or could have financial, legal, medical, security, or operational consequences. When unsure, PASS.

Never ask clarifying questions or answer partially. The reason is concise audit metadata, not hidden reasoning, and must appear only in the routing tool arguments. Emit only the PASS call, or exactly the assistant message or single tool call that completes the request."#;

/// Base routing policy used instead of [`DEFAULT_INSTRUCTIONS`] when
/// integrated mode is on and the operator has not overridden the prompt.
/// The concrete provider routes and their hints are appended dynamically.
const DEFAULT_INTEGRATED_INSTRUCTIONS: &str = r#"You are the routing tier for agent requests. Give every request exactly one outcome: answer it locally, or hand it to the provider route that owns its kind of work.

Answer locally only when ALL of these hold:
- The request is explicit and unambiguous, and everything needed is already in this conversation.
- The task is deterministic or routine: formatting, extraction, classification, validation, a simple transformation of supplied content, or a direct operational command.
- It needs at most one tool call, and the tool and its exact arguments are obvious without discovery.
- One short response completes it, and you are certain it is correct.

Everything else is routed, never answered locally: research, planning, investigation, design, implementation, debugging, comparison, recommendation, judgment, and anything with financial, legal, medical, security, or operational consequences. Call exactly one provider route from the list below, choosing the route whose hint matches the request; a route hinted as the default owns everything no other route clearly claims. Include a brief reason naming the policy condition or route hint that matched. Call ROUTE_DEFAULT only when no route fits.

Complex online research, current-information investigation, and source-backed multi-step analysis must use the matching dedicated research route at max or xhigh reasoning effort when one is configured. Never answer that work locally or send it to a weaker general route.

Never ask clarifying questions or answer partially. The reason is concise audit metadata, not hidden reasoning, and must appear only in the routing tool arguments. Emit only one provider route call, the ROUTE_DEFAULT call, or exactly the assistant message or single tool call that completes the request."#;

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
    /// Fail-closed local screening before any normal/local routing outcome.
    pub(crate) reject_sensitive: bool,
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
    /// Newest non-system messages copied into the triage request. The whole
    /// conversation is otherwise copied, so triage input grows with the
    /// conversation; trimming it cuts cost and reduces the noise the hints are
    /// matched against. Zero copies everything.
    pub(crate) triage_max_messages: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub(crate) struct IntegratedConfig {
    pub(crate) enabled: bool,
    /// Idle-gap TTL. It slides on every successful redirect, so on its own it
    /// bounds nothing about an actively worked session.
    pub(crate) sticky_ttl_secs: u64,
    /// Wall-clock lifetime measured from when the route was first chosen and
    /// *not* refreshed, so a long session is re-triaged periodically even
    /// under continuous use. Zero leaves the route pinned indefinitely.
    pub(crate) sticky_absolute_ttl_secs: u64,
    /// Turns one sticky entry may serve before routing is reconsidered.
    /// Zero is unlimited.
    pub(crate) sticky_max_turns: u32,
    /// What a re-triage is allowed to do to an already-pinned route.
    pub(crate) sticky_policy: StickyPolicy,
    pub(crate) sticky_capacity: usize,
    /// Recognise in-band routing verbs at the start of the newest user
    /// message. Disable to pass every message through byte-for-byte.
    pub(crate) verbs: bool,
    /// Embedding-cached routing, sticky drift detection, and retry-driven
    /// escalation, all of which need one vector per request.
    pub(crate) embedding: crate::route_cache::EmbeddingConfig,
    /// Default failover chain for targets that do not define their own.
    /// Quota exhaustion and outages are absorbed here rather than reported to
    /// the caller mid-task.
    pub(crate) failover: Vec<String>,
    /// Issue the presumed-default route concurrently with triage and cancel
    /// the loser, taking the triage round trip off the critical path at the
    /// cost of some wasted tokens.
    pub(crate) speculative: bool,
    /// Integrated-mode routing policy override; empty uses the built-in
    /// integrated default.
    pub(crate) instructions: String,
    pub(crate) targets: BTreeMap<String, IntegratedTarget>,
}

/// How a re-triage interacts with the route a session is already pinned to.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StickyPolicy {
    /// A re-triage may move the session anywhere, including to a weaker route.
    Pin,
    /// A later turn may move up to a stronger route but never back down, so
    /// cost control survives without pinning quality to whatever the first
    /// message of the session happened to be.
    #[default]
    Escalate,
}

impl StickyPolicy {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Pin => "pin",
            Self::Escalate => "escalate",
        }
    }
}

impl Default for IntegratedConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sticky_ttl_secs: 1800,
            sticky_absolute_ttl_secs: 3600,
            sticky_max_turns: 0,
            sticky_policy: StickyPolicy::Escalate,
            sticky_capacity: 1024,
            verbs: true,
            embedding: crate::route_cache::EmbeddingConfig::default(),
            failover: Vec::new(),
            speculative: false,
            instructions: String::new(),
            targets: BTreeMap::new(),
        }
    }
}

impl IntegratedConfig {
    fn sticky_bounds(&self) -> StickyBounds {
        StickyBounds {
            absolute_ttl: (self.sticky_absolute_ttl_secs > 0)
                .then(|| Duration::from_secs(self.sticky_absolute_ttl_secs)),
            max_turns: (self.sticky_max_turns > 0).then_some(self.sticky_max_turns),
            drift_below: if self.embedding.is_active() {
                self.embedding.retriage_below
            } else {
                0.0
            },
        }
    }

    /// The next route up from `current`: the weakest target that is still
    /// stronger than it. Used when a retry says the answer was unsatisfying.
    fn stronger_than(&self, current: &str) -> Option<String> {
        let floor = self.strength(current);
        self.targets
            .iter()
            .filter(|(name, target)| {
                name.as_str() != current && target.strength.unwrap_or(0) > floor
            })
            .min_by_key(|(name, target)| (target.strength.unwrap_or(0), (*name).clone()))
            .map(|(name, _)| name.clone())
    }

    /// Routing strength of a target, defaulting to the weakest rank so an
    /// unranked configuration can never accidentally block an escalation.
    fn strength(&self, target: &str) -> u32 {
        self.targets
            .get(target)
            .and_then(|target| target.strength)
            .unwrap_or(0)
    }
}

fn integrated_routing_requested(config: &IntegratedConfig, client_model: Option<&str>) -> bool {
    config.enabled && client_model == Some(INTEGRATED_AUTO_MODEL)
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct IntegratedTarget {
    pub(crate) provider: String,
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) hint: String,
    #[serde(default)]
    pub(crate) reasoning_effort: Option<crate::ReasoningEffort>,
    /// The catch-all route. `ROUTE_DEFAULT` resolves here instead of falling
    /// through to first-upstream-model discovery. At most one target may claim
    /// it once project and user-level targets have been merged.
    #[serde(default, rename = "default")]
    pub(crate) is_default: bool,
    /// Relative routing strength, higher being stronger. Only the ordering
    /// matters, and only to `sticky_policy = "escalate"`, which uses it to
    /// refuse a mid-session demotion.
    #[serde(default)]
    pub(crate) strength: Option<u32>,
    /// Other targets to try, in order, when this one is unavailable or
    /// answers with a retryable failure. Empty inherits
    /// `integrated.failover`.
    #[serde(default)]
    pub(crate) failover: Vec<String>,
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
            triage_max_messages: 24,
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
    Continue(RoutingAuditDraft),
    Respond(Response),
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

    fn note_escalation(&mut self, from: &str, to: &str) {
        self.audit.reason = format!(
            "{} The request repeated a recent one with no intervening tool use, so it was escalated from {from:?} to {to:?}.",
            self.audit.reason
        );
    }

    /// Record what the failover chain did, so the admin Decisions view shows
    /// an absorbed outage rather than an unexplained provider change.
    fn note_failover(&mut self, skipped: &[String], failed: &[String]) {
        let mut notes = Vec::new();
        if !skipped.is_empty() {
            notes.push(format!(
                "skipped unavailable routes: {}",
                skipped.join(", ")
            ));
        }
        if !failed.is_empty() {
            notes.push(format!("failed routes: {}", failed.join(", ")));
        }
        if notes.is_empty() {
            return;
        }
        let note = notes.join("; ");
        if self.audit.reason.is_empty() {
            self.audit.reason = note;
        } else {
            self.audit.reason = format!("{} Failover {note}.", self.audit.reason);
        }
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
    if let Err(error) = config
        .embedding
        .validate(|name| providers.contains_key(name))
    {
        bail!(error);
    }
    for candidate in &config.failover {
        if !config.targets.contains_key(candidate) {
            bail!("integrated.failover references unknown target {candidate:?}");
        }
    }
    for (name, target) in &config.targets {
        for candidate in &target.failover {
            if candidate == name {
                bail!("integrated target {name:?} lists itself in its failover chain");
            }
            if !config.targets.contains_key(candidate) {
                bail!(
                    "integrated target {name:?} failover references unknown target {candidate:?}"
                );
            }
        }
    }
    if config.speculative && default_target(config).is_none() {
        bail!(
            "integrated.speculative requires exactly one target with default = true to speculate on"
        );
    }
    let defaults = config
        .targets
        .iter()
        .filter(|(_, target)| target.is_default)
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>();
    if defaults.len() > 1 {
        bail!(
            "integrated targets {defaults:?} all set default = true; exactly one target may be the catch-all"
        );
    }
    for (name, target) in &config.targets {
        if !valid_target_name(name) || is_default_route_name(name) {
            bail!(
                "integrated target {name:?} must be a non-reserved 1-64 character tool name containing only letters, digits, _, or -"
            );
        }
        if config.verbs
            && crate::routing_verbs::RESERVED_VERBS
                .iter()
                .any(|verb| name.eq_ignore_ascii_case(verb))
        {
            bail!(
                "integrated target {name:?} collides with the in-band routing verb of the same name; \
                 rename the target or set integrated.verbs = false"
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

/// Whether a cached decision still names something this config can route to.
fn cached_decision_is_live(state: &AppState, decision: &CachedDecision) -> bool {
    match decision {
        CachedDecision::Local => true,
        CachedDecision::DefaultRoute => true,
        CachedDecision::Redirect(target) => state.config.integrated.targets.contains_key(target),
    }
}

/// Outcomes that are safe to replay. A rejection or an invalid control must
/// always be re-derived, never remembered.
fn cacheable_decision(decision: &CompletionDecision) -> Option<CachedDecision> {
    match decision {
        CompletionDecision::Local => Some(CachedDecision::Local),
        CompletionDecision::DefaultRoute => Some(CachedDecision::DefaultRoute),
        CompletionDecision::Redirect(target) => Some(CachedDecision::Redirect(target.clone())),
        CompletionDecision::Reject | CompletionDecision::InvalidControl => None,
    }
}

/// Targets worth advertising to the triage model right now.
///
/// A route whose provider is rate-limited or circuit-open is withheld, so
/// triage picks a working route instead of choosing one that would only fail
/// over afterwards. If that would leave nothing, every target is advertised:
/// an empty route list is worse than an optimistic one.
fn routable_targets(
    config: &IntegratedConfig,
    health: &crate::health::ProviderHealth,
) -> Vec<String> {
    let available = config
        .targets
        .iter()
        .filter(|(_, target)| health.peek(&target.provider).is_available())
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    if available.is_empty() {
        return config.targets.keys().cloned().collect();
    }
    available
}

/// The target a `<name>:` or `default:` directive names, when it resolves.
fn forced_route_for(config: &IntegratedConfig, verb: Option<&RoutingVerb>) -> Option<String> {
    match verb? {
        RoutingVerb::Target(name) if config.targets.contains_key(name) => Some(name.clone()),
        RoutingVerb::Default => default_target(config).map(|(name, _)| name.to_string()),
        _ => None,
    }
}

fn verb_route_reason(verb: Option<&RoutingVerb>) -> String {
    match verb {
        Some(RoutingVerb::Default) => {
            "An in-band \"default\" directive took the catch-all route without triage.".to_string()
        }
        Some(verb) => format!(
            "An in-band \"{}\" directive pinned this route without triage.",
            verb.as_str()
        ),
        None => "An in-band directive pinned this route without triage.".to_string(),
    }
}

/// Apply the escalate policy: a re-triage may move a session to a stronger
/// route but never back down to a weaker one.
fn apply_escalation_floor(
    config: &IntegratedConfig,
    chosen: &str,
    floor: Option<&str>,
) -> Option<String> {
    let floor = floor?;
    if floor == chosen || !config.targets.contains_key(floor) {
        return None;
    }
    (config.strength(chosen) < config.strength(floor)).then(|| floor.to_string())
}

/// The single target marked `default = true`, if the merged config has one.
pub(crate) fn default_target(config: &IntegratedConfig) -> Option<(&str, &IntegratedTarget)> {
    config
        .targets
        .iter()
        .find(|(_, target)| target.is_default)
        .map(|(name, target)| (name.as_str(), target))
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
    /// The request that pinned this route. Kept so a later turn can be tested
    /// for material drift rather than only for elapsed time.
    vector: Option<crate::route_cache::Vector>,
    /// When the route was first chosen. Deliberately *not* refreshed, so the
    /// absolute bound measures the age of the decision rather than the age of
    /// the last message.
    created_at: Instant,
    /// Sliding idle deadline, refreshed on every reuse.
    expires_at: Instant,
    /// Turns this entry has already served.
    turns: u32,
}

/// The limits that decide whether a pinned route may serve another turn.
#[derive(Debug, Clone, Copy, Default)]
struct StickyBounds {
    absolute_ttl: Option<Duration>,
    max_turns: Option<u32>,
    /// Similarity to the pinning request below which the conversation has
    /// changed subject enough to deserve re-triage. Zero disables the check.
    drift_below: f32,
}

/// What the store has to say about a conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StickyDecision {
    /// Reuse the pinned route; it is inside every bound.
    Reuse(String),
    /// A bound was reached. Routing is reconsidered, but the route that was in
    /// force is carried along so an escalate policy can refuse a demotion.
    Retriage { previous: String },
    /// Nothing is pinned.
    Fresh,
}

#[derive(Default)]
struct StickyRoutes {
    entries: Mutex<(HashMap<u64, StickyRoute>, VecDeque<u64>)>,
}

impl StickyRoutes {
    fn lookup(
        &self,
        key: u64,
        bounds: StickyBounds,
        vector: Option<&crate::route_cache::Vector>,
    ) -> StickyDecision {
        let now = Instant::now();
        let mut routes = self.entries.lock().expect("sticky route store poisoned");
        let (entries, _) = &mut *routes;
        let Some(route) = entries.get(&key) else {
            return StickyDecision::Fresh;
        };
        if route.expires_at <= now {
            // A real idle gap: the conversation is over as far as routing is
            // concerned, so nothing is carried forward.
            let target = route.target.clone();
            entries.remove(&key);
            let _ = target;
            return StickyDecision::Fresh;
        }
        let aged_out = bounds
            .absolute_ttl
            .is_some_and(|ttl| now.duration_since(route.created_at) >= ttl);
        let turned_out = bounds.max_turns.is_some_and(|max| route.turns >= max);
        // Material change: the conversation has moved far enough from the
        // request that chose this route that the choice is worth revisiting.
        let drifted = bounds.drift_below > 0.0
            && match (vector, route.vector.as_ref()) {
                (Some(current), Some(pinned)) => {
                    crate::route_cache::dot(current, pinned) < bounds.drift_below
                }
                _ => false,
            };
        if aged_out || turned_out || drifted {
            return StickyDecision::Retriage {
                previous: route.target.clone(),
            };
        }
        StickyDecision::Reuse(route.target.clone())
    }

    fn insert(
        &self,
        key: u64,
        target: String,
        ttl_secs: u64,
        capacity: usize,
        vector: Option<crate::route_cache::Vector>,
    ) {
        let mut routes = self.entries.lock().expect("sticky route store poisoned");
        let (entries, order) = &mut *routes;
        let now = Instant::now();
        entries.retain(|_, route| route.expires_at > now);
        // A refresh keeps the original decision timestamp and counts the turn;
        // re-pinning a different target starts a fresh decision.
        let previous = entries.remove(&key);
        let (created_at, turns) = match previous {
            Some(route) if route.target == target => (route.created_at, route.turns + 1),
            _ => (now, 1),
        };
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
                vector,
                created_at,
                expires_at: now + Duration::from_secs(ttl_secs),
                turns,
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

    #[cfg(test)]
    fn clear(&self) {
        let mut routes = self.entries.lock().expect("sticky route store poisoned");
        routes.0.clear();
        routes.1.clear();
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
    // `verb` was already detected and stripped from the caller's native body
    // by the protocol handler, so neither copy still carries it.
    verb: Option<RoutingVerb>,
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
            "An explicit model bypassed integrated triage and standalone interception is disabled.",
        );
        return InterceptionOutcome::Continue(audit);
    }

    if let Some(request_kind) = policy.bypass_kind() {
        info!(
            request_id,
            protocol = protocol.as_str(),
            request_kind,
            route = "external",
            "local interceptor bypassed control request"
        );
        proxy_stats().record_intercept_bypass();
        audit.set_decision(
            "bypass",
            format!("Native control request kind {request_kind:?} requires external passthrough."),
        );
        return InterceptionOutcome::Continue(audit);
    }
    proxy_stats().record_intercept_attempt();

    let conversation_key =
        integrated.then(|| conversation_key(incoming_headers, &local_chat_request));

    // Sensitive screening must precede anything that leaves this process and
    // anything that can skip triage, so embeddings and the route cache are
    // both off whenever it is enabled.
    let embedding = &state.config.integrated.embedding;
    let embeddings_active = integrated && embedding.is_active() && !config.reject_sensitive;
    let route_context = if embeddings_active {
        build_route_context(state, &local_chat_request, embedding).await
    } else {
        RouteContext::default()
    };

    // An explicit verb is the user overruling the router, so it takes
    // precedence over a pinned route. `unstick` also drops the pin itself.
    if matches!(verb, Some(RoutingVerb::Unstick))
        && let Some(key) = conversation_key
    {
        sticky_routes().remove(key);
    }
    let verb_overrides_sticky = verb.is_some();

    let mut escalation_floor: Option<String> = None;
    if !config.reject_sensitive
        && !verb_overrides_sticky
        && let Some(key) = conversation_key
    {
        match sticky_routes().lookup(
            key,
            state.config.integrated.sticky_bounds(),
            route_context.vector.as_ref(),
        ) {
            StickyDecision::Reuse(target)
                if state.config.integrated.targets.contains_key(&target) =>
            {
                let target_config = state
                    .config
                    .integrated
                    .targets
                    .get(&target)
                    .expect("validated sticky route target missing");
                audit.set_decision(
                    "sticky",
                    "The conversation already has a successful integrated route within the sticky bounds.",
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
                    protocol,
                    client_model,
                    client_streaming,
                    Some(key),
                    &route_context,
                    audit,
                )
                .await;
            }
            StickyDecision::Retriage { previous }
                if state.config.integrated.sticky_policy == StickyPolicy::Escalate
                    && state.config.integrated.targets.contains_key(&previous) =>
            {
                // The pin aged out, so routing is reconsidered — but an
                // escalate policy will not let the reconsideration move the
                // session to a weaker route than it already had.
                escalation_floor = Some(previous);
            }
            _ => {}
        }
    }

    // A target or default verb is a deterministic, triage-free route. Sensitive
    // screening is mandatory and fails closed, so when it is enabled the forced
    // route is applied after the screening request instead of skipping it.
    let forced_route = (!config.reject_sensitive)
        .then(|| forced_route_for(&state.config.integrated, verb.as_ref()))
        .flatten();
    if let Some(name) = forced_route {
        let target_config = state
            .config
            .integrated
            .targets
            .get(&name)
            .expect("forced verb route missing from config");
        audit.set_decision("verb_route", verb_route_reason(verb.as_ref()));
        audit.set_target(&name, target_config);
        info!(
            request_id,
            protocol = protocol.as_str(),
            target = name,
            route = "verb_route",
            "in-band directive pinned an integrated route"
        );
        return execute_integrated_redirect(
            state,
            &name,
            local_chat_request,
            protocol,
            client_model,
            client_streaming,
            conversation_key,
            &route_context,
            audit,
        )
        .await;
    }
    if !config.reject_sensitive && matches!(verb, Some(RoutingVerb::Default)) {
        audit.set_decision(
            "default_route",
            "An in-band \"default\" directive selected normal routing without triage.",
        );
        proxy_stats().record_intercept_pass();
        return InterceptionOutcome::Continue(audit);
    }
    let mut force_local = matches!(verb, Some(RoutingVerb::Local));

    let redirect_request = local_chat_request.clone();

    // Embedding-cached routing. A request near-identical to one already
    // decided routes the same way by construction, without calling the triage
    // model at all. An explicit verb outranks the cache, because the user
    // asking for re-triage is asking for exactly what the cache skips.
    if embeddings_active
        && verb.is_none()
        && let Some(vector) = route_context.vector.clone()
        && let Some(hit) = crate::route_cache::route_cache().lookup(
            &vector,
            &state.config.integrated.embedding,
            |decision| cached_decision_is_live(state, decision),
        )
    {
        proxy_stats().record_route_cache_hit();
        let reason = format!(
            "A cached decision {:.1}% similar to this request was replayed without triage. {}",
            hit.similarity * 100.0,
            hit.reason
        );
        info!(
            request_id,
            protocol = protocol.as_str(),
            decision = hit.decision.as_str(),
            similarity = hit.similarity,
            route = "cached",
            "routing cache hit"
        );
        match hit.decision {
            CachedDecision::Redirect(target) => {
                let target_config = &state.config.integrated.targets[&target];
                audit.set_decision("cached", reason);
                audit.set_target(&target, target_config);
                return execute_integrated_redirect(
                    state,
                    &target,
                    redirect_request,
                    protocol,
                    client_model,
                    client_streaming,
                    conversation_key,
                    &route_context,
                    audit,
                )
                .await;
            }
            CachedDecision::DefaultRoute => {
                if let Some((name, _)) = default_target(&state.config.integrated) {
                    let name = name.to_string();
                    let target_config = &state.config.integrated.targets[&name];
                    audit.set_decision("cached", reason);
                    audit.set_target(&name, target_config);
                    return execute_integrated_redirect(
                        state,
                        &name,
                        redirect_request,
                        protocol,
                        client_model,
                        client_streaming,
                        conversation_key,
                        &route_context,
                        audit,
                    )
                    .await;
                }
                audit.set_decision("cached", reason);
                proxy_stats().record_intercept_pass();
                return InterceptionOutcome::Continue(audit);
            }
            CachedDecision::Local => {
                // The local tier still has to produce the answer; what the
                // cache saved is the decision about whether to route at all.
                audit.set_decision("cached", reason);
                force_local = true;
            }
        }
    }

    // Speculative triage: issue the presumed-default route concurrently with
    // the triage call and cancel the loser. Never with sensitive screening on,
    // because that would put the prompt in front of a provider before it has
    // been screened, and never when a verb already decided the route.
    let speculation = (integrated
        && state.config.integrated.speculative
        && !config.reject_sensitive
        && verb.is_none())
    .then(|| {
        start_speculation(
            state,
            redirect_request.clone(),
            protocol,
            &client_model,
            client_streaming,
        )
    })
    .flatten();

    let local_request = match build_local_request(
        local_chat_request,
        config,
        protocol,
        client_model.as_deref(),
        &state.config.role_map,
        state.config.as_ref(),
        force_local,
    ) {
        Ok(request) => request,
        Err(message) => {
            error!(
                request_id,
                protocol = protocol.as_str(),
                error = %message,
                "local interceptor could not prepare request"
            );
            return fallback_or_error(
                config,
                StatusCode::BAD_GATEWAY,
                &request_id,
                message,
                audit,
                protocol,
                client_model,
                client_streaming,
            );
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
    let meta = if config.reject_sensitive {
        meta.without_body_logging()
    } else {
        meta
    };
    let local_headers = HeaderMap::new();
    let estimated_input_tokens = crate::estimate_tokens(&local_request) as u64;
    audit.audit.triage_provider = Some(provider.name.clone());
    audit.audit.triage_model = Some(config.model.clone());

    if !config.reject_sensitive {
        log_request_value(RouteClass::Local, &provider.name, &local_request);
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
            audit.set_error(error.to_string());
            if config.reject_sensitive {
                return fallback_or_error(
                    config,
                    StatusCode::BAD_GATEWAY,
                    &meta.request_id,
                    error.to_string(),
                    audit,
                    protocol,
                    client_model,
                    client_streaming,
                );
            }
            return if config.fallback_on_error {
                proxy_stats().record_intercept_fallback();
                audit.set_decision(
                    "fallback",
                    "The local triage request failed and fallback_on_error selected normal routing.",
                );
                InterceptionOutcome::Continue(audit)
            } else {
                proxy_stats().record_intercept_error();
                audit.set_decision(
                    "error",
                    "The local triage request failed and fallback_on_error is disabled.",
                );
                audit.record();
                InterceptionOutcome::Respond(upstream_error_response(&error))
            };
        }
        Err(_) => {
            audit.audit.triage_elapsed_ms =
                Some(started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64);
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
                audit,
                protocol,
                client_model,
                client_streaming,
            );
        }
    };
    let local_elapsed = started_at.elapsed();
    let local_usage = TokenUsage::from_response(
        completion.get("usage"),
        estimated_input_tokens,
        crate::estimate_tokens(&completion) as u64,
    );
    audit.set_triage(provider, &config.model, local_elapsed);
    proxy_stats().record_model_usage(
        state.config.project.as_deref(),
        &provider.name,
        &config.model,
        local_elapsed,
        local_usage,
    );
    if !config.reject_sensitive {
        log_response_value(RouteClass::Local, &provider.name, &completion);
    }

    if first_chat_choice_message(&completion).is_none() {
        let message = "local interceptor returned no assistant message".to_string();
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
            protocol,
            client_model,
            client_streaming,
        );
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
        return fallback_or_error(
            config,
            StatusCode::BAD_GATEWAY,
            &meta.request_id,
            message,
            audit,
            protocol,
            client_model,
            client_streaming,
        );
    }

    let no_targets = BTreeMap::new();
    let decision_targets = if integrated {
        &state.config.integrated.targets
    } else {
        &no_targets
    };
    let classification =
        classify_completion_details(&completion, decision_targets, config.reject_sensitive);
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
    // Remember what triage decided so a near-identical request can replay it.
    // A forced or cached turn is not a triage judgement, so it is not stored.
    if embeddings_active
        && verb.is_none()
        && !force_local
        && let Some(vector) = route_context.vector.clone()
        && let Some(cached) = cacheable_decision(&classification.decision)
    {
        crate::route_cache::route_cache().insert(
            vector,
            cached,
            reason.clone(),
            &state.config.integrated.embedding,
        );
    }

    if let Some(speculation) = speculation {
        let wanted = match &classification.decision {
            CompletionDecision::Redirect(target) => target == &speculation.target,
            CompletionDecision::DefaultRoute => default_target(&state.config.integrated)
                .is_some_and(|(name, _)| name == speculation.target),
            _ => false,
        };
        if wanted {
            // The route was already in flight, so triage cost nothing.
            match speculation.handle.await {
                Ok(response) if response.status().is_success() => {
                    proxy_stats().record_speculation_used();
                    proxy_stats().record_intercept_redirect(&speculation.target);
                    if let Some(key) = conversation_key {
                        sticky_routes().insert(
                            key,
                            speculation.target.clone(),
                            state.config.integrated.sticky_ttl_secs,
                            state.config.integrated.sticky_capacity,
                            route_context.vector.clone(),
                        );
                    }
                    let target = &state.config.integrated.targets[&speculation.target];
                    let provider = state
                        .providers
                        .get(&target.provider)
                        .expect("validated integrated provider missing");
                    audit.set_decision(
                        "redirect",
                        format!("{reason} The route was speculated on concurrently with triage."),
                    );
                    audit.set_target(&speculation.target, target);
                    audit.record_destination(provider, &target.model);
                    return InterceptionOutcome::Respond(response);
                }
                // A failed or cancelled speculation falls through to the
                // normal path, which retries with the full failover chain.
                _ => proxy_stats().record_speculation_discarded(),
            }
        } else {
            speculation.handle.abort();
            proxy_stats().record_speculation_discarded();
        }
    }

    // Screening ran first because it is mandatory and fails closed. Anything
    // short of a rejection now defers to the user's explicit directive.
    if config.reject_sensitive
        && !matches!(classification.decision, CompletionDecision::Reject)
        && let Some(name) = forced_route_for(&state.config.integrated, verb.as_ref())
    {
        let target_config = state
            .config
            .integrated
            .targets
            .get(&name)
            .expect("forced verb route missing from config");
        audit.set_decision("verb_route", verb_route_reason(verb.as_ref()));
        audit.set_target(&name, target_config);
        return execute_integrated_redirect(
            state,
            &name,
            redirect_request,
            protocol,
            client_model,
            client_streaming,
            conversation_key,
            &route_context,
            audit,
        )
        .await;
    }

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
            // A configured catch-all target makes ROUTE_DEFAULT a real
            // destination instead of a fall-through to first-upstream-model
            // discovery, which is what makes a project overlay able to own
            // "everything else".
            if let Some((name, target)) = default_target(&state.config.integrated) {
                let (name, target, reason) = match apply_escalation_floor(
                    &state.config.integrated,
                    name,
                    escalation_floor.as_deref(),
                ) {
                    Some(floor) => {
                        let target = state
                            .config
                            .integrated
                            .targets
                            .get(&floor)
                            .expect("escalation floor target missing");
                        let reason = format!(
                            "{reason} Re-triage fell back to the catch-all route, and the escalate policy kept the stronger {floor:?} this session already had."
                        );
                        (floor, target, reason)
                    }
                    None => (
                        name.to_string(),
                        target,
                        format!("{reason} Routed to the configured catch-all target."),
                    ),
                };
                let name = name.as_str();
                audit.set_decision("default_route", reason);
                audit.set_target(name, target);
                info!(
                    request_id = %meta.request_id,
                    protocol = protocol.as_str(),
                    provider = %provider.name,
                    target = name,
                    elapsed_ms = local_elapsed.as_millis(),
                    route = "default_target",
                    "local interceptor selected the catch-all route"
                );
                return execute_integrated_redirect(
                    state,
                    name,
                    redirect_request,
                    protocol,
                    client_model,
                    client_streaming,
                    conversation_key,
                    &route_context,
                    audit,
                )
                .await;
            }
            audit.set_decision(if integrated { "default_route" } else { "pass" }, reason);
            info!(
                request_id = %meta.request_id,
                protocol = protocol.as_str(),
                provider = %provider.name,
                elapsed_ms = local_elapsed.as_millis(),
                route = "external",
                "local interceptor selected route"
            );
            proxy_stats().record_intercept_pass();
            InterceptionOutcome::Continue(audit)
        }
        CompletionDecision::Redirect(target) => {
            let (target, reason) = match apply_escalation_floor(
                &state.config.integrated,
                &target,
                escalation_floor.as_deref(),
            ) {
                Some(floor) => {
                    let reason = format!(
                        "{reason} Re-triage selected a weaker route, and the escalate policy kept the stronger {floor:?} this session already had."
                    );
                    (floor, reason)
                }
                None => (target, reason),
            };
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
                protocol,
                client_model,
                client_streaming,
                conversation_key,
                &route_context,
                audit,
            )
            .await
        }
        CompletionDecision::InvalidControl => {
            audit.set_decision("invalid", reason);
            let message = "local interceptor mixed routing control with other output".to_string();
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
                protocol,
                client_model,
                client_streaming,
            )
        }
        CompletionDecision::Local => {
            audit.set_decision("local", reason);
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

/// One embedding of the newest user turn, plus the retry check that reuses it.
///
/// Both answers come from the same provider round trip wherever possible: the
/// newest turn is embedded once and compared against the previous turn only
/// when retry escalation is switched on.
async fn build_route_context(
    state: &AppState,
    chat_request: &Value,
    config: &crate::route_cache::EmbeddingConfig,
) -> RouteContext {
    let newest = request_prompt_preview(chat_request);
    let Some(vector) = embed_text(state, config, &newest).await else {
        return RouteContext::default();
    };
    let mut context = RouteContext {
        vector: Some(vector.clone()),
        escalate: false,
    };
    if !config.retry_escalation {
        return context;
    }
    // A repeat only counts when nothing happened in between: intervening tool
    // use means the agent made progress, so a similar follow-up is
    // continuation rather than dissatisfaction.
    let Some(window) = crate::route_cache::retry_window(chat_request) else {
        return context;
    };
    let Some(previous) = embed_text(state, config, &window.previous).await else {
        return context;
    };
    let similarity = crate::route_cache::dot(&vector, &previous);
    context.escalate = similarity >= config.retry_threshold;
    context
}

/// Embed one string through the configured embeddings provider. Every failure
/// is a soft failure: routing must never depend on the cache being reachable.
async fn embed_text(
    state: &AppState,
    config: &crate::route_cache::EmbeddingConfig,
    text: &str,
) -> Option<crate::route_cache::Vector> {
    if text.trim().is_empty() {
        return None;
    }
    let provider = state.providers.get(&config.provider)?;
    let headers = HeaderMap::new();
    let meta = UpstreamRequestMeta::new(
        &provider.name,
        &headers,
        Method::POST,
        "embeddings",
        UpstreamResponseKind::Json,
        false,
    );
    let request = json!({ "model": config.model, "input": text });
    let response = tokio::time::timeout(
        Duration::from_millis(config.timeout_ms),
        upstream_json(provider, &headers, &meta, Some(&request)),
    )
    .await;
    match response {
        Ok(Ok(response)) => crate::route_cache::vector_from_response(&response),
        Ok(Err(error)) => {
            info!(
                provider = %provider.name,
                error = %error,
                "embedding request failed; routing continues without the cache"
            );
            None
        }
        Err(_) => {
            info!(
                provider = %provider.name,
                timeout_ms = config.timeout_ms,
                "embedding request timed out; routing continues without the cache"
            );
            None
        }
    }
}

/// Per-request routing extras that several paths need but that would
/// otherwise be threaded through every signature individually.
#[derive(Default, Clone)]
struct RouteContext {
    /// Embedding of the newest user request, when embeddings are active.
    vector: Option<crate::route_cache::Vector>,
    /// The user re-asked the same question a different way, so the chosen
    /// route moves up one rank if a stronger one exists.
    escalate: bool,
}

/// A default-route request already in flight while triage runs.
///
/// Nothing here ever reaches the conversation unless triage agrees with it;
/// otherwise the task is aborted and its buffered output is dropped.
struct Speculation {
    target: String,
    handle: tokio::task::JoinHandle<Response>,
}

fn start_speculation(
    state: &AppState,
    chat_request: Value,
    protocol: InterceptorProtocol,
    client_model: &Option<String>,
    client_streaming: bool,
) -> Option<Speculation> {
    let (name, target) = default_target(&state.config.integrated)?;
    let provider = state.providers.get(&target.provider)?;
    if !crate::health::provider_health()
        .peek(&provider.name)
        .is_available()
    {
        // Speculating into a provider that is already failing spends tokens
        // to produce an error.
        return None;
    }
    let name = name.to_string();
    let state = state.clone();
    let client_model = client_model.clone();
    let speculated = name.clone();
    let handle = tokio::spawn(async move {
        let target = &state.config.integrated.targets[&speculated];
        let provider = state
            .providers
            .get(&target.provider)
            .expect("validated integrated provider missing");
        proxy_integrated_chat(
            provider,
            chat_request,
            target,
            protocol,
            client_model,
            client_streaming,
            state.config.as_ref(),
        )
        .await
    });
    Some(Speculation {
        target: name,
        handle,
    })
}

/// The ordered list of targets to try: the chosen one, then its failover
/// chain, then the shared default chain. Deduplicated and bounded so a
/// mis-configured cycle cannot loop.
fn failover_chain(config: &IntegratedConfig, target_name: &str) -> Vec<String> {
    const MAX_ATTEMPTS: usize = 4;
    let mut chain = vec![target_name.to_string()];
    let configured = config
        .targets
        .get(target_name)
        .map(|target| &target.failover)
        .filter(|failover| !failover.is_empty())
        .unwrap_or(&config.failover);
    for candidate in configured {
        if chain.len() >= MAX_ATTEMPTS {
            break;
        }
        if chain.iter().any(|existing| existing == candidate) {
            continue;
        }
        if config.targets.contains_key(candidate) {
            chain.push(candidate.clone());
        }
    }
    chain
}

/// Send an integrated route, absorbing quota exhaustion and outages by
/// continuing down the failover chain instead of reporting them to the caller.
///
/// Failover is safe here because it only happens on a non-success status,
/// which means the provider rejected the request before generating anything:
/// no work is duplicated and nothing is billed twice.
async fn execute_integrated_redirect(
    state: &AppState,
    target_name: &str,
    mut chat_request: Value,
    protocol: InterceptorProtocol,
    client_model: Option<String>,
    client_streaming: bool,
    conversation_key: Option<u64>,
    context: &RouteContext,
    mut audit: RoutingAuditDraft,
) -> InterceptionOutcome {
    // Retry-driven escalation applies here, at the one point every routing
    // path funnels through, so a re-ask is upgraded however the route was
    // chosen: by triage, by the sticky pin, or by the embedding cache.
    let escalated = context
        .escalate
        .then(|| state.config.integrated.stronger_than(target_name))
        .flatten();
    let target_name = match escalated {
        Some(stronger) => {
            proxy_stats().record_retry_escalation();
            info!(
                from = target_name,
                to = stronger,
                "escalating a re-asked request to a stronger route"
            );
            audit.note_escalation(target_name, &stronger);
            stronger
        }
        None => target_name.to_string(),
    };
    let target_name = target_name.as_str();
    let chain = failover_chain(&state.config.integrated, target_name);
    let mut skipped: Vec<String> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    let mut last: Option<(String, Response)> = None;

    for (attempt, name) in chain.iter().enumerate() {
        let target = state
            .config
            .integrated
            .targets
            .get(name)
            .expect("validated integrated target missing");
        let provider: &ProviderState = state
            .providers
            .get(&target.provider)
            .expect("validated integrated provider missing");
        let more_candidates = attempt + 1 < chain.len();

        let availability = crate::health::provider_health().availability(&provider.name);
        if !availability.is_available() && more_candidates {
            info!(
                target = name,
                provider = provider.name,
                reason = availability.reason(),
                "skipping an unavailable integrated route"
            );
            skipped.push(format!("{name}: {}", availability.reason()));
            continue;
        }

        if attempt > 0 {
            proxy_stats().record_intercept_failover();
            audit.set_target(name, target);
        }
        info!(
            target = name,
            provider = target.provider,
            model = target.model,
            attempt,
            reasoning_effort = target.reasoning_effort.map(crate::ReasoningEffort::as_str),
            protocol = protocol.as_str(),
            "sending integrated route"
        );
        // Only the final attempt can consume the request outright.
        let request = if more_candidates {
            chat_request.clone()
        } else {
            std::mem::take(&mut chat_request)
        };
        let response = proxy_integrated_chat(
            provider,
            request,
            target,
            protocol,
            client_model.clone(),
            client_streaming,
            state.config.as_ref(),
        )
        .await;

        if response.status().is_success() {
            proxy_stats().record_intercept_redirect(name);
            if let Some(key) = conversation_key {
                sticky_routes().insert(
                    key,
                    name.clone(),
                    state.config.integrated.sticky_ttl_secs,
                    state.config.integrated.sticky_capacity,
                    context.vector.clone(),
                );
            }
            if !skipped.is_empty() || !failed.is_empty() {
                audit.note_failover(&skipped, &failed);
            }
            audit.record_destination(provider, &target.model);
            return InterceptionOutcome::Respond(response);
        }

        let status = response.status();
        error!(
            target = name,
            provider = target.provider,
            model = target.model,
            status = %status,
            "integrated redirect failed"
        );
        crate::stats::error_log().record(
            &target.provider,
            "integrated redirect",
            format!("{name} failed with HTTP {status}"),
        );
        failed.push(format!("{name}: HTTP {status}"));
        last = Some((name.clone(), response));
        if !crate::health::status_is_failover_worthy(status.as_u16()) {
            break;
        }
    }

    if let Some(key) = conversation_key {
        sticky_routes().remove(key);
    }
    // The request reached a generation endpoint, so a fall-through to normal
    // routing could duplicate work or spend. Surface the upstream failure.
    proxy_stats().record_intercept_error();
    audit.note_failover(&skipped, &failed);
    let Some((name, response)) = last else {
        // Every candidate was skipped as unavailable, so nothing was sent.
        let message = format!(
            "every integrated route for {target_name:?} is unavailable ({})",
            skipped.join(", ")
        );
        let request_id = audit.audit.request_id.clone();
        audit.set_error(message.clone());
        audit.record();
        return InterceptionOutcome::Respond(interceptor_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &request_id,
            message,
        ));
    };
    let target = state
        .config
        .integrated
        .targets
        .get(&name)
        .expect("failed integrated target missing");
    let provider = state
        .providers
        .get(&target.provider)
        .expect("failed integrated provider missing");
    audit.set_error(format!("HTTP {}", response.status()));
    audit.record_destination(provider, &target.model);
    InterceptionOutcome::Respond(response)
}

fn fallback_or_error(
    config: &InterceptorConfig,
    status: StatusCode,
    request_id: &str,
    message: String,
    mut audit: RoutingAuditDraft,
    protocol: InterceptorProtocol,
    client_model: Option<String>,
    client_streaming: bool,
) -> InterceptionOutcome {
    crate::stats::error_log().record(&config.provider, "interceptor", message.as_str());
    audit.set_error(message.clone());
    if config.reject_sensitive {
        proxy_stats().record_intercept_reject();
        audit.redact_prompt();
        audit.set_decision(
            "reject",
            "Sensitive-data screening failed; fail-closed policy prevented external routing.",
        );
        audit.record();
        return InterceptionOutcome::Respond(render_rejection(
            protocol,
            "sensitive information that could not be screened safely",
            client_model,
            client_streaming,
            request_id,
        ));
    }
    if config.fallback_on_error {
        proxy_stats().record_intercept_fallback();
        if audit.audit.decision == "pending" {
            audit.set_decision(
                "fallback",
                "Local triage could not produce a valid decision and fallback_on_error selected normal routing.",
            );
        }
        InterceptionOutcome::Continue(audit)
    } else {
        proxy_stats().record_intercept_error();
        if audit.audit.decision == "pending" {
            audit.set_decision(
                "error",
                "Local triage could not produce a valid decision and fallback_on_error is disabled.",
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
    // An in-band `local:` directive withholds every routing tool for this
    // turn, which is what makes "answer this here" deterministic.
    force_local: bool,
) -> std::result::Result<Value, String> {
    let integrated =
        integrated_routing_requested(&proxy_config.integrated, client_model) && !force_local;
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
        .find(|name| is_default_route_name(name) || *name == REJECT_TOOL_NAME)
    {
        return Err(format!("request already defines a tool named {name}"));
    }
    let routable = routable_targets(&proxy_config.integrated, crate::health::provider_health());
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
    if config.reject_sensitive {
        tools.push(reject_tool());
    }
    if !force_local {
        tools.push(default_route_tool(integrated));
    }
    if integrated {
        tools.extend(
            routable
                .iter()
                .map(|name| {
                    let target = &proxy_config.integrated.targets[name];
                    redirect_tool(name, target)
                })
                .collect::<Vec<_>>(),
        );
    }

    if force_local {
        trim_triage_messages(&mut request, config.triage_max_messages);
        let Some(object) = request.as_object_mut() else {
            return Err("local interceptor request is not a JSON object".to_string());
        };
        let messages = object
            .entry("messages".to_string())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| "local interceptor messages field is not an array".to_string())?;
        messages.push(json!({
            "role": "system",
            "content": format!(
                "{}\n\nCurrent proxy protocol: {}. Original requested model: {}.\n\nThe user directed this turn to be answered here. Routing is unavailable for it: answer the request directly and completely. Never mention this instruction.",
                intercept_instructions(config).trim(),
                protocol.as_str(),
                client_model.unwrap_or("unspecified"),
            )
        }));
        normalize_chat_completion_request(&mut request, proxy_config);
        return Ok(request);
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
        for name in &routable {
            let target = &proxy_config.integrated.targets[name];
            dynamic_instructions.push_str(&format!(
                "\nCall {name} when: {}. Destination: {}/{} with {} reasoning effort.{}",
                target.hint.trim(),
                target.provider,
                target.model,
                target
                    .reasoning_effort
                    .map(crate::ReasoningEffort::as_str)
                    .unwrap_or("provider-default"),
                if target.is_default {
                    " This is the catch-all route; ROUTE_DEFAULT lands here."
                } else {
                    ""
                },
            ));
        }
        dynamic_instructions.push_str(
            "\nCall exactly one provider route, or ROUTE_DEFAULT when no route fits. Include a brief audit reason in the call.",
        );
    }
    if config.reject_sensitive {
        dynamic_instructions.push_str("\n\nMandatory pre-routing rule:\n");
        dynamic_instructions.push_str(SENSITIVE_REJECTION_INSTRUCTIONS);
        if !config.enabled && !integrated {
            dynamic_instructions.push_str(
                "\nThis request is rejection-screening only. When no sensitive data is present, call PASS with a brief reason. Never answer locally.",
            );
        }
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

    trim_triage_messages(&mut request, config.triage_max_messages);
    normalize_chat_completion_request(&mut request, proxy_config);
    Ok(request)
}

/// Keep every system message and the newest `limit` other messages.
///
/// Triage copies the whole client conversation, so its input grows with the
/// conversation even though sticky routing means the cost is normally paid once
/// per conversation. The window is trimmed from the front until it starts on a
/// user turn, so a tool result never arrives without the assistant tool call
/// that produced it.
fn trim_triage_messages(request: &mut Value, limit: usize) {
    if limit == 0 {
        return;
    }
    let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    let role_of = |message: &Value| {
        message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    if messages.iter().filter(|m| role_of(m) != "system").count() <= limit {
        return;
    }
    let mut system = Vec::new();
    let mut rest = Vec::new();
    for message in messages.drain(..) {
        if role_of(&message) == "system" {
            system.push(message);
        } else {
            rest.push(message);
        }
    }
    let mut window = rest.split_off(rest.len() - limit);
    while window
        .first()
        .is_some_and(|message| role_of(message) != "user")
    {
        window.remove(0);
    }
    system.extend(window);
    *messages = system;
}

fn reject_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": REJECT_TOOL_NAME,
            "description": "Stop before any local answer or provider route because the original request contains sensitive data. Never include the sensitive value.",
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
                "properties": {
                    "reason": {
                        "type": "string",
                        "maxLength": 240,
                        "description": "Briefly name the observable policy condition or route hint that matched. Do not provide chain-of-thought."
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
                        "description": "Briefly name the observable policy condition or route hint that matched. Do not provide chain-of-thought."
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

#[cfg(test)]
fn classify_completion_with_targets(
    completion: &Value,
    targets: &BTreeMap<String, IntegratedTarget>,
) -> CompletionDecision {
    classify_completion_details(completion, targets, false).decision
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
    let fallback_note = if config.reject_sensitive {
        "live traffic would fail closed without contacting an external provider"
    } else if config.fallback_on_error {
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
        false,
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
    let classification = classify_completion_details(&completion, targets, config.reject_sensitive);
    let audit_reason = classification.reason.clone();
    let (decision, target, note) = match classification.decision {
        CompletionDecision::Local => (
            "local",
            None,
            "the local model would answer this request itself".to_string(),
        ),
        CompletionDecision::Reject => (
            "reject",
            None,
            "REJECT — returns a no-action response without contacting an external provider"
                .to_string(),
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
        "reason": audit_reason,
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
use render::{render_local_completion, render_rejection};

#[cfg(test)]
#[path = "interceptor_tests.rs"]
mod tests;
