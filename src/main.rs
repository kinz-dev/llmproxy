use std::{
    collections::{HashMap, HashSet},
    env,
    ffi::OsString,
    fs, io,
    net::SocketAddr,
    path::{Path as FsPath, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path as AxumPath, Request, State},
    http::{
        HeaderMap, HeaderName, HeaderValue, StatusCode,
        header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE},
    },
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use clap::{Parser, ValueEnum};
use futures_util::StreamExt;
use reqwest::{Client, Method, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::{net::TcpListener, sync::Mutex as AsyncMutex, time::sleep};
use tracing::{debug, error, info, trace, warn};

mod admin;
mod agent_launcher;
mod backend_adapters;
mod claude_launcher;
mod codex_launcher;
mod copilot_launcher;
mod interceptor;
mod logging;
mod oauth;
mod stats;

use agent_launcher::{
    AgentLaunch, LauncherKind, LauncherMode, LauncherSession, probe_existing_llmproxy,
};
use backend_adapters::{
    AnthropicStreamTranslator, anthropic_to_chat_response, chat_to_anthropic_request,
    chat_to_codex_responses_request, codex_sse_to_chat_completion, is_claude_model,
    sanitize_copilot_chat_request, sanitize_cross_provider_chat_request,
};
use interceptor::{
    IntegratedConfig, InterceptionOutcome, InterceptionPolicy, InterceptorConfig,
    InterceptorProtocol, maybe_intercept,
};
use stats::{
    ModelPrice, RemapKind, RequestPricing, ResponseTracking, RouteClass, log_request_value,
    proxy_stats, route_class_for_base_url, track_response,
};

#[cfg(unix)]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);
pub(crate) const INTEGRATED_AUTO_MODEL: &str = "Auto";

type ResponseResult<T> = std::result::Result<T, Box<Response>>;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Small OpenAI-compatible v1 proxy for vLLM-style servers",
    group(clap::ArgGroup::new("agent_launcher").args(["claude", "codex", "copilot"]).multiple(false)),
    group(clap::ArgGroup::new("proxy_mode").args(["proxy", "intercept", "integrated", "mode"]).multiple(false))
)]
struct Cli {
    /// Path to a TOML config file. Defaults to LLMPROXY_CONFIG, ./config.toml,
    /// then ~/.llmproxy/config.toml.
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Override the listen address, for example 127.0.0.1:8080.
    #[arg(long)]
    listen: Option<String>,

    /// Override the default provider base URL, normally ending in /v1.
    #[arg(long)]
    default_provider_url: Option<String>,

    /// Enable the configured local-first interceptor for this process.
    #[arg(long)]
    intercept: bool,

    /// Run this instance in proxy mode: default provider only, no routing prompt.
    #[arg(long)]
    proxy: bool,

    /// Enable integrated cross-provider triage for requests using model Auto.
    #[arg(long)]
    integrated: bool,

    /// Set the instance routing mode: proxy, intercept, or integrated.
    #[arg(long, value_enum)]
    mode: Option<RoutingMode>,

    /// Stop the managed llmproxy instance for this listen address and exit.
    #[arg(long)]
    shutdown: bool,

    /// Restart the managed llmproxy instance for this listen address.
    #[arg(long, conflicts_with = "shutdown")]
    restart: bool,

    /// Internal: address of the instance this restart replaces.
    #[arg(long, requires = "restart")]
    restart_from: Option<String>,

    /// Launch Codex through this proxy using the selected routing mode.
    #[arg(long)]
    codex: bool,

    /// Launch Claude Code through this proxy using the selected routing mode.
    #[arg(long)]
    claude: bool,

    /// Launch GitHub Copilot CLI through this proxy using the selected routing mode.
    #[arg(long)]
    copilot: bool,

    /// Codex executable to launch. Defaults to LLMPROXY_CODEX_BIN or codex.
    #[arg(long, requires = "codex")]
    codex_bin: Option<PathBuf>,

    /// Claude executable to launch. Defaults to LLMPROXY_CLAUDE_BIN or claude.
    #[arg(long, requires = "claude")]
    claude_bin: Option<PathBuf>,

    /// Copilot executable to launch. Defaults to LLMPROXY_COPILOT_BIN or copilot.
    #[arg(long, requires = "copilot")]
    copilot_bin: Option<PathBuf>,

    /// Arguments passed to the selected agent after a -- separator.
    #[arg(last = true, requires = "agent_launcher")]
    agent_args: Vec<OsString>,

    /// Internal: keep the proxy in the foreground instead of daemonizing.
    #[arg(long, hide = true)]
    foreground: bool,
}

impl Cli {
    fn has_agent_launcher(&self) -> bool {
        self.codex || self.claude || self.copilot
    }

    /// The instance routing mode the user asked for, via `--mode` or the
    /// equivalent shorthand flags. `None` keeps whatever the config declares.
    fn requested_mode(&self) -> Option<RoutingMode> {
        if let Some(mode) = self.mode {
            return Some(mode);
        }
        if self.proxy {
            Some(RoutingMode::Proxy)
        } else if self.intercept {
            Some(RoutingMode::Intercept)
        } else if self.integrated {
            Some(RoutingMode::Integrated)
        } else {
            None
        }
    }
}

fn validate_cli(cli: &Cli) -> Result<()> {
    if !cli.has_agent_launcher() {
        return Ok(());
    }
    if cli.requested_mode().is_some() {
        bail!(
            "routing mode flags cannot be combined with a launcher; launchers inherit the live instance mode, change it with --restart --mode"
        );
    }
    if cli.default_provider_url.is_some() {
        bail!(
            "--default-provider-url configures the instance default provider; start or restart the instance with it instead of a launcher"
        );
    }
    Ok(())
}

/// The single routing behavior that applies to a whole llmproxy instance.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
#[value(rename_all = "lowercase")]
enum RoutingMode {
    /// Send requests to the default provider without a routing prompt.
    #[default]
    Proxy,
    /// Let the triage provider answer deterministic work or PASS.
    Intercept,
    /// Route model `Auto` among the configured integrated targets.
    Integrated,
}

impl RoutingMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Proxy => "proxy",
            Self::Intercept => "intercept",
            Self::Integrated => "integrated",
        }
    }

    /// The launcher session shape that matches this instance mode.
    fn launcher_mode(self) -> LauncherMode {
        match self {
            Self::Proxy => LauncherMode::Proxy,
            Self::Intercept => LauncherMode::Intercept,
            Self::Integrated => LauncherMode::Integrated,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct Config {
    listen_addr: String,
    max_logging_retention_days: u64,
    default_provider: String,
    upstream: UpstreamConfig,
    providers: HashMap<String, UpstreamConfig>,
    interceptor: InterceptorConfig,
    integrated: IntegratedConfig,
    model_aliases: HashMap<String, ModelAlias>,
    role_map: HashMap<String, String>,
    chat: ChatConfig,
    responses: ResponsesConfig,
    models: ModelsConfig,
    admin: AdminConfig,
    /// Deterministic routes evaluated before any triage request, in order.
    routing_rules: Vec<RoutingRule>,
    pricing: PricingConfig,
    /// The one routing behavior for this instance. Legacy configs without this
    /// key migrate from the per-section `enabled` flags in `parse_config`.
    routing_mode: RoutingMode,
    #[serde(skip)]
    model_routing: ModelRoutingMode,
    #[serde(skip)]
    anthropic_mode: AnthropicMode,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_addr: env::var("LLMPROXY_LISTEN_ADDR")
                .unwrap_or_else(|_| "127.0.0.1:8080".to_string()),
            max_logging_retention_days: 7,
            default_provider: "default".to_string(),
            upstream: UpstreamConfig::default(),
            providers: HashMap::new(),
            interceptor: InterceptorConfig::default(),
            integrated: IntegratedConfig::default(),
            model_aliases: HashMap::new(),
            role_map: HashMap::new(),
            chat: ChatConfig::default(),
            responses: ResponsesConfig::default(),
            models: ModelsConfig::default(),
            admin: AdminConfig::default(),
            routing_rules: Vec::new(),
            pricing: PricingConfig::default(),
            routing_mode: RoutingMode::Proxy,
            model_routing: ModelRoutingMode::Remap,
            anthropic_mode: AnthropicMode::ChatCompletions,
        }
    }
}

impl Config {
    /// Read-only views for the admin API. Config internals stay private so the
    /// admin cannot accidentally become a second source of truth for them.
    pub(crate) fn pricing_baseline_model(&self) -> Option<&str> {
        self.pricing.baseline_model.as_deref()
    }

    pub(crate) fn pricing_models(&self) -> Map<String, Value> {
        self.pricing
            .models
            .iter()
            .map(|(model, price)| {
                (
                    model.clone(),
                    json!({
                        "input_per_mtok": price.input_per_mtok,
                        "output_per_mtok": price.output_per_mtok,
                    }),
                )
            })
            .collect()
    }

    /// `routing_mode` is authoritative: the per-section `enabled` flags are
    /// derived from it so every downstream check stays consistent with it.
    fn set_routing_mode(&mut self, mode: RoutingMode) {
        self.routing_mode = mode;
        self.interceptor.enabled = mode == RoutingMode::Intercept;
        self.integrated.enabled = mode == RoutingMode::Integrated;
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum ModelRoutingMode {
    #[default]
    Remap,
    Passthrough,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum AnthropicMode {
    #[default]
    ChatCompletions,
    Passthrough,
}

/// A deterministic route decided without asking the triage model. Every
/// condition that is present must hold; an empty rule matches everything.
/// Rules exist because triage costs a full extra model round-trip on requests
/// whose destination was never actually in question.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
struct RoutingRule {
    /// Shown in the decision audit so a route can be traced to its rule.
    name: String,
    min_input_tokens: Option<u64>,
    max_input_tokens: Option<u64>,
    has_tools: Option<bool>,
    /// Case-insensitive substrings; any one matching satisfies the condition.
    contains_any: Vec<String>,
    requested_model: Option<String>,
    /// `PASS` for normal routing, an integrated target name, or
    /// `provider/model`.
    route: String,
}

impl Default for RoutingRule {
    fn default() -> Self {
        Self {
            name: String::new(),
            min_input_tokens: None,
            max_input_tokens: None,
            has_tools: None,
            contains_any: Vec::new(),
            requested_model: None,
            route: ROUTING_RULE_PASS.to_string(),
        }
    }
}

pub(crate) const ROUTING_RULE_PASS: &str = "PASS";

impl RoutingRule {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn route(&self) -> &str {
        &self.route
    }

    pub(crate) fn min_input_tokens(&self) -> Option<u64> {
        self.min_input_tokens
    }

    pub(crate) fn max_input_tokens(&self) -> Option<u64> {
        self.max_input_tokens
    }

    pub(crate) fn has_tools(&self) -> Option<bool> {
        self.has_tools
    }

    pub(crate) fn contains_any(&self) -> &[String] {
        &self.contains_any
    }

    pub(crate) fn requested_model(&self) -> Option<&str> {
        self.requested_model.as_deref()
    }

    /// Every condition the rule declares must hold. Conditions left unset are
    /// not constraints, so a rule with only a `route` matches everything —
    /// which is what makes a final catch-all rule expressible.
    pub(crate) fn matches(
        &self,
        estimated_tokens: u64,
        has_tools: bool,
        lowercase_text: &str,
        client_model: Option<&str>,
    ) -> bool {
        if self
            .min_input_tokens
            .is_some_and(|min| estimated_tokens < min)
        {
            return false;
        }
        if self
            .max_input_tokens
            .is_some_and(|max| estimated_tokens > max)
        {
            return false;
        }
        if self.has_tools.is_some_and(|expected| expected != has_tools) {
            return false;
        }
        if let Some(expected) = self.requested_model.as_deref()
            && client_model != Some(expected)
        {
            return false;
        }
        if !self.contains_any.is_empty()
            && !self
                .contains_any
                .iter()
                .any(|needle| lowercase_text.contains(&needle.to_lowercase()))
        {
            return false;
        }
        true
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct PricingConfig {
    /// Model whose rates answer "what would this have cost otherwise".
    baseline_model: Option<String>,
    /// Rates keyed by `model` or `provider/model`; the namespaced key wins.
    models: HashMap<String, ModelPriceConfig>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
struct ModelPriceConfig {
    input_per_mtok: f64,
    output_per_mtok: f64,
}

impl From<ModelPriceConfig> for ModelPrice {
    fn from(value: ModelPriceConfig) -> Self {
        Self {
            input_per_mtok: value.input_per_mtok,
            output_per_mtok: value.output_per_mtok,
        }
    }
}

impl PricingConfig {
    /// A namespaced rate is more specific than a bare model name, so it wins.
    /// The same model served by two providers rarely costs the same.
    fn price_for(&self, provider: &str, model: &str) -> ModelPrice {
        self.models
            .get(&format!("{provider}/{model}"))
            .or_else(|| self.models.get(model))
            .copied()
            .map(ModelPrice::from)
            .unwrap_or_default()
    }

    fn baseline(&self) -> ModelPrice {
        self.baseline_model
            .as_deref()
            .and_then(|model| {
                self.models.get(model).or_else(|| {
                    model
                        .split_once('/')
                        .and_then(|(_, bare)| self.models.get(bare))
                })
            })
            .copied()
            .map(ModelPrice::from)
            .unwrap_or_default()
    }
}

/// What one completed request should be billed at, and what the same tokens
/// would have cost on the baseline model.
fn request_pricing(config: &Config, provider: &str, model: &str) -> RequestPricing {
    RequestPricing {
        actual: config.pricing.price_for(provider, model),
        baseline: config.pricing.baseline(),
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
enum ModelAlias {
    /// Backward-compatible alias targeting a model on the default provider.
    Model(String),
    /// Alias targeting a specific provider and model.
    Route { provider: String, model: String },
}

impl ModelAlias {
    fn provider<'a>(&'a self, default_provider: &'a str) -> &'a str {
        match self {
            Self::Model(_) => default_provider,
            Self::Route { provider, .. } => provider,
        }
    }

    fn model(&self) -> &str {
        match self {
            Self::Model(model) | Self::Route { model, .. } => model,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
struct UpstreamConfig {
    kind: ProviderKind,
    base_url: String,
    api_key: Option<String>,
    api_key_env: Option<String>,
    /// Borrowed subscription OAuth: attach the local CLI's login token instead
    /// of a static API key. `none` (default) keeps API-key/passthrough auth.
    auth: ProviderAuth,
    headers: HashMap<String, String>,
    reasoning_effort: Option<ReasoningEffort>,
    timeout_secs: u64,
    connect_timeout_secs: u64,
    pool_max_idle_per_host: usize,
    pool_idle_timeout_secs: u64,
    /// Providers to try when this one is unavailable, in order. Each entry is
    /// a provider name, or `provider/model` to also switch model.
    fallback: Vec<String>,
    /// Consecutive availability failures that open the breaker. 0 disables it.
    circuit_breaker_threshold: u32,
    /// How long an open breaker refuses traffic before one probe is allowed.
    circuit_breaker_cooldown_secs: u64,
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            kind: ProviderKind::OpenAiCompatible,
            base_url: env::var("OPENAI_BASE_URL")
                .or_else(|_| env::var("VLLM_BASE_URL"))
                .unwrap_or_else(|_| "http://127.0.0.1:8000/v1".to_string()),
            api_key: None,
            api_key_env: None,
            auth: ProviderAuth::None,
            headers: HashMap::new(),
            reasoning_effort: None,
            timeout_secs: 600,
            connect_timeout_secs: 10,
            pool_max_idle_per_host: 64,
            pool_idle_timeout_secs: 90,
            fallback: Vec::new(),
            circuit_breaker_threshold: 5,
            circuit_breaker_cooldown_secs: 30,
        }
    }
}

/// How a provider authenticates outbound requests.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum ProviderAuth {
    /// Static API key or caller-forwarded credentials (the existing behavior).
    #[default]
    None,
    /// Borrow `~/.codex/auth.json` and talk to the ChatGPT Codex backend.
    CodexOauth,
    /// Borrow `~/.claude/.credentials.json` and talk to Claude as Claude Code.
    ClaudeOauth,
}

impl ProviderAuth {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::CodexOauth => "codex-oauth",
            Self::ClaudeOauth => "claude-oauth",
        }
    }

    fn oauth_provider(self) -> Option<oauth::OAuthProvider> {
        match self {
            Self::None => None,
            Self::CodexOauth => Some(oauth::OAuthProvider::Codex),
            Self::ClaudeOauth => Some(oauth::OAuthProvider::Claude),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum ProviderKind {
    #[default]
    #[serde(rename = "openai-compatible", alias = "open-ai-compatible")]
    OpenAiCompatible,
    #[serde(rename = "openai", alias = "open-ai")]
    OpenAi,
    Anthropic,
    Copilot,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ReasoningEffort {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

impl std::str::FromStr for ReasoningEffort {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" => Ok(Self::None),
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            "max" => Ok(Self::Max),
            _ => Err(format!(
                "reasoning_effort must be one of none, minimal, low, medium, high, xhigh, or max; got {value:?}"
            )),
        }
    }
}

impl ProviderKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "openai-compatible",
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Copilot => "copilot",
        }
    }

    fn can_inherit_openai_api_key(self) -> bool {
        matches!(self, Self::OpenAiCompatible | Self::OpenAi)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct ChatConfig {
    disable_thinking: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct ResponsesConfig {
    mode: ResponsesMode,
}

impl Default for ResponsesConfig {
    fn default() -> Self {
        Self {
            mode: ResponsesMode::ChatCompletions,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ResponsesMode {
    /// Translate /v1/responses requests to upstream /v1/chat/completions.
    #[default]
    ChatCompletions,
    /// Forward /v1/responses through to the same upstream path.
    Passthrough,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct AdminConfig {
    /// Enable the admin web interface. Off unless the config opts in.
    #[serde(alias = "enabled")]
    enable: bool,
    /// Loopback address to serve the admin interface on. Missing means
    /// 127.0.0.1 with a randomly assigned port.
    listen_address: Option<String>,
    /// Override the directory the admin UI files are served from.
    ui_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct ModelsConfig {
    expose_aliases: bool,
    expose_upstream: bool,
    cache_ttl_secs: Option<u64>,
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            expose_aliases: true,
            expose_upstream: true,
            cache_ttl_secs: None,
        }
    }
}

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    providers: Arc<HashMap<String, ProviderState>>,
    launcher_kind: Option<LauncherKind>,
    launcher_mode: Option<LauncherMode>,
    forward_caller_credentials: bool,
}

const LAUNCHER_EXTERNAL_PROVIDER: &str = "__launcher_external";

/// Swappable handle to the current [`AppState`]. Handlers snapshot it once at
/// entry so a concurrent runtime-config swap never changes routing mid-request.
#[derive(Clone)]
struct SharedState {
    inner: Arc<RwLock<AppState>>,
}

impl SharedState {
    fn new(state: AppState) -> Self {
        Self {
            inner: Arc::new(RwLock::new(state)),
        }
    }

    fn snapshot(&self) -> AppState {
        self.inner.read().expect("app state lock poisoned").clone()
    }

    fn replace(&self, state: AppState) {
        *self.inner.write().expect("app state lock poisoned") = state;
    }
}

#[derive(Clone)]
struct ProviderState {
    name: String,
    config: Arc<UpstreamConfig>,
    client: Client,
    model_cache: Arc<ModelCache>,
    model_refresh: Arc<AsyncMutex<()>>,
    breaker: Arc<CircuitBreaker>,
}

/// Per-provider availability gate. A provider that is refusing traffic should
/// be skipped outright rather than made to time out on every single request,
/// and it needs a way back once it recovers, so one probe is admitted after
/// the cooldown instead of reopening the floodgates.
#[derive(Debug)]
struct CircuitBreaker {
    threshold: u32,
    cooldown: Duration,
    state: Mutex<BreakerState>,
}

#[derive(Debug, Default)]
struct BreakerState {
    consecutive_failures: u32,
    opened_at: Option<Instant>,
    /// A half-open probe is already in flight; everyone else keeps skipping.
    probing: bool,
}

impl CircuitBreaker {
    fn new(threshold: u32, cooldown: Duration) -> Self {
        Self {
            threshold,
            cooldown,
            state: Mutex::new(BreakerState::default()),
        }
    }

    fn disabled(&self) -> bool {
        self.threshold == 0
    }

    /// Whether to send. Claims the single half-open probe as a side effect, so
    /// a recovering provider gets one request rather than the whole backlog.
    fn allow(&self) -> bool {
        if self.disabled() {
            return true;
        }
        let mut state = self.state.lock().expect("circuit breaker poisoned");
        let Some(opened_at) = state.opened_at else {
            return true;
        };
        if opened_at.elapsed() < self.cooldown || state.probing {
            return false;
        }
        state.probing = true;
        true
    }

    fn record_success(&self) {
        let mut state = self.state.lock().expect("circuit breaker poisoned");
        *state = BreakerState::default();
    }

    fn record_failure(&self) {
        if self.disabled() {
            return;
        }
        let mut state = self.state.lock().expect("circuit breaker poisoned");
        state.probing = false;
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        if state.consecutive_failures >= self.threshold {
            // A failed probe restarts the cooldown rather than retrying at once.
            state.opened_at = Some(Instant::now());
        }
    }

    fn status(&self) -> &'static str {
        if self.disabled() {
            return "disabled";
        }
        let state = self.state.lock().expect("circuit breaker poisoned");
        match state.opened_at {
            None => "closed",
            Some(opened_at) if opened_at.elapsed() >= self.cooldown => "half_open",
            Some(_) => "open",
        }
    }

    fn consecutive_failures(&self) -> u32 {
        self.state
            .lock()
            .expect("circuit breaker poisoned")
            .consecutive_failures
    }
}

#[derive(Default)]
struct ModelCache {
    upstream: Mutex<UpstreamModels>,
    seen_client_models: Mutex<HashSet<String>>,
}

struct PidFileGuard {
    path: PathBuf,
    pid: u32,
}

struct RuntimeInfoGuard {
    path: PathBuf,
    pid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RuntimeInfo {
    pid: u32,
    proxy_url: String,
    admin_url: Option<String>,
    /// Legacy flag kept so older runtime files still decode.
    #[serde(default)]
    integrated: bool,
    #[serde(default)]
    routing_mode: Option<RoutingMode>,
    #[serde(default)]
    log_file: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct UpstreamModels {
    ids: Vec<String>,
    response: Option<Value>,
    fetched_at: Option<Instant>,
}

#[derive(Debug, Clone, Copy)]
enum ModelMappingSource {
    Alias,
    Namespace,
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpstreamResponseKind {
    Json,
    Stream,
    Relay,
}

impl UpstreamResponseKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Stream => "stream",
            Self::Relay => "relay",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpstreamErrorKind {
    Connect,
    Timeout,
    Request,
    Body,
    Decode,
    Status,
    Unknown,
}

impl UpstreamErrorKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Timeout => "timeout",
            Self::Request => "request",
            Self::Body => "body",
            Self::Decode => "decode",
            Self::Status => "status",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone)]
struct UpstreamRequestMeta {
    provider: String,
    request_id: String,
    method: Method,
    path: String,
    response_kind: UpstreamResponseKind,
    idempotent: bool,
    log_bodies: bool,
}

impl UpstreamRequestMeta {
    fn new(
        provider: &str,
        incoming_headers: &HeaderMap,
        method: Method,
        path: &str,
        response_kind: UpstreamResponseKind,
        idempotent: bool,
    ) -> Self {
        Self {
            provider: provider.to_string(),
            request_id: request_id_from_headers(incoming_headers),
            method,
            path: path.to_string(),
            response_kind,
            idempotent,
            log_bodies: true,
        }
    }

    fn without_body_logging(mut self) -> Self {
        self.log_bodies = false;
        self
    }
}

#[derive(Debug, Clone)]
struct UpstreamFailure {
    provider: String,
    request_id: String,
    method: Method,
    path: String,
    response_kind: UpstreamResponseKind,
    kind: UpstreamErrorKind,
    message: String,
    upstream_status: Option<StatusCode>,
}

impl UpstreamFailure {
    fn from_reqwest(meta: &UpstreamRequestMeta, error: reqwest::Error) -> Self {
        Self {
            provider: meta.provider.clone(),
            request_id: meta.request_id.clone(),
            method: meta.method.clone(),
            path: meta.path.clone(),
            response_kind: meta.response_kind,
            kind: classify_reqwest_error(&error),
            message: reqwest_error_message(&error),
            upstream_status: error.status(),
        }
    }

    fn timeout(meta: &UpstreamRequestMeta, error: impl Into<String>) -> Self {
        Self {
            provider: meta.provider.clone(),
            request_id: meta.request_id.clone(),
            method: meta.method.clone(),
            path: meta.path.clone(),
            response_kind: meta.response_kind,
            kind: UpstreamErrorKind::Timeout,
            message: error.into(),
            upstream_status: None,
        }
    }

    /// A borrowed-OAuth token could not be resolved (missing file, refresh
    /// failure). Treated as a request-setup error, never retried.
    fn oauth(meta: &UpstreamRequestMeta, error: impl Into<String>) -> Self {
        Self {
            provider: meta.provider.clone(),
            request_id: meta.request_id.clone(),
            method: meta.method.clone(),
            path: meta.path.clone(),
            response_kind: meta.response_kind,
            kind: UpstreamErrorKind::Request,
            message: format!("borrowed OAuth login unavailable: {}", error.into()),
            upstream_status: None,
        }
    }

    fn status_with_body(meta: &UpstreamRequestMeta, status: StatusCode, bytes: &[u8]) -> Self {
        let detail = if meta.log_bodies {
            upstream_error_detail(bytes).unwrap_or_else(|| body_prefix(bytes))
        } else {
            String::new()
        };
        let message = if detail.is_empty() {
            format!("upstream /{} returned {status}", meta.path)
        } else {
            format!(
                "upstream /{} returned {status}: {}",
                meta.path,
                truncate_detail(&detail)
            )
        };

        Self {
            provider: meta.provider.clone(),
            request_id: meta.request_id.clone(),
            method: meta.method.clone(),
            path: meta.path.clone(),
            response_kind: meta.response_kind,
            kind: UpstreamErrorKind::Status,
            message,
            upstream_status: Some(status),
        }
    }

    fn decode(meta: &UpstreamRequestMeta, status: StatusCode, error: impl Into<String>) -> Self {
        let error = error.into();
        Self {
            provider: meta.provider.clone(),
            request_id: meta.request_id.clone(),
            method: meta.method.clone(),
            path: meta.path.clone(),
            response_kind: meta.response_kind,
            kind: UpstreamErrorKind::Decode,
            message: format!("failed to decode upstream JSON response: {error}"),
            upstream_status: Some(status),
        }
    }

    /// Whether this failure proves the request produced no output, so sending
    /// it to another provider cannot duplicate generation that already ran.
    /// Body and decode failures are deliberately excluded: those mean the
    /// upstream did generate, and a second call would bill for it twice.
    fn is_failover_safe(&self) -> bool {
        matches!(
            self.kind,
            UpstreamErrorKind::Connect | UpstreamErrorKind::Timeout | UpstreamErrorKind::Request
        ) || self.upstream_status.is_some_and(is_failover_status)
    }

    fn is_retryable(&self) -> bool {
        matches!(
            self.kind,
            UpstreamErrorKind::Connect
                | UpstreamErrorKind::Timeout
                | UpstreamErrorKind::Request
                | UpstreamErrorKind::Body
                | UpstreamErrorKind::Decode
        ) || matches!(
            self.upstream_status,
            Some(StatusCode::REQUEST_TIMEOUT)
                | Some(StatusCode::TOO_MANY_REQUESTS)
                | Some(StatusCode::BAD_GATEWAY)
                | Some(StatusCode::SERVICE_UNAVAILABLE)
                | Some(StatusCode::GATEWAY_TIMEOUT)
        )
    }
}

impl std::fmt::Display for UpstreamFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "provider {}: {} {} failed with {}: {}",
            self.provider,
            self.method,
            self.path,
            self.kind.as_str(),
            self.message
        )
    }
}

impl std::error::Error for UpstreamFailure {}

impl ModelMappingSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Alias => "alias",
            Self::Namespace => "namespace",
            Self::Auto => "auto",
        }
    }
}

impl ModelCache {
    fn replace_upstream_models(&self, ids: Vec<String>, response: Value) -> bool {
        let mut upstream = self
            .upstream
            .lock()
            .expect("upstream model cache mutex poisoned");
        let changed = upstream.ids != ids;
        upstream.ids = ids;
        upstream.response = Some(response);
        upstream.fetched_at = Some(Instant::now());
        changed
    }

    fn first_upstream_model(&self) -> Option<String> {
        let upstream = self
            .upstream
            .lock()
            .expect("upstream model cache mutex poisoned");
        upstream.ids.first().cloned()
    }

    /// Cached upstream model ids and whether the cache was ever filled.
    fn cached_ids(&self) -> (Vec<String>, bool) {
        let upstream = self
            .upstream
            .lock()
            .expect("upstream model cache mutex poisoned");
        (upstream.ids.clone(), upstream.fetched_at.is_some())
    }

    fn cached_upstream_models_response(&self, ttl: Option<Duration>) -> Option<Value> {
        let upstream = self
            .upstream
            .lock()
            .expect("upstream model cache mutex poisoned");
        if let Some(ttl) = ttl {
            if ttl.is_zero() {
                return None;
            }
            let fetched_at = upstream.fetched_at?;
            if fetched_at.elapsed() > ttl {
                return None;
            }
        }
        upstream.response.clone()
    }

    fn log_first_seen_model(
        &self,
        provider: &str,
        requested: &str,
        target: &str,
        source: ModelMappingSource,
    ) {
        let mut seen = self
            .seen_client_models
            .lock()
            .expect("seen model mutex poisoned");
        if !seen.insert(requested.to_string()) {
            return;
        }

        info!(
            provider,
            client_model = %requested,
            upstream_model = %target,
            mapping = source.as_str(),
            "first seen model"
        );
    }
}

fn request_id_from_headers(headers: &HeaderMap) -> String {
    headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(generate_request_id)
}

fn generate_request_id() -> String {
    let sequence = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("llmproxy-{}-{sequence}", unix_timestamp())
}

fn classify_reqwest_error(error: &reqwest::Error) -> UpstreamErrorKind {
    if error.is_timeout() {
        UpstreamErrorKind::Timeout
    } else if error.is_connect() {
        UpstreamErrorKind::Connect
    } else if error.is_body() {
        UpstreamErrorKind::Body
    } else if error.is_decode() {
        UpstreamErrorKind::Decode
    } else if error.is_request() {
        UpstreamErrorKind::Request
    } else {
        UpstreamErrorKind::Unknown
    }
}

fn reqwest_error_message(error: &reqwest::Error) -> String {
    let mut message = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(error) = source {
        let source_message = error.to_string();
        if !source_message.is_empty() && !message.contains(&source_message) {
            message.push_str(": ");
            message.push_str(&source_message);
        }
        source = error.source();
    }
    message
}

fn body_prefix(bytes: &[u8]) -> String {
    const MAX_PREFIX_BYTES: usize = 512;
    let prefix_len = bytes.len().min(MAX_PREFIX_BYTES);
    String::from_utf8_lossy(&bytes[..prefix_len])
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn upstream_error_detail(bytes: &[u8]) -> Option<String> {
    let value = serde_json::from_slice::<Value>(bytes).ok()?;
    value
        .get("error")
        .and_then(|error| {
            error
                .get("message")
                .or_else(|| error.get("detail"))
                .or_else(|| error.get("type"))
        })
        .or_else(|| value.get("message"))
        .or_else(|| value.get("detail"))
        .map(text_from_value)
        .filter(|detail| !detail.is_empty())
}

fn truncate_detail(detail: &str) -> String {
    const MAX_DETAIL_CHARS: usize = 1200;
    let mut chars = detail.chars();
    let truncated = chars.by_ref().take(MAX_DETAIL_CHARS).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

const SIMD_JSON_MIN_BYTES: usize = 4096;

fn parse_upstream_json(bytes: &[u8]) -> std::result::Result<Value, String> {
    if bytes.len() < SIMD_JSON_MIN_BYTES {
        return serde_json::from_slice::<Value>(bytes).map_err(|error| error.to_string());
    }

    let mut body = bytes.to_vec();
    simd_json::serde::from_slice::<Value>(&mut body).map_err(|error| error.to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    validate_cli(&cli)?;
    if cli.foreground && cli.has_agent_launcher() {
        bail!("--foreground cannot be combined with --codex, --claude, or --copilot");
    }

    let (config, config_path) = load_config(&cli)?;
    let logging_guard = logging::init(
        cli.foreground,
        agent_launcher::home_dir().as_deref(),
        config.max_logging_retention_days,
    )?;
    if let Some(path) = config_path.as_deref() {
        report_unknown_config_keys(path);
    }
    log_loaded_config(&config, config_path.as_deref());
    proxy_stats().touch_start();
    let addr: SocketAddr = config
        .listen_addr
        .parse()
        .with_context(|| format!("invalid listen address: {}", config.listen_addr))?;

    if cli.shutdown {
        shutdown_daemon(addr).await?;
        return Ok(());
    }

    if cli.restart {
        let previous = match &cli.restart_from {
            Some(address) => address
                .parse::<SocketAddr>()
                .with_context(|| format!("invalid --restart-from address: {address:?}"))?,
            None => addr,
        };
        let _ = shutdown_daemon(previous).await?;
    }

    if cli.foreground {
        run_server(
            config,
            config_path,
            None,
            logging_guard.file_name().map(ToOwned::to_owned),
        )
        .await?;
        return Ok(());
    }

    if cli.has_agent_launcher() {
        // Launchers inherit the live instance mode, so the daemon has to be up
        // and publishing that mode before the session shape is decided.
        ensure_daemon_running(&cli, config_path.as_deref(), false, addr).await?;
        let mode = live_routing_mode(addr, &config)?.launcher_mode();
        validate_launcher_runtime(&config, mode)?;
        if let Some(agent_launch) = AgentLaunch::discover(&cli, mode).await? {
            run_agent(agent_launch, addr).await?;
        }
        return Ok(());
    }

    ensure_daemon_running(&cli, config_path.as_deref(), true, addr).await?;
    Ok(())
}

async fn run_server(
    config: Config,
    config_path: Option<PathBuf>,
    launcher_name: Option<&'static str>,
    log_file: Option<String>,
) -> Result<()> {
    let config = Arc::new(config);
    let mut providers = HashMap::new();
    for (name, upstream) in effective_provider_configs(&config)? {
        providers.insert(name.clone(), build_provider_state(name, upstream)?);
    }
    let state = AppState {
        config: config.clone(),
        providers: Arc::new(providers),
        launcher_kind: None,
        launcher_mode: None,
        forward_caller_credentials: config.routing_mode != RoutingMode::Integrated,
    };
    let shared = SharedState::new(state.clone());

    if config.model_routing == ModelRoutingMode::Remap {
        for provider in state.providers.values().cloned() {
            tokio::spawn(async move {
                let _refresh_guard = provider.model_refresh.lock().await;
                if let Err(error) = refresh_upstream_models(&provider, &HeaderMap::new()).await {
                    error!(
                        error = %error,
                        provider = %provider.name,
                        upstream = %provider.config.base_url,
                        "failed to load upstream models for auto mapping; will retry on request"
                    );
                }
            });
        }
    }

    let app = build_app(shared.clone());
    let addr: SocketAddr = config
        .listen_addr
        .parse()
        .with_context(|| format!("invalid listen address: {}", config.listen_addr))?;
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {}", addr))?;
    let bound_addr = listener
        .local_addr()
        .context("failed to read proxy listen address")?;
    let _pid_file = PidFileGuard::create(bound_addr)?;

    info!(
        listen_addr = %bound_addr,
        default_provider = %config.default_provider,
        providers = state.providers.len(),
        responses_mode = ?config.responses.mode,
        "llmproxy listening"
    );

    let admin_addr = if config.admin.enable {
        stats::traffic_log().set_enabled(true);
        stats::error_log().set_enabled(true);
        stats::routing_decision_log().set_enabled(true);
        let admin_context = Arc::new(admin::AdminContext::new(
            config_path,
            launcher_name,
            &config.admin,
        ));
        admin_context.set_proxy_addr(bound_addr);
        Some(admin::spawn(shared.clone(), admin_context, &config.admin).await?)
    } else {
        None
    };
    let runtime_info = RuntimeInfo::new(bound_addr, admin_addr, config.routing_mode, log_file);
    let _runtime_info_file = RuntimeInfoGuard::create(bound_addr, &runtime_info)?;
    println!("{}", startup_banner(&runtime_info, "running"));

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server failed")?;
    Ok(())
}

fn build_app(shared: SharedState) -> Router {
    let api = Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/v1/models/{model}", get(model_by_id))
        .route("/v1/model", get(models))
        .route("/v1/completions", post(completions))
        .route("/v1/completion", post(completions))
        .route("/v1/embeddings", post(embeddings))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/responses", post(responses))
        .route("/v1/response", post(responses))
        .route("/v1/responses/compact", post(codex_responses_compact))
        .route(
            "/v1/memories/trace_summarize",
            post(codex_memories_summarize),
        )
        .route("/v1/alpha/search", post(codex_search))
        .route("/v1/messages", post(anthropic_messages))
        .route("/v1/messages/count_tokens", post(anthropic_count_tokens))
        .with_state(shared.clone());

    let launcher_api = Router::new()
        .route("/models", get(launcher_copilot_models))
        .route("/models/{model}", get(launcher_copilot_model_by_id))
        .route(
            "/models/{model}/policy",
            post(launcher_copilot_model_policy),
        )
        .route("/chat/completions", post(launcher_chat_completions))
        .route("/responses", post(launcher_responses))
        .route("/v1/models", get(launcher_models))
        .route("/v1/models/{model}", get(launcher_model_by_id))
        .route("/v1/model", get(launcher_models))
        .route("/v1/completions", post(launcher_completions))
        .route("/v1/completion", post(launcher_completions))
        .route("/v1/embeddings", post(launcher_embeddings))
        .route("/v1/chat/completions", post(launcher_chat_completions))
        .route("/v1/responses", post(launcher_responses))
        .route("/v1/response", post(launcher_responses))
        .route(
            "/v1/responses/compact",
            post(launcher_codex_responses_compact),
        )
        .route(
            "/v1/memories/trace_summarize",
            post(launcher_codex_memories_summarize),
        )
        .route("/v1/alpha/search", post(launcher_codex_search))
        .route("/v1/messages", post(launcher_anthropic_messages))
        .route(
            "/v1/messages/count_tokens",
            post(launcher_anthropic_count_tokens),
        )
        .with_state(shared);

    Router::new()
        .merge(api)
        .nest("/_launcher/{launcher_token}", launcher_api)
        .fallback(route_not_found)
        .layer(middleware::from_fn(trace_http_request))
}

async fn route_not_found(request: Request) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error": {
                "message": format!("no llmproxy endpoint matches {method} {path}"),
                "type": "route_not_found"
            }
        })),
    )
        .into_response()
}

async fn trace_http_request(mut request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let request_id = request_id_from_headers(request.headers());
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        request
            .headers_mut()
            .insert(HeaderName::from_static("x-request-id"), value);
    }
    let started = Instant::now();
    trace!(request_id, method = %method, path, "client request started");
    let mut response = next.run(request).await;
    if !response.headers().contains_key("x-request-id")
        && let Ok(value) = HeaderValue::from_str(&request_id)
    {
        response
            .headers_mut()
            .insert(HeaderName::from_static("x-request-id"), value);
    }
    let status = response.status();
    if status.is_success() {
        trace!(
            method = %method,
            request_id,
            path,
            status = status.as_u16(),
            elapsed_ms = started.elapsed().as_millis(),
            "client response completed"
        );
    } else {
        error!(
            method = %method,
            request_id,
            path,
            status = status.as_u16(),
            elapsed_ms = started.elapsed().as_millis(),
            "client request returned an error response"
        );
    }
    if status.is_success() {
        return response;
    }

    let (parts, body) = response.into_parts();
    let mut body_stream = body.into_data_stream();
    let error_body = async_stream::stream! {
        let mut chunk_index = 0_u64;
        while let Some(chunk) = body_stream.next().await {
            match &chunk {
                Ok(bytes) => {
                    chunk_index = chunk_index.saturating_add(1);
                    error!(
                        method = %method,
                        request_id,
                        path,
                        status = status.as_u16(),
                        chunk_index,
                        body = %String::from_utf8_lossy(bytes),
                        "client error response body"
                    );
                }
                Err(body_error) => error!(
                    method = %method,
                    request_id,
                    path,
                    status = status.as_u16(),
                    error = %body_error,
                    "client error response stream failed"
                ),
            }
            yield chunk;
        }
    };
    Response::from_parts(parts, Body::from_stream(error_body))
}

fn build_provider_state(name: String, upstream: UpstreamConfig) -> Result<ProviderState> {
    let client = Client::builder()
        // Do not set a client-wide timeout here. Reqwest applies that timeout
        // to the entire response body, which cuts off long-running SSE streams.
        // Non-streaming requests get the configured timeout per request below.
        .connect_timeout(Duration::from_secs(upstream.connect_timeout_secs))
        .pool_max_idle_per_host(upstream.pool_max_idle_per_host)
        .pool_idle_timeout(Duration::from_secs(upstream.pool_idle_timeout_secs))
        .tcp_nodelay(true)
        .build()
        .with_context(|| format!("failed to build HTTP client for provider {name}"))?;
    let breaker = Arc::new(CircuitBreaker::new(
        upstream.circuit_breaker_threshold,
        Duration::from_secs(upstream.circuit_breaker_cooldown_secs),
    ));
    Ok(ProviderState {
        name,
        config: Arc::new(upstream),
        client,
        model_cache: Arc::new(ModelCache::default()),
        model_refresh: Arc::new(AsyncMutex::new(())),
        breaker,
    })
}

/// The routing mode the running instance published. A daemon started by an
/// older build may not declare one, and then the local config is the best
/// remaining answer.
fn live_routing_mode(addr: SocketAddr, config: &Config) -> Result<RoutingMode> {
    Ok(read_runtime_info(addr)?
        .and_then(|info| info.declared_routing_mode())
        .unwrap_or(config.routing_mode))
}

async fn run_agent(agent_launch: AgentLaunch, addr: SocketAddr) -> Result<()> {
    let agent_name = agent_launch.name();
    let mut child = agent_launch.spawn(addr)?;
    let status = child
        .wait()
        .await
        .with_context(|| format!("failed waiting for {agent_name}"))?;
    if !status.success() {
        bail!("{agent_name} exited with status {status}");
    }
    Ok(())
}

async fn ensure_daemon_running(
    cli: &Cli,
    config_path: Option<&FsPath>,
    include_runtime_overrides: bool,
    addr: SocketAddr,
) -> Result<()> {
    ensure_managed_addr(addr)?;
    if probe_existing_llmproxy(addr).await {
        print_daemon_banner(addr, "already running")?;
        return Ok(());
    }
    remove_stale_daemon_files(addr)?;
    spawn_daemon_process(cli, config_path, include_runtime_overrides)?;
    wait_for_daemon(addr, true).await?;
    print_daemon_banner(addr, "started")?;
    Ok(())
}

fn print_daemon_banner(addr: SocketAddr, status: &str) -> Result<()> {
    let info = read_runtime_info(addr)?.unwrap_or_else(|| RuntimeInfo {
        pid: 0,
        proxy_url: format!("http://{addr}/"),
        admin_url: None,
        integrated: false,
        routing_mode: None,
        log_file: None,
    });
    println!("{}", startup_banner(&info, status));
    Ok(())
}

const LLMPROXY_BANNER: &str = r#" _ _
| | |_ __ ___  _ __  _ __ _____  ___   _
| | | '_ ` _ \| '_ \| '__/ _ \ \/ / | | |
| | | | | | | | |_) | | | (_) >  <| |_| |
|_|_|_| |_| |_| .__/|_|  \___/_/\_\__, |
              |_|                   |___/"#;

fn startup_banner(info: &RuntimeInfo, status: &str) -> String {
    let admin = info
        .admin_url
        .as_deref()
        .unwrap_or("disabled (set [admin] enable = true)");
    let log = info
        .log_file
        .as_deref()
        .map(|name| format!("~/.llmproxy/{name}"))
        .unwrap_or_else(|| "disabled".to_string());
    format!(
        "{LLMPROXY_BANNER}\n\nStatus : {status}\nMode   : {}\nProxy  : {}\nAdmin  : {admin}\nLog    : {log}",
        info.routing_mode_label(),
        info.proxy_url
    )
}

fn spawn_daemon_process(
    cli: &Cli,
    config_path: Option<&FsPath>,
    include_runtime_overrides: bool,
) -> Result<()> {
    let executable = env::current_exe().context("failed to locate llmproxy executable")?;
    let mut command = std::process::Command::new(executable);
    command
        .arg("--foreground")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    if let Some(path) = config_path {
        command.arg("--config").arg(path);
    }
    if let Some(listen) = &cli.listen {
        command.arg("--listen").arg(listen);
    }
    if include_runtime_overrides {
        if let Some(base_url) = &cli.default_provider_url {
            command.arg("--default-provider-url").arg(base_url);
        }
        if let Some(mode) = cli.requested_mode() {
            command.arg("--mode").arg(mode.as_str());
        }
    }

    configure_daemon_process(&mut command);

    command
        .spawn()
        .context("failed to spawn background llmproxy daemon")?;
    Ok(())
}

#[cfg(unix)]
fn configure_daemon_process(command: &mut std::process::Command) {
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

#[cfg(windows)]
fn configure_daemon_process(command: &mut std::process::Command) {
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};

    command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
}

async fn shutdown_daemon(addr: SocketAddr) -> Result<bool> {
    ensure_managed_addr(addr)?;
    let pid_path = daemon_pid_path(addr);
    let running = probe_existing_llmproxy(addr).await;
    let Some(pid) = read_pid_file(&pid_path)? else {
        if running {
            bail!(
                "llmproxy is running at {addr} but pid file {} is missing",
                pid_path.display()
            );
        }
        let _ = fs::remove_file(daemon_info_path(addr));
        println!("llmproxy is not running at {addr}");
        return Ok(false);
    };

    if !running && !process_exists(pid) {
        let _ = fs::remove_file(&pid_path);
        let _ = fs::remove_file(daemon_info_path(addr));
        println!("llmproxy is not running at {addr}");
        return Ok(false);
    }

    if let Err(error) = terminate_daemon_process(pid) {
        if !process_exists(pid) {
            let _ = fs::remove_file(&pid_path);
            let _ = fs::remove_file(daemon_info_path(addr));
            if running {
                bail!("llmproxy is listening at {addr} but pid {pid} does not exist");
            }
            println!("llmproxy is not running at {addr}");
            return Ok(false);
        }
        return Err(error).context(format!("failed to stop llmproxy pid {pid}"));
    }

    wait_for_daemon(addr, false).await?;
    let _ = fs::remove_file(&pid_path);
    let _ = fs::remove_file(daemon_info_path(addr));
    println!("llmproxy stopped at {addr}");
    Ok(true)
}

async fn wait_for_daemon(addr: SocketAddr, expect_running: bool) -> Result<()> {
    for _ in 0..50 {
        if probe_existing_llmproxy(addr).await == expect_running {
            return Ok(());
        }
        sleep(Duration::from_millis(100)).await;
    }
    if expect_running {
        bail!("llmproxy did not become healthy at {addr} within 5s");
    }
    bail!("llmproxy did not shut down at {addr} within 5s");
}

fn ensure_managed_addr(addr: SocketAddr) -> Result<()> {
    if addr.port() == 0 {
        bail!("daemon mode requires a fixed listen port; got {addr}");
    }
    Ok(())
}

fn daemon_pid_path(addr: SocketAddr) -> PathBuf {
    let runtime_dir = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let sanitized = addr
        .to_string()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    runtime_dir.join(format!("llmproxy-{sanitized}.pid"))
}

fn daemon_info_path(addr: SocketAddr) -> PathBuf {
    daemon_pid_path(addr).with_extension("json")
}

fn read_pid_file(path: &FsPath) -> Result<Option<u32>> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let pid = raw
        .trim()
        .parse::<u32>()
        .with_context(|| format!("invalid pid file {}", path.display()))?;
    if pid == 0 {
        bail!("invalid pid file {}: pid must be positive", path.display());
    }
    Ok(Some(pid))
}

fn read_runtime_info(addr: SocketAddr) -> Result<Option<RuntimeInfo>> {
    let path = daemon_info_path(addr);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    serde_json::from_str(&raw)
        .map(Some)
        .with_context(|| format!("invalid runtime metadata {}", path.display()))
}

fn remove_stale_daemon_files(addr: SocketAddr) -> Result<()> {
    for path in [daemon_pid_path(addr), daemon_info_path(addr)] {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("failed to remove {}", path.display()));
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn terminate_daemon_process(pid: u32) -> io::Result<()> {
    let pid = libc::pid_t::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "pid exceeds platform range"))?;
    if unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn terminate_daemon_process(pid: u32) -> io::Result<()> {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess},
    };

    let process = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    let terminated = unsafe { TerminateProcess(process, 0) };
    let error = (terminated == 0).then(io::Error::last_os_error);
    let _ = unsafe {
        CloseHandle(process);
    };
    match error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn process_exists(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_ACCESS_DENIED, STILL_ACTIVE},
        System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };

    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return io::Error::last_os_error().raw_os_error() == Some(ERROR_ACCESS_DENIED as i32);
    }
    let mut exit_code = 0;
    let queried = unsafe { GetExitCodeProcess(process, &mut exit_code) } != 0;
    let _ = unsafe {
        CloseHandle(process);
    };
    queried && exit_code == STILL_ACTIVE as u32
}

impl PidFileGuard {
    fn create(addr: SocketAddr) -> Result<Option<Self>> {
        if addr.port() == 0 {
            return Ok(None);
        }
        let path = daemon_pid_path(addr);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let pid = std::process::id();
        fs::write(&path, format!("{pid}\n"))
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(Some(Self { path, pid }))
    }
}

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        let should_remove = fs::read_to_string(&self.path)
            .ok()
            .and_then(|raw| raw.trim().parse::<u32>().ok())
            == Some(self.pid);
        if should_remove {
            let _ = fs::remove_file(&self.path);
        }
    }
}

impl RuntimeInfo {
    fn new(
        proxy_addr: SocketAddr,
        admin_addr: Option<SocketAddr>,
        routing_mode: RoutingMode,
        log_file: Option<String>,
    ) -> Self {
        Self {
            pid: std::process::id(),
            proxy_url: format!("http://{proxy_addr}/"),
            admin_url: admin_addr.map(|addr| format!("http://{addr}/")),
            integrated: routing_mode == RoutingMode::Integrated,
            routing_mode: Some(routing_mode),
            log_file,
        }
    }

    /// The mode this instance published. Runtime files written before
    /// `routing_mode` existed only distinguish integrated from everything else,
    /// so they stay ambiguous rather than claiming a mode they never recorded.
    fn declared_routing_mode(&self) -> Option<RoutingMode> {
        self.routing_mode
            .or_else(|| self.integrated.then_some(RoutingMode::Integrated))
    }

    fn routing_mode_label(&self) -> &'static str {
        self.declared_routing_mode()
            .map_or("legacy/unknown", RoutingMode::as_str)
    }
}

fn update_runtime_mode(addr: SocketAddr, routing_mode: RoutingMode) -> Result<()> {
    let Some(mut info) = read_runtime_info(addr)? else {
        return Ok(());
    };
    if info.pid != std::process::id() || info.routing_mode == Some(routing_mode) {
        return Ok(());
    }
    info.routing_mode = Some(routing_mode);
    info.integrated = routing_mode == RoutingMode::Integrated;
    let path = daemon_info_path(addr);
    let raw = serde_json::to_vec(&info).context("failed to encode runtime metadata")?;
    fs::write(&path, raw)
        .with_context(|| format!("failed to update runtime metadata {}", path.display()))
}

impl RuntimeInfoGuard {
    fn create(addr: SocketAddr, info: &RuntimeInfo) -> Result<Option<Self>> {
        if addr.port() == 0 {
            return Ok(None);
        }
        let path = daemon_info_path(addr);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let raw = serde_json::to_vec(info).context("failed to encode runtime metadata")?;
        fs::write(&path, raw)
            .with_context(|| format!("failed to write runtime metadata {}", path.display()))?;
        Ok(Some(Self {
            path,
            pid: info.pid,
        }))
    }
}

impl Drop for RuntimeInfoGuard {
    fn drop(&mut self) {
        let should_remove = fs::read_to_string(&self.path)
            .ok()
            .and_then(|raw| serde_json::from_str::<RuntimeInfo>(&raw).ok())
            .is_some_and(|info| info.pid == self.pid);
        if should_remove {
            let _ = fs::remove_file(&self.path);
        }
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("failed to register SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn resolve_config_path(
    explicit: Option<PathBuf>,
    configured: Option<PathBuf>,
    home: Option<&FsPath>,
    exists: &impl Fn(&FsPath) -> bool,
) -> Option<PathBuf> {
    explicit.or(configured).or_else(|| {
        let local = PathBuf::from("config.toml");
        if exists(&local) {
            return Some(local);
        }
        home.map(|home| home.join(".llmproxy").join("config.toml"))
            .filter(|path| exists(path))
    })
}

fn discovered_config_path(cli: &Cli) -> Option<PathBuf> {
    let configured = env::var_os("LLMPROXY_CONFIG")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    let home = agent_launcher::home_dir();
    resolve_config_path(
        cli.config.clone(),
        configured,
        home.as_deref(),
        &FsPath::exists,
    )
}

fn load_config(cli: &Cli) -> Result<(Config, Option<PathBuf>)> {
    let config_path = discovered_config_path(cli);
    let mut config = if let Some(path) = &config_path {
        read_config(path)?
    } else {
        Config::default()
    };

    if let Some(listen) = &cli.listen {
        config.listen_addr = listen.clone();
    }
    if let Some(mode) = cli.requested_mode() {
        config.set_routing_mode(mode);
    }
    if let Some(base_url) = &cli.default_provider_url {
        if let Some(provider) = config.providers.get_mut(&config.default_provider) {
            provider.base_url = base_url.clone();
        } else {
            config.upstream.base_url = base_url.clone();
        }
    }
    finalize_config_keys(&mut config);
    validate_config(&config)?;

    Ok((config, config_path))
}

/// Re-run the unknown-key scan once logging exists. Config parsing happens
/// before the log file is opened, so findings from that pass would otherwise
/// be written to a subscriber nobody is listening to.
fn report_unknown_config_keys(path: &FsPath) {
    let Ok(raw) = fs::read_to_string(path) else {
        return;
    };
    let Ok(document) = raw.parse::<toml::Table>() else {
        return;
    };
    let mut found = Vec::new();
    warn_on_unknown_config_keys(&document, &path.display().to_string(), &mut found);
}

fn log_loaded_config(config: &Config, config_path: Option<&FsPath>) {
    info!(
        config_source = %config_path.map_or_else(
            || "environment/defaults".to_string(),
            |path| path.display().to_string(),
        ),
        default_provider = %config.default_provider,
        providers = effective_provider_configs(config).map_or(0, |providers| providers.len()),
        routing_mode = config.routing_mode.as_str(),
        model_aliases = config.model_aliases.len(),
        role_mappings = config.role_map.len(),
        max_logging_retention_days = config.max_logging_retention_days,
        disable_thinking = config.chat.disable_thinking,
        expose_aliases = config.models.expose_aliases,
        auto_model_mapping = config.model_routing == ModelRoutingMode::Remap,
        admin_enabled = config.admin.enable,
        "loaded config"
    );
}

fn validate_launcher_runtime(config: &Config, mode: LauncherMode) -> Result<()> {
    if mode == LauncherMode::Proxy {
        effective_provider_configs(config)?;
        return Ok(());
    }
    let providers = effective_provider_configs(config)?;
    let mut interceptor_config = config.interceptor.clone();
    interceptor_config.enabled = true;
    interceptor::validate_config(&interceptor_config, &providers)?;
    if mode == LauncherMode::Integrated {
        let mut integrated = config.integrated.clone();
        integrated.enabled = true;
        interceptor::validate_integrated_config(&integrated, &interceptor_config, &providers)?;
    }
    Ok(())
}

/// Normalize base URLs and resolve provider API keys. Runs after config load
/// and after every runtime config edit so both paths share identical rules.
fn finalize_config_keys(config: &mut Config) {
    finalize_config_keys_with_env(config, &|name| env::var(name).ok());
}

fn finalize_config_keys_with_env(config: &mut Config, env_value: &impl Fn(&str) -> Option<String>) {
    config.upstream.base_url = config.upstream.base_url.trim_end_matches('/').to_string();
    let legacy_upstream_is_interceptor = config.interceptor.provider == config.default_provider
        && !config.providers.contains_key(&config.default_provider);
    config.upstream.api_key =
        resolve_api_key_with_env(&config.upstream, !legacy_upstream_is_interceptor, env_value);
    for (name, provider) in &mut config.providers {
        provider.base_url = provider.base_url.trim_end_matches('/').to_string();
        let uses_client_auth = config.model_routing == ModelRoutingMode::Passthrough;
        let is_interceptor_provider = name == &config.interceptor.provider;
        // Borrowed-OAuth providers never carry a static key; their token comes
        // from the local CLI's on-disk login at request time.
        provider.api_key = if provider.auth != ProviderAuth::None {
            None
        } else {
            resolve_api_key_with_env(
                provider,
                !is_interceptor_provider && !uses_client_auth,
                env_value,
            )
        };
    }
}

fn read_config(path: &FsPath) -> Result<Config> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    parse_config(&raw, &path.display().to_string())
}

/// Parse a config and settle its routing mode. Configs written before
/// `routing_mode` existed declared the same thing with two `enabled` flags, so
/// they migrate here; enabling both was never a valid instance shape.
fn parse_config(raw: &str, source: &str) -> Result<Config> {
    let document = raw
        .parse::<toml::Table>()
        .with_context(|| format!("invalid TOML config {source}"))?;
    let mut config: Config =
        toml::from_str(raw).with_context(|| format!("invalid TOML config {source}"))?;
    if !document.contains_key("routing_mode") {
        config.routing_mode = match (config.interceptor.enabled, config.integrated.enabled) {
            (false, false) => RoutingMode::Proxy,
            (true, false) => RoutingMode::Intercept,
            (false, true) => RoutingMode::Integrated,
            (true, true) => bail!(
                "legacy config enables both interceptor and integrated routing; replace both enabled fields with one top-level routing_mode"
            ),
        };
    }
    config.set_routing_mode(config.routing_mode);
    let mut unknown = Vec::new();
    warn_on_unknown_config_keys(&document, source, &mut unknown);
    Ok(config)
}

/// Keys each config table accepts. Serde ignores anything it does not
/// recognize, so a typo like `timout_ms` silently keeps the default and
/// quietly changes routing behavior. These are reported instead, and only
/// reported: an unknown key must never stop a working config from loading.
const CONFIG_KEYS: &[&str] = &[
    "listen_addr",
    "max_logging_retention_days",
    "default_provider",
    "routing_mode",
    "upstream",
    "providers",
    "interceptor",
    "integrated",
    "model_aliases",
    "role_map",
    "chat",
    "responses",
    "models",
    "admin",
    "routing_rules",
    "pricing",
];
const PROVIDER_KEYS: &[&str] = &[
    "kind",
    "base_url",
    "api_key",
    "api_key_env",
    "auth",
    "headers",
    "reasoning_effort",
    "timeout_secs",
    "connect_timeout_secs",
    "pool_max_idle_per_host",
    "pool_idle_timeout_secs",
    "fallback",
    "circuit_breaker_threshold",
    "circuit_breaker_cooldown_secs",
];
const INTERCEPTOR_KEYS: &[&str] = &[
    "enabled",
    "reject_sensitive",
    "provider",
    "model",
    "timeout_ms",
    "fallback_on_error",
    "instructions",
    "temperature",
    "max_tokens",
    "parallel_tool_calls",
];
const INTEGRATED_KEYS: &[&str] = &[
    "enabled",
    "sticky_ttl_secs",
    "sticky_capacity",
    "instructions",
    "targets",
];
const INTEGRATED_TARGET_KEYS: &[&str] = &["provider", "model", "hint", "reasoning_effort"];
const MODEL_ALIAS_KEYS: &[&str] = &["provider", "model"];
const CHAT_KEYS: &[&str] = &["disable_thinking"];
const RESPONSES_KEYS: &[&str] = &["mode"];
const MODELS_KEYS: &[&str] = &["expose_aliases", "expose_upstream", "cache_ttl_secs"];
const ADMIN_KEYS: &[&str] = &["enable", "enabled", "listen_address", "ui_dir"];
const ROUTING_RULE_KEYS: &[&str] = &[
    "name",
    "min_input_tokens",
    "max_input_tokens",
    "has_tools",
    "contains_any",
    "requested_model",
    "route",
];
const PRICING_KEYS: &[&str] = &["baseline_model", "models"];
const MODEL_PRICE_KEYS: &[&str] = &["input_per_mtok", "output_per_mtok"];

/// Collect dotted paths of config keys no struct claims, and log each one.
/// Free-form tables (`role_map`, provider `headers`) are never descended into
/// because any key in them is valid by definition.
fn warn_on_unknown_config_keys(document: &toml::Table, source: &str, found: &mut Vec<String>) {
    fn check(table: &toml::Table, path: &str, known: &[&str], found: &mut Vec<String>) {
        for key in table.keys() {
            if !known.contains(&key.as_str()) {
                found.push(format!("{path}.{key}"));
            }
        }
    }
    fn table<'a>(parent: &'a toml::Table, key: &str) -> Option<&'a toml::Table> {
        parent.get(key).and_then(toml::Value::as_table)
    }

    let start = found.len();
    check(document, "config", CONFIG_KEYS, found);
    if let Some(upstream) = table(document, "upstream") {
        check(upstream, "upstream", PROVIDER_KEYS, found);
    }
    if let Some(providers) = table(document, "providers") {
        for (name, item) in providers {
            if let Some(provider) = item.as_table() {
                check(provider, &format!("providers.{name}"), PROVIDER_KEYS, found);
            }
        }
    }
    if let Some(interceptor) = table(document, "interceptor") {
        check(interceptor, "interceptor", INTERCEPTOR_KEYS, found);
    }
    if let Some(integrated) = table(document, "integrated") {
        check(integrated, "integrated", INTEGRATED_KEYS, found);
        if let Some(targets) = table(integrated, "targets") {
            for (name, item) in targets {
                if let Some(target) = item.as_table() {
                    check(
                        target,
                        &format!("integrated.targets.{name}"),
                        INTEGRATED_TARGET_KEYS,
                        found,
                    );
                }
            }
        }
    }
    if let Some(aliases) = table(document, "model_aliases") {
        for (name, item) in aliases {
            // A bare `alias = "model"` has no keys to check.
            if let Some(alias) = item.as_table() {
                check(
                    alias,
                    &format!("model_aliases.{name}"),
                    MODEL_ALIAS_KEYS,
                    found,
                );
            }
        }
    }
    // `[[routing_rules]]` is an array of tables, not a table.
    if let Some(rules) = document
        .get("routing_rules")
        .and_then(toml::Value::as_array)
    {
        for (index, item) in rules.iter().enumerate() {
            if let Some(rule) = item.as_table() {
                check(
                    rule,
                    &format!("routing_rules[{index}]"),
                    ROUTING_RULE_KEYS,
                    found,
                );
            }
        }
    }
    if let Some(pricing) = table(document, "pricing") {
        check(pricing, "pricing", PRICING_KEYS, found);
        if let Some(models) = table(pricing, "models") {
            for (name, item) in models {
                if let Some(price) = item.as_table() {
                    check(
                        price,
                        &format!("pricing.models.{name}"),
                        MODEL_PRICE_KEYS,
                        found,
                    );
                }
            }
        }
    }
    for (section, known) in [
        ("chat", CHAT_KEYS),
        ("responses", RESPONSES_KEYS),
        ("models", MODELS_KEYS),
        ("admin", ADMIN_KEYS),
    ] {
        if let Some(found_table) = table(document, section) {
            check(found_table, section, known, found);
        }
    }

    for key in &found[start..] {
        warn!(config_source = source, key = %key, "unrecognized config key ignored");
    }
}

fn resolve_api_key_with_env(
    upstream: &UpstreamConfig,
    inherit_openai_api_key: bool,
    env_value: &impl Fn(&str) -> Option<String>,
) -> Option<String> {
    upstream
        .api_key
        .as_deref()
        .filter(|key| !key.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            upstream
                .api_key_env
                .as_ref()
                .and_then(|name| env_value(name))
                .filter(|key| !key.is_empty())
        })
        .or_else(|| {
            (inherit_openai_api_key && upstream.kind.can_inherit_openai_api_key())
                .then(|| env_value("OPENAI_API_KEY"))
                .flatten()
        })
}

fn effective_provider_configs(config: &Config) -> Result<HashMap<String, UpstreamConfig>> {
    let mut providers = config.providers.clone();
    if !providers.contains_key(&config.default_provider) {
        if config.default_provider == "default" {
            providers.insert("default".to_string(), config.upstream.clone());
        } else {
            bail!(
                "default_provider {:?} is not defined in [providers]",
                config.default_provider
            );
        }
    }
    Ok(providers)
}

async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

fn launcher_error_response(message: impl Into<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": {
                "message": message.into(),
                "type": "invalid_request_error"
            }
        })),
    )
        .into_response()
}

fn request_state(shared: &SharedState, launcher_token: Option<&str>) -> ResponseResult<AppState> {
    let state = shared.snapshot();
    let Some(token) = launcher_token else {
        return Ok(state);
    };
    let session = LauncherSession::decode(token)
        .map_err(|error| Box::new(launcher_error_response(error.to_string())))?;
    state
        .with_launcher_session(&session)
        .map_err(|error| Box::new(launcher_error_response(error.to_string())))
}

fn trace_client_request(
    frontend: &str,
    launcher_token: Option<&str>,
    body: &Value,
    redact_body: bool,
) {
    if redact_body {
        trace!(
            frontend,
            launcher_session = launcher_token.is_some(),
            "client request body omitted during sensitive-data screening"
        );
    } else {
        trace!(
            frontend,
            launcher_session = launcher_token.is_some(),
            body = %body,
            "client request body"
        );
    }
}

fn provider_request_headers(
    state: &AppState,
    incoming: &HeaderMap,
    provider: &ProviderState,
) -> HeaderMap {
    if !state.forward_caller_credentials || provider.name != state.config.default_provider {
        return HeaderMap::new();
    }
    if state.config.model_routing == ModelRoutingMode::Passthrough
        || route_class_for_base_url(&provider.config.base_url) == stats::RouteClass::External
    {
        incoming.clone()
    } else {
        HeaderMap::new()
    }
}

async fn models(State(shared): State<SharedState>, headers: HeaderMap) -> Response {
    models_impl(&shared, None, headers).await
}

async fn launcher_models(
    AxumPath(launcher_token): AxumPath<String>,
    State(shared): State<SharedState>,
    headers: HeaderMap,
) -> Response {
    models_impl(&shared, Some(launcher_token.as_str()), headers).await
}

async fn launcher_copilot_models(
    AxumPath(launcher_token): AxumPath<String>,
    State(shared): State<SharedState>,
    headers: HeaderMap,
) -> Response {
    copilot_relay_endpoint_impl(
        &shared,
        launcher_token.as_str(),
        &headers,
        Method::GET,
        "models",
    )
    .await
}

async fn launcher_copilot_model_by_id(
    AxumPath((launcher_token, model)): AxumPath<(String, String)>,
    State(shared): State<SharedState>,
    headers: HeaderMap,
) -> Response {
    copilot_relay_endpoint_impl(
        &shared,
        launcher_token.as_str(),
        &headers,
        Method::GET,
        &format!("models/{model}"),
    )
    .await
}

async fn launcher_copilot_model_policy(
    AxumPath((launcher_token, model)): AxumPath<(String, String)>,
    State(shared): State<SharedState>,
    headers: HeaderMap,
) -> Response {
    copilot_relay_endpoint_impl(
        &shared,
        launcher_token.as_str(),
        &headers,
        Method::POST,
        &format!("models/{model}/policy"),
    )
    .await
}

async fn copilot_relay_endpoint_impl(
    shared: &SharedState,
    launcher_token: &str,
    headers: &HeaderMap,
    method: Method,
    upstream_path: &str,
) -> Response {
    let state = match request_state(shared, Some(launcher_token)) {
        Ok(state) => state,
        Err(response) => return *response,
    };
    if state.launcher_kind != Some(LauncherKind::Copilot) {
        return StatusCode::NOT_FOUND.into_response();
    }
    proxy_relay(
        state.default_provider(),
        headers,
        method,
        upstream_path,
        None,
        true,
    )
    .await
}

async fn models_impl(
    shared: &SharedState,
    launcher_token: Option<&str>,
    headers: HeaderMap,
) -> Response {
    let state = match request_state(shared, launcher_token) {
        Ok(state) => state,
        Err(response) => return *response,
    };
    if state.config.model_routing == ModelRoutingMode::Passthrough {
        let provider = state.default_provider();
        let provider_headers = provider_request_headers(&state, &headers, provider);
        return match proxy_get(provider, &provider_headers, "models").await {
            Ok(models) => Json(models).into_response(),
            Err(error_response) => error_response,
        };
    }
    Json(aggregated_models_json(&state, &headers).await).into_response()
}

/// The remap-mode `/v1/models` payload: every reachable provider's models
/// (raw + namespaced), plus alias and integrated-target entries.
async fn aggregated_models_json(state: &AppState, headers: &HeaderMap) -> Value {
    if !state.config.models.expose_upstream {
        return rewrite_models_response(json!({ "object": "list", "data": [] }), &state.config);
    }

    let mut provider_names = state.providers.keys().cloned().collect::<Vec<_>>();
    provider_names.sort();
    let ttl = models_cache_ttl(&state.config);

    // Providers are discovered concurrently: fetching them in sequence adds
    // every provider's latency to the catalog, and one unreachable provider
    // delays all the rest. Results are reassembled in sorted name order so the
    // catalog stays deterministic regardless of which upstream answers first.
    let fetches = provider_names.iter().map(|provider_name| {
        let provider = state
            .providers
            .get(provider_name)
            .expect("provider name came from map");
        let provider_headers = provider_request_headers(state, headers, provider);
        async move {
            if let Some(cached) = provider.model_cache.cached_upstream_models_response(ttl) {
                debug!(provider = %provider.name, "serving upstream models from cache");
                return Some(cached);
            }
            let discovery = model_discovery_timeout(provider);
            match tokio::time::timeout(
                discovery,
                fetch_upstream_models(provider, &provider_headers),
            )
            .await
            {
                Ok(Ok(upstream)) => {
                    cache_upstream_models(provider, upstream.clone(), "models_endpoint");
                    Some(upstream)
                }
                Ok(Err(error)) => {
                    error!(
                        provider = %provider.name,
                        upstream = %provider.config.base_url,
                        error = %error,
                        "omitting unavailable provider from aggregated model list"
                    );
                    None
                }
                Err(_) => {
                    error!(
                        provider = %provider.name,
                        upstream = %provider.config.base_url,
                        timeout_secs = discovery.as_secs(),
                        "omitting provider whose model discovery timed out"
                    );
                    None
                }
            }
        }
    });
    let discovered = futures_util::future::join_all(fetches).await;

    let mut data = Vec::new();
    for (provider_name, upstream) in provider_names.iter().zip(discovered) {
        let Some(upstream) = upstream else {
            continue;
        };
        let provider = state
            .providers
            .get(provider_name)
            .expect("provider name came from map");
        append_provider_models(
            &mut data,
            upstream,
            &provider.name,
            provider.name == state.config.default_provider,
        );
    }

    rewrite_models_response(json!({ "object": "list", "data": data }), &state.config)
}

/// The model catalog for the admin Models tab: exactly what a client of this
/// proxy sees from `GET /v1/models`, using proxy-owned credentials.
pub(crate) async fn models_catalog_json(state: &AppState) -> Value {
    let headers = HeaderMap::new();
    if state.config.model_routing == ModelRoutingMode::Passthrough {
        let provider = state.default_provider();
        let provider_headers = provider_request_headers(state, &headers, provider);
        return match proxy_get(provider, &provider_headers, "models").await {
            Ok(models) => models,
            Err(_) => json!({ "object": "list", "data": [] }),
        };
    }
    aggregated_models_json(state, &headers).await
}

async fn model_by_id(
    State(shared): State<SharedState>,
    AxumPath(model): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    model_by_id_impl(&shared, None, model, headers).await
}

async fn launcher_model_by_id(
    AxumPath((launcher_token, model)): AxumPath<(String, String)>,
    State(shared): State<SharedState>,
    headers: HeaderMap,
) -> Response {
    model_by_id_impl(&shared, Some(launcher_token.as_str()), model, headers).await
}

async fn model_by_id_impl(
    shared: &SharedState,
    launcher_token: Option<&str>,
    model: String,
    headers: HeaderMap,
) -> Response {
    let state = match request_state(shared, launcher_token) {
        Ok(state) => state,
        Err(response) => return *response,
    };
    if state.config.model_routing == ModelRoutingMode::Passthrough {
        let provider = state.default_provider();
        let provider_headers = provider_request_headers(&state, &headers, provider);
        return match proxy_get(provider, &provider_headers, &format!("models/{model}")).await {
            Ok(model) => Json(model).into_response(),
            Err(error_response) => error_response,
        };
    }
    if model == INTEGRATED_AUTO_MODEL && !state.config.integrated.targets.is_empty() {
        return Json(integrated_auto_model()).into_response();
    }
    if let Some(route) = state.config.model_aliases.get(&model) {
        let provider_name = route.provider(&state.config.default_provider);
        let Some(provider) = state.providers.get(provider_name) else {
            return unknown_provider_response(provider_name);
        };
        provider.model_cache.log_first_seen_model(
            provider_name,
            &model,
            route.model(),
            ModelMappingSource::Alias,
        );
        return Json(model_object(
            &model,
            Some(route.model()),
            Some(provider_name),
        ))
        .into_response();
    }
    if let Some((provider_name, target)) = namespaced_model_target(&state.config, &model) {
        let Some(provider) = state.providers.get(provider_name) else {
            return unknown_provider_response(provider_name);
        };
        provider.model_cache.log_first_seen_model(
            provider_name,
            &model,
            target,
            ModelMappingSource::Namespace,
        );
        return Json(model_object(&model, Some(target), Some(provider_name))).into_response();
    }

    let provider = state.default_provider();
    let provider_headers = provider_request_headers(&state, &headers, provider);
    match ensure_default_upstream_model(provider, &provider_headers).await {
        Ok(target) => {
            provider.model_cache.log_first_seen_model(
                &provider.name,
                &model,
                &target,
                ModelMappingSource::Auto,
            );
            Json(model_object(&model, Some(&target), Some(&provider.name))).into_response()
        }
        Err(error_response) => error_response,
    }
}

async fn completions(
    State(shared): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    completions_impl(&shared, None, headers, body).await
}

async fn launcher_completions(
    AxumPath(launcher_token): AxumPath<String>,
    State(shared): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    completions_impl(&shared, Some(launcher_token.as_str()), headers, body).await
}

async fn completions_impl(
    shared: &SharedState,
    launcher_token: Option<&str>,
    headers: HeaderMap,
    mut body: Value,
) -> Response {
    let state = match request_state(shared, launcher_token) {
        Ok(state) => state,
        Err(response) => return *response,
    };
    trace_client_request("completions", launcher_token, &body, false);
    proxy_stats().record_request();
    let started = Instant::now();
    let provider = match rewrite_proxy_request_value(&state, &headers, &mut body).await {
        Ok(provider) => provider,
        Err(error_response) => return *error_response,
    };
    let provider_headers = provider_request_headers(&state, &headers, &provider);
    let class = route_class_for_base_url(&provider.config.base_url);
    let model = request_model(&body).to_string();
    let estimated_input_tokens = estimate_tokens(&body) as u64;
    log_request_value(class, &provider.name, &body);
    let model_started = Instant::now();
    let response = proxy_json(&provider, &provider_headers, "completions", &body).await;
    track_response(
        ResponseTracking::new(
            class,
            &provider.name,
            &model,
            "completions",
            started,
            model_started,
            estimated_input_tokens,
        )
        .with_pricing(request_pricing(&state.config, &provider.name, &model)),
        response,
    )
}

async fn embeddings(
    State(shared): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    embeddings_impl(&shared, None, headers, body).await
}

async fn launcher_embeddings(
    AxumPath(launcher_token): AxumPath<String>,
    State(shared): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    embeddings_impl(&shared, Some(launcher_token.as_str()), headers, body).await
}

async fn embeddings_impl(
    shared: &SharedState,
    launcher_token: Option<&str>,
    headers: HeaderMap,
    mut body: Value,
) -> Response {
    let state = match request_state(shared, launcher_token) {
        Ok(state) => state,
        Err(response) => return *response,
    };
    trace_client_request("embeddings", launcher_token, &body, false);
    proxy_stats().record_request();
    let started = Instant::now();
    let provider = match rewrite_proxy_request_value(&state, &headers, &mut body).await {
        Ok(provider) => provider,
        Err(error_response) => return *error_response,
    };
    let provider_headers = provider_request_headers(&state, &headers, &provider);
    let class = route_class_for_base_url(&provider.config.base_url);
    let model = request_model(&body).to_string();
    let estimated_input_tokens = estimate_tokens(&body) as u64;
    log_request_value(class, &provider.name, &body);
    let model_started = Instant::now();
    let response = proxy_json(&provider, &provider_headers, "embeddings", &body).await;
    track_response(
        ResponseTracking::new(
            class,
            &provider.name,
            &model,
            "embeddings",
            started,
            model_started,
            estimated_input_tokens,
        )
        .without_output_estimation()
        .with_pricing(request_pricing(&state.config, &provider.name, &model)),
        response,
    )
}

async fn chat_completions(
    State(shared): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    chat_completions_impl(&shared, None, headers, body).await
}

async fn launcher_chat_completions(
    AxumPath(launcher_token): AxumPath<String>,
    State(shared): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    chat_completions_impl(&shared, Some(launcher_token.as_str()), headers, body).await
}

async fn chat_completions_impl(
    shared: &SharedState,
    launcher_token: Option<&str>,
    headers: HeaderMap,
    body: Value,
) -> Response {
    let state = match request_state(shared, launcher_token) {
        Ok(state) => state,
        Err(response) => return *response,
    };
    trace_client_request(
        "chat_completions",
        launcher_token,
        &body,
        state.config.interceptor.reject_sensitive,
    );
    proxy_stats().record_request();
    let started = Instant::now();
    let routing_audit = if state.triage_enabled() {
        match maybe_intercept(
            &state,
            &headers,
            body.clone(),
            InterceptorProtocol::ChatCompletions,
            InterceptionPolicy::Eligible,
            body.get("model")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            request_is_streaming(&body),
        )
        .await
        {
            InterceptionOutcome::Continue(audit) => Some(audit),
            InterceptionOutcome::Respond(response) => return response,
        }
    } else {
        None
    };

    let mut body = body;
    let provider = match rewrite_proxy_request_value(&state, &headers, &mut body).await {
        Ok(provider) => provider,
        Err(error_response) => {
            if let Some(audit) = routing_audit {
                audit.record_route_error("normal routing rejected the request");
            }
            return *error_response;
        }
    };
    let provider_headers = provider_request_headers(&state, &headers, &provider);
    if state.launcher_kind != Some(LauncherKind::Copilot) {
        normalize_chat_completion_request(&mut body, &state.config);
    }
    let class = route_class_for_base_url(&provider.config.base_url);
    let model = request_model(&body).to_string();
    if let Some(audit) = routing_audit {
        audit.record_destination(&provider, &model);
    }
    let estimated_input_tokens = estimate_tokens(&body) as u64;
    log_request_value(class, &provider.name, &body);
    let model_started = Instant::now();
    let (response, served) = if state.launcher_kind == Some(LauncherKind::Copilot)
        && state.config.model_routing == ModelRoutingMode::Passthrough
    {
        (
            proxy_json(&provider, &provider_headers, "chat/completions", &body).await,
            ServedBy::routed(&provider, &model),
        )
    } else {
        proxy_chat_failover(&state, &provider, &headers, &body, ChatWire::Chat, None).await
    };
    track_response(
        ResponseTracking::new(
            served.class,
            &served.provider,
            &served.model,
            "chat_completions",
            started,
            model_started,
            estimated_input_tokens,
        )
        .with_pricing(request_pricing(
            &state.config,
            &served.provider,
            &served.model,
        )),
        response,
    )
}

async fn responses(
    State(shared): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    responses_impl(&shared, None, headers, body).await
}

async fn launcher_responses(
    AxumPath(launcher_token): AxumPath<String>,
    State(shared): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    responses_impl(&shared, Some(launcher_token.as_str()), headers, body).await
}

async fn responses_impl(
    shared: &SharedState,
    launcher_token: Option<&str>,
    headers: HeaderMap,
    body: Value,
) -> Response {
    let state = match request_state(shared, launcher_token) {
        Ok(state) => state,
        Err(response) => return *response,
    };
    trace_client_request(
        "responses",
        launcher_token,
        &body,
        state.config.interceptor.reject_sensitive,
    );
    proxy_stats().record_request();
    let started = Instant::now();
    let client_model = body
        .get("model")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    let routing_audit = if state.triage_enabled() {
        let policy = InterceptionPolicy::for_responses(&headers, &body);
        let local_chat_request = responses_to_chat_request(&body);
        match maybe_intercept(
            &state,
            &headers,
            local_chat_request,
            InterceptorProtocol::Responses,
            policy,
            client_model.clone(),
            request_is_streaming(&body),
        )
        .await
        {
            InterceptionOutcome::Continue(audit) => Some(audit),
            InterceptionOutcome::Respond(response) => return response,
        }
    } else {
        None
    };

    let mut rewritten = body;
    let provider = match rewrite_proxy_request_value(&state, &headers, &mut rewritten).await {
        Ok(provider) => provider,
        Err(error_response) => {
            if let Some(audit) = routing_audit {
                audit.record_route_error("normal routing rejected the request");
            }
            return *error_response;
        }
    };
    let passthrough = state.config.responses.mode == ResponsesMode::Passthrough
        && matches!(
            provider.config.kind,
            ProviderKind::OpenAiCompatible | ProviderKind::OpenAi | ProviderKind::Copilot
        );
    if passthrough {
        apply_provider_reasoning_effort(&provider, ReasoningWire::Responses, &mut rewritten);
    }
    let provider_headers = provider_request_headers(&state, &headers, &provider);
    let class = route_class_for_base_url(&provider.config.base_url);
    let model = request_model(&rewritten).to_string();
    if let Some(audit) = routing_audit {
        audit.record_destination(&provider, &model);
    }
    let estimated_input_tokens = estimate_tokens(&rewritten) as u64;
    log_request_value(class, &provider.name, &rewritten);

    let model_started = Instant::now();
    let (response, served) = if passthrough {
        (
            proxy_json(&provider, &provider_headers, "responses", &rewritten).await,
            ServedBy::routed(&provider, &model),
        )
    } else {
        let mut chat_request = responses_to_chat_request(&rewritten);
        normalize_chat_completion_request(&mut chat_request, &state.config);
        if request_is_streaming(&chat_request) {
            ensure_chat_stream_include_usage(&mut chat_request);
        }
        proxy_chat_failover(
            &state,
            &provider,
            &headers,
            &chat_request,
            ChatWire::Responses,
            client_model,
        )
        .await
    };
    track_response(
        ResponseTracking::new(
            served.class,
            &served.provider,
            &served.model,
            "responses",
            started,
            model_started,
            estimated_input_tokens,
        )
        .with_pricing(request_pricing(
            &state.config,
            &served.provider,
            &served.model,
        )),
        response,
    )
}

async fn anthropic_messages(
    State(shared): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    anthropic_messages_impl(&shared, None, headers, body).await
}

async fn launcher_anthropic_messages(
    AxumPath(launcher_token): AxumPath<String>,
    State(shared): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    anthropic_messages_impl(&shared, Some(launcher_token.as_str()), headers, body).await
}

async fn anthropic_messages_impl(
    shared: &SharedState,
    launcher_token: Option<&str>,
    headers: HeaderMap,
    body: Value,
) -> Response {
    let state = match request_state(shared, launcher_token) {
        Ok(state) => state,
        Err(response) => return *response,
    };
    trace_client_request(
        "anthropic_messages",
        launcher_token,
        &body,
        state.config.interceptor.reject_sensitive,
    );
    proxy_stats().record_request();
    let started = Instant::now();
    let client_model = body
        .get("model")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    let routing_audit = if state.triage_enabled() {
        let local_chat_request = anthropic_message_to_chat_request(&body);
        match maybe_intercept(
            &state,
            &headers,
            local_chat_request,
            InterceptorProtocol::AnthropicMessages,
            InterceptionPolicy::Eligible,
            client_model.clone(),
            request_is_streaming(&body),
        )
        .await
        {
            InterceptionOutcome::Continue(audit) => Some(audit),
            InterceptionOutcome::Respond(response) => return response,
        }
    } else {
        None
    };

    let mut rewritten = body;
    let provider = match rewrite_proxy_request_value(&state, &headers, &mut rewritten).await {
        Ok(provider) => provider,
        Err(error_response) => {
            if let Some(audit) = routing_audit {
                audit.record_route_error("normal routing rejected the request");
            }
            return *error_response;
        }
    };
    let native_anthropic = provider.config.kind == ProviderKind::Anthropic
        || (provider.config.kind == ProviderKind::Copilot
            && is_claude_model(rewritten.get("model").and_then(Value::as_str)));
    let passthrough = state.config.anthropic_mode == AnthropicMode::Passthrough || native_anthropic;
    if passthrough {
        apply_provider_reasoning_effort(
            &provider,
            ReasoningWire::AnthropicMessages,
            &mut rewritten,
        );
    }
    let provider_headers = provider_request_headers(&state, &headers, &provider);
    let class = route_class_for_base_url(&provider.config.base_url);
    let model = request_model(&rewritten).to_string();
    if let Some(audit) = routing_audit {
        audit.record_destination(&provider, &model);
    }
    let estimated_input_tokens = estimate_tokens(&rewritten) as u64;
    log_request_value(class, &provider.name, &rewritten);
    if passthrough {
        let upstream_path = if provider.config.kind == ProviderKind::Copilot {
            "v1/messages"
        } else {
            "messages"
        };
        let model_started = Instant::now();
        let response = proxy_json(&provider, &provider_headers, upstream_path, &rewritten).await;
        return track_response(
            ResponseTracking::new(
                class,
                &provider.name,
                &model,
                "anthropic_messages",
                started,
                model_started,
                estimated_input_tokens,
            )
            .with_pricing(request_pricing(&state.config, &provider.name, &model)),
            response,
        );
    }
    let mut chat_request = anthropic_message_to_chat_request(&rewritten);
    normalize_chat_completion_request(&mut chat_request, &state.config);

    let model_started = Instant::now();
    if request_is_streaming(&chat_request) {
        ensure_chat_stream_include_usage(&mut chat_request);
    }
    let (response, served) = proxy_chat_failover(
        &state,
        &provider,
        &headers,
        &chat_request,
        ChatWire::Anthropic,
        client_model,
    )
    .await;
    track_response(
        ResponseTracking::new(
            served.class,
            &served.provider,
            &served.model,
            "anthropic_messages",
            started,
            model_started,
            estimated_input_tokens,
        )
        .with_pricing(request_pricing(
            &state.config,
            &served.provider,
            &served.model,
        )),
        response,
    )
}

async fn anthropic_count_tokens(Json(body): Json<Value>) -> Response {
    anthropic_count_tokens_impl(body).await
}

async fn launcher_anthropic_count_tokens(
    AxumPath(_launcher_token): AxumPath<String>,
    Json(body): Json<Value>,
) -> Response {
    anthropic_count_tokens_impl(body).await
}

async fn anthropic_count_tokens_impl(body: Value) -> Response {
    Json(json!({
        "input_tokens": estimate_tokens(&body)
    }))
    .into_response()
}

async fn codex_responses_compact(
    State(shared): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    codex_json_endpoint_impl(&shared, None, &headers, "responses/compact", &body).await
}

async fn launcher_codex_responses_compact(
    AxumPath(launcher_token): AxumPath<String>,
    State(shared): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    codex_json_endpoint_impl(
        &shared,
        Some(launcher_token.as_str()),
        &headers,
        "responses/compact",
        &body,
    )
    .await
}

async fn codex_memories_summarize(
    State(shared): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    codex_json_endpoint_impl(&shared, None, &headers, "memories/trace_summarize", &body).await
}

async fn launcher_codex_memories_summarize(
    AxumPath(launcher_token): AxumPath<String>,
    State(shared): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    codex_json_endpoint_impl(
        &shared,
        Some(launcher_token.as_str()),
        &headers,
        "memories/trace_summarize",
        &body,
    )
    .await
}

async fn codex_search(
    State(shared): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    codex_json_endpoint_impl(&shared, None, &headers, "alpha/search", &body).await
}

async fn launcher_codex_search(
    AxumPath(launcher_token): AxumPath<String>,
    State(shared): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    codex_json_endpoint_impl(
        &shared,
        Some(launcher_token.as_str()),
        &headers,
        "alpha/search",
        &body,
    )
    .await
}

async fn codex_json_endpoint_impl(
    shared: &SharedState,
    launcher_token: Option<&str>,
    headers: &HeaderMap,
    upstream_path: &str,
    body: &Value,
) -> Response {
    let state = match request_state(shared, launcher_token) {
        Ok(state) => state,
        Err(response) => return *response,
    };
    if state.config.model_routing != ModelRoutingMode::Passthrough
        && state.launcher_mode != Some(LauncherMode::Proxy)
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    if state.launcher_mode == Some(LauncherMode::Proxy) {
        let mut rewritten = body.clone();
        let provider = match rewrite_proxy_request_value(&state, headers, &mut rewritten).await {
            Ok(provider) => provider,
            Err(response) => return *response,
        };
        let provider_headers = provider_request_headers(&state, headers, &provider);
        proxy_json(&provider, &provider_headers, upstream_path, &rewritten).await
    } else {
        let provider = state.default_provider();
        let provider_headers = provider_request_headers(&state, headers, provider);
        proxy_json(provider, &provider_headers, upstream_path, body).await
    }
}

async fn refresh_upstream_models(
    provider: &ProviderState,
    incoming_headers: &HeaderMap,
) -> Result<Vec<String>> {
    let body = fetch_upstream_models(provider, incoming_headers)
        .await
        .with_context(|| "failed to refresh upstream /models")?;
    Ok(cache_upstream_models(provider, body, "refresh"))
}

async fn fetch_upstream_models(
    provider: &ProviderState,
    incoming_headers: &HeaderMap,
) -> std::result::Result<Value, UpstreamFailure> {
    let first_meta = UpstreamRequestMeta::new(
        &provider.name,
        incoming_headers,
        Method::GET,
        "models",
        UpstreamResponseKind::Json,
        true,
    );
    let mut combined = upstream_json(provider, incoming_headers, &first_meta, None).await?;
    if provider.config.kind != ProviderKind::Anthropic {
        return Ok(combined);
    }

    const MAX_MODEL_PAGES: usize = 100;
    let mut seen_cursors = HashSet::new();
    for _ in 1..MAX_MODEL_PAGES {
        if combined.get("has_more").and_then(Value::as_bool) != Some(true) {
            return Ok(combined);
        }
        let Some(cursor) = combined
            .get("last_id")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return Err(UpstreamFailure::decode(
                &first_meta,
                StatusCode::OK,
                "Anthropic models response has_more=true without last_id",
            ));
        };
        if !seen_cursors.insert(cursor.clone()) {
            return Err(UpstreamFailure::decode(
                &first_meta,
                StatusCode::OK,
                "Anthropic models pagination repeated a cursor",
            ));
        }

        let page_path = anthropic_models_page_path(&cursor);
        let page_meta = UpstreamRequestMeta::new(
            &provider.name,
            incoming_headers,
            Method::GET,
            &page_path,
            UpstreamResponseKind::Json,
            true,
        );
        let page = upstream_json(provider, incoming_headers, &page_meta, None).await?;
        merge_model_page(&mut combined, page)
            .map_err(|message| UpstreamFailure::decode(&page_meta, StatusCode::OK, message))?;
    }

    Err(UpstreamFailure::decode(
        &first_meta,
        StatusCode::OK,
        format!("Anthropic models pagination exceeded {MAX_MODEL_PAGES} pages"),
    ))
}

fn anthropic_models_page_path(cursor: &str) -> String {
    let mut url = Url::parse("http://localhost/models").expect("static models URL is valid");
    url.query_pairs_mut().append_pair("after_id", cursor);
    format!("models?{}", url.query().unwrap_or_default())
}

fn merge_model_page(combined: &mut Value, page: Value) -> std::result::Result<(), &'static str> {
    let page_data = page
        .get("data")
        .and_then(Value::as_array)
        .ok_or("Anthropic models page is missing a data array")?
        .clone();
    let combined_object = combined
        .as_object_mut()
        .ok_or("Anthropic models response is not an object")?;
    combined_object
        .get_mut("data")
        .and_then(Value::as_array_mut)
        .ok_or("Anthropic models response is missing a data array")?
        .extend(page_data);
    for key in ["first_id", "last_id", "has_more"] {
        if let Some(value) = page.get(key) {
            combined_object.insert(key.to_string(), value.clone());
        }
    }
    Ok(())
}

fn cache_upstream_models(provider: &ProviderState, upstream: Value, source: &str) -> Vec<String> {
    let ids = extract_model_ids(&upstream);
    let changed = provider
        .model_cache
        .replace_upstream_models(ids.clone(), upstream);

    if let Some(default_model) = ids.first() {
        if changed {
            info!(
                provider = %provider.name,
                upstream = %provider.config.base_url,
                upstream_models = ids.len(),
                default_upstream_model = %default_model,
                source = %source,
                "selected upstream model"
            );
        } else {
            debug!(
                provider = %provider.name,
                upstream = %provider.config.base_url,
                upstream_models = ids.len(),
                default_upstream_model = %default_model,
                source = %source,
                "upstream model cache unchanged"
            );
        }
    } else {
        warn!(
            provider = %provider.name,
            upstream = %provider.config.base_url,
            source = %source,
            "upstream /models returned no model ids; model auto mapping will retry on request"
        );
    }

    ids
}

/// Model discovery must not inherit the generation timeout. That budget is
/// sized for long completions, so a hung provider would otherwise hold the
/// whole catalog for minutes on a call agents make at startup.
const MODEL_DISCOVERY_TIMEOUT_SECS: u64 = 15;

fn model_discovery_timeout(provider: &ProviderState) -> Duration {
    Duration::from_secs(
        provider
            .config
            .timeout_secs
            .min(MODEL_DISCOVERY_TIMEOUT_SECS),
    )
}

fn models_cache_ttl(config: &Config) -> Option<Duration> {
    config.models.cache_ttl_secs.map(Duration::from_secs)
}

async fn ensure_default_upstream_model(
    provider: &ProviderState,
    incoming_headers: &HeaderMap,
) -> std::result::Result<String, Response> {
    if let Some(model) = provider.model_cache.first_upstream_model() {
        return Ok(model);
    }

    let _refresh_guard = provider.model_refresh.lock().await;
    if let Some(model) = provider.model_cache.first_upstream_model() {
        return Ok(model);
    }

    match refresh_upstream_models(provider, incoming_headers).await {
        Ok(ids) => ids.first().cloned().ok_or_else(no_upstream_models_response),
        Err(error) => Err((
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": {
                    "message": error.to_string(),
                    "type": "upstream_error"
                }
            })),
        )
            .into_response()),
    }
}

async fn rewrite_proxy_request_value(
    state: &AppState,
    incoming_headers: &HeaderMap,
    value: &mut Value,
) -> ResponseResult<ProviderState> {
    let provider = route_request(state, value)?;
    if state.config.model_routing == ModelRoutingMode::Passthrough {
        return Ok(provider);
    }
    let requested_model = value
        .get("model")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let provider_headers = provider_request_headers(state, incoming_headers, &provider);
    let preserve_explicit_model = requested_model
        .as_deref()
        .is_some_and(|model| preserve_integrated_model(&state.config, model));
    let default_model =
        if !preserve_explicit_model && request_needs_default_model(value, &state.config) {
            Some(ensure_default_upstream_model(&provider, &provider_headers).await?)
        } else {
            None
        };

    rewrite_request_value(value, &state.config, &provider, default_model.as_deref());
    if let Some(requested) = requested_model
        && value.get("model").and_then(Value::as_str) != Some(requested.as_str())
    {
        let kind = if state.config.model_aliases.contains_key(requested.as_str()) {
            RemapKind::Alias
        } else if namespaced_model_target(&state.config, &requested).is_some() {
            RemapKind::Namespace
        } else {
            RemapKind::Auto
        };
        proxy_stats().record_remap(kind);
    }
    Ok(provider)
}

fn preserve_integrated_model(config: &Config, model: &str) -> bool {
    config.integrated.enabled
        && model != INTEGRATED_AUTO_MODEL
        && !config.model_aliases.contains_key(model)
        && namespaced_model_target(config, model).is_none()
}

fn route_request(state: &AppState, value: &Value) -> ResponseResult<ProviderState> {
    let mut provider_names = HashSet::new();
    collect_routed_providers(value, &state.config, &mut provider_names);
    if provider_names.len() > 1 {
        let mut names = provider_names.into_iter().collect::<Vec<_>>();
        names.sort();
        return Err(Box::new((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": {
                    "message": format!("one request cannot target multiple providers: {}", names.join(", ")),
                    "type": "invalid_request_error"
                }
            })),
        )
            .into_response()));
    }

    let provider_name = provider_names
        .into_iter()
        .next()
        .unwrap_or_else(|| state.config.default_provider.clone());
    state
        .providers
        .get(&provider_name)
        .cloned()
        .ok_or_else(|| Box::new(unknown_provider_response(&provider_name)))
}

fn collect_routed_providers(value: &Value, config: &Config, providers: &mut HashSet<String>) {
    match value {
        Value::Object(map) => {
            if let Some(model) = map.get("model").and_then(Value::as_str) {
                if let Some(route) = config.model_aliases.get(model) {
                    providers.insert(route.provider(&config.default_provider).to_string());
                } else if let Some((provider, _)) = namespaced_model_target(config, model) {
                    providers.insert(provider.to_string());
                }
            }
            for child in map.values() {
                collect_routed_providers(child, config, providers);
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_routed_providers(child, config, providers);
            }
        }
        _ => {}
    }
}

fn namespaced_model_target<'a>(config: &Config, model: &'a str) -> Option<(&'a str, &'a str)> {
    let (provider, target) = model.split_once('/')?;
    if provider.is_empty() || target.is_empty() {
        return None;
    }
    (provider == config.default_provider || config.providers.contains_key(provider))
        .then_some((provider, target))
}

fn unknown_provider_response(provider: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": {
                "message": format!("unknown upstream provider: {provider}"),
                "type": "invalid_request_error"
            }
        })),
    )
        .into_response()
}

fn no_upstream_models_response() -> Response {
    (
        StatusCode::BAD_GATEWAY,
        Json(json!({
            "error": {
                "message": "upstream /models returned no model ids",
                "type": "upstream_error"
            }
        })),
    )
        .into_response()
}

fn extract_model_ids(value: &Value) -> Vec<String> {
    let values = value
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| value.as_array());
    let Some(values) = values else {
        return Vec::new();
    };

    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    for model in values {
        let Some(id) = model.get("id").and_then(Value::as_str) else {
            continue;
        };
        if seen.insert(id.to_string()) {
            ids.push(id.to_string());
        }
    }
    ids
}

async fn send_upstream_once(
    provider: &ProviderState,
    incoming_headers: &HeaderMap,
    meta: &UpstreamRequestMeta,
    body: Option<&Value>,
) -> std::result::Result<reqwest::Response, UpstreamFailure> {
    debug!(
        provider = %meta.provider,
        request_id = %meta.request_id,
        method = %meta.method,
        upstream_path = %meta.path,
        response_kind = meta.response_kind.as_str(),
        idempotent = meta.idempotent,
        "sending upstream request"
    );
    if meta.log_bodies
        && let Some(body) = body
    {
        trace!(
            provider = %meta.provider,
            request_id = %meta.request_id,
            method = %meta.method,
            upstream_path = %meta.path,
            response_kind = meta.response_kind.as_str(),
            body = %body,
            "upstream request body"
        );
    }

    let mut header_map = upstream_headers(&provider.config, incoming_headers, &meta.request_id);
    if let Some(oauth_provider) = provider.config.auth.oauth_provider() {
        match oauth::access_token(oauth_provider).await {
            Ok(token) => apply_oauth_headers(&mut header_map, &token, &meta.request_id),
            Err(error) => return Err(UpstreamFailure::oauth(meta, error.to_string())),
        }
    }

    let mut request = provider
        .client
        .request(meta.method.clone(), provider.upstream_url(&meta.path))
        .headers(header_map);
    if let Some(body) = body {
        request = request.json(body);
    }
    if meta.response_kind != UpstreamResponseKind::Stream {
        request = request.timeout(Duration::from_secs(provider.config.timeout_secs));
    }

    let send = request.send();
    let response = if meta.response_kind == UpstreamResponseKind::Stream {
        match tokio::time::timeout(Duration::from_secs(provider.config.timeout_secs), send).await {
            Ok(response) => response,
            Err(_) => {
                return Err(UpstreamFailure::timeout(
                    meta,
                    format!(
                        "upstream /{} did not return response headers within {}s",
                        meta.path, provider.config.timeout_secs
                    ),
                ));
            }
        }
    } else {
        send.await
    };

    response.map_err(|error| UpstreamFailure::from_reqwest(meta, error))
}

async fn upstream_json(
    provider: &ProviderState,
    incoming_headers: &HeaderMap,
    meta: &UpstreamRequestMeta,
    body: Option<&Value>,
) -> std::result::Result<Value, UpstreamFailure> {
    const MAX_RETRIES: usize = 2;
    let mut attempt = 1;

    loop {
        match upstream_json_once(provider, incoming_headers, meta, body).await {
            Ok(value) => return Ok(value),
            Err(error) if meta.idempotent && attempt <= MAX_RETRIES && error.is_retryable() => {
                let delay = retry_delay(attempt);
                error!(
                    provider = %error.provider,
                    request_id = %error.request_id,
                    method = %error.method,
                    upstream_path = %error.path,
                    response_kind = error.response_kind.as_str(),
                    error_kind = error.kind.as_str(),
                    upstream_status = error.upstream_status.map(|status| status.as_u16()),
                    attempt,
                    retry_after_ms = delay.as_millis(),
                    error = %error.message,
                    "retrying idempotent upstream request"
                );
                sleep(delay).await;
                attempt += 1;
            }
            Err(error) => {
                log_upstream_failure(&error, "final");
                return Err(error);
            }
        }
    }
}

async fn upstream_json_once(
    provider: &ProviderState,
    incoming_headers: &HeaderMap,
    meta: &UpstreamRequestMeta,
    body: Option<&Value>,
) -> std::result::Result<Value, UpstreamFailure> {
    let response = send_upstream_once(provider, incoming_headers, meta, body).await?;
    let status = response.status();
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    if !status.is_success() {
        let bytes = response
            .bytes()
            .await
            .map_err(|error| UpstreamFailure::from_reqwest(meta, error))?;
        if meta.log_bodies {
            error!(
                provider = %meta.provider,
                request_id = %meta.request_id,
                method = %meta.method,
                upstream_path = %meta.path,
                response_kind = meta.response_kind.as_str(),
                upstream_status = status.as_u16(),
                content_type = content_type.as_deref().unwrap_or(""),
                body_bytes = bytes.len(),
                body_prefix = %body_prefix(&bytes),
                body = %String::from_utf8_lossy(&bytes),
                "upstream returned non-success status"
            );
        } else {
            error!(
                provider = %meta.provider,
                request_id = %meta.request_id,
                method = %meta.method,
                upstream_path = %meta.path,
                response_kind = meta.response_kind.as_str(),
                upstream_status = status.as_u16(),
                content_type = content_type.as_deref().unwrap_or(""),
                body_bytes = bytes.len(),
                "upstream returned non-success status; body omitted during sensitive-data screening"
            );
        }
        return Err(UpstreamFailure::status_with_body(meta, status, &bytes));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|error| UpstreamFailure::from_reqwest(meta, error))?;
    if meta.log_bodies {
        trace!(
            provider = %meta.provider,
            request_id = %meta.request_id,
            method = %meta.method,
            upstream_path = %meta.path,
            upstream_status = status.as_u16(),
            content_type = content_type.as_deref().unwrap_or(""),
            body_bytes = bytes.len(),
            body = %String::from_utf8_lossy(&bytes),
            "upstream response body"
        );
    } else {
        trace!(
            provider = %meta.provider,
            request_id = %meta.request_id,
            method = %meta.method,
            upstream_path = %meta.path,
            upstream_status = status.as_u16(),
            content_type = content_type.as_deref().unwrap_or(""),
            body_bytes = bytes.len(),
            "upstream response body omitted during sensitive-data screening"
        );
    }
    match parse_upstream_json(&bytes) {
        Ok(body) => Ok(body),
        Err(error) => {
            let prefix = body_prefix(&bytes);
            if meta.log_bodies {
                error!(
                    provider = %meta.provider,
                    request_id = %meta.request_id,
                    method = %meta.method,
                    upstream_path = %meta.path,
                    response_kind = meta.response_kind.as_str(),
                    upstream_status = status.as_u16(),
                    content_type = content_type.as_deref().unwrap_or(""),
                    body_bytes = bytes.len(),
                    body_prefix = %prefix,
                    body = %String::from_utf8_lossy(&bytes),
                    error = %error,
                    "failed to decode upstream JSON response"
                );
            } else {
                error!(
                    provider = %meta.provider,
                    request_id = %meta.request_id,
                    method = %meta.method,
                    upstream_path = %meta.path,
                    response_kind = meta.response_kind.as_str(),
                    upstream_status = status.as_u16(),
                    content_type = content_type.as_deref().unwrap_or(""),
                    body_bytes = bytes.len(),
                    error = %error,
                    "failed to decode upstream JSON response; body omitted during sensitive-data screening"
                );
            }
            Err(UpstreamFailure::decode(meta, status, error))
        }
    }
}

fn retry_delay(attempt: usize) -> Duration {
    match attempt {
        1 => Duration::from_millis(100),
        _ => Duration::from_millis(300),
    }
}

fn log_upstream_failure(error: &UpstreamFailure, phase: &str) {
    error!(
        provider = %error.provider,
        request_id = %error.request_id,
        method = %error.method,
        upstream_path = %error.path,
        response_kind = error.response_kind.as_str(),
        error_kind = error.kind.as_str(),
        upstream_status = error.upstream_status.map(|status| status.as_u16()),
        phase,
        error = %error.message,
        "upstream request failed"
    );
}

fn log_upstream_status(meta: &UpstreamRequestMeta, status: StatusCode, phase: &str) {
    error!(
        provider = %meta.provider,
        request_id = %meta.request_id,
        method = %meta.method,
        upstream_path = %meta.path,
        response_kind = meta.response_kind.as_str(),
        upstream_status = status.as_u16(),
        phase,
        "upstream returned non-success status"
    );
}

async fn proxy_get(
    provider: &ProviderState,
    incoming_headers: &HeaderMap,
    upstream_path: &str,
) -> std::result::Result<Value, Response> {
    let meta = UpstreamRequestMeta::new(
        &provider.name,
        incoming_headers,
        Method::GET,
        upstream_path,
        UpstreamResponseKind::Json,
        true,
    );
    upstream_json(provider, incoming_headers, &meta, None)
        .await
        .map_err(|error| upstream_error_response(&error))
}

struct PreparedChatRequest {
    path: &'static str,
    body: Value,
    anthropic_wire: bool,
}

#[derive(Clone, Copy)]
enum ReasoningWire {
    ChatCompletions,
    Responses,
    AnthropicMessages,
}

fn apply_provider_reasoning_effort(
    provider: &ProviderState,
    wire: ReasoningWire,
    body: &mut Value,
) {
    let Some(effort) = provider.config.reasoning_effort else {
        return;
    };
    let Some(object) = body.as_object_mut() else {
        return;
    };
    let effort = Value::String(effort.as_str().to_string());
    match wire {
        ReasoningWire::ChatCompletions => {
            object.insert("reasoning_effort".to_string(), effort);
        }
        ReasoningWire::Responses => {
            let reasoning = object
                .entry("reasoning")
                .or_insert_with(|| Value::Object(Map::new()));
            if !reasoning.is_object() {
                *reasoning = Value::Object(Map::new());
            }
            reasoning
                .as_object_mut()
                .expect("reasoning was normalized to an object")
                .insert("effort".to_string(), effort);
        }
        ReasoningWire::AnthropicMessages => {
            let output_config = object
                .entry("output_config")
                .or_insert_with(|| Value::Object(Map::new()));
            if !output_config.is_object() {
                *output_config = Value::Object(Map::new());
            }
            output_config
                .as_object_mut()
                .expect("output_config was normalized to an object")
                .insert("effort".to_string(), effort);
        }
    }
}

fn prepare_provider_chat_request(
    provider: &ProviderState,
    chat_request: &Value,
) -> PreparedChatRequest {
    let mut chat_request = chat_request.clone();
    if provider.config.kind == ProviderKind::Copilot {
        sanitize_copilot_chat_request(&mut chat_request);
    }
    let anthropic_wire = provider.config.kind == ProviderKind::Anthropic
        || (provider.config.kind == ProviderKind::Copilot
            && is_claude_model(chat_request.get("model").and_then(Value::as_str)));
    if anthropic_wire {
        let mut body = chat_to_anthropic_request(&chat_request);
        apply_provider_reasoning_effort(provider, ReasoningWire::AnthropicMessages, &mut body);
        if provider.config.auth == ProviderAuth::ClaudeOauth {
            backend_adapters::ensure_claude_code_identity(&mut body);
        }
        PreparedChatRequest {
            path: if provider.config.kind == ProviderKind::Copilot {
                "v1/messages"
            } else {
                "messages"
            },
            body,
            anthropic_wire: true,
        }
    } else {
        apply_provider_reasoning_effort(
            provider,
            ReasoningWire::ChatCompletions,
            &mut chat_request,
        );
        PreparedChatRequest {
            path: "chat/completions",
            body: chat_request,
            anthropic_wire: false,
        }
    }
}

async fn upstream_chat_json(
    provider: &ProviderState,
    incoming_headers: &HeaderMap,
    chat_request: &Value,
) -> std::result::Result<Value, UpstreamFailure> {
    if provider.config.auth == ProviderAuth::CodexOauth {
        return codex_chat_completion(provider, incoming_headers, chat_request).await;
    }
    let prepared = prepare_provider_chat_request(provider, chat_request);
    let meta = UpstreamRequestMeta::new(
        &provider.name,
        incoming_headers,
        Method::POST,
        prepared.path,
        UpstreamResponseKind::Json,
        false,
    );
    let response = upstream_json(provider, incoming_headers, &meta, Some(&prepared.body)).await?;
    if prepared.anthropic_wire {
        Ok(anthropic_to_chat_response(
            response,
            chat_request.get("model").and_then(Value::as_str),
            unix_timestamp(),
        ))
    } else {
        Ok(response)
    }
}

async fn upstream_chat_stream(
    provider: &ProviderState,
    incoming_headers: &HeaderMap,
    chat_request: &Value,
) -> std::result::Result<(reqwest::Response, UpstreamRequestMeta), UpstreamFailure> {
    if provider.config.auth == ProviderAuth::CodexOauth {
        // The Codex backend is stream-only; fold its Responses stream into one
        // chat completion, then re-emit it as a chat SSE stream so the existing
        // client-protocol converters relay it unchanged.
        let meta = UpstreamRequestMeta::new(
            &provider.name,
            incoming_headers,
            Method::POST,
            "responses",
            UpstreamResponseKind::Stream,
            false,
        );
        let completion = codex_chat_completion(provider, incoming_headers, chat_request).await?;
        return Ok((chat_completion_as_chat_stream(&completion), meta));
    }
    let prepared = prepare_provider_chat_request(provider, chat_request);
    let meta = UpstreamRequestMeta::new(
        &provider.name,
        incoming_headers,
        Method::POST,
        prepared.path,
        UpstreamResponseKind::Stream,
        false,
    );
    let response =
        send_upstream_once(provider, incoming_headers, &meta, Some(&prepared.body)).await?;
    let response = if response.status().is_success() && prepared.anthropic_wire {
        anthropic_response_as_chat(
            response,
            chat_request
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string),
            Some(meta.clone()),
        )
    } else {
        response
    };
    Ok((response, meta))
}

/// Send an internal chat request to the Codex Responses backend and fold the
/// stream-only reply into a single Chat Completions object.
async fn codex_chat_completion(
    provider: &ProviderState,
    incoming_headers: &HeaderMap,
    chat_request: &Value,
) -> std::result::Result<Value, UpstreamFailure> {
    let mut responses_request = chat_to_codex_responses_request(chat_request);
    apply_provider_reasoning_effort(provider, ReasoningWire::Responses, &mut responses_request);
    let meta = UpstreamRequestMeta::new(
        &provider.name,
        incoming_headers,
        Method::POST,
        "responses",
        UpstreamResponseKind::Stream,
        false,
    );
    let response =
        send_upstream_once(provider, incoming_headers, &meta, Some(&responses_request)).await?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| UpstreamFailure::from_reqwest(&meta, error))?;
    if !status.is_success() {
        return Err(UpstreamFailure::status_with_body(&meta, status, &bytes));
    }
    let model = chat_request
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("codex");
    Ok(codex_sse_to_chat_completion(
        &String::from_utf8_lossy(&bytes),
        model,
    ))
}

/// Wrap a Chat Completions object as a minimal chat-completions SSE stream
/// (one content chunk, tool-call chunks, then `[DONE]`) inside a synthetic
/// `reqwest::Response`, so streaming clients receive it through the normal path.
fn chat_completion_as_chat_stream(completion: &Value) -> reqwest::Response {
    let model = completion
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("codex");
    let id = completion
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("chatcmpl-codex");
    let message = first_chat_choice_message(completion);
    let content = message
        .and_then(|message| message.get("content"))
        .map(text_from_value)
        .unwrap_or_default();
    let tool_calls = message
        .and_then(|message| message.get("tool_calls"))
        .cloned()
        .unwrap_or(Value::Null);
    let finish_reason = completion
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("finish_reason"))
        .and_then(Value::as_str)
        .unwrap_or("stop");
    let usage = completion.get("usage").cloned();

    let base = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": 0,
        "model": model,
    });
    let mut body = String::new();
    let mut push = |delta: Value, finish: Option<&str>| {
        let mut chunk = base.clone();
        chunk["choices"] = json!([{
            "index": 0,
            "delta": delta,
            "finish_reason": finish,
        }]);
        body.push_str("data: ");
        body.push_str(&chunk.to_string());
        body.push_str("\n\n");
    };
    push(json!({ "role": "assistant", "content": content }), None);
    if let Some(tool_calls) = tool_calls.as_array() {
        push(json!({ "tool_calls": tool_calls }), None);
    }
    // Final chunk carries the finish reason and (as an extra chunk) usage.
    push(json!({}), Some(finish_reason));
    if let Some(usage) = usage {
        let mut chunk = base.clone();
        chunk["choices"] = json!([]);
        chunk["usage"] = usage;
        body.push_str("data: ");
        body.push_str(&chunk.to_string());
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");

    let http_response = axum::http::Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/event-stream")
        .body(reqwest::Body::from(body))
        .expect("synthetic chat stream response is valid");
    reqwest::Response::from(http_response)
}

async fn proxy_chat(
    provider: &ProviderState,
    incoming_headers: &HeaderMap,
    chat_request: &Value,
) -> Response {
    if !request_is_streaming(chat_request) {
        return match upstream_chat_json(provider, incoming_headers, chat_request).await {
            Ok(response) => Json(response).into_response(),
            Err(error) => upstream_error_response(&error),
        };
    }

    match upstream_chat_stream(provider, incoming_headers, chat_request).await {
        Ok((response, meta)) => relay_response(&meta, response).await,
        Err(error) => {
            log_upstream_failure(&error, "send");
            upstream_error_response(&error)
        }
    }
}

/// One destination in a request's failover chain.
struct FailoverHop<'a> {
    provider: &'a ProviderState,
    /// Model to use on this hop; `None` keeps the request's own model.
    model: Option<&'a str>,
}

/// Which provider and model actually served a request. Failover means the
/// routed destination and the serving destination can differ, and statistics
/// must record what happened rather than what was intended.
pub(crate) struct ServedBy {
    provider: String,
    model: String,
    class: RouteClass,
}

impl ServedBy {
    /// The routed destination, for paths that never fail over: native
    /// passthrough wires are bound to the provider that owns them.
    fn routed(provider: &ProviderState, model: &str) -> Self {
        Self {
            provider: provider.name.clone(),
            model: model.to_string(),
            class: route_class_for_base_url(&provider.config.base_url),
        }
    }
}

/// The routed provider first, then its configured fallbacks. Fallbacks are
/// deliberately not followed recursively: one level keeps the worst-case
/// latency of a request bounded and makes a misconfigured cycle impossible.
fn failover_chain<'a>(state: &'a AppState, routed: &'a ProviderState) -> Vec<FailoverHop<'a>> {
    let mut hops = vec![FailoverHop {
        provider: routed,
        model: None,
    }];
    let mut seen = HashSet::from([routed.name.as_str()]);
    for entry in &routed.config.fallback {
        let (name, model) = match entry.split_once('/') {
            Some((name, model)) => (name, Some(model)),
            None => (entry.as_str(), None),
        };
        if !seen.insert(name) {
            continue;
        }
        if let Some(provider) = state.providers.get(name) {
            hops.push(FailoverHop { provider, model });
        }
    }
    hops
}

/// How a successful upstream chat result is rendered back to the caller. All
/// three client protocols share one upstream pipeline, so they can share one
/// failover driver too.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ChatWire {
    Chat,
    Responses,
    Anthropic,
}

impl ChatWire {
    fn render_json(self, upstream: Value, client_model: Option<String>) -> Response {
        match self {
            Self::Chat => Json(upstream).into_response(),
            Self::Responses => {
                Json(chat_completion_to_response(upstream, client_model)).into_response()
            }
            Self::Anthropic => {
                Json(chat_completion_to_anthropic_message(upstream, client_model)).into_response()
            }
        }
    }

    async fn render_stream(
        self,
        response: reqwest::Response,
        meta: UpstreamRequestMeta,
        client_model: Option<String>,
    ) -> Response {
        match self {
            Self::Chat => relay_response(&meta, response).await,
            Self::Responses => {
                stream_chat_as_responses(response, client_model, Some(meta)).into_response()
            }
            Self::Anthropic => {
                stream_chat_as_anthropic(response, client_model, Some(meta)).into_response()
            }
        }
    }
}

fn is_failover_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::REQUEST_TIMEOUT
            | StatusCode::TOO_MANY_REQUESTS
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

/// Send one chat request to one provider. When another hop is still available,
/// a failover-worthy streaming status becomes an error instead of being
/// relayed, because nothing has reached the caller yet and that is exactly the
/// window in which switching providers is still safe. With no hop left the
/// original relay behavior is preserved so the caller still sees the upstream's
/// own error.
async fn attempt_chat(
    provider: &ProviderState,
    provider_headers: &HeaderMap,
    chat_request: &Value,
    wire: ChatWire,
    client_model: Option<String>,
    can_failover: bool,
) -> std::result::Result<Response, UpstreamFailure> {
    if !request_is_streaming(chat_request) {
        let upstream = upstream_chat_json(provider, provider_headers, chat_request).await?;
        return Ok(wire.render_json(upstream, client_model));
    }

    let (response, meta) = upstream_chat_stream(provider, provider_headers, chat_request).await?;
    let status = response.status();
    if !status.is_success() {
        if can_failover && is_failover_status(status) {
            let bytes = response.bytes().await.unwrap_or_default();
            return Err(UpstreamFailure::status_with_body(&meta, status, &bytes));
        }
        return Ok(relay_response(&meta, response).await);
    }
    Ok(wire.render_stream(response, meta, client_model).await)
}

/// Run a chat request through its failover chain. Returns the response and the
/// destination that produced it.
async fn proxy_chat_failover(
    state: &AppState,
    routed: &ProviderState,
    incoming_headers: &HeaderMap,
    chat_request: &Value,
    wire: ChatWire,
    client_model: Option<String>,
) -> (Response, ServedBy) {
    let hops = failover_chain(state, routed);
    let last_index = hops.len().saturating_sub(1);
    let mut exhausted: Option<UpstreamFailure> = None;

    for (index, hop) in hops.iter().enumerate() {
        let provider = hop.provider;
        if !provider.breaker.allow() {
            proxy_stats().record_breaker_skip();
            warn!(
                provider = %provider.name,
                consecutive_failures = provider.breaker.consecutive_failures(),
                "skipping provider with an open circuit breaker"
            );
            continue;
        }

        let mut request = chat_request.clone();
        if index > 0 {
            // A different provider must not receive another provider's signed
            // reasoning artifacts, and may not recognize the original model.
            sanitize_cross_provider_chat_request(&mut request);
            if let Some(model) = hop.model
                && let Some(object) = request.as_object_mut()
            {
                object.insert("model".to_string(), Value::String(model.to_string()));
            }
            normalize_chat_completion_request(&mut request, &state.config);
        }
        let served = ServedBy {
            provider: provider.name.clone(),
            model: request_model(&request).to_string(),
            class: route_class_for_base_url(&provider.config.base_url),
        };
        let provider_headers = provider_request_headers(state, incoming_headers, provider);

        match attempt_chat(
            provider,
            &provider_headers,
            &request,
            wire,
            client_model.clone(),
            index < last_index,
        )
        .await
        {
            Ok(response) => {
                provider.breaker.record_success();
                if index > 0 {
                    proxy_stats().record_failover();
                    info!(
                        routed_provider = %routed.name,
                        served_provider = %provider.name,
                        served_model = %served.model,
                        hop = index,
                        "failed over to a fallback provider"
                    );
                }
                return (response, served);
            }
            Err(error) => {
                provider.breaker.record_failure();
                let can_continue = error.is_failover_safe() && index < last_index;
                log_upstream_failure(&error, if can_continue { "failover" } else { "send" });
                if !can_continue {
                    return (upstream_error_response(&error), served);
                }
                exhausted = Some(error);
            }
        }
    }

    // Every hop either failed a safe failure or was skipped by its breaker.
    let served = ServedBy {
        provider: routed.name.clone(),
        model: request_model(chat_request).to_string(),
        class: route_class_for_base_url(&routed.config.base_url),
    };
    match exhausted {
        Some(error) => (upstream_error_response(&error), served),
        None => (
            internal_error_response(format!(
                "every provider for {} has an open circuit breaker",
                routed.name
            )),
            served,
        ),
    }
}

async fn proxy_json(
    provider: &ProviderState,
    incoming_headers: &HeaderMap,
    upstream_path: &str,
    body: &Value,
) -> Response {
    let response_kind = if request_is_streaming(body) {
        UpstreamResponseKind::Stream
    } else {
        UpstreamResponseKind::Relay
    };
    let meta = UpstreamRequestMeta::new(
        &provider.name,
        incoming_headers,
        Method::POST,
        upstream_path,
        response_kind,
        false,
    );
    match send_upstream_once(provider, incoming_headers, &meta, Some(body)).await {
        Ok(response) => relay_response(&meta, response).await,
        Err(error) => {
            log_upstream_failure(&error, "send");
            upstream_error_response(&error)
        }
    }
}

async fn proxy_relay(
    provider: &ProviderState,
    incoming_headers: &HeaderMap,
    method: Method,
    upstream_path: &str,
    body: Option<&Value>,
    idempotent: bool,
) -> Response {
    let meta = UpstreamRequestMeta::new(
        &provider.name,
        incoming_headers,
        method,
        upstream_path,
        UpstreamResponseKind::Relay,
        idempotent,
    );
    match send_upstream_once(provider, incoming_headers, &meta, body).await {
        Ok(response) => relay_response(&meta, response).await,
        Err(error) => {
            log_upstream_failure(&error, "send");
            upstream_error_response(&error)
        }
    }
}

fn request_is_streaming(value: &Value) -> bool {
    value
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn request_model(value: &Value) -> &str {
    value
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
}

async fn proxy_responses_once_via_chat(
    provider: &ProviderState,
    incoming_headers: &HeaderMap,
    chat_request: &Value,
    client_model: Option<String>,
) -> Response {
    match upstream_chat_json(provider, incoming_headers, chat_request).await {
        Ok(upstream_body) => {
            Json(chat_completion_to_response(upstream_body, client_model)).into_response()
        }
        Err(error) => upstream_error_response(&error),
    }
}

async fn proxy_responses_stream_via_chat(
    provider: &ProviderState,
    incoming_headers: &HeaderMap,
    chat_request: &Value,
    client_model: Option<String>,
) -> Response {
    match upstream_chat_stream(provider, incoming_headers, chat_request).await {
        Ok((response, meta)) if response.status().is_success() => {
            stream_chat_as_responses(response, client_model, Some(meta)).into_response()
        }
        Ok((response, meta)) => relay_response(&meta, response).await,
        Err(error) => {
            log_upstream_failure(&error, "send");
            upstream_error_response(&error)
        }
    }
}

async fn proxy_anthropic_once_via_chat(
    provider: &ProviderState,
    incoming_headers: &HeaderMap,
    chat_request: &Value,
    client_model: Option<String>,
) -> Response {
    match upstream_chat_json(provider, incoming_headers, chat_request).await {
        Ok(upstream_body) => Json(chat_completion_to_anthropic_message(
            upstream_body,
            client_model,
        ))
        .into_response(),
        Err(error) => upstream_error_response(&error),
    }
}

async fn proxy_anthropic_stream_via_chat(
    provider: &ProviderState,
    incoming_headers: &HeaderMap,
    chat_request: &Value,
    client_model: Option<String>,
) -> Response {
    match upstream_chat_stream(provider, incoming_headers, chat_request).await {
        Ok((response, meta)) if response.status().is_success() => {
            stream_chat_as_anthropic(response, client_model, Some(meta)).into_response()
        }
        Ok((response, meta)) => relay_response(&meta, response).await,
        Err(error) => {
            log_upstream_failure(&error, "send");
            upstream_error_response(&error)
        }
    }
}

pub(crate) async fn proxy_integrated_chat(
    provider: &ProviderState,
    mut chat_request: Value,
    target: &interceptor::IntegratedTarget,
    protocol: InterceptorProtocol,
    client_model: Option<String>,
    client_streaming: bool,
    config: &Config,
) -> Response {
    let provider_override = target.reasoning_effort.map(|effort| {
        let mut provider_config = (*provider.config).clone();
        provider_config.reasoning_effort = Some(effort);
        ProviderState {
            config: Arc::new(provider_config),
            ..provider.clone()
        }
    });
    let provider = provider_override.as_ref().unwrap_or(provider);
    let target_model = target.model.as_str();
    let Some(object) = chat_request.as_object_mut() else {
        return launcher_error_response("integrated redirect request is not a JSON object");
    };
    object.insert("model".to_string(), Value::String(target_model.to_string()));
    object.insert("stream".to_string(), Value::Bool(client_streaming));
    sanitize_cross_provider_chat_request(&mut chat_request);
    normalize_chat_completion_request(&mut chat_request, config);

    // Cross-provider redirects intentionally have no caller headers. Provider
    // credentials and identity headers come only from its configured state.
    let headers = HeaderMap::new();
    let class = route_class_for_base_url(&provider.config.base_url);
    let started = Instant::now();
    let estimated_input_tokens = estimate_tokens(&chat_request) as u64;
    log_request_value(class, &provider.name, &chat_request);
    let model_started = Instant::now();
    let response = match protocol {
        InterceptorProtocol::ChatCompletions => proxy_chat(provider, &headers, &chat_request).await,
        InterceptorProtocol::Responses if client_streaming => {
            ensure_chat_stream_include_usage(&mut chat_request);
            proxy_responses_stream_via_chat(provider, &headers, &chat_request, client_model).await
        }
        InterceptorProtocol::Responses => {
            proxy_responses_once_via_chat(provider, &headers, &chat_request, client_model).await
        }
        InterceptorProtocol::AnthropicMessages if client_streaming => {
            ensure_chat_stream_include_usage(&mut chat_request);
            proxy_anthropic_stream_via_chat(provider, &headers, &chat_request, client_model).await
        }
        InterceptorProtocol::AnthropicMessages => {
            proxy_anthropic_once_via_chat(provider, &headers, &chat_request, client_model).await
        }
    };
    track_response(
        ResponseTracking::new(
            class,
            &provider.name,
            target_model,
            protocol.as_str(),
            started,
            model_started,
            estimated_input_tokens,
        )
        .with_pricing(request_pricing(config, &provider.name, target_model)),
        response,
    )
}

impl AppState {
    /// Whether this request path consults the triage provider at all. Proxy
    /// mode is prompt-free, so dormant sensitive screening never revives it.
    fn triage_enabled(&self) -> bool {
        self.config.routing_mode != RoutingMode::Proxy
            && (self.config.interceptor.enabled
                || self.config.integrated.enabled
                || self.sensitive_rejection_enabled())
    }

    fn sensitive_rejection_enabled(&self) -> bool {
        self.config.routing_mode != RoutingMode::Proxy && self.config.interceptor.reject_sensitive
    }

    fn default_provider(&self) -> &ProviderState {
        self.providers
            .get(&self.config.default_provider)
            .expect("validated default provider missing from runtime state")
    }

    fn with_launcher_session(&self, session: &LauncherSession) -> Result<Self> {
        validate_launcher_runtime(&self.config, session.mode())?;

        match session.mode() {
            LauncherMode::Proxy => self.with_local_proxy_session(session),
            LauncherMode::Intercept => self.with_native_interceptor_session(session),
            LauncherMode::Integrated => self.with_integrated_session(session),
        }
    }

    fn with_local_proxy_session(&self, session: &LauncherSession) -> Result<Self> {
        let mut config = (*self.config).clone();
        config.providers.clear();
        config.model_aliases.clear();
        config.model_routing = ModelRoutingMode::Remap;
        config.set_routing_mode(RoutingMode::Proxy);
        config.interceptor.reject_sensitive = false;
        config.responses.mode = ResponsesMode::ChatCompletions;
        config.anthropic_mode = AnthropicMode::ChatCompletions;

        let default_provider = self.default_provider().clone();
        let providers = HashMap::from([(config.default_provider.clone(), default_provider)]);
        Ok(Self {
            config: Arc::new(config),
            providers: Arc::new(providers),
            launcher_kind: None,
            launcher_mode: Some(session.mode()),
            forward_caller_credentials: false,
        })
    }

    fn with_integrated_session(&self, session: &LauncherSession) -> Result<Self> {
        let mut config = (*self.config).clone();
        config.set_routing_mode(RoutingMode::Integrated);
        Ok(Self {
            config: Arc::new(config),
            providers: self.providers.clone(),
            launcher_kind: None,
            launcher_mode: Some(session.mode()),
            forward_caller_credentials: false,
        })
    }

    fn with_native_interceptor_session(&self, session: &LauncherSession) -> Result<Self> {
        let mut config = (*self.config).clone();
        config.default_provider = LAUNCHER_EXTERNAL_PROVIDER.to_string();
        config.model_aliases.clear();
        config.model_routing = ModelRoutingMode::Passthrough;
        config.set_routing_mode(RoutingMode::Intercept);
        match session.kind() {
            LauncherKind::Codex => {
                config.responses.mode = ResponsesMode::Passthrough;
            }
            LauncherKind::Claude => {
                config.anthropic_mode = AnthropicMode::Passthrough;
            }
            LauncherKind::Copilot => {
                config.responses.mode = ResponsesMode::Passthrough;
                config.anthropic_mode = AnthropicMode::Passthrough;
            }
        }

        let template_provider = self.default_provider();
        let mut external_config = (*template_provider.config).clone();
        external_config.base_url = session
            .upstream_base_url()
            .trim_end_matches('/')
            .to_string();
        external_config.api_key = None;
        external_config.api_key_env = None;
        external_config.headers.clear();
        external_config.reasoning_effort = None;
        // The agent's own upstream is the only valid destination for a native
        // session, so it inherits neither fallbacks nor a breaker.
        external_config.fallback.clear();
        external_config.circuit_breaker_threshold = 0;
        external_config.kind = match session.kind() {
            LauncherKind::Codex => ProviderKind::OpenAi,
            LauncherKind::Claude => ProviderKind::Anthropic,
            LauncherKind::Copilot => ProviderKind::Copilot,
        };

        let mut providers = self.providers.as_ref().clone();
        providers.insert(
            LAUNCHER_EXTERNAL_PROVIDER.to_string(),
            ProviderState {
                name: LAUNCHER_EXTERNAL_PROVIDER.to_string(),
                config: Arc::new(external_config),
                client: template_provider.client.clone(),
                model_cache: Arc::new(ModelCache::default()),
                model_refresh: Arc::new(AsyncMutex::new(())),
                breaker: Arc::new(CircuitBreaker::new(0, Duration::from_secs(0))),
            },
        );

        Ok(Self {
            config: Arc::new(config),
            providers: Arc::new(providers),
            launcher_kind: Some(session.kind()),
            launcher_mode: Some(session.mode()),
            forward_caller_credentials: true,
        })
    }
}

impl ProviderState {
    pub(crate) fn breaker_status(&self) -> &'static str {
        self.breaker.status()
    }

    pub(crate) fn breaker_failures(&self) -> u32 {
        self.breaker.consecutive_failures()
    }

    fn upstream_url(&self, upstream_path: &str) -> String {
        format!(
            "{}/{}",
            self.config.base_url.trim_end_matches('/'),
            upstream_path.trim_start_matches('/')
        )
    }
}

fn upstream_headers(
    config: &UpstreamConfig,
    incoming_headers: &HeaderMap,
    request_id: &str,
) -> HeaderMap {
    let mut headers = HeaderMap::new();

    // Borrowed-OAuth providers own their credentials; the bearer token is
    // attached later (asynchronously) in send_upstream_once. Here we only make
    // sure caller credentials are not forwarded and set the static beta header.
    let oauth_mode = config.auth != ProviderAuth::None;
    let uses_proxy_credentials = config.api_key.is_some() || oauth_mode;
    if let Some(api_key) = &config.api_key {
        if let Ok(value) = HeaderValue::from_str(api_key) {
            match config.kind {
                ProviderKind::Anthropic => {
                    headers.insert(HeaderName::from_static("x-api-key"), value);
                }
                _ => {
                    if let Ok(value) = HeaderValue::from_str(&format!("Bearer {api_key}")) {
                        headers.insert(AUTHORIZATION, value);
                    }
                }
            }
        }
    } else if !oauth_mode && let Some(value) = incoming_headers.get(AUTHORIZATION) {
        headers.insert(AUTHORIZATION, value.clone());
    }

    if !uses_proxy_credentials {
        for (name, value) in incoming_headers {
            if name != AUTHORIZATION && should_forward_incoming_header(name) {
                headers.insert(name.clone(), value.clone());
            }
        }
    }

    if config.auth == ProviderAuth::ClaudeOauth {
        // Anthropic accepts subscription OAuth only when the request presents
        // as Claude Code: the oauth beta flag plus the Claude Code beta.
        headers.insert(
            HeaderName::from_static("anthropic-beta"),
            HeaderValue::from_static("oauth-2025-04-20,claude-code-20250219"),
        );
    }

    for (name, value) in &config.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }

    if matches!(config.kind, ProviderKind::Anthropic | ProviderKind::Copilot) {
        headers
            .entry(HeaderName::from_static("anthropic-version"))
            .or_insert(HeaderValue::from_static("2023-06-01"));
    }
    if config.kind == ProviderKind::Copilot {
        for (name, value) in [
            ("copilot-integration-id", "vscode-chat"),
            ("editor-version", "vscode/1.107.1"),
            ("editor-plugin-version", "copilot-chat/0.26.7"),
            ("openai-intent", "conversation-panel"),
            ("x-github-api-version", "2022-11-28"),
        ] {
            headers
                .entry(HeaderName::from_static(name))
                .or_insert(HeaderValue::from_static(value));
        }
    }

    if let Ok(value) = HeaderValue::from_str(request_id) {
        headers.insert(HeaderName::from_static("x-request-id"), value);
    }

    headers
}

/// Attach a borrowed-OAuth access token and its identity headers. The bearer
/// token replaces any static credential; Codex additionally needs the account
/// binding and CLI originator so the ChatGPT backend accepts the request.
fn apply_oauth_headers(headers: &mut HeaderMap, token: &oauth::OAuthToken, request_id: &str) {
    if let Ok(value) = HeaderValue::from_str(&format!("Bearer {}", token.access_token)) {
        headers.insert(AUTHORIZATION, value);
    }
    headers.remove(HeaderName::from_static("x-api-key"));
    if let Some(account_id) = &token.account_id {
        if let Ok(value) = HeaderValue::from_str(account_id) {
            headers.insert(HeaderName::from_static("chatgpt-account-id"), value);
        }
        headers.insert(
            HeaderName::from_static("originator"),
            HeaderValue::from_static("codex_cli_rs"),
        );
        if let Ok(value) = HeaderValue::from_str(request_id) {
            headers.insert(HeaderName::from_static("session_id"), value);
        }
    }
}

fn should_forward_incoming_header(name: &HeaderName) -> bool {
    let name = name.as_str();
    matches!(
        name,
        "accept"
            | "anthropic-beta"
            | "anthropic-version"
            | "chatgpt-account-id"
            | "copilot-integration-id"
            | "copilot-session-token"
            | "openai-intent"
            | "originator"
            | "session-id"
            | "thread-id"
            | "user-agent"
            | "version"
            | "x-agent-task-id"
            | "x-api-key"
            | "x-app"
            | "x-client-machine-id"
            | "x-client-request-id"
            | "x-github-api-version"
            | "x-initiator"
            | "x-interaction-id"
            | "x-interaction-type"
            | "x-parent-agent-id"
    ) || is_provider_metadata_header(name)
        || name.starts_with("x-stainless-")
}

fn is_provider_metadata_header(name: &str) -> bool {
    [
        "anthropic-",
        "copilot-",
        "openai-",
        "x-anthropic-",
        "x-codex-",
        "x-copilot-",
        "x-github-",
        "x-oai-",
        "x-openai-",
        "x-responsesapi-",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
}

async fn relay_response(meta: &UpstreamRequestMeta, response: reqwest::Response) -> Response {
    let status = response.status();
    if !status.is_success() {
        log_upstream_status(meta, status, "relay_headers");
    }
    let response_headers = response.headers().clone();
    let content_type = response_headers.get(CONTENT_TYPE).cloned();
    let is_sse = content_type
        .as_ref()
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_event_stream_content_type);
    let stream_meta = meta.clone();
    let mut chunk_index = 0_u64;
    let stream = response.bytes_stream().map(move |chunk| match chunk {
        Ok(chunk) => {
            chunk_index = chunk_index.saturating_add(1);
            trace!(
                provider = %stream_meta.provider,
                request_id = %stream_meta.request_id,
                method = %stream_meta.method,
                upstream_path = %stream_meta.path,
                upstream_status = status.as_u16(),
                chunk_index,
                body_bytes = chunk.len(),
                body = %String::from_utf8_lossy(&chunk),
                "relayed upstream response body"
            );
            if !status.is_success() {
                error!(
                    provider = %stream_meta.provider,
                    request_id = %stream_meta.request_id,
                    method = %stream_meta.method,
                    upstream_path = %stream_meta.path,
                    upstream_status = status.as_u16(),
                    chunk_index,
                    body = %String::from_utf8_lossy(&chunk),
                    "relayed upstream error response body"
                );
            }
            Ok(chunk)
        }
        Err(error) => {
            let message = error.to_string();
            let failure = UpstreamFailure::from_reqwest(&stream_meta, error);
            log_upstream_failure(&failure, "relay_stream");
            if is_sse {
                Ok(generic_stream_error_event(&failure))
            } else {
                Err(std::io::Error::other(message))
            }
        }
    });

    let mut response = Response::builder()
        .status(status)
        .body(Body::from_stream(stream))
        .unwrap_or_else(|error| internal_error_response(error.to_string()));
    for (name, value) in &response_headers {
        if should_forward_response_header(name) {
            response.headers_mut().insert(name.clone(), value.clone());
        }
    }
    if !response.headers().contains_key("x-request-id")
        && let Ok(value) = HeaderValue::from_str(&meta.request_id)
    {
        response
            .headers_mut()
            .insert(HeaderName::from_static("x-request-id"), value);
    }
    response
}

fn should_forward_response_header(name: &HeaderName) -> bool {
    let name = name.as_str();
    matches!(
        name,
        "cache-control"
            | "cf-ray"
            | "content-type"
            | "location"
            | "openai-processing-ms"
            | "openai-version"
            | "request-id"
            | "retry-after"
            | "www-authenticate"
            | "x-reasoning-included"
            | "x-should-retry"
    ) || is_provider_metadata_header(name)
        || name.starts_with("x-ratelimit-")
        || name.starts_with("x-request-")
}

fn upstream_error_response(error: &UpstreamFailure) -> Response {
    stats::error_log().record(
        &error.provider,
        &error.path,
        match error.upstream_status {
            Some(status) => format!("{} — HTTP {status}", error.message),
            None => error.message.clone(),
        },
    );
    let status = match error.kind {
        UpstreamErrorKind::Timeout => StatusCode::GATEWAY_TIMEOUT,
        UpstreamErrorKind::Status => error
            .upstream_status
            .filter(|status| status.is_client_error() || status.is_server_error())
            .unwrap_or(StatusCode::BAD_GATEWAY),
        _ => StatusCode::BAD_GATEWAY,
    };
    let mut response = (
        status,
        Json(json!({
            "error": {
                "message": error.message,
                "type": "upstream_error",
                "kind": error.kind.as_str(),
                "upstream_status": error.upstream_status.map(|status| status.as_u16()),
                "request_id": error.request_id,
                "proxy_provider": error.provider,
                "upstream_path": error.path
            }
        })),
    )
        .into_response();
    if let Ok(value) = HeaderValue::from_str(&error.request_id) {
        response
            .headers_mut()
            .insert(HeaderName::from_static("x-request-id"), value);
    }
    response
}

fn internal_error_response(message: String) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "error": {
                "message": message,
                "type": "internal_error"
            }
        })),
    )
        .into_response()
}

fn request_needs_default_model(value: &Value, config: &Config) -> bool {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(model)) = map.get("model")
                && !config.model_aliases.contains_key(model.as_str())
                && namespaced_model_target(config, model).is_none()
            {
                return true;
            }

            map.values()
                .any(|child| request_needs_default_model(child, config))
        }
        Value::Array(values) => values
            .iter()
            .any(|child| request_needs_default_model(child, config)),
        _ => false,
    }
}

fn rewrite_request_value(
    value: &mut Value,
    config: &Config,
    provider: &ProviderState,
    default_upstream_model: Option<&str>,
) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(model)) = map.get_mut("model") {
                let requested = model.clone();
                let mapping = config
                    .model_aliases
                    .get(requested.as_str())
                    .map(|route| (route.model(), ModelMappingSource::Alias))
                    .or_else(|| {
                        namespaced_model_target(config, &requested).and_then(
                            |(route_provider, target)| {
                                (route_provider == provider.name)
                                    .then_some((target, ModelMappingSource::Namespace))
                            },
                        )
                    })
                    .or_else(|| {
                        default_upstream_model.map(|target| (target, ModelMappingSource::Auto))
                    });

                if let Some((target, source)) = mapping {
                    provider.model_cache.log_first_seen_model(
                        &provider.name,
                        &requested,
                        target,
                        source,
                    );
                    *model = target.to_string();
                }
            }
            if let Some(Value::String(role)) = map.get_mut("role")
                && let Some(mapped) = config.role_map.get(role.as_str())
            {
                *role = mapped.clone();
            }

            for child in map.values_mut() {
                rewrite_request_value(child, config, provider, default_upstream_model);
            }
        }
        Value::Array(values) => {
            for child in values {
                rewrite_request_value(child, config, provider, default_upstream_model);
            }
        }
        _ => {}
    }
}

fn rewrite_request_roles(value: &mut Value, role_map: &HashMap<String, String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(role)) = map.get_mut("role")
                && let Some(mapped) = role_map.get(role.as_str())
            {
                *role = mapped.clone();
            }
            for child in map.values_mut() {
                rewrite_request_roles(child, role_map);
            }
        }
        Value::Array(values) => {
            for child in values {
                rewrite_request_roles(child, role_map);
            }
        }
        _ => {}
    }
}

fn normalize_chat_completion_request(request: &mut Value, config: &Config) {
    normalize_chat_request_system_messages(request);
    normalize_chat_request_stream_options(request);
    normalize_chat_request_thinking(request, config);
}

fn normalize_chat_request_system_messages(request: &mut Value) {
    let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };

    let system_count = messages
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("system"))
        .count();
    if system_count == 0
        || (system_count == 1
            && messages
                .first()
                .and_then(|message| message.get("role"))
                .and_then(Value::as_str)
                == Some("system"))
    {
        return;
    }

    let original_messages = std::mem::take(messages);
    let mut system_parts = Vec::new();
    let mut non_system_messages = Vec::new();

    for message in original_messages {
        if message.get("role").and_then(Value::as_str) == Some("system") {
            if let Some(content) = message.get("content") {
                let text = text_from_value(content);
                if !text.is_empty() {
                    system_parts.push(text);
                }
            }
        } else {
            non_system_messages.push(message);
        }
    }

    messages.reserve(1 + non_system_messages.len());
    messages.push(json!({
        "role": "system",
        "content": system_parts.join("\n\n")
    }));
    messages.extend(non_system_messages);
}

fn normalize_chat_request_stream_options(request: &mut Value) {
    if request_is_streaming(request) {
        return;
    }
    if let Some(object) = request.as_object_mut() {
        object.remove("stream_options");
    }
}

fn normalize_chat_request_thinking(request: &mut Value, config: &Config) {
    if !config.chat.disable_thinking {
        return;
    }
    let Some(object) = request.as_object_mut() else {
        return;
    };

    let entry = object
        .entry("chat_template_kwargs".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(kwargs) = entry.as_object_mut() {
        kwargs
            .entry("enable_thinking".to_string())
            .or_insert(Value::Bool(false));
    }
}

fn append_provider_models(
    data: &mut Vec<Value>,
    upstream: Value,
    provider: &str,
    include_raw: bool,
) {
    let models = upstream
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| upstream.as_array());
    let Some(models) = models else {
        return;
    };
    for model in models {
        let Some(id) = model.get("id").and_then(Value::as_str) else {
            continue;
        };
        if include_raw {
            let mut raw_model = model.clone();
            if let Some(object) = raw_model.as_object_mut() {
                object.insert(
                    "proxy_provider".to_string(),
                    Value::String(provider.to_string()),
                );
            }
            data.push(raw_model);
        }

        let mut namespaced = model.clone();
        if let Some(object) = namespaced.as_object_mut() {
            object.insert("id".to_string(), Value::String(format!("{provider}/{id}")));
            object.insert("proxy_target".to_string(), Value::String(id.to_string()));
            object.insert(
                "proxy_provider".to_string(),
                Value::String(provider.to_string()),
            );
            object.insert(
                "proxy_route".to_string(),
                Value::String("namespaced".to_string()),
            );
        }
        data.push(namespaced);
    }
}

fn rewrite_models_response(mut upstream: Value, config: &Config) -> Value {
    let expose_aliases = config.models.expose_aliases && !config.model_aliases.is_empty();
    // Targets stay configured across mode changes, so the catalog keys off
    // whether integrated routing is actually live, not merely configured.
    let expose_integrated = config.integrated.enabled && !config.integrated.targets.is_empty();
    if !expose_aliases && !expose_integrated {
        return upstream;
    }

    let mut data = if config.models.expose_upstream {
        upstream
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let existing_ids: HashSet<String> = data
        .iter()
        .filter_map(|model| {
            model
                .get("id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .collect();

    if expose_aliases {
        append_alias_models(&mut data, config, &existing_ids);
    }
    if expose_integrated {
        append_integrated_models(&mut data, config);
    }

    if !upstream.is_object() {
        upstream = json!({ "object": "list" });
    }
    let object = upstream.as_object_mut().expect("upstream is object");
    object.insert("object".to_string(), Value::String("list".to_string()));
    object.insert("data".to_string(), Value::Array(data));
    upstream
}

fn append_alias_models(data: &mut Vec<Value>, config: &Config, existing_ids: &HashSet<String>) {
    for (alias, route) in &config.model_aliases {
        if existing_ids.contains(alias) {
            continue;
        }
        let target = route.model();
        let provider = route.provider(&config.default_provider);
        let template = data
            .iter()
            .find(|model| {
                model.get("id").and_then(Value::as_str) == Some(target)
                    && model.get("proxy_provider").and_then(Value::as_str) == Some(provider)
            })
            .cloned();
        let mut alias_model = template.unwrap_or_else(|| {
            json!({
                "id": alias,
                "object": "model",
                "created": 0,
                "owned_by": "llmproxy"
            })
        });
        if let Some(object) = alias_model.as_object_mut() {
            object.insert("id".to_string(), Value::String(alias.clone()));
            object.insert(
                "proxy_target".to_string(),
                Value::String(target.to_string()),
            );
            object.insert(
                "proxy_provider".to_string(),
                Value::String(provider.to_string()),
            );
        }
        data.push(alias_model);
    }
}

/// Advertise the integrated routing sentinel and ensure every configured
/// target remains selectable through its explicit `provider/model` id.
fn append_integrated_models(data: &mut Vec<Value>, config: &Config) {
    let mut listed = data
        .iter()
        .filter_map(|model| model.get("id").and_then(Value::as_str).map(str::to_string))
        .collect::<HashSet<_>>();
    if let Some(model) = data
        .iter_mut()
        .find(|model| model.get("id").and_then(Value::as_str) == Some(INTEGRATED_AUTO_MODEL))
    {
        if let Some(object) = model.as_object_mut() {
            object.insert(
                "proxy_route".to_string(),
                Value::String("integrated".to_string()),
            );
            object.insert(
                "proxy_target".to_string(),
                Value::String("dynamic provider/model routing".to_string()),
            );
        }
    } else {
        data.push(integrated_auto_model());
    }

    for target in config.integrated.targets.values() {
        if target.provider != config.default_provider
            && !config.providers.contains_key(&target.provider)
        {
            continue;
        }
        let namespaced = format!("{}/{}", target.provider, target.model);
        if listed.insert(namespaced.clone()) {
            data.push(json!({
                "id": namespaced,
                "object": "model",
                "created": 0,
                "owned_by": "llmproxy",
                "proxy_provider": target.provider,
                "proxy_target": target.model,
                "proxy_route": "namespaced",
            }));
        }
    }
}

fn integrated_auto_model() -> Value {
    json!({
        "id": INTEGRATED_AUTO_MODEL,
        "object": "model",
        "type": "model",
        "display_name": INTEGRATED_AUTO_MODEL,
        "created": 0,
        "owned_by": "llmproxy",
        "proxy_route": "integrated",
        "proxy_target": "dynamic provider/model routing",
    })
}

fn model_object(model: &str, proxy_target: Option<&str>, provider: Option<&str>) -> Value {
    let mut object = json!({
        "id": model,
        "object": "model",
        "type": "model",
        "display_name": model,
        "created": 0,
        "owned_by": "llmproxy"
    });

    if let Some(proxy_target) = proxy_target {
        object["proxy_target"] = Value::String(proxy_target.to_string());
        object["root"] = Value::String(proxy_target.to_string());
    }
    if let Some(provider) = provider {
        object["proxy_provider"] = Value::String(provider.to_string());
    }

    object
}

fn anthropic_message_to_chat_request(anthropic_request: &Value) -> Value {
    let mut chat = Map::new();
    let Some(source) = anthropic_request.as_object() else {
        return json!({ "messages": [] });
    };

    copy_keys(
        source,
        &mut chat,
        &[
            "model",
            "max_tokens",
            "temperature",
            "top_p",
            "stream",
            "user",
        ],
    );

    if let Some(stop_sequences) = source.get("stop_sequences") {
        chat.insert("stop".to_string(), stop_sequences.clone());
    }

    if let Some(tools) = source.get("tools").and_then(Value::as_array) {
        let converted_tools: Vec<Value> = tools
            .iter()
            .filter_map(anthropic_tool_to_openai_tool)
            .collect();
        if !converted_tools.is_empty() {
            chat.insert("tools".to_string(), Value::Array(converted_tools));
        }
    }

    if let Some(tool_choice) = source
        .get("tool_choice")
        .and_then(anthropic_tool_choice_to_openai)
    {
        chat.insert("tool_choice".to_string(), tool_choice);
    }

    let mut messages = Vec::new();
    if let Some(system) = source.get("system") {
        messages.push(json!({
            "role": "system",
            "content": anthropic_content_to_text(system)
        }));
    }

    if let Some(Value::Array(source_messages)) = source.get("messages") {
        for message in source_messages {
            append_anthropic_message_to_chat(message, &mut messages);
        }
    }

    chat.insert("messages".to_string(), Value::Array(messages));
    let mut chat_request = Value::Object(chat);
    normalize_chat_request_system_messages(&mut chat_request);
    chat_request
}

fn anthropic_tool_to_openai_tool(tool: &Value) -> Option<Value> {
    let object = tool.as_object()?;
    let name = object.get("name")?.as_str()?;
    let description = object
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let parameters = object
        .get("input_schema")
        .cloned()
        .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));

    Some(json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": parameters
        }
    }))
}

fn anthropic_tool_choice_to_openai(tool_choice: &Value) -> Option<Value> {
    let choice_type = tool_choice.get("type").and_then(Value::as_str)?;
    match choice_type {
        "auto" => Some(Value::String("auto".to_string())),
        "any" => Some(Value::String("required".to_string())),
        "none" => Some(Value::String("none".to_string())),
        "tool" => tool_choice.get("name").and_then(Value::as_str).map(|name| {
            json!({
                "type": "function",
                "function": { "name": name }
            })
        }),
        _ => None,
    }
}

fn append_anthropic_message_to_chat(message: &Value, messages: &mut Vec<Value>) {
    let Some(object) = message.as_object() else {
        return;
    };
    let role = object
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("user")
        .to_string();
    let source_content = object.get("content").unwrap_or(&Value::Null);

    if role == "user"
        && let Some(parts) = source_content.as_array()
        && parts
            .iter()
            .any(|part| part.get("type").and_then(Value::as_str) == Some("tool_result"))
    {
        let mut pending = Vec::new();
        for part in parts {
            if part.get("type").and_then(Value::as_str) == Some("tool_result") {
                push_chat_content_message(messages, "user", &mut pending);
                let call_id = part
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let content = part
                    .get("content")
                    .map(anthropic_content_to_text)
                    .unwrap_or_default();
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": content
                }));
            } else if let Some(content) = anthropic_content_part_to_chat_part(part) {
                pending.push(content);
            }
        }
        push_chat_content_message(messages, "user", &mut pending);
        return;
    }

    let content = anthropic_content_to_chat_content(source_content);

    let mut converted = Map::from_iter([
        ("role".to_string(), Value::String(role.clone())),
        ("content".to_string(), content),
    ]);

    if role == "assistant" {
        let tool_calls = object
            .get("content")
            .map(anthropic_content_to_openai_tool_calls)
            .unwrap_or_default();
        if !tool_calls.is_empty() {
            converted.insert("tool_calls".to_string(), Value::Array(tool_calls));
        }
    }

    messages.push(Value::Object(converted));
}

fn anthropic_content_to_chat_content(content: &Value) -> Value {
    match content {
        Value::String(text) => Value::String(text.clone()),
        Value::Array(parts) => chat_content_parts_to_value(
            parts
                .iter()
                .filter_map(anthropic_content_part_to_chat_part)
                .collect(),
        ),
        Value::Object(_) => anthropic_content_part_to_chat_part(content)
            .map(|part| chat_content_parts_to_value(vec![part]))
            .unwrap_or_else(|| Value::String(content.to_string())),
        Value::Null => Value::String(String::new()),
        other => Value::String(other.to_string()),
    }
}

fn anthropic_content_part_to_chat_part(part: &Value) -> Option<Value> {
    match part {
        Value::String(text) => Some(json!({ "type": "text", "text": text })),
        Value::Object(object) => match object.get("type").and_then(Value::as_str) {
            Some("text") => object
                .get("text")
                .and_then(Value::as_str)
                .map(|text| json!({ "type": "text", "text": text })),
            Some("image") => anthropic_image_to_chat_part(object),
            Some("tool_use" | "thinking" | "redacted_thinking") => None,
            Some("tool_result") => anthropic_content_part_to_text(part)
                .map(|text| json!({ "type": "text", "text": text })),
            _ => object
                .get("text")
                .and_then(Value::as_str)
                .map(|text| json!({ "type": "text", "text": text })),
        },
        Value::Null => None,
        other => Some(json!({ "type": "text", "text": other.to_string() })),
    }
}

fn anthropic_image_to_chat_part(object: &Map<String, Value>) -> Option<Value> {
    let source = object.get("source")?;
    let source_type = source.get("type").and_then(Value::as_str);
    let url = match source_type {
        Some("base64") => {
            let media_type = source.get("media_type").and_then(Value::as_str)?;
            let data = source.get("data").and_then(Value::as_str)?;
            format!("data:{media_type};base64,{data}")
        }
        Some("url") => source.get("url").and_then(Value::as_str)?.to_string(),
        _ => source.get("url").and_then(Value::as_str)?.to_string(),
    };
    Some(json!({ "type": "image_url", "image_url": { "url": url } }))
}

fn chat_content_parts_to_value(parts: Vec<Value>) -> Value {
    if parts
        .iter()
        .all(|part| part.get("type").and_then(Value::as_str) == Some("text"))
    {
        return Value::String(
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    Value::Array(parts)
}

fn push_chat_content_message(messages: &mut Vec<Value>, role: &str, parts: &mut Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    messages.push(json!({
        "role": role,
        "content": chat_content_parts_to_value(std::mem::take(parts))
    }));
}

fn anthropic_content_to_openai_tool_calls(content: &Value) -> Vec<Value> {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter_map(|part| {
            let id = part.get("id").and_then(Value::as_str)?;
            let name = part.get("name").and_then(Value::as_str)?;
            let input = part.get("input").cloned().unwrap_or_else(|| json!({}));
            Some(json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": input.to_string()
                }
            }))
        })
        .collect()
}

fn anthropic_content_to_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(anthropic_content_part_to_text)
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) => {
            anthropic_content_part_to_text(content).unwrap_or_else(|| content.to_string())
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn anthropic_content_part_to_text(part: &Value) -> Option<String> {
    match part {
        Value::String(text) => Some(text.clone()),
        Value::Object(object) => match object.get("type").and_then(Value::as_str) {
            Some("text") => object
                .get("text")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            Some("tool_result") => {
                let id = object
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let content = object
                    .get("content")
                    .map(anthropic_content_to_text)
                    .unwrap_or_default();
                Some(format!("[tool_result {id}]\n{content}"))
            }
            Some("image") => Some("[image omitted]".to_string()),
            Some("tool_use") => None,
            _ => object
                .get("text")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        },
        _ => Some(text_from_value(part)),
    }
}

fn chat_completion_to_anthropic_message(upstream: Value, client_model: Option<String>) -> Value {
    let id = upstream
        .get("id")
        .and_then(Value::as_str)
        .map(|id| format!("msg_{id}"))
        .unwrap_or_else(|| format!("msg_{}", unix_timestamp()));
    let model = client_model
        .or_else(|| {
            upstream
                .get("model")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| "unknown".to_string());
    let choice = upstream
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let message = choice.get("message").cloned().unwrap_or_else(|| json!({}));
    let mut content = Vec::new();

    if let Some(text) = message.get("content").map(text_from_value)
        && !text.is_empty()
    {
        content.push(json!({ "type": "text", "text": text }));
    }

    let tool_use_blocks = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(openai_tool_call_to_anthropic_content)
        .collect::<Vec<_>>();
    content.extend(tool_use_blocks);

    if content.is_empty() {
        content.push(json!({ "type": "text", "text": "" }));
    }

    let has_tool_use = content
        .iter()
        .any(|part| part.get("type").and_then(Value::as_str) == Some("tool_use"));
    let stop_reason = if has_tool_use {
        "tool_use"
    } else {
        match choice.get("finish_reason").and_then(Value::as_str) {
            Some("length") => "max_tokens",
            Some("tool_calls") => "tool_use",
            _ => "end_turn",
        }
    };

    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": anthropic_usage(upstream.get("usage"))
    })
}

fn openai_tool_call_to_anthropic_content(tool_call: &Value) -> Option<Value> {
    let id = tool_call.get("id").and_then(Value::as_str)?;
    let function = tool_call.get("function")?;
    let name = function.get("name").and_then(Value::as_str)?;
    let input = function
        .get("arguments")
        .map(arguments_to_string)
        .map(|arguments| normalize_tool_arguments(&arguments))
        .and_then(|arguments| serde_json::from_str::<Value>(&arguments).ok())
        .unwrap_or_else(|| json!({}));

    Some(json!({
        "type": "tool_use",
        "id": id,
        "name": name,
        "input": input
    }))
}

fn anthropic_usage(usage: Option<&Value>) -> Value {
    let input_tokens = usage
        .and_then(|usage| usage.get("prompt_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .and_then(|usage| usage.get("completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens
    })
}

fn estimate_tokens(value: &Value) -> usize {
    let text = text_from_value(value);
    (text.chars().count() / 4).max(1)
}

fn responses_to_chat_request(response_request: &Value) -> Value {
    let mut chat = Map::new();
    let Some(source) = response_request.as_object() else {
        return json!({ "messages": [] });
    };

    copy_keys(
        source,
        &mut chat,
        &[
            "model",
            "temperature",
            "top_p",
            "presence_penalty",
            "frequency_penalty",
            "seed",
            "stream",
            "stop",
            "n",
            "logprobs",
            "top_logprobs",
            "user",
        ],
    );

    let has_tools = if let Some(tools) = source.get("tools").and_then(Value::as_array) {
        let converted_tools = tools
            .iter()
            .filter_map(responses_tool_to_chat_tool)
            .collect::<Vec<_>>();
        if converted_tools.is_empty() {
            false
        } else {
            chat.insert("tools".to_string(), Value::Array(converted_tools));
            true
        }
    } else {
        false
    };

    if has_tools {
        if let Some(tool_choice) = source
            .get("tool_choice")
            .and_then(responses_tool_choice_to_chat_tool_choice)
        {
            chat.insert("tool_choice".to_string(), tool_choice);
        }
        if let Some(parallel_tool_calls) = source.get("parallel_tool_calls") {
            chat.insert(
                "parallel_tool_calls".to_string(),
                parallel_tool_calls.clone(),
            );
        }
    }

    if let Some(value) = source
        .get("max_tokens")
        .or_else(|| source.get("max_output_tokens"))
    {
        chat.insert("max_tokens".to_string(), value.clone());
    }

    let mut messages = Vec::new();
    if let Some(instructions) = source.get("instructions") {
        messages.push(json!({
            "role": "system",
            "content": text_from_value(instructions)
        }));
    }
    if let Some(input) = source.get("input") {
        append_response_input_as_messages(input, &mut messages);
    }
    if let Some(Value::Array(extra_messages)) = source.get("messages") {
        messages.extend(extra_messages.iter().cloned());
    }

    chat.insert("messages".to_string(), Value::Array(messages));
    let mut chat_request = Value::Object(chat);
    normalize_chat_request_system_messages(&mut chat_request);
    chat_request
}

fn responses_tool_to_chat_tool(tool: &Value) -> Option<Value> {
    let object = tool.as_object()?;
    if object.get("type").and_then(Value::as_str) != Some("function") {
        return None;
    }

    if let Some(function) = object.get("function").and_then(Value::as_object) {
        function.get("name").and_then(Value::as_str)?;
        return Some(json!({
            "type": "function",
            "function": function
        }));
    }

    let name = object.get("name").and_then(Value::as_str)?;
    let mut function = Map::new();
    function.insert("name".to_string(), Value::String(name.to_string()));

    for key in ["description", "parameters", "strict"] {
        if let Some(value) = object.get(key) {
            function.insert(key.to_string(), value.clone());
        }
    }

    Some(json!({
        "type": "function",
        "function": function
    }))
}

fn responses_tool_choice_to_chat_tool_choice(tool_choice: &Value) -> Option<Value> {
    match tool_choice {
        Value::String(choice) if matches!(choice.as_str(), "auto" | "none" | "required") => {
            Some(tool_choice.clone())
        }
        Value::Object(object) if object.get("type").and_then(Value::as_str) == Some("function") => {
            if let Some(function) = object.get("function").and_then(Value::as_object) {
                function.get("name").and_then(Value::as_str)?;
                return Some(json!({
                    "type": "function",
                    "function": function
                }));
            }

            let name = object.get("name").and_then(Value::as_str)?;
            Some(json!({
                "type": "function",
                "function": {
                    "name": name
                }
            }))
        }
        _ => None,
    }
}

fn copy_keys(source: &Map<String, Value>, target: &mut Map<String, Value>, keys: &[&str]) {
    for key in keys {
        if let Some(value) = source.get(*key) {
            target.insert((*key).to_string(), value.clone());
        }
    }
}

fn append_response_input_as_messages(input: &Value, messages: &mut Vec<Value>) {
    match input {
        Value::String(text) => messages.push(json!({ "role": "user", "content": text })),
        Value::Array(items) => {
            for item in items {
                append_response_input_item(item, messages);
            }
        }
        Value::Object(object) if object.contains_key("role") || object.contains_key("content") => {
            append_response_input_item(input, messages);
        }
        other => messages.push(json!({ "role": "user", "content": text_from_value(other) })),
    }
}

fn append_response_input_item(item: &Value, messages: &mut Vec<Value>) {
    match item {
        Value::String(text) => messages.push(json!({ "role": "user", "content": text })),
        Value::Object(object) => {
            match object.get("type").and_then(Value::as_str) {
                Some("function_call") => {
                    if let Some(message) = response_function_call_to_chat_message(object) {
                        messages.push(message);
                        return;
                    }
                }
                Some("function_call_output") => {
                    if let Some(message) = response_function_call_output_to_chat_message(object) {
                        messages.push(message);
                        return;
                    }
                }
                Some("reasoning" | "compaction" | "compaction_trigger") => return,
                Some("input_text" | "output_text") => {
                    if let Some(text) = object.get("text").and_then(Value::as_str) {
                        messages.push(json!({ "role": "user", "content": text }));
                        return;
                    }
                }
                Some("input_image") => {
                    if let Some(part) = response_content_part_to_chat(item) {
                        messages.push(json!({ "role": "user", "content": [part] }));
                        return;
                    }
                }
                _ => {}
            }

            let role = object
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user")
                .to_string();
            let content = object
                .get("content")
                .or_else(|| object.get("text"))
                .map(chat_content_from_response_content)
                .unwrap_or_else(|| Value::String(text_from_value(item)));

            messages.push(json!({ "role": role, "content": content }));
        }
        other => messages.push(json!({ "role": "user", "content": text_from_value(other) })),
    }
}

fn response_function_call_to_chat_message(object: &Map<String, Value>) -> Option<Value> {
    let name = object.get("name").and_then(Value::as_str)?;
    let call_id = object
        .get("call_id")
        .or_else(|| object.get("id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("call_{}", unix_timestamp()));
    let arguments = object
        .get("arguments")
        .map(arguments_to_string)
        .unwrap_or_default();

    Some(json!({
        "role": "assistant",
        "content": null,
        "tool_calls": [{
            "id": call_id,
            "type": "function",
            "function": {
                "name": name,
                "arguments": arguments
            }
        }]
    }))
}

fn response_function_call_output_to_chat_message(object: &Map<String, Value>) -> Option<Value> {
    let call_id = object
        .get("call_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())?;
    let content = object
        .get("output")
        .map(function_call_output_to_text)
        .unwrap_or_default();

    Some(json!({
        "role": "tool",
        "tool_call_id": call_id,
        "content": content
    }))
}

fn chat_content_from_response_content(content: &Value) -> Value {
    match content {
        Value::Array(parts) => {
            let converted = parts
                .iter()
                .filter_map(response_content_part_to_chat)
                .collect::<Vec<_>>();
            if converted.is_empty() {
                Value::String(text_from_value(content))
            } else {
                chat_content_parts_to_value(converted)
            }
        }
        Value::String(_) => content.clone(),
        other => Value::String(text_from_value(other)),
    }
}

fn response_content_part_to_chat(part: &Value) -> Option<Value> {
    match part.get("type").and_then(Value::as_str) {
        Some("text" | "input_text" | "output_text") => part
            .get("text")
            .or_else(|| part.get("input_text"))
            .or_else(|| part.get("output_text"))
            .and_then(Value::as_str)
            .map(|text| json!({ "type": "text", "text": text })),
        Some("input_image" | "image_url") => {
            let image_url = part.get("image_url")?;
            let url = image_url
                .as_str()
                .or_else(|| image_url.get("url").and_then(Value::as_str))?;
            let mut converted = json!({ "type": "image_url", "image_url": { "url": url } });
            if let Some(detail) = part
                .get("detail")
                .or_else(|| image_url.get("detail"))
                .and_then(Value::as_str)
            {
                converted["image_url"]["detail"] = Value::String(detail.to_string());
            }
            Some(converted)
        }
        Some("reasoning") => None,
        _ => part
            .get("text")
            .and_then(Value::as_str)
            .map(|text| json!({ "type": "text", "text": text })),
    }
}

fn function_call_output_to_text(output: &Value) -> String {
    match output {
        Value::String(text) => text.clone(),
        Value::Array(parts) => {
            let text_parts = parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .and_then(Value::as_str)
                        .or_else(|| part.get("input_text").and_then(Value::as_str))
                        .or_else(|| part.get("output_text").and_then(Value::as_str))
                        .map(ToOwned::to_owned)
                })
                .collect::<Vec<_>>();
            if text_parts.is_empty() {
                text_from_value(output)
            } else {
                text_parts.join("\n")
            }
        }
        other => text_from_value(other),
    }
}

fn chat_completion_to_response(upstream: Value, client_model: Option<String>) -> Value {
    let id = upstream
        .get("id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("resp_{}", unix_timestamp()));
    let created_at = upstream
        .get("created")
        .and_then(Value::as_i64)
        .unwrap_or_else(unix_timestamp);
    let model = client_model.or_else(|| {
        upstream
            .get("model")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    });
    let message = first_chat_choice_message(&upstream);
    let content = message
        .and_then(|message| message.get("content"))
        .map(text_from_value)
        .unwrap_or_default();
    let message_id = format!("msg_{id}");
    let tool_call_items = message
        .and_then(|message| message.get("tool_calls"))
        .and_then(Value::as_array)
        .map(|tool_calls| {
            tool_calls
                .iter()
                .enumerate()
                .filter_map(|(index, tool_call)| {
                    chat_tool_call_to_response_item(tool_call, index, &id)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let mut output = Vec::new();
    if !content.is_empty() || tool_call_items.is_empty() {
        output.push(response_message_item(&message_id, "completed", &content));
    }
    output.extend(tool_call_items);

    let mut response = json!({
        "id": id,
        "object": "response",
        "created_at": created_at,
        "status": "completed",
        "output": output,
        "output_text": content
    });

    if let Some(model) = model {
        response["model"] = Value::String(model);
    }
    if let Some(usage) = upstream.get("usage") {
        response["usage"] = usage.clone();
    }

    response
}

fn first_chat_choice_message(upstream: &Value) -> Option<&Value> {
    upstream
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
}

fn response_message_item(item_id: &str, status: &str, output_text: &str) -> Value {
    json!({
        "id": item_id,
        "type": "message",
        "status": status,
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": output_text,
            "annotations": []
        }]
    })
}

fn chat_tool_call_to_response_item(
    tool_call: &Value,
    index: usize,
    response_id: &str,
) -> Option<Value> {
    let function = tool_call.get("function").and_then(Value::as_object);
    let name = function
        .and_then(|function| function.get("name"))
        .or_else(|| tool_call.get("name"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())?;
    let arguments = function
        .and_then(|function| function.get("arguments"))
        .or_else(|| tool_call.get("arguments"))
        .map(arguments_to_string)
        .map(|arguments| normalize_tool_arguments(&arguments))
        .unwrap_or_default();
    let call_id = tool_call
        .get("id")
        .or_else(|| tool_call.get("call_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("call_{response_id}_{index}"));

    Some(json!({
        "id": format!("fc_{call_id}"),
        "type": "function_call",
        "status": "completed",
        "call_id": call_id,
        "name": name,
        "arguments": arguments
    }))
}

fn arguments_to_string(arguments: &Value) -> String {
    match arguments {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn normalize_tool_arguments(arguments: &str) -> String {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if serde_json::from_str::<Value>(trimmed).is_ok() {
        return trimmed.to_string();
    }
    if let Some(parameters) = qwen_xml_tool_arguments_to_json(trimmed) {
        return Value::Object(parameters).to_string();
    }

    arguments.to_string()
}

fn qwen_xml_tool_arguments_to_json(input: &str) -> Option<Map<String, Value>> {
    let mut parameters = Map::new();
    let mut cursor = 0;
    let open_marker = "<parameter=";
    let close_marker = "</parameter>";

    while let Some(relative_start) = input[cursor..].find(open_marker) {
        let name_start = cursor + relative_start + open_marker.len();
        let rest = &input[name_start..];
        let Some(name_end) = rest.find('>') else {
            break;
        };
        let name = rest[..name_end]
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_string();
        if name.is_empty() {
            cursor = name_start + name_end + 1;
            continue;
        }

        let value_start = name_start + name_end + 1;
        let rest = &input[value_start..];
        let Some(value_end) = rest.find(close_marker) else {
            break;
        };
        let raw_value = rest[..value_end].trim();
        let value = serde_json::from_str::<Value>(raw_value)
            .unwrap_or_else(|_| Value::String(raw_value.to_string()));
        parameters.insert(name, value);
        cursor = value_start + value_end + close_marker.len();
    }

    if parameters.is_empty() {
        None
    } else {
        Some(parameters)
    }
}

fn anthropic_response_as_chat(
    response: reqwest::Response,
    client_model: Option<String>,
    meta: Option<UpstreamRequestMeta>,
) -> reqwest::Response {
    let mut upstream_stream = response.bytes_stream();
    let response_request_id = meta.as_ref().map(|meta| meta.request_id.clone());
    let stream_meta = meta.clone();
    let mut translator = AnthropicStreamTranslator::new(client_model, unix_timestamp());

    let stream = async_stream::stream! {
        let mut buffer = Vec::new();
        let mut data_lines = Vec::new();
        let mut upstream_done = false;

        while !upstream_done {
            match upstream_stream.next().await {
                Some(Ok(chunk)) => buffer.extend_from_slice(&chunk),
                Some(Err(error)) => {
                    let error_message = error.to_string();
                    if let Some(meta) = &stream_meta {
                        let failure = UpstreamFailure::from_reqwest(meta, error);
                        log_upstream_failure(&failure, "stream");
                        yield Ok::<Bytes, std::io::Error>(generic_stream_error_event(&failure));
                    } else {
                        error!(error = %error_message, "upstream Anthropic stream failed");
                        yield Ok(sse_error_event(
                            "upstream_error",
                            &format!("upstream stream failed: {error_message}"),
                            None,
                            None,
                        ));
                    }
                    return;
                }
                None => upstream_done = true,
            }

            let mut events = take_sse_data_events(&mut buffer, &mut data_lines);
            if upstream_done {
                let (tail, incomplete) = finish_sse_data_events(&mut buffer, &mut data_lines);
                if incomplete {
                    log_incomplete_sse_tail(stream_meta.as_ref());
                }
                events.extend(tail);
            }

            for data in events {
                let Ok(value) = serde_json::from_str::<Value>(data.trim()) else {
                    log_malformed_sse_data(stream_meta.as_ref(), &data);
                    continue;
                };
                if value.get("type").and_then(Value::as_str) == Some("error") {
                    yield Ok(sse_event("error", value));
                    return;
                }
                for chunk in translator.translate(&value) {
                    yield Ok(chat_sse_data(chunk));
                }
            }
        }
        yield Ok(chat_sse_done());
    };

    let mut builder = axum::http::Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/event-stream")
        .header(CACHE_CONTROL, "no-cache");
    if let Some(request_id) = &response_request_id {
        builder = builder.header("x-request-id", request_id.as_str());
    }
    reqwest::Response::from(
        builder
            .body(reqwest::Body::wrap_stream(stream))
            .expect("translated Anthropic response is valid"),
    )
}

fn chat_sse_data(value: Value) -> Bytes {
    Bytes::from(format!("data: {value}\n\n"))
}

fn chat_sse_done() -> Bytes {
    Bytes::from_static(b"data: [DONE]\n\n")
}

fn stream_chat_as_responses(
    response: reqwest::Response,
    client_model: Option<String>,
    meta: Option<UpstreamRequestMeta>,
) -> Response {
    let response_id = format!("resp_{}", unix_timestamp());
    let item_id = format!("msg_{response_id}");
    let created_at = unix_timestamp();
    let model = client_model.unwrap_or_else(|| "unknown".to_string());
    let mut upstream_stream = response.bytes_stream();
    let response_request_id = meta.as_ref().map(|meta| meta.request_id.clone());
    let stream_meta = meta.clone();

    let stream = async_stream::stream! {
        let mut sequence: u64 = 0;
        let mut buffer: Vec<u8> = Vec::new();
        let mut sse_data_lines: Vec<String> = Vec::new();
        let mut output_text = String::new();
        let mut tool_calls: Vec<StreamingToolCall> = Vec::new();
        let mut text_block_open = false;
        let mut usage_value: Option<Value> = None;
        let mut finish_reason: Option<String> = None;
        let mut upstream_done = false;

        yield Ok::<Bytes, std::io::Error>(sse_event("response.created", json!({
            "type": "response.created",
            "sequence_number": sequence,
            "response": responses_response_value(&response_id, &model, created_at, "in_progress", Value::Array(vec![]), None)
        })));
        sequence += 1;

        yield Ok(sse_event("response.in_progress", json!({
            "type": "response.in_progress",
            "sequence_number": sequence,
            "response": responses_response_value(&response_id, &model, created_at, "in_progress", Value::Array(vec![]), None)
        })));
        sequence += 1;

        yield Ok(sse_event("response.output_item.added", json!({
            "type": "response.output_item.added",
            "sequence_number": sequence,
            "output_index": 0,
            "item": {
                "id": item_id,
                "type": "message",
                "status": "in_progress",
                "role": "assistant",
                "content": []
            }
        })));
        sequence += 1;

        'outer: while !upstream_done {
            match upstream_stream.next().await {
                Some(Ok(chunk)) => buffer.extend_from_slice(&chunk),
                Some(Err(error)) => {
                    let error_message = error.to_string();
                    if let Some(meta) = &stream_meta {
                        let failure = UpstreamFailure::from_reqwest(meta, error);
                        log_upstream_failure(&failure, "stream");
                        yield Ok(responses_stream_failed_event(
                            &failure,
                            sequence,
                            &response_id,
                            &model,
                            created_at,
                        ));
                    } else {
                        error!(
                            error = %error_message,
                            "upstream response stream failed"
                        );
                        yield Ok(sse_error_event(
                            "upstream_error",
                            &format!("upstream stream failed: {error_message}"),
                            None,
                            None,
                        ));
                    }
                    return;
                }
                None => {
                    upstream_done = true;
                }
            }

            let mut data_events = take_sse_data_events(&mut buffer, &mut sse_data_lines);
            if upstream_done {
                let (tail_events, had_incomplete_tail) =
                    finish_sse_data_events(&mut buffer, &mut sse_data_lines);
                if had_incomplete_tail {
                    log_incomplete_sse_tail(stream_meta.as_ref());
                }
                data_events.extend(tail_events);
            }

            for data in data_events {
                let data = data.trim();
                if data == "[DONE]" {
                    break 'outer;
                }

                let Ok(value) = serde_json::from_str::<Value>(data) else {
                    log_malformed_sse_data(stream_meta.as_ref(), data);
                    continue;
                };
                if let Some(usage) = value.get("usage").filter(|v| !v.is_null()) {
                    usage_value = Some(usage.clone());
                }
                if let Some(reason) = value
                    .get("choices")
                    .and_then(Value::as_array)
                    .and_then(|choices| choices.iter().find_map(|choice| choice.get("finish_reason")))
                    .and_then(Value::as_str)
                {
                    finish_reason = Some(reason.to_string());
                }

                for delta in chat_stream_text_deltas(&value) {
                    if delta.is_empty() {
                        continue;
                    }
                    if !text_block_open {
                        yield Ok(sse_event("response.content_part.added", json!({
                            "type": "response.content_part.added",
                            "sequence_number": sequence,
                            "item_id": item_id,
                            "output_index": 0,
                            "content_index": 0,
                            "part": {
                                "type": "output_text",
                                "text": "",
                                "annotations": []
                            }
                        })));
                        sequence += 1;
                        text_block_open = true;
                    }
                    output_text.push_str(&delta);
                    yield Ok(sse_event("response.output_text.delta", json!({
                        "type": "response.output_text.delta",
                        "sequence_number": sequence,
                        "item_id": item_id,
                        "output_index": 0,
                        "content_index": 0,
                        "delta": delta
                    })));
                    sequence += 1;
                }

                for delta in chat_stream_tool_call_deltas(&value) {
                    let arguments_delta = delta.arguments_delta.clone();
                    let position = apply_stream_tool_call_delta(&mut tool_calls, delta, &response_id);
                    let added = {
                        let tool_call = &mut tool_calls[position];
                        if tool_call.added || tool_call.name.is_empty() {
                            None
                        } else {
                            tool_call.added = true;
                            Some((
                                tool_call.output_index,
                                streaming_tool_call_item(tool_call, "in_progress"),
                            ))
                        }
                    };

                    if let Some((output_index, item)) = added {
                        yield Ok(sse_event("response.output_item.added", json!({
                            "type": "response.output_item.added",
                            "sequence_number": sequence,
                            "output_index": output_index,
                            "item": item
                        })));
                        sequence += 1;
                    }

                    if !arguments_delta.is_empty() && tool_calls[position].added {
                        let item_id = tool_calls[position].item_id.clone();
                        let call_id = tool_calls[position].call_id.clone();
                        let output_index = tool_calls[position].output_index;
                        yield Ok(sse_event("response.function_call_arguments.delta", json!({
                            "type": "response.function_call_arguments.delta",
                            "sequence_number": sequence,
                            "item_id": item_id,
                            "call_id": call_id,
                            "output_index": output_index,
                            "delta": arguments_delta
                        })));
                        sequence += 1;
                    }
                }
            }
        }

        if text_block_open {
            yield Ok(sse_event("response.output_text.done", json!({
                "type": "response.output_text.done",
                "sequence_number": sequence,
                "item_id": item_id,
                "output_index": 0,
                "content_index": 0,
                "text": output_text
            })));
            sequence += 1;

            yield Ok(sse_event("response.content_part.done", json!({
                "type": "response.content_part.done",
                "sequence_number": sequence,
                "item_id": item_id,
                "output_index": 0,
                "content_index": 0,
                "part": {
                    "type": "output_text",
                    "text": output_text,
                    "annotations": []
                }
            })));
            sequence += 1;
        }

        let message_output = response_message_item(&item_id, "completed", &output_text);
        yield Ok(sse_event("response.output_item.done", json!({
            "type": "response.output_item.done",
            "sequence_number": sequence,
            "output_index": 0,
            "item": message_output
        })));
        sequence += 1;

        for tool_call in &mut tool_calls {
            if tool_call.name.is_empty() {
                continue;
            }
            let added = if tool_call.added {
                None
            } else {
                tool_call.added = true;
                Some((
                    tool_call.output_index,
                    streaming_tool_call_item(tool_call, "in_progress"),
                ))
            };
            if let Some((output_index, item)) = added {
                yield Ok(sse_event("response.output_item.added", json!({
                    "type": "response.output_item.added",
                    "sequence_number": sequence,
                    "output_index": output_index,
                    "item": item
                })));
                sequence += 1;
            }

            let output_index = tool_call.output_index;
            let item = streaming_tool_call_item(tool_call, "completed");
            yield Ok(sse_event("response.output_item.done", json!({
                "type": "response.output_item.done",
                "sequence_number": sequence,
                "output_index": output_index,
                "item": item
            })));
            sequence += 1;
        }

        let mut output_items = vec![response_message_item(&item_id, "completed", &output_text)];
        output_items.extend(
            tool_calls
                .iter()
                .filter(|tool_call| !tool_call.name.is_empty())
                .map(|tool_call| streaming_tool_call_item(tool_call, "completed")),
        );
        let output = Value::Array(output_items);

        let mut completed_response = responses_response_value(
            &response_id,
            &model,
            created_at,
            "completed",
            output,
            usage_value.as_ref().map(chat_usage_to_responses_usage),
        );
        if finish_reason.as_deref() == Some("stop")
            && let Some(object) = completed_response.as_object_mut()
        {
            object.insert("end_turn".to_string(), Value::Bool(true));
        }

        yield Ok(sse_event("response.completed", json!({
            "type": "response.completed",
            "sequence_number": sequence,
            "response": completed_response
        })));
    };

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/event-stream")
        .header(CACHE_CONTROL, "no-cache");
    if let Some(request_id) = &response_request_id {
        builder = builder.header("x-request-id", request_id.as_str());
    }
    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|error| internal_error_response(error.to_string()))
}

fn stream_chat_as_anthropic(
    response: reqwest::Response,
    client_model: Option<String>,
    meta: Option<UpstreamRequestMeta>,
) -> Response {
    let message_id = format!("msg_{}", unix_timestamp());
    let model = client_model.unwrap_or_else(|| "unknown".to_string());
    let mut upstream_stream = response.bytes_stream();
    let response_request_id = meta.as_ref().map(|meta| meta.request_id.clone());
    let stream_meta = meta.clone();

    let stream = async_stream::stream! {
        let mut buffer: Vec<u8> = Vec::new();
        let mut sse_data_lines: Vec<String> = Vec::new();
        let mut text_block_open = false;
        let mut tool_calls: Vec<StreamingToolCall> = Vec::new();
        let mut finish_reason: Option<String> = None;
        let mut usage_value: Option<Value> = None;
        let mut input_tokens: u64 = 0;
        let mut next_block_index: usize = 0;
        let mut text_block_index: usize = 0;
        let mut upstream_done = false;

        let message_start = json!({
            "type": "message_start",
            "message": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": Value::Null,
                "stop_sequence": Value::Null,
                "usage": {
                    "input_tokens": 0,
                    "output_tokens": 0
                }
            }
        });
        yield Ok::<Bytes, std::io::Error>(sse_event("message_start", message_start));

        'outer: while !upstream_done {
            match upstream_stream.next().await {
                Some(Ok(chunk)) => buffer.extend_from_slice(&chunk),
                Some(Err(error)) => {
                    let error_message = error.to_string();
                    if let Some(meta) = &stream_meta {
                        let failure = UpstreamFailure::from_reqwest(meta, error);
                        log_upstream_failure(&failure, "stream");
                        yield Ok(anthropic_stream_error_event(&failure));
                    } else {
                        error!(
                            error = %error_message,
                            "upstream anthropic stream failed"
                        );
                        yield Ok(sse_error_event(
                            "api_error",
                            &format!("upstream stream failed: {error_message}"),
                            None,
                            None,
                        ));
                    }
                    return;
                }
                None => {
                    upstream_done = true;
                }
            }

            let mut data_events = take_sse_data_events(&mut buffer, &mut sse_data_lines);
            if upstream_done {
                let (tail_events, had_incomplete_tail) =
                    finish_sse_data_events(&mut buffer, &mut sse_data_lines);
                if had_incomplete_tail {
                    log_incomplete_sse_tail(stream_meta.as_ref());
                }
                data_events.extend(tail_events);
            }

            for data in data_events {
                let data = data.trim();
                if data == "[DONE]" {
                    break 'outer;
                }
                let Ok(value) = serde_json::from_str::<Value>(data) else {
                    log_malformed_sse_data(stream_meta.as_ref(), data);
                    continue;
                };

                if let Some(usage) = value.get("usage").filter(|v| !v.is_null()) {
                    if let Some(tokens) = usage.get("prompt_tokens").and_then(Value::as_u64) {
                        input_tokens = tokens;
                    }
                    usage_value = Some(usage.clone());
                }
                if let Some(reason) = value
                    .get("choices")
                    .and_then(Value::as_array)
                    .and_then(|choices| choices.iter().find_map(|choice| choice.get("finish_reason")))
                    .and_then(Value::as_str)
                {
                    finish_reason = Some(reason.to_string());
                }

                for delta in chat_stream_text_deltas(&value) {
                    if delta.is_empty() {
                        continue;
                    }
                    if !text_block_open {
                        text_block_index = next_block_index;
                        next_block_index += 1;
                        yield Ok(sse_event("content_block_start", json!({
                            "type": "content_block_start",
                            "index": text_block_index,
                            "content_block": {
                                "type": "text",
                                "text": ""
                            }
                        })));
                        text_block_open = true;
                    }
                    yield Ok(sse_event("content_block_delta", json!({
                        "type": "content_block_delta",
                        "index": text_block_index,
                        "delta": {
                            "type": "text_delta",
                            "text": delta
                        }
                    })));
                }

                for tool_delta in chat_stream_tool_call_deltas(&value) {
                    let arguments_delta = tool_delta.arguments_delta.clone();
                    let position = apply_stream_tool_call_delta(&mut tool_calls, tool_delta, &message_id);
                    let ready_to_open = {
                        let tool_call = &mut tool_calls[position];
                        if tool_call.added || tool_call.name.is_empty() {
                            None
                        } else {
                            if text_block_open {
                                let to_close = text_block_index;
                                text_block_open = false;
                                Some((true, to_close, 0_usize, String::new(), String::new()))
                            } else {
                                Some((false, 0_usize, 0_usize, String::new(), String::new()))
                            }
                        }
                    };

                    if let Some((needs_text_close, text_close_index, _, _, _)) = ready_to_open {
                        if needs_text_close {
                            yield Ok(sse_event("content_block_stop", json!({
                                "type": "content_block_stop",
                                "index": text_close_index
                            })));
                        }
                        let block_index = next_block_index;
                        next_block_index += 1;
                        {
                            let tool_call = &mut tool_calls[position];
                            tool_call.block_index = block_index;
                            tool_call.added = true;
                        }
                        let tool_call = &tool_calls[position];
                        yield Ok(sse_event("content_block_start", json!({
                            "type": "content_block_start",
                            "index": tool_call.block_index,
                            "content_block": {
                                "type": "tool_use",
                                "id": tool_call.call_id,
                                "name": tool_call.name,
                                "input": {}
                            }
                        })));
                    }

                    if !arguments_delta.is_empty() && tool_calls[position].added {
                        let block_index = tool_calls[position].block_index;
                        yield Ok(sse_event("content_block_delta", json!({
                            "type": "content_block_delta",
                            "index": block_index,
                            "delta": {
                                "type": "input_json_delta",
                                "partial_json": arguments_delta
                            }
                        })));
                    }
                }
            }
        }

        if text_block_open {
            yield Ok(sse_event("content_block_stop", json!({
                "type": "content_block_stop",
                "index": text_block_index
            })));
        }

        for tool_call in &mut tool_calls {
            if tool_call.name.is_empty() {
                continue;
            }
            if !tool_call.added {
                let block_index = next_block_index;
                next_block_index += 1;
                tool_call.block_index = block_index;
                tool_call.added = true;
                let arguments = normalize_tool_arguments(&tool_call.arguments);
                let initial_input = serde_json::from_str::<Value>(&arguments)
                    .unwrap_or_else(|_| json!({}));
                yield Ok(sse_event("content_block_start", json!({
                    "type": "content_block_start",
                    "index": block_index,
                    "content_block": {
                        "type": "tool_use",
                        "id": tool_call.call_id,
                        "name": tool_call.name,
                        "input": initial_input
                    }
                })));
            }
            yield Ok(sse_event("content_block_stop", json!({
                "type": "content_block_stop",
                "index": tool_call.block_index
            })));
        }

        let has_tool_use = tool_calls.iter().any(|tool_call| !tool_call.name.is_empty());
        let stop_reason = if has_tool_use {
            "tool_use"
        } else {
            match finish_reason.as_deref() {
                Some("length") => "max_tokens",
                Some("tool_calls") => "tool_use",
                Some("stop") => "end_turn",
                _ => "end_turn",
            }
        };

        let output_tokens = usage_value
            .as_ref()
            .and_then(|usage| usage.get("completion_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);

        yield Ok(sse_event("message_delta", json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": stop_reason,
                "stop_sequence": Value::Null
            },
            "usage": {
                "input_tokens": input_tokens,
                "output_tokens": output_tokens
            }
        })));

        yield Ok(sse_event("message_stop", json!({
            "type": "message_stop"
        })));
    };

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/event-stream")
        .header(CACHE_CONTROL, "no-cache");
    if let Some(request_id) = &response_request_id {
        builder = builder.header("x-request-id", request_id.as_str());
    }
    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|error| internal_error_response(error.to_string()))
}

fn ensure_chat_stream_include_usage(chat_request: &mut Value) {
    let Some(object) = chat_request.as_object_mut() else {
        return;
    };
    if object.get("stream").and_then(Value::as_bool) != Some(true) {
        return;
    }
    let entry = object
        .entry("stream_options".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(map) = entry.as_object_mut()
        && !map.contains_key("include_usage")
    {
        map.insert("include_usage".to_string(), Value::Bool(true));
    }
}

fn responses_response_value(
    response_id: &str,
    model: &str,
    created_at: i64,
    status: &str,
    output: Value,
    usage: Option<Value>,
) -> Value {
    let mut response = json!({
        "id": response_id,
        "object": "response",
        "created_at": created_at,
        "model": model,
        "status": status,
        "output": output
    });
    if let Some(usage) = usage
        && let Some(object) = response.as_object_mut()
    {
        object.insert("usage".to_string(), usage);
    }
    response
}

fn chat_usage_to_responses_usage(chat_usage: &Value) -> Value {
    let prompt_tokens = chat_usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion_tokens = chat_usage
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total_tokens = chat_usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(prompt_tokens + completion_tokens);
    let cached_tokens = chat_usage
        .get("prompt_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning_tokens = chat_usage
        .get("completion_tokens_details")
        .and_then(|details| details.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    json!({
        "input_tokens": prompt_tokens,
        "input_tokens_details": { "cached_tokens": cached_tokens },
        "output_tokens": completion_tokens,
        "output_tokens_details": { "reasoning_tokens": reasoning_tokens },
        "total_tokens": total_tokens
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChatStreamToolCallDelta {
    index: usize,
    id: Option<String>,
    name: Option<String>,
    arguments_delta: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StreamingToolCall {
    chat_index: usize,
    output_index: usize,
    block_index: usize,
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
    added: bool,
}

fn chat_stream_text_deltas(value: &Value) -> Vec<String> {
    value
        .get("choices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|choice| choice.get("delta"))
        .filter_map(|delta| delta.get("content"))
        .flat_map(|content| match content {
            Value::String(text) => vec![text.clone()],
            Value::Array(parts) => parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                })
                .collect(),
            _ => Vec::new(),
        })
        .collect()
}

fn chat_stream_tool_call_deltas(value: &Value) -> Vec<ChatStreamToolCallDelta> {
    value
        .get("choices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|choice| choice.get("delta"))
        .flat_map(|delta| {
            delta
                .get("tool_calls")
                .and_then(Value::as_array)
                .map(|tool_calls| {
                    tool_calls
                        .iter()
                        .enumerate()
                        .filter_map(|(fallback_index, tool_call)| {
                            let function = tool_call.get("function");
                            let id = tool_call
                                .get("id")
                                .and_then(Value::as_str)
                                .filter(|value| !value.is_empty())
                                .map(ToOwned::to_owned);
                            let name = function
                                .and_then(|function| function.get("name"))
                                .or_else(|| tool_call.get("name"))
                                .and_then(Value::as_str)
                                .filter(|value| !value.is_empty())
                                .map(ToOwned::to_owned);
                            let arguments_delta = function
                                .and_then(|function| function.get("arguments"))
                                .map(arguments_to_string)
                                .unwrap_or_default();
                            if id.is_none() && name.is_none() && arguments_delta.is_empty() {
                                return None;
                            }

                            Some(ChatStreamToolCallDelta {
                                index: tool_call
                                    .get("index")
                                    .and_then(Value::as_u64)
                                    .map(|value| value as usize)
                                    .unwrap_or(fallback_index),
                                id,
                                name,
                                arguments_delta,
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .collect()
}

fn apply_stream_tool_call_delta(
    tool_calls: &mut Vec<StreamingToolCall>,
    delta: ChatStreamToolCallDelta,
    response_id: &str,
) -> usize {
    let position = tool_calls
        .iter()
        .position(|tool_call| tool_call.chat_index == delta.index)
        .unwrap_or_else(|| {
            let call_id = delta
                .id
                .clone()
                .unwrap_or_else(|| format!("call_{response_id}_{}", delta.index));
            let output_index = tool_calls.len() + 1;
            tool_calls.push(StreamingToolCall {
                chat_index: delta.index,
                output_index,
                block_index: 0,
                item_id: format!("fc_{call_id}"),
                call_id,
                name: String::new(),
                arguments: String::new(),
                added: false,
            });
            tool_calls.len() - 1
        });

    let tool_call = &mut tool_calls[position];
    if !tool_call.added
        && let Some(id) = delta.id
    {
        tool_call.call_id = id;
        tool_call.item_id = format!("fc_{}", tool_call.call_id);
    }
    if let Some(name) = delta.name {
        tool_call.name = name;
    }
    tool_call.arguments.push_str(&delta.arguments_delta);

    position
}

fn streaming_tool_call_item(tool_call: &StreamingToolCall, status: &str) -> Value {
    json!({
        "id": tool_call.item_id,
        "type": "function_call",
        "status": status,
        "call_id": tool_call.call_id,
        "name": tool_call.name,
        "arguments": normalize_tool_arguments(&tool_call.arguments)
    })
}

fn take_sse_line_bytes(buffer: &mut Vec<u8>) -> Option<String> {
    let position = buffer.iter().position(|byte| *byte == b'\n')?;
    let line_bytes: Vec<u8> = buffer.drain(..=position).collect();
    let mut line = String::from_utf8_lossy(&line_bytes).into_owned();
    while line.ends_with('\n') || line.ends_with('\r') {
        line.pop();
    }
    Some(line)
}

fn take_sse_data_events(buffer: &mut Vec<u8>, data_lines: &mut Vec<String>) -> Vec<String> {
    let mut events = Vec::new();
    while let Some(line) = take_sse_line_bytes(buffer) {
        push_sse_line(&line, data_lines, &mut events);
    }
    events
}

fn finish_sse_data_events(
    buffer: &mut Vec<u8>,
    data_lines: &mut Vec<String>,
) -> (Vec<String>, bool) {
    let mut events = Vec::new();
    let had_incomplete_tail = !buffer.is_empty() || !data_lines.is_empty();
    if !buffer.is_empty() {
        let line = String::from_utf8_lossy(buffer).into_owned();
        buffer.clear();
        push_sse_line(&line, data_lines, &mut events);
    }
    if !data_lines.is_empty() {
        events.push(std::mem::take(data_lines).join("\n"));
    }
    (events, had_incomplete_tail)
}

fn push_sse_line(line: &str, data_lines: &mut Vec<String>, events: &mut Vec<String>) {
    let trimmed_end = line.trim_end_matches('\r');
    if trimmed_end.is_empty() {
        if !data_lines.is_empty() {
            events.push(std::mem::take(data_lines).join("\n"));
        }
        return;
    }
    if trimmed_end.starts_with(':') {
        return;
    }
    if let Some(rest) = trimmed_end.strip_prefix("data:") {
        data_lines.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
    }
}

fn log_incomplete_sse_tail(meta: Option<&UpstreamRequestMeta>) {
    if let Some(meta) = meta {
        error!(
            request_id = %meta.request_id,
            method = %meta.method,
            upstream_path = %meta.path,
            response_kind = meta.response_kind.as_str(),
            "upstream SSE stream ended with an incomplete event"
        );
    } else {
        error!("upstream SSE stream ended with an incomplete event");
    }
}

fn log_malformed_sse_data(meta: Option<&UpstreamRequestMeta>, data: &str) {
    let prefix = body_prefix(data.as_bytes());
    if let Some(meta) = meta {
        error!(
            request_id = %meta.request_id,
            method = %meta.method,
            upstream_path = %meta.path,
            response_kind = meta.response_kind.as_str(),
            data_prefix = %prefix,
            "discarded malformed upstream SSE data event"
        );
    } else {
        error!(
            data_prefix = %prefix,
            "discarded malformed upstream SSE data event"
        );
    }
}

fn is_event_stream_content_type(content_type: &str) -> bool {
    content_type
        .split(';')
        .next()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"))
}

fn upstream_stream_error_message(failure: &UpstreamFailure) -> String {
    format!(
        "upstream stream failed with {}: {}",
        failure.kind.as_str(),
        failure.message
    )
}

fn generic_stream_error_event(failure: &UpstreamFailure) -> Bytes {
    sse_error_event(
        "upstream_error",
        &upstream_stream_error_message(failure),
        Some(&failure.request_id),
        Some(failure.kind.as_str()),
    )
}

fn anthropic_stream_error_event(failure: &UpstreamFailure) -> Bytes {
    sse_error_event(
        "api_error",
        &upstream_stream_error_message(failure),
        Some(&failure.request_id),
        Some(failure.kind.as_str()),
    )
}

fn responses_stream_failed_event(
    failure: &UpstreamFailure,
    sequence: u64,
    response_id: &str,
    model: &str,
    created_at: i64,
) -> Bytes {
    sse_event(
        "response.failed",
        json!({
            "type": "response.failed",
            "sequence_number": sequence,
            "response": {
                "id": response_id,
                "object": "response",
                "created_at": created_at,
                "model": model,
                "status": "failed",
                "output": [],
                "error": {
                    "type": "upstream_error",
                    "code": failure.kind.as_str(),
                    "message": upstream_stream_error_message(failure),
                    "request_id": failure.request_id
                }
            }
        }),
    )
}

fn sse_error_event(
    error_type: &str,
    message: &str,
    request_id: Option<&str>,
    kind: Option<&str>,
) -> Bytes {
    sse_event(
        "error",
        json!({
            "type": "error",
            "error": {
                "type": error_type,
                "message": message,
                "request_id": request_id,
                "kind": kind
            }
        }),
    )
}

fn sse_event(event: &str, data: Value) -> Bytes {
    Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
}

fn text_from_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(values) => values
            .iter()
            .map(text_from_value)
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(object) => object
            .get("text")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| value.to_string()),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn validate_config(config: &Config) -> Result<()> {
    let providers = effective_provider_configs(config)?;
    for (name, provider) in &providers {
        if name.is_empty() || name.contains('/') {
            bail!("provider names must be non-empty and cannot contain '/': {name:?}");
        }
        if provider.base_url.trim().is_empty() {
            bail!("provider {name:?} base_url cannot be empty");
        }
        if provider.kind == ProviderKind::Copilot && provider.reasoning_effort.is_some() {
            bail!(
                "provider {name:?} cannot set reasoning_effort for kind \"copilot\"; configure the native client instead"
            );
        }
        if provider.kind == ProviderKind::Anthropic
            && matches!(
                provider.reasoning_effort,
                Some(ReasoningEffort::None | ReasoningEffort::Minimal)
            )
        {
            bail!(
                "provider {name:?} reasoning_effort must be low, medium, high, xhigh, or max for Anthropic"
            );
        }
    }
    for (alias, route) in &config.model_aliases {
        if alias == INTEGRATED_AUTO_MODEL {
            bail!("model alias {INTEGRATED_AUTO_MODEL:?} is reserved for integrated routing");
        }
        let provider = route.provider(&config.default_provider);
        if !providers.contains_key(provider) {
            bail!("model alias {alias:?} references unknown provider {provider:?}");
        }
        if route.model().trim().is_empty() {
            bail!("model alias {alias:?} has an empty upstream model");
        }
    }
    for (name, provider) in &providers {
        for entry in &provider.fallback {
            let target = entry
                .split_once('/')
                .map_or(entry.as_str(), |(name, _)| name);
            if target == name {
                bail!("provider {name:?} lists itself as its own fallback");
            }
            if !providers.contains_key(target) {
                bail!(
                    "provider {name:?} fallback {entry:?} references unknown provider {target:?}"
                );
            }
        }
        if provider.circuit_breaker_threshold > 0 && provider.circuit_breaker_cooldown_secs == 0 {
            bail!(
                "provider {name:?} circuit_breaker_cooldown_secs must be greater than zero when the breaker is enabled"
            );
        }
    }
    for (index, rule) in config.routing_rules.iter().enumerate() {
        let label = if rule.name.is_empty() {
            format!("routing_rules[{index}]")
        } else {
            format!("routing rule {:?}", rule.name)
        };
        if rule.name.trim().is_empty() {
            bail!("{label} must have a name; the decision audit reports routes by rule name");
        }
        if let (Some(min), Some(max)) = (rule.min_input_tokens, rule.max_input_tokens)
            && min > max
        {
            bail!("{label} has min_input_tokens greater than max_input_tokens");
        }
        if rule.route.eq_ignore_ascii_case(ROUTING_RULE_PASS) {
            continue;
        }
        if config.integrated.targets.contains_key(&rule.route) {
            continue;
        }
        match rule.route.split_once('/') {
            Some((provider, model)) if !model.trim().is_empty() => {
                if !providers.contains_key(provider) {
                    bail!("{label} routes to unknown provider {provider:?}");
                }
            }
            _ => bail!(
                "{label} route {:?} must be {ROUTING_RULE_PASS:?}, an [integrated.targets.*] name, or provider/model",
                rule.route
            ),
        }
    }
    for (model, price) in &config.pricing.models {
        if price.input_per_mtok < 0.0 || price.output_per_mtok < 0.0 {
            bail!("pricing for {model:?} cannot be negative");
        }
    }
    if let Some(baseline) = &config.pricing.baseline_model
        && config.pricing.baseline().is_zero()
    {
        bail!(
            "pricing.baseline_model {baseline:?} has no entry in [pricing.models]; a baseline with no rate cannot answer what traffic would have cost"
        );
    }
    let mut routing_interceptor = config.interceptor.clone();
    routing_interceptor.enabled = config.routing_mode != RoutingMode::Proxy;
    routing_interceptor.reject_sensitive &= config.routing_mode != RoutingMode::Proxy;
    let mut integrated = config.integrated.clone();
    integrated.enabled = config.routing_mode == RoutingMode::Integrated;
    interceptor::validate_config(&routing_interceptor, &providers)?;
    interceptor::validate_integrated_config(&integrated, &routing_interceptor, &providers)?;
    if config.admin.enable {
        let address = admin::listen_address(&config.admin);
        let parsed: SocketAddr = address
            .parse()
            .with_context(|| format!("invalid admin.listen_address: {address:?}"))?;
        if !parsed.ip().is_loopback() {
            bail!(
                "admin.listen_address {address:?} is not a loopback address; \
                 the admin interface is unauthenticated and must stay on 127.0.0.1/::1"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
