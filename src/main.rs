use std::{
    collections::{HashMap, HashSet},
    env,
    ffi::OsString,
    fs, io,
    net::SocketAddr,
    os::unix::process::CommandExt,
    path::{Path as FsPath, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path as AxumPath, State},
    http::{
        HeaderMap, HeaderName, HeaderValue, StatusCode,
        header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE},
    },
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use clap::Parser;
use futures_util::StreamExt;
use reqwest::{Client, Method};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::{net::TcpListener, sync::Mutex as AsyncMutex, time::sleep};
use tracing::{debug, info, warn};

mod admin;
mod agent_launcher;
mod claude_launcher;
mod codex_launcher;
mod copilot_launcher;
mod interceptor;
mod stats;

use agent_launcher::{AgentLaunch, LauncherKind, LauncherSession, probe_existing_llmproxy};
use interceptor::{InterceptionPolicy, InterceptorConfig, InterceptorProtocol, maybe_intercept};
use stats::{RemapKind, log_request_value, proxy_stats, route_class_for_base_url, track_response};

#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

type ResponseResult<T> = std::result::Result<T, Box<Response>>;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Small OpenAI-compatible v1 proxy for vLLM-style servers",
    group(clap::ArgGroup::new("agent_launcher").args(["claude", "codex", "copilot"]).multiple(false))
)]
struct Cli {
    /// Path to a TOML config file.
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Override the listen address, for example 127.0.0.1:8080.
    #[arg(long)]
    listen: Option<String>,

    /// Override the external API base URL, normally ending in /v1.
    #[arg(long)]
    upstream: Option<String>,

    /// Enable the configured local-first interceptor for this process.
    #[arg(long)]
    intercept: bool,

    /// Stop the managed llmproxy instance for this listen address and exit.
    #[arg(long)]
    shutdown: bool,

    /// Restart the managed llmproxy instance for this listen address.
    #[arg(long, conflicts_with = "shutdown")]
    restart: bool,

    /// Launch Codex through this proxy using Codex's existing OpenAI login.
    #[arg(long, requires = "intercept")]
    codex: bool,

    /// Launch Claude Code through this proxy using Claude's existing Anthropic login.
    #[arg(long, requires = "intercept")]
    claude: bool,

    /// Launch GitHub Copilot CLI through this proxy using Copilot's existing GitHub login.
    #[arg(long, requires = "intercept")]
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
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct Config {
    listen_addr: String,
    default_provider: String,
    upstream: UpstreamConfig,
    providers: HashMap<String, UpstreamConfig>,
    interceptor: InterceptorConfig,
    model_aliases: HashMap<String, ModelAlias>,
    role_map: HashMap<String, String>,
    chat: ChatConfig,
    responses: ResponsesConfig,
    models: ModelsConfig,
    admin: AdminConfig,
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
            default_provider: "default".to_string(),
            upstream: UpstreamConfig::default(),
            providers: HashMap::new(),
            interceptor: InterceptorConfig::default(),
            model_aliases: HashMap::new(),
            role_map: HashMap::new(),
            chat: ChatConfig::default(),
            responses: ResponsesConfig::default(),
            models: ModelsConfig::default(),
            admin: AdminConfig::default(),
            model_routing: ModelRoutingMode::Remap,
            anthropic_mode: AnthropicMode::ChatCompletions,
        }
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
    base_url: String,
    api_key: Option<String>,
    api_key_env: Option<String>,
    headers: HashMap<String, String>,
    timeout_secs: u64,
    connect_timeout_secs: u64,
    pool_max_idle_per_host: usize,
    pool_idle_timeout_secs: u64,
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            base_url: env::var("OPENAI_BASE_URL")
                .or_else(|_| env::var("VLLM_BASE_URL"))
                .unwrap_or_else(|_| "http://127.0.0.1:8000/v1".to_string()),
            api_key: None,
            api_key_env: None,
            headers: HashMap::new(),
            timeout_secs: 600,
            connect_timeout_secs: 10,
            pool_max_idle_per_host: 64,
            pool_idle_timeout_secs: 90,
        }
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
        }
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

    fn status_with_body(meta: &UpstreamRequestMeta, status: StatusCode, bytes: &[u8]) -> Self {
        let detail = upstream_error_detail(bytes).unwrap_or_else(|| body_prefix(bytes));
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "llmproxy=info,tower_http=info".into()),
        )
        .init();

    let cli = Cli::parse();
    proxy_stats().touch_start();
    if cli.foreground && cli.has_agent_launcher() {
        bail!("--foreground cannot be combined with --codex, --claude, or --copilot");
    }

    let (config, config_path) = load_config(&cli, cli.intercept && !cli.has_agent_launcher())?;
    let addr: SocketAddr = config
        .listen_addr
        .parse()
        .with_context(|| format!("invalid listen address: {}", config.listen_addr))?;

    if cli.shutdown {
        shutdown_daemon(addr).await?;
        return Ok(());
    }

    let agent_launch = AgentLaunch::discover(&cli).await?;
    if agent_launch.is_some() {
        validate_launcher_runtime(&config)?;
    }

    if cli.restart {
        let _ = shutdown_daemon(addr).await?;
    }

    if cli.foreground {
        run_server(config, config_path, None).await?;
        return Ok(());
    }

    if let Some(agent_launch) = agent_launch {
        ensure_daemon_running(&cli, config_path.as_deref(), false, addr).await?;
        run_agent(agent_launch, addr).await?;
        return Ok(());
    }

    ensure_daemon_running(&cli, config_path.as_deref(), true, addr).await?;
    Ok(())
}

async fn run_server(
    config: Config,
    config_path: Option<PathBuf>,
    launcher_name: Option<&'static str>,
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
    };
    let shared = SharedState::new(state.clone());

    if config.model_routing == ModelRoutingMode::Remap {
        for provider in state.providers.values().cloned() {
            tokio::spawn(async move {
                let _refresh_guard = provider.model_refresh.lock().await;
                if let Err(error) = refresh_upstream_models(&provider, &HeaderMap::new()).await {
                    warn!(
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
    let runtime_info = RuntimeInfo::new(bound_addr, admin_addr);
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
    Ok(ProviderState {
        name,
        config: Arc::new(upstream),
        client,
        model_cache: Arc::new(ModelCache::default()),
        model_refresh: Arc::new(AsyncMutex::new(())),
    })
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
    let info = read_runtime_info(addr)?.unwrap_or_else(|| RuntimeInfo::new(addr, None));
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
    format!(
        "{LLMPROXY_BANNER}\n\nStatus : {status}\nProxy  : {}\nAdmin  : {admin}",
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
        if let Some(upstream) = &cli.upstream {
            command.arg("--upstream").arg(upstream);
        }
        if cli.intercept {
            command.arg("--intercept");
        }
    }

    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }

    command
        .spawn()
        .context("failed to spawn background llmproxy daemon")?;
    Ok(())
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

    if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
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

fn read_pid_file(path: &FsPath) -> Result<Option<i32>> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let pid = raw
        .trim()
        .parse::<i32>()
        .with_context(|| format!("invalid pid file {}", path.display()))?;
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

fn process_exists(pid: i32) -> bool {
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
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
    fn new(proxy_addr: SocketAddr, admin_addr: Option<SocketAddr>) -> Self {
        Self {
            pid: std::process::id(),
            proxy_url: format!("http://{proxy_addr}/"),
            admin_url: admin_addr.map(|addr| format!("http://{addr}/")),
        }
    }
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

fn load_config(cli: &Cli, force_intercept: bool) -> Result<(Config, Option<PathBuf>)> {
    let config_path = cli
        .config
        .clone()
        .or_else(|| env::var_os("LLMPROXY_CONFIG").map(PathBuf::from))
        .or_else(|| {
            ["config.toml", "llmproxy.toml"]
                .into_iter()
                .map(PathBuf::from)
                .find(|path| path.exists())
        });
    let (mut config, config_source) = if let Some(path) = &config_path {
        (read_config(path)?, path.display().to_string())
    } else {
        (Config::default(), "environment/defaults".to_string())
    };

    if let Some(listen) = &cli.listen {
        config.listen_addr = listen.clone();
    }
    if force_intercept {
        config.interceptor.enabled = true;
    }
    if let Some(upstream) = &cli.upstream {
        if let Some(provider) = config.providers.get_mut(&config.default_provider) {
            provider.base_url = upstream.clone();
        } else {
            config.upstream.base_url = upstream.clone();
        }
    }
    finalize_config_keys(&mut config);
    validate_config(&config)?;

    info!(
        config_source = %config_source,
        default_provider = %config.default_provider,
        providers = effective_provider_configs(&config)?.len(),
        interceptor_enabled = config.interceptor.enabled,
        model_aliases = config.model_aliases.len(),
        role_mappings = config.role_map.len(),
        disable_thinking = config.chat.disable_thinking,
        expose_aliases = config.models.expose_aliases,
        auto_model_mapping = config.model_routing == ModelRoutingMode::Remap,
        admin_enabled = config.admin.enable,
        "loaded config"
    );

    Ok((config, config_path))
}

fn validate_launcher_runtime(config: &Config) -> Result<()> {
    let providers = effective_provider_configs(config)?;
    if config.interceptor.provider.trim().is_empty() {
        bail!("launchers require interceptor.provider to be configured");
    }
    if !providers.contains_key(&config.interceptor.provider) {
        bail!(
            "launcher interceptor provider {:?} is not configured",
            config.interceptor.provider
        );
    }
    if config.interceptor.model.trim().is_empty() {
        bail!("launchers require interceptor.model to be configured");
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
        provider.api_key = resolve_api_key_with_env(
            provider,
            !is_interceptor_provider && !uses_client_auth,
            env_value,
        );
    }
}

fn read_config(path: &FsPath) -> Result<Config> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("invalid TOML config {}", path.display()))
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
            inherit_openai_api_key
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
        return match proxy_get(state.default_provider(), &headers, "models").await {
            Ok(models) => Json(models).into_response(),
            Err(error_response) => error_response,
        };
    }
    if !state.config.models.expose_upstream {
        return Json(rewrite_models_response(
            json!({ "object": "list", "data": [] }),
            &state.config,
        ))
        .into_response();
    }

    let mut provider_names = state.providers.keys().cloned().collect::<Vec<_>>();
    provider_names.sort();
    let mut data = Vec::new();
    for provider_name in provider_names {
        let provider = state
            .providers
            .get(&provider_name)
            .expect("provider name came from map");
        let upstream = if let Some(cached) = provider
            .model_cache
            .cached_upstream_models_response(models_cache_ttl(&state.config))
        {
            debug!(provider = %provider.name, "serving upstream models from cache");
            cached
        } else {
            match proxy_get(provider, &headers, "models").await {
                Ok(upstream) => {
                    cache_upstream_models(provider, upstream.clone(), "models_endpoint");
                    upstream
                }
                Err(_) => {
                    warn!(
                        provider = %provider.name,
                        upstream = %provider.config.base_url,
                        "omitting unavailable provider from aggregated model list"
                    );
                    continue;
                }
            }
        };
        append_provider_models(&mut data, upstream, &provider.name);
    }

    Json(rewrite_models_response(
        json!({ "object": "list", "data": data }),
        &state.config,
    ))
    .into_response()
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
        return match proxy_get(
            state.default_provider(),
            &headers,
            &format!("models/{model}"),
        )
        .await
        {
            Ok(model) => Json(model).into_response(),
            Err(error_response) => error_response,
        };
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

    let provider = state.default_provider();
    match ensure_default_upstream_model(provider, &headers).await {
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
    proxy_stats().record_request();
    let started = Instant::now();
    let provider = match rewrite_proxy_request_value(&state, &headers, &mut body).await {
        Ok(provider) => provider,
        Err(error_response) => return *error_response,
    };
    let class = route_class_for_base_url(&provider.config.base_url);
    log_request_value(class, &body);
    let response = proxy_json(&provider, &headers, "completions", &body).await;
    track_response(class, started, response)
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
    let provider = match rewrite_proxy_request_value(&state, &headers, &mut body).await {
        Ok(provider) => provider,
        Err(error_response) => return *error_response,
    };
    proxy_json(&provider, &headers, "embeddings", &body).await
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
    proxy_stats().record_request();
    let started = Instant::now();
    if state.config.interceptor.enabled
        && let Some(response) = maybe_intercept(
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
        return response;
    }

    let mut body = body;
    let provider = match rewrite_proxy_request_value(&state, &headers, &mut body).await {
        Ok(provider) => provider,
        Err(error_response) => return *error_response,
    };
    if state.launcher_kind != Some(LauncherKind::Copilot) {
        normalize_chat_completion_request(&mut body, &state.config);
    }
    let class = route_class_for_base_url(&provider.config.base_url);
    log_request_value(class, &body);
    let response = proxy_json(&provider, &headers, "chat/completions", &body).await;
    track_response(class, started, response)
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
    proxy_stats().record_request();
    let started = Instant::now();
    let client_model = body
        .get("model")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    if state.config.interceptor.enabled {
        let policy = InterceptionPolicy::for_responses(&headers, &body);
        let local_chat_request = responses_to_chat_request(&body);
        if let Some(response) = maybe_intercept(
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
            return response;
        }
    }

    let mut rewritten = body;
    let provider = match rewrite_proxy_request_value(&state, &headers, &mut rewritten).await {
        Ok(provider) => provider,
        Err(error_response) => return *error_response,
    };
    let class = route_class_for_base_url(&provider.config.base_url);
    log_request_value(class, &rewritten);

    let response = match state.config.responses.mode {
        ResponsesMode::Passthrough => {
            proxy_json(&provider, &headers, "responses", &rewritten).await
        }
        ResponsesMode::ChatCompletions => {
            let mut chat_request = responses_to_chat_request(&rewritten);
            normalize_chat_completion_request(&mut chat_request, &state.config);
            if chat_request
                .get("stream")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                ensure_chat_stream_include_usage(&mut chat_request);
                proxy_responses_stream_via_chat(&provider, &headers, &chat_request, client_model)
                    .await
            } else {
                proxy_responses_once_via_chat(&provider, &headers, &chat_request, client_model)
                    .await
            }
        }
    };
    track_response(class, started, response)
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
    proxy_stats().record_request();
    let started = Instant::now();
    let client_model = body
        .get("model")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    if state.config.interceptor.enabled {
        let local_chat_request = anthropic_message_to_chat_request(&body);
        if let Some(response) = maybe_intercept(
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
            return response;
        }
    }

    let mut rewritten = body;
    let provider = match rewrite_proxy_request_value(&state, &headers, &mut rewritten).await {
        Ok(provider) => provider,
        Err(error_response) => return *error_response,
    };
    let class = route_class_for_base_url(&provider.config.base_url);
    log_request_value(class, &rewritten);
    if state.config.anthropic_mode == AnthropicMode::Passthrough {
        let upstream_path = if state.launcher_kind == Some(LauncherKind::Copilot) {
            "v1/messages"
        } else {
            "messages"
        };
        let response = proxy_json(&provider, &headers, upstream_path, &rewritten).await;
        return track_response(class, started, response);
    }
    let mut chat_request = anthropic_message_to_chat_request(&rewritten);
    normalize_chat_completion_request(&mut chat_request, &state.config);

    let response = if chat_request
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        ensure_chat_stream_include_usage(&mut chat_request);
        proxy_anthropic_stream_via_chat(&provider, &headers, &chat_request, client_model).await
    } else {
        proxy_anthropic_once_via_chat(&provider, &headers, &chat_request, client_model).await
    };
    track_response(class, started, response)
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
    if state.config.model_routing != ModelRoutingMode::Passthrough {
        return StatusCode::NOT_FOUND.into_response();
    }
    proxy_json(state.default_provider(), headers, upstream_path, body).await
}

async fn refresh_upstream_models(
    provider: &ProviderState,
    incoming_headers: &HeaderMap,
) -> Result<Vec<String>> {
    let meta = UpstreamRequestMeta::new(
        &provider.name,
        incoming_headers,
        Method::GET,
        "models",
        UpstreamResponseKind::Json,
        true,
    );
    let body = upstream_json(provider, incoming_headers, &meta, None)
        .await
        .with_context(|| "failed to refresh upstream /models")?;
    Ok(cache_upstream_models(provider, body, "refresh"))
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
    let default_model = if request_needs_default_model(value, &state.config) {
        Some(ensure_default_upstream_model(&provider, incoming_headers).await?)
    } else {
        None
    };

    rewrite_request_value(value, &state.config, &provider, default_model.as_deref());
    if let Some(requested) = requested_model
        && value.get("model").and_then(Value::as_str) != Some(requested.as_str())
    {
        let kind = if state.config.model_aliases.contains_key(requested.as_str()) {
            RemapKind::Alias
        } else {
            RemapKind::Auto
        };
        proxy_stats().record_remap(kind);
    }
    Ok(provider)
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
            if let Some(model) = map.get("model").and_then(Value::as_str)
                && let Some(route) = config.model_aliases.get(model)
            {
                providers.insert(route.provider(&config.default_provider).to_string());
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

    let mut request = provider
        .client
        .request(meta.method.clone(), provider.upstream_url(&meta.path))
        .headers(upstream_headers(
            &provider.config,
            incoming_headers,
            &meta.request_id,
        ));
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
                warn!(
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
        warn!(
            provider = %meta.provider,
            request_id = %meta.request_id,
            method = %meta.method,
            upstream_path = %meta.path,
            response_kind = meta.response_kind.as_str(),
            upstream_status = status.as_u16(),
            content_type = content_type.as_deref().unwrap_or(""),
            body_bytes = bytes.len(),
            body_prefix = %body_prefix(&bytes),
            "upstream returned non-success status"
        );
        return Err(UpstreamFailure::status_with_body(meta, status, &bytes));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|error| UpstreamFailure::from_reqwest(meta, error))?;
    match parse_upstream_json(&bytes) {
        Ok(body) => Ok(body),
        Err(error) => {
            let prefix = body_prefix(&bytes);
            warn!(
                provider = %meta.provider,
                request_id = %meta.request_id,
                method = %meta.method,
                upstream_path = %meta.path,
                response_kind = meta.response_kind.as_str(),
                upstream_status = status.as_u16(),
                content_type = content_type.as_deref().unwrap_or(""),
                body_bytes = bytes.len(),
                body_prefix = %prefix,
                error = %error,
                "failed to decode upstream JSON response"
            );
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
    warn!(
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
    warn!(
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
        Ok(response) => {
            if !response.status().is_success() {
                log_upstream_status(&meta, response.status(), "relay");
            }
            relay_response(&meta, response).await
        }
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
        Ok(response) => {
            if !response.status().is_success() {
                log_upstream_status(&meta, response.status(), "relay");
            }
            relay_response(&meta, response).await
        }
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

async fn proxy_responses_once_via_chat(
    provider: &ProviderState,
    incoming_headers: &HeaderMap,
    chat_request: &Value,
    client_model: Option<String>,
) -> Response {
    let meta = UpstreamRequestMeta::new(
        &provider.name,
        incoming_headers,
        Method::POST,
        "chat/completions",
        UpstreamResponseKind::Json,
        false,
    );
    match upstream_json(provider, incoming_headers, &meta, Some(chat_request)).await {
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
    let meta = UpstreamRequestMeta::new(
        &provider.name,
        incoming_headers,
        Method::POST,
        "chat/completions",
        UpstreamResponseKind::Stream,
        false,
    );
    match send_upstream_once(provider, incoming_headers, &meta, Some(chat_request)).await {
        Ok(response) if response.status().is_success() => {
            stream_chat_as_responses(response, client_model, Some(meta)).into_response()
        }
        Ok(response) => relay_response(&meta, response).await,
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
    let meta = UpstreamRequestMeta::new(
        &provider.name,
        incoming_headers,
        Method::POST,
        "chat/completions",
        UpstreamResponseKind::Json,
        false,
    );
    match upstream_json(provider, incoming_headers, &meta, Some(chat_request)).await {
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
    let meta = UpstreamRequestMeta::new(
        &provider.name,
        incoming_headers,
        Method::POST,
        "chat/completions",
        UpstreamResponseKind::Stream,
        false,
    );
    match send_upstream_once(provider, incoming_headers, &meta, Some(chat_request)).await {
        Ok(response) if response.status().is_success() => {
            stream_chat_as_anthropic(response, client_model, Some(meta)).into_response()
        }
        Ok(response) => relay_response(&meta, response).await,
        Err(error) => {
            log_upstream_failure(&error, "send");
            upstream_error_response(&error)
        }
    }
}

impl AppState {
    fn default_provider(&self) -> &ProviderState {
        self.providers
            .get(&self.config.default_provider)
            .expect("validated default provider missing from runtime state")
    }

    fn with_launcher_session(&self, session: &LauncherSession) -> Result<Self> {
        validate_launcher_runtime(&self.config)?;

        let mut config = (*self.config).clone();
        config.default_provider = LAUNCHER_EXTERNAL_PROVIDER.to_string();
        config.model_aliases.clear();
        config.model_routing = ModelRoutingMode::Passthrough;
        config.interceptor.enabled = true;
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

        let mut providers = self.providers.as_ref().clone();
        providers.insert(
            LAUNCHER_EXTERNAL_PROVIDER.to_string(),
            ProviderState {
                name: LAUNCHER_EXTERNAL_PROVIDER.to_string(),
                config: Arc::new(external_config),
                client: template_provider.client.clone(),
                model_cache: Arc::new(ModelCache::default()),
                model_refresh: Arc::new(AsyncMutex::new(())),
            },
        );

        Ok(Self {
            config: Arc::new(config),
            providers: Arc::new(providers),
            launcher_kind: Some(session.kind()),
        })
    }
}

impl ProviderState {
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

    if let Some(api_key) = &config.api_key {
        if let Ok(value) = HeaderValue::from_str(&format!("Bearer {api_key}")) {
            headers.insert(AUTHORIZATION, value);
        }
    } else if let Some(value) = incoming_headers.get(AUTHORIZATION) {
        headers.insert(AUTHORIZATION, value.clone());
    }

    for (name, value) in incoming_headers {
        if name != AUTHORIZATION && should_forward_incoming_header(name) {
            headers.insert(name.clone(), value.clone());
        }
    }

    for (name, value) in &config.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }

    if let Ok(value) = HeaderValue::from_str(request_id) {
        headers.insert(HeaderName::from_static("x-request-id"), value);
    }

    headers
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
    ) || name.starts_with("anthropic-")
        || name.starts_with("copilot-")
        || name.starts_with("openai-")
        || name.starts_with("x-anthropic-")
        || name.starts_with("x-codex-")
        || name.starts_with("x-copilot-")
        || name.starts_with("x-github-")
        || name.starts_with("x-oai-")
        || name.starts_with("x-openai-")
        || name.starts_with("x-responsesapi-")
        || name.starts_with("x-stainless-")
}

async fn relay_response(meta: &UpstreamRequestMeta, response: reqwest::Response) -> Response {
    let status = response.status();
    let response_headers = response.headers().clone();
    let content_type = response_headers.get(CONTENT_TYPE).cloned();
    let is_sse = content_type
        .as_ref()
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_event_stream_content_type);
    let stream_meta = meta.clone();
    let stream = response.bytes_stream().map(move |chunk| match chunk {
        Ok(chunk) => Ok(chunk),
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
    ) || name.starts_with("anthropic-")
        || name.starts_with("copilot-")
        || name.starts_with("openai-")
        || name.starts_with("x-anthropic-")
        || name.starts_with("x-codex-")
        || name.starts_with("x-copilot-")
        || name.starts_with("x-github-")
        || name.starts_with("x-oai-")
        || name.starts_with("x-openai-")
        || name.starts_with("x-ratelimit-")
        || name.starts_with("x-request-")
        || name.starts_with("x-responsesapi-")
}

fn upstream_error_response(error: &UpstreamFailure) -> Response {
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

fn append_provider_models(data: &mut Vec<Value>, upstream: Value, provider: &str) {
    let models = upstream
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| upstream.as_array());
    let Some(models) = models else {
        return;
    };
    for model in models {
        let mut model = model.clone();
        if let Some(object) = model.as_object_mut() {
            object.insert(
                "proxy_provider".to_string(),
                Value::String(provider.to_string()),
            );
        }
        data.push(model);
    }
}

fn rewrite_models_response(mut upstream: Value, config: &Config) -> Value {
    if !config.models.expose_aliases || config.model_aliases.is_empty() {
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

    if !upstream.is_object() {
        upstream = json!({ "object": "list" });
    }
    let object = upstream.as_object_mut().expect("upstream is object");
    object.insert("object".to_string(), Value::String("list".to_string()));
    object.insert("data".to_string(), Value::Array(data));
    upstream
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
            if let Some(converted) = anthropic_message_item_to_chat(message) {
                messages.push(converted);
            }
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

fn anthropic_message_item_to_chat(message: &Value) -> Option<Value> {
    let object = message.as_object()?;
    let role = object
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("user")
        .to_string();
    let content = object
        .get("content")
        .map(anthropic_content_to_text)
        .unwrap_or_default();

    let mut converted = Map::from_iter([
        ("role".to_string(), Value::String(role.clone())),
        ("content".to_string(), Value::String(content)),
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

    Some(Value::Object(converted))
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
            let text_parts: Vec<String> = parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .and_then(Value::as_str)
                        .or_else(|| part.get("input_text").and_then(Value::as_str))
                        .or_else(|| part.get("output_text").and_then(Value::as_str))
                        .map(ToOwned::to_owned)
                })
                .collect();
            if text_parts.is_empty() {
                Value::String(text_from_value(content))
            } else {
                Value::String(text_parts.join("\n"))
            }
        }
        Value::String(_) => content.clone(),
        other => Value::String(text_from_value(other)),
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
                        warn!(
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
                        warn!(
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
        warn!(
            request_id = %meta.request_id,
            method = %meta.method,
            upstream_path = %meta.path,
            response_kind = meta.response_kind.as_str(),
            "upstream SSE stream ended with an incomplete event"
        );
    } else {
        warn!("upstream SSE stream ended with an incomplete event");
    }
}

fn log_malformed_sse_data(meta: Option<&UpstreamRequestMeta>, data: &str) {
    let prefix = body_prefix(data.as_bytes());
    if let Some(meta) = meta {
        warn!(
            request_id = %meta.request_id,
            method = %meta.method,
            upstream_path = %meta.path,
            response_kind = meta.response_kind.as_str(),
            data_prefix = %prefix,
            "discarded malformed upstream SSE data event"
        );
    } else {
        warn!(
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
        if provider.base_url.trim().is_empty() {
            bail!("provider {name:?} base_url cannot be empty");
        }
    }
    for (alias, route) in &config.model_aliases {
        let provider = route.provider(&config.default_provider);
        if !providers.contains_key(provider) {
            bail!("model alias {alias:?} references unknown provider {provider:?}");
        }
        if route.model().trim().is_empty() {
            bail!("model alias {alias:?} has an empty upstream model");
        }
    }
    interceptor::validate_config(&config.interceptor, &providers)?;
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
mod tests {
    use super::*;

    #[test]
    fn startup_banner_shows_proxy_and_admin_destinations() {
        let info = RuntimeInfo {
            pid: 42,
            proxy_url: "http://127.0.0.1:8080/".to_string(),
            admin_url: Some("http://127.0.0.1:18080/".to_string()),
        };
        let banner = startup_banner(&info, "started");
        assert!(banner.contains("llmproxy") || banner.contains("| | |_"));
        assert!(banner.contains("Status : started"));
        assert!(banner.contains("Proxy  : http://127.0.0.1:8080/"));
        assert!(banner.contains("Admin  : http://127.0.0.1:18080/"));
    }

    #[test]
    fn startup_banner_explains_when_admin_is_disabled() {
        let info = RuntimeInfo {
            pid: 42,
            proxy_url: "http://127.0.0.1:8080/".to_string(),
            admin_url: None,
        };
        assert!(startup_banner(&info, "running").contains("enable = true"));
    }

    #[test]
    fn configured_interceptor_providers_do_not_inherit_external_api_keys() {
        let mut config = Config {
            default_provider: "external".to_string(),
            providers: HashMap::from([
                ("external".to_string(), UpstreamConfig::default()),
                ("local".to_string(), UpstreamConfig::default()),
            ]),
            interceptor: InterceptorConfig {
                enabled: false,
                provider: "local".to_string(),
                model: "local-smoke".to_string(),
                ..InterceptorConfig::default()
            },
            ..Config::default()
        };
        let env_value =
            |name: &str| (name == "OPENAI_API_KEY").then(|| "external-api-key".to_string());

        finalize_config_keys_with_env(&mut config, &env_value);

        assert_eq!(
            config.providers["external"].api_key.as_deref(),
            Some("external-api-key")
        );
        assert_eq!(config.providers["local"].api_key, None);

        config.providers.get_mut("local").unwrap().api_key_env = Some("LOCAL_API_KEY".to_string());
        let env_value = |name: &str| match name {
            "OPENAI_API_KEY" => Some("external-api-key".to_string()),
            "LOCAL_API_KEY" => Some("local-api-key".to_string()),
            _ => None,
        };
        finalize_config_keys_with_env(&mut config, &env_value);
        assert_eq!(
            config.providers["local"].api_key.as_deref(),
            Some("local-api-key")
        );
    }

    #[test]
    fn disabled_legacy_interceptor_upstream_does_not_inherit_external_api_key() {
        let mut config = Config {
            default_provider: "default".to_string(),
            interceptor: InterceptorConfig {
                enabled: false,
                provider: "default".to_string(),
                model: "local-smoke".to_string(),
                ..InterceptorConfig::default()
            },
            ..Config::default()
        };
        let env_value =
            |name: &str| (name == "OPENAI_API_KEY").then(|| "external-api-key".to_string());

        finalize_config_keys_with_env(&mut config, &env_value);

        assert_eq!(config.upstream.api_key, None);
    }

    #[test]
    fn parses_agent_intercept_launcher_arguments() {
        let cli = Cli::try_parse_from(["llmproxy", "--intercept", "--codex"]).unwrap();
        assert!(cli.intercept);
        assert!(cli.codex);
        assert!(!cli.claude);
        assert!(!cli.copilot);
        assert!(cli.agent_args.is_empty());

        let cli = Cli::try_parse_from([
            "llmproxy",
            "--intercept",
            "--codex",
            "--",
            "exec",
            "--model",
            "gpt-test",
            "hello",
        ])
        .unwrap();
        assert_eq!(
            cli.agent_args,
            ["exec", "--model", "gpt-test", "hello"].map(OsString::from)
        );
        let cli = Cli::try_parse_from([
            "llmproxy",
            "--intercept",
            "--claude",
            "--",
            "--model",
            "sonnet",
            "hello",
        ])
        .unwrap();
        assert!(cli.claude);
        assert!(!cli.codex);
        assert!(!cli.copilot);
        assert_eq!(
            cli.agent_args,
            ["--model", "sonnet", "hello"].map(OsString::from)
        );
        let cli = Cli::try_parse_from([
            "llmproxy",
            "--intercept",
            "--copilot",
            "--",
            "--model",
            "gpt-5.4",
        ])
        .unwrap();
        assert!(cli.copilot);
        assert!(!cli.codex);
        assert!(!cli.claude);
        assert_eq!(cli.agent_args, ["--model", "gpt-5.4"].map(OsString::from));
        assert!(Cli::try_parse_from(["llmproxy", "--codex"]).is_err());
        assert!(Cli::try_parse_from(["llmproxy", "--claude"]).is_err());
        assert!(Cli::try_parse_from(["llmproxy", "--copilot"]).is_err());
        assert!(Cli::try_parse_from(["llmproxy", "--intercept", "--codex", "--claude"]).is_err());
        assert!(Cli::try_parse_from(["llmproxy", "--intercept", "--claude", "--copilot"]).is_err());
        assert!(Cli::try_parse_from(["llmproxy", "--", "--model", "sonnet"]).is_err());
    }

    fn test_config() -> Config {
        Config {
            model_aliases: HashMap::from([(
                "codex-qwen".to_string(),
                ModelAlias::Model("Qwen/Qwen2.5-Coder-32B-Instruct".to_string()),
            )]),
            role_map: HashMap::from([("developer".to_string(), "system".to_string())]),
            ..Config::default()
        }
    }

    fn test_state(upstream_base_url: String) -> AppState {
        let mut config = Config::default();
        config.upstream.base_url = upstream_base_url;
        test_state_with_config(config)
    }

    fn test_state_with_config(config: Config) -> AppState {
        let provider_configs = effective_provider_configs(&config).unwrap();
        let providers = provider_configs
            .into_iter()
            .map(|(name, upstream)| {
                (
                    name.clone(),
                    ProviderState {
                        name,
                        config: Arc::new(upstream),
                        client: Client::new(),
                        model_cache: Arc::new(ModelCache::default()),
                        model_refresh: Arc::new(AsyncMutex::new(())),
                    },
                )
            })
            .collect();
        AppState {
            config: Arc::new(config),
            providers: Arc::new(providers),
            launcher_kind: None,
        }
    }

    fn test_provider(upstream_base_url: String) -> ProviderState {
        let state = test_state(upstream_base_url);
        state.default_provider().clone()
    }

    async fn spawn_test_upstream(app: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}/v1"), server)
    }

    fn local_interceptor_config(provider: &str, model: &str) -> InterceptorConfig {
        InterceptorConfig {
            enabled: true,
            provider: provider.to_string(),
            model: model.to_string(),
            timeout_ms: 2_000,
            fallback_on_error: true,
            ..InterceptorConfig::default()
        }
    }

    fn chat_completion(model: &str, content: Value, tool_calls: Value) -> Value {
        let has_tool_calls = tool_calls.as_array().is_some_and(|calls| !calls.is_empty());
        json!({
            "id": "chatcmpl-local",
            "object": "chat.completion",
            "created": 1,
            "model": model,
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": content,
                    "tool_calls": tool_calls
                },
                "finish_reason": if has_tool_calls {
                    "tool_calls"
                } else {
                    "stop"
                }
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 3,
                "total_tokens": 13
            }
        })
    }

    fn pass_completion() -> Value {
        chat_completion(
            "local-fast",
            Value::Null,
            json!([{
                "id": "call-pass",
                "type": "function",
                "function": {
                    "name": "PASS",
                    "arguments": "{}"
                }
            }]),
        )
    }

    #[tokio::test]
    async fn interceptor_pass_preserves_external_model_remapping_and_original_request() {
        let local_body = Arc::new(Mutex::new(None::<Value>));
        let local_authorization = Arc::new(Mutex::new(None::<String>));
        let local_body_for_handler = local_body.clone();
        let local_authorization_for_handler = local_authorization.clone();
        let local_app = Router::new().route(
            "/v1/chat/completions",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let local_body = local_body_for_handler.clone();
                let local_authorization = local_authorization_for_handler.clone();
                async move {
                    *local_body.lock().unwrap() = Some(body);
                    *local_authorization.lock().unwrap() = headers
                        .get(AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        .map(ToOwned::to_owned);
                    Json(pass_completion())
                }
            }),
        );
        let (local_base_url, local_server) = spawn_test_upstream(local_app).await;

        let external_body = Arc::new(Mutex::new(None::<Value>));
        let external_body_for_handler = external_body.clone();
        let external_app = Router::new().route(
            "/v1/chat/completions",
            post(move |Json(body): Json<Value>| {
                let external_body = external_body_for_handler.clone();
                async move {
                    *external_body.lock().unwrap() = Some(body);
                    Json(chat_completion(
                        "destination-model-y",
                        json!("external answer"),
                        json!([]),
                    ))
                }
            }),
        );
        let (external_base_url, external_server) = spawn_test_upstream(external_app).await;

        let mut config = Config::default();
        config.upstream.base_url = external_base_url;
        config.providers.insert(
            "local".to_string(),
            UpstreamConfig {
                base_url: local_base_url,
                ..UpstreamConfig::default()
            },
        );
        config.interceptor = local_interceptor_config("local", "local-fast");
        config.model_aliases.insert(
            "agent-openai".to_string(),
            ModelAlias::Model("destination-model-y".to_string()),
        );
        let state = test_state_with_config(config);
        let original = json!({
            "model": "agent-openai",
            "stream": false,
            "messages": [{ "role": "user", "content": "inspect this carefully" }],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "exec_command",
                    "parameters": { "type": "object" }
                }
            }],
            "metadata": { "preserve": true }
        });
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer placeholder"),
        );

        let response =
            chat_completions(State(SharedState::new(state)), headers, Json(original)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let response_body: Value =
            serde_json::from_str(&collect_response_text(response).await).unwrap();
        assert_eq!(
            response_body["choices"][0]["message"]["content"],
            "external answer"
        );

        let local = local_body.lock().unwrap().clone().unwrap();
        assert_eq!(local["model"], "local-fast");
        assert_eq!(local["stream"], false);
        assert!(
            local["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| { tool["function"]["name"].as_str() == Some("PASS") })
        );
        assert!(local_authorization.lock().unwrap().is_none());

        let external = external_body.lock().unwrap().clone().unwrap();
        assert_eq!(external["model"], "destination-model-y");
        assert_eq!(external["metadata"]["preserve"], true);
        assert_eq!(external["tools"].as_array().unwrap().len(), 1);
        assert!(
            external["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|tool| { tool["function"]["name"].as_str() != Some("PASS") })
        );
        assert!(
            external["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|message| {
                    !text_from_value(&message["content"]).contains("local execution path")
                })
        );

        local_server.abort();
        external_server.abort();
    }

    #[tokio::test]
    async fn interceptor_local_tool_call_streams_as_responses_without_external_request() {
        let local_body = Arc::new(Mutex::new(None::<Value>));
        let local_body_for_handler = local_body.clone();
        let local_app = Router::new().route(
            "/v1/chat/completions",
            post(move |Json(body): Json<Value>| {
                let local_body = local_body_for_handler.clone();
                async move {
                    *local_body.lock().unwrap() = Some(body);
                    Json(chat_completion(
                        "local-fast",
                        Value::Null,
                        json!([{
                            "id": "call-exec",
                            "type": "function",
                            "function": {
                                "name": "exec_command",
                                "arguments": "{\"cmd\":\"pwd\"}"
                            }
                        }]),
                    ))
                }
            }),
        );
        let (local_base_url, local_server) = spawn_test_upstream(local_app).await;
        let mut config = Config::default();
        config.providers.insert(
            "local".to_string(),
            UpstreamConfig {
                base_url: local_base_url,
                ..UpstreamConfig::default()
            },
        );
        config.interceptor = local_interceptor_config("local", "local-fast");
        let state = test_state_with_config(config);
        let body = json!({
            "model": "agent-openai",
            "stream": true,
            "instructions": "Act as an agent.",
            "input": "show the current directory",
            "tools": [{
                "type": "function",
                "name": "exec_command",
                "description": "Run a command",
                "parameters": {
                    "type": "object",
                    "properties": { "cmd": { "type": "string" } },
                    "required": ["cmd"]
                }
            }]
        });

        let response =
            responses(State(SharedState::new(state)), HeaderMap::new(), Json(body)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("x-llmproxy-route")
                .and_then(|value| value.to_str().ok()),
            Some("local")
        );
        let events = collect_sse_events(response).await;
        assert!(events.iter().any(|(event, value)| {
            event == "response.output_item.done"
                && value["item"]["type"] == "function_call"
                && value["item"]["name"] == "exec_command"
        }));
        assert!(events.iter().any(|(event, value)| {
            event == "response.completed" && value["response"]["model"] == "agent-openai"
        }));

        let local = local_body.lock().unwrap().clone().unwrap();
        assert_eq!(local["model"], "local-fast");
        assert!(local["messages"].as_array().unwrap().iter().any(|message| {
            text_from_value(&message["content"]).contains("show the current directory")
        }));
        local_server.abort();
    }

    #[tokio::test]
    async fn interceptor_local_text_streams_as_anthropic_messages() {
        let local_app = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                Json(chat_completion(
                    "local-fast",
                    json!("local answer"),
                    json!([]),
                ))
            }),
        );
        let (local_base_url, local_server) = spawn_test_upstream(local_app).await;
        let mut config = Config::default();
        config.providers.insert(
            "local".to_string(),
            UpstreamConfig {
                base_url: local_base_url,
                ..UpstreamConfig::default()
            },
        );
        config.interceptor = local_interceptor_config("local", "local-fast");
        let state = test_state_with_config(config);
        let body = json!({
            "model": "agent-claude",
            "stream": true,
            "max_tokens": 100,
            "messages": [{ "role": "user", "content": "say hello" }]
        });

        let response =
            anthropic_messages(State(SharedState::new(state)), HeaderMap::new(), Json(body)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let events = collect_sse_events(response).await;
        assert!(events.iter().any(|(event, value)| {
            event == "content_block_delta"
                && value["delta"]["type"] == "text_delta"
                && value["delta"]["text"] == "local answer"
        }));
        assert!(events.iter().any(|(event, _)| event == "message_stop"));
        local_server.abort();
    }

    #[tokio::test]
    async fn interceptor_failure_falls_back_through_existing_external_route() {
        let local_app = Router::new().route(
            "/v1/chat/completions",
            post(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "local failed") }),
        );
        let (local_base_url, local_server) = spawn_test_upstream(local_app).await;
        let external_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let external_calls_for_handler = external_calls.clone();
        let external_app = Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let external_calls = external_calls_for_handler.clone();
                async move {
                    external_calls.fetch_add(1, Ordering::Relaxed);
                    Json(chat_completion(
                        "destination-model-y",
                        json!("external fallback"),
                        json!([]),
                    ))
                }
            }),
        );
        let (external_base_url, external_server) = spawn_test_upstream(external_app).await;
        let mut config = Config::default();
        config.upstream.base_url = external_base_url;
        config.providers.insert(
            "local".to_string(),
            UpstreamConfig {
                base_url: local_base_url,
                ..UpstreamConfig::default()
            },
        );
        config.interceptor = local_interceptor_config("local", "local-fast");
        config.model_aliases.insert(
            "agent-openai".to_string(),
            ModelAlias::Model("destination-model-y".to_string()),
        );
        let state = test_state_with_config(config);
        let body = json!({
            "model": "agent-openai",
            "messages": [{ "role": "user", "content": "hard task" }]
        });

        let response =
            chat_completions(State(SharedState::new(state)), HeaderMap::new(), Json(body)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let response_body: Value =
            serde_json::from_str(&collect_response_text(response).await).unwrap();
        assert_eq!(
            response_body["choices"][0]["message"]["content"],
            "external fallback"
        );
        assert_eq!(external_calls.load(Ordering::Relaxed), 1);
        local_server.abort();
        external_server.abort();
    }

    #[tokio::test]
    async fn codex_pass_forwards_login_headers_and_preserves_model() {
        let local_app = Router::new().route(
            "/v1/chat/completions",
            post(|| async { Json(pass_completion()) }),
        );
        let (local_base_url, local_server) = spawn_test_upstream(local_app).await;

        let external_body = Arc::new(Mutex::new(None::<Value>));
        let external_headers = Arc::new(Mutex::new(None::<HeaderMap>));
        let external_body_for_handler = external_body.clone();
        let external_headers_for_handler = external_headers.clone();
        let external_app = Router::new().route(
            "/v1/responses",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let external_body = external_body_for_handler.clone();
                let external_headers = external_headers_for_handler.clone();
                async move {
                    *external_body.lock().unwrap() = Some(body);
                    *external_headers.lock().unwrap() = Some(headers);
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(CONTENT_TYPE, "text/event-stream")
                        .header("x-codex-turn-state", "sticky-route")
                        .header("x-oai-request-id", "oai-request-123")
                        .header("x-ratelimit-remaining-requests", "42")
                        .header("x-reasoning-included", "true")
                        .body(Body::from(
                            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-external\"}}\n\n",
                        ))
                        .unwrap()
                }
            }),
        );
        let (external_base_url, external_server) = spawn_test_upstream(external_app).await;

        let mut config = Config {
            default_provider: "external".to_string(),
            providers: HashMap::from([
                (
                    "external".to_string(),
                    UpstreamConfig {
                        base_url: external_base_url,
                        ..UpstreamConfig::default()
                    },
                ),
                (
                    "local".to_string(),
                    UpstreamConfig {
                        base_url: local_base_url,
                        ..UpstreamConfig::default()
                    },
                ),
            ]),
            interceptor: local_interceptor_config("local", "local-fast"),
            responses: ResponsesConfig {
                mode: ResponsesMode::Passthrough,
            },
            model_routing: ModelRoutingMode::Passthrough,
            ..Config::default()
        };
        config.model_aliases.insert(
            "gpt-codex".to_string(),
            ModelAlias::Model("must-not-rewrite".to_string()),
        );
        let state = test_state_with_config(config);
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer placeholder"),
        );
        headers.insert(
            HeaderName::from_static("chatgpt-account-id"),
            HeaderValue::from_static("account-123"),
        );
        headers.insert(
            HeaderName::from_static("originator"),
            HeaderValue::from_static("codex_cli_rs"),
        );
        headers.insert(
            HeaderName::from_static("session-id"),
            HeaderValue::from_static("session-123"),
        );
        headers.insert(
            HeaderName::from_static("x-codex-beta-features"),
            HeaderValue::from_static("feature-a"),
        );
        headers.insert(
            HeaderName::from_static("x-client-request-id"),
            HeaderValue::from_static("client-request-123"),
        );
        headers.insert(
            HeaderName::from_static("x-oai-attestation"),
            HeaderValue::from_static("attestation-123"),
        );
        let body = json!({
            "model": "gpt-codex",
            "stream": true,
            "input": "complex external task"
        });

        let response = responses(State(SharedState::new(state)), headers, Json(body)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("x-codex-turn-state")
                .and_then(|value| value.to_str().ok()),
            Some("sticky-route")
        );
        assert_eq!(
            response
                .headers()
                .get("x-oai-request-id")
                .and_then(|value| value.to_str().ok()),
            Some("oai-request-123")
        );
        assert_eq!(
            response
                .headers()
                .get("x-ratelimit-remaining-requests")
                .and_then(|value| value.to_str().ok()),
            Some("42")
        );
        assert_eq!(
            response
                .headers()
                .get("x-reasoning-included")
                .and_then(|value| value.to_str().ok()),
            Some("true")
        );
        assert!(
            collect_response_text(response)
                .await
                .contains("response.completed")
        );

        assert_eq!(
            external_body.lock().unwrap().as_ref().unwrap()["model"],
            "gpt-codex"
        );
        let captured_headers = external_headers.lock().unwrap();
        let captured_headers = captured_headers.as_ref().unwrap();
        for (name, expected) in [
            ("authorization", "Bearer placeholder"),
            ("chatgpt-account-id", "account-123"),
            ("originator", "codex_cli_rs"),
            ("session-id", "session-123"),
            ("x-codex-beta-features", "feature-a"),
            ("x-client-request-id", "client-request-123"),
            ("x-oai-attestation", "attestation-123"),
        ] {
            assert_eq!(
                captured_headers
                    .get(name)
                    .and_then(|value| value.to_str().ok()),
                Some(expected)
            );
        }

        local_server.abort();
        external_server.abort();
    }

    #[tokio::test]
    async fn launcher_codex_session_attaches_to_normal_proxy_and_preserves_model() {
        let local_body = Arc::new(Mutex::new(None::<Value>));
        let local_headers = Arc::new(Mutex::new(None::<HeaderMap>));
        let local_body_for_handler = local_body.clone();
        let local_headers_for_handler = local_headers.clone();
        let local_app = Router::new().route(
            "/v1/chat/completions",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let local_body = local_body_for_handler.clone();
                let local_headers = local_headers_for_handler.clone();
                async move {
                    *local_body.lock().unwrap() = Some(body);
                    *local_headers.lock().unwrap() = Some(headers);
                    Json(pass_completion())
                }
            }),
        );
        let (local_base_url, local_server) = spawn_test_upstream(local_app).await;

        let external_body = Arc::new(Mutex::new(None::<Value>));
        let external_body_for_handler = external_body.clone();
        let external_app = Router::new().route(
            "/v1/responses",
            post(move |Json(body): Json<Value>| {
                let external_body = external_body_for_handler.clone();
                async move {
                    *external_body.lock().unwrap() = Some(body);
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(CONTENT_TYPE, "text/event-stream")
                        .body(Body::from(
                            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-external\"}}\n\n",
                        ))
                        .unwrap()
                }
            }),
        );
        let (external_base_url, external_server) = spawn_test_upstream(external_app).await;

        let mut interceptor = local_interceptor_config("local", "local-fast");
        interceptor.enabled = false;
        let mut config = Config::default();
        config.providers.insert(
            "local".to_string(),
            UpstreamConfig {
                base_url: local_base_url,
                ..UpstreamConfig::default()
            },
        );
        config.interceptor = interceptor;
        config.model_aliases.insert(
            "gpt-codex".to_string(),
            ModelAlias::Model("must-not-rewrite".to_string()),
        );

        let token = LauncherSession::new(LauncherKind::Codex, external_base_url)
            .unwrap()
            .encode()
            .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer placeholder"),
        );
        let body = json!({
            "model": "gpt-codex",
            "stream": true,
            "input": "complex external task"
        });

        let response = launcher_responses(
            AxumPath(token),
            State(SharedState::new(test_state_with_config(config))),
            headers,
            Json(body),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            collect_response_text(response)
                .await
                .contains("response.completed")
        );

        assert_eq!(
            local_body.lock().unwrap().as_ref().unwrap()["model"],
            "local-fast"
        );
        assert!(
            local_headers
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .get(AUTHORIZATION)
                .is_none()
        );
        assert_eq!(
            external_body.lock().unwrap().as_ref().unwrap()["model"],
            "gpt-codex"
        );

        local_server.abort();
        external_server.abort();
    }

    #[tokio::test]
    async fn launcher_copilot_pass_is_credential_blind_and_preserves_native_chat() {
        let local_body = Arc::new(Mutex::new(None::<Value>));
        let local_headers = Arc::new(Mutex::new(None::<HeaderMap>));
        let local_body_for_handler = local_body.clone();
        let local_headers_for_handler = local_headers.clone();
        let local_app = Router::new().route(
            "/v1/chat/completions",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let local_body = local_body_for_handler.clone();
                let local_headers = local_headers_for_handler.clone();
                async move {
                    *local_body.lock().unwrap() = Some(body);
                    *local_headers.lock().unwrap() = Some(headers);
                    Json(pass_completion())
                }
            }),
        );
        let (local_base_url, local_server) = spawn_test_upstream(local_app).await;

        let external_body = Arc::new(Mutex::new(None::<Value>));
        let external_headers = Arc::new(Mutex::new(None::<HeaderMap>));
        let external_body_for_handler = external_body.clone();
        let external_headers_for_handler = external_headers.clone();
        let external_app = Router::new().route(
            "/chat/completions",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let external_body = external_body_for_handler.clone();
                let external_headers = external_headers_for_handler.clone();
                async move {
                    *external_body.lock().unwrap() = Some(body);
                    *external_headers.lock().unwrap() = Some(headers);
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(CONTENT_TYPE, "application/json")
                        .header("x-github-request-id", "req-copilot")
                        .header("x-copilot-model", "gpt-5.4")
                        .body(Body::from(
                            json!({
                                "id": "chatcmpl-copilot",
                                "object": "chat.completion",
                                "choices": [{
                                    "index": 0,
                                    "message": { "role": "assistant", "content": "external" },
                                    "finish_reason": "stop"
                                }]
                            })
                            .to_string(),
                        ))
                        .unwrap()
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let external_addr = listener.local_addr().unwrap();
        let external_server = tokio::spawn(async move {
            axum::serve(listener, external_app).await.unwrap();
        });
        let external_base_url = format!("http://{external_addr}");

        let mut config = Config::default();
        config.providers.insert(
            "local".to_string(),
            UpstreamConfig {
                base_url: local_base_url,
                ..UpstreamConfig::default()
            },
        );
        config.interceptor = local_interceptor_config("local", "local-fast");
        config
            .role_map
            .insert("developer".to_string(), "system-must-not-leak".to_string());
        config.model_aliases.insert(
            "gpt-5.4".to_string(),
            ModelAlias::Model("must-not-rewrite".to_string()),
        );

        let token = LauncherSession::new(LauncherKind::Copilot, external_base_url)
            .unwrap()
            .encode()
            .unwrap();
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("authorization", "Bearer placeholder"),
            ("copilot-integration-id", "copilot-cli"),
            ("openai-intent", "conversation-agent"),
            ("x-github-api-version", "2025-04-01"),
            ("x-initiator", "user"),
            ("x-interaction-id", "interaction-123"),
            ("x-copilot-client-timezone", "Europe/London"),
        ] {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        let original = json!({
            "model": "gpt-5.4",
            "stream": false,
            "messages": [
                { "role": "developer", "content": "Use AGENTS.md." },
                { "role": "user", "content": "complex external task" }
            ],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "shell",
                    "description": "Run a command",
                    "parameters": { "type": "object" }
                }
            }],
            "copilot_thread_id": "thread-123"
        });

        let response = launcher_chat_completions(
            AxumPath(token),
            State(SharedState::new(test_state_with_config(config))),
            headers,
            Json(original.clone()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("x-github-request-id")
                .and_then(|value| value.to_str().ok()),
            Some("req-copilot")
        );
        assert_eq!(
            response
                .headers()
                .get("x-copilot-model")
                .and_then(|value| value.to_str().ok()),
            Some("gpt-5.4")
        );

        assert_eq!(*external_body.lock().unwrap(), Some(original));
        let captured_external_headers = external_headers.lock().unwrap();
        let captured_external_headers = captured_external_headers.as_ref().unwrap();
        for (name, expected) in [
            ("authorization", "Bearer placeholder"),
            ("copilot-integration-id", "copilot-cli"),
            ("openai-intent", "conversation-agent"),
            ("x-github-api-version", "2025-04-01"),
            ("x-initiator", "user"),
            ("x-interaction-id", "interaction-123"),
            ("x-copilot-client-timezone", "Europe/London"),
        ] {
            assert_eq!(
                captured_external_headers
                    .get(name)
                    .and_then(|value| value.to_str().ok()),
                Some(expected)
            );
        }

        assert_eq!(
            local_body.lock().unwrap().as_ref().unwrap()["model"],
            "local-fast"
        );
        let captured_local_headers = local_headers.lock().unwrap();
        let captured_local_headers = captured_local_headers.as_ref().unwrap();
        for name in [
            "authorization",
            "copilot-integration-id",
            "openai-intent",
            "x-github-api-version",
            "x-interaction-id",
            "x-copilot-client-timezone",
        ] {
            assert!(captured_local_headers.get(name).is_none(), "leaked {name}");
        }

        local_server.abort();
        external_server.abort();
    }

    #[tokio::test]
    async fn launcher_copilot_models_relays_native_status_and_headers() {
        let captured_headers = Arc::new(Mutex::new(None::<HeaderMap>));
        let captured_headers_for_handler = captured_headers.clone();
        let external_app = Router::new().route(
            "/models",
            get(move |headers: HeaderMap| {
                let captured_headers = captured_headers_for_handler.clone();
                async move {
                    *captured_headers.lock().unwrap() = Some(headers);
                    Response::builder()
                        .status(StatusCode::UNAUTHORIZED)
                        .header(CONTENT_TYPE, "application/json")
                        .header("x-github-request-id", "req-models")
                        .body(Body::from(r#"{"error":"authentication failed"}"#))
                        .unwrap()
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let external_addr = listener.local_addr().unwrap();
        let external_server = tokio::spawn(async move {
            axum::serve(listener, external_app).await.unwrap();
        });

        let token = LauncherSession::new(LauncherKind::Copilot, format!("http://{external_addr}"))
            .unwrap()
            .encode()
            .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer placeholder"),
        );
        headers.insert(
            HeaderName::from_static("copilot-integration-id"),
            HeaderValue::from_static("copilot-cli"),
        );

        let mut config = Config::default();
        config.upstream.base_url = "http://unused.invalid/v1".to_string();
        config.interceptor = local_interceptor_config("default", "local-fast");
        let response = launcher_copilot_models(
            AxumPath(token),
            State(SharedState::new(test_state_with_config(config))),
            headers,
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response
                .headers()
                .get("x-github-request-id")
                .and_then(|value| value.to_str().ok()),
            Some("req-models")
        );
        let captured_headers = captured_headers.lock().unwrap();
        let captured_headers = captured_headers.as_ref().unwrap();
        assert_eq!(
            captured_headers
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer placeholder")
        );
        assert_eq!(
            captured_headers
                .get("copilot-integration-id")
                .and_then(|value| value.to_str().ok()),
            Some("copilot-cli")
        );

        external_server.abort();
    }

    #[tokio::test]
    async fn launcher_copilot_preserves_native_anthropic_capi_path() {
        let local_app = Router::new().route(
            "/v1/chat/completions",
            post(|| async { Json(pass_completion()) }),
        );
        let (local_base_url, local_server) = spawn_test_upstream(local_app).await;

        let external_calls = Arc::new(AtomicU64::new(0));
        let external_calls_for_handler = external_calls.clone();
        let external_app = Router::new().route(
            "/v1/messages",
            post(move || {
                let external_calls = external_calls_for_handler.clone();
                async move {
                    external_calls.fetch_add(1, Ordering::Relaxed);
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(CONTENT_TYPE, "application/json")
                        .body(Body::from(
                            json!({
                                "id": "msg-copilot",
                                "type": "message",
                                "role": "assistant",
                                "content": [{ "type": "text", "text": "external" }],
                                "model": "claude-sonnet-4.5",
                                "stop_reason": "end_turn",
                                "usage": { "input_tokens": 1, "output_tokens": 1 }
                            })
                            .to_string(),
                        ))
                        .unwrap()
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let external_addr = listener.local_addr().unwrap();
        let external_server = tokio::spawn(async move {
            axum::serve(listener, external_app).await.unwrap();
        });

        let mut config = Config::default();
        config.providers.insert(
            "local".to_string(),
            UpstreamConfig {
                base_url: local_base_url,
                ..UpstreamConfig::default()
            },
        );
        config.interceptor = local_interceptor_config("local", "local-fast");
        let token = LauncherSession::new(LauncherKind::Copilot, format!("http://{external_addr}"))
            .unwrap()
            .encode()
            .unwrap();
        let original = json!({
            "model": "claude-sonnet-4.5",
            "max_tokens": 100,
            "messages": [{ "role": "user", "content": "complex external task" }]
        });

        let response = launcher_anthropic_messages(
            AxumPath(token),
            State(SharedState::new(test_state_with_config(config))),
            HeaderMap::new(),
            Json(original),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(external_calls.load(Ordering::Relaxed), 1);

        local_server.abort();
        external_server.abort();
    }

    #[tokio::test]
    async fn launcher_codex_control_request_bypasses_local_interceptor() {
        let local_calls = Arc::new(AtomicU64::new(0));
        let local_calls_for_handler = local_calls.clone();
        let local_app = Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let local_calls = local_calls_for_handler.clone();
                async move {
                    local_calls.fetch_add(1, Ordering::Relaxed);
                    Json(chat_completion(
                        "local-fast",
                        json!("incorrect local response"),
                        json!([]),
                    ))
                }
            }),
        );
        let (local_base_url, local_server) = spawn_test_upstream(local_app).await;

        let external_calls = Arc::new(AtomicU64::new(0));
        let external_body = Arc::new(Mutex::new(None::<Value>));
        let external_headers = Arc::new(Mutex::new(None::<HeaderMap>));
        let external_calls_for_handler = external_calls.clone();
        let external_body_for_handler = external_body.clone();
        let external_headers_for_handler = external_headers.clone();
        let external_app = Router::new().route(
            "/v1/responses",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let external_calls = external_calls_for_handler.clone();
                let external_body = external_body_for_handler.clone();
                let external_headers = external_headers_for_handler.clone();
                async move {
                    external_calls.fetch_add(1, Ordering::Relaxed);
                    *external_body.lock().unwrap() = Some(body);
                    *external_headers.lock().unwrap() = Some(headers);
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(CONTENT_TYPE, "text/event-stream")
                        .body(Body::from(
                            "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"cmp-1\",\"type\":\"compaction\",\"encrypted_content\":\"ciphertext\"}}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-compact\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
                        ))
                        .unwrap()
                }
            }),
        );
        let (external_base_url, external_server) = spawn_test_upstream(external_app).await;

        let mut interceptor = local_interceptor_config("local", "local-fast");
        interceptor.enabled = false;
        let mut config = Config::default();
        config.providers.insert(
            "local".to_string(),
            UpstreamConfig {
                base_url: local_base_url,
                ..UpstreamConfig::default()
            },
        );
        config.interceptor = interceptor;

        let token = LauncherSession::new(LauncherKind::Codex, external_base_url)
            .unwrap()
            .encode()
            .unwrap();
        let turn_metadata = json!({
            "request_kind": "compaction",
            "compaction": { "implementation": "responses_compaction_v2" }
        })
        .to_string();
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer placeholder"),
        );
        headers.insert(
            HeaderName::from_static("x-codex-turn-metadata"),
            HeaderValue::from_str(&turn_metadata).unwrap(),
        );
        let body = json!({
            "model": "gpt-codex",
            "stream": true,
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": "history" }]
                },
                { "type": "compaction_trigger" }
            ],
            "client_metadata": {
                "x-codex-turn-metadata": turn_metadata
            }
        });

        let response = launcher_responses(
            AxumPath(token),
            State(SharedState::new(test_state_with_config(config))),
            headers,
            Json(body.clone()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let response_body = collect_response_text(response).await;
        assert!(response_body.contains("\"type\":\"compaction\""));
        assert!(response_body.contains("response.completed"));

        assert_eq!(local_calls.load(Ordering::Relaxed), 0);
        assert_eq!(external_calls.load(Ordering::Relaxed), 1);
        assert_eq!(external_body.lock().unwrap().as_ref(), Some(&body));
        assert!(
            external_headers
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .contains_key("x-codex-turn-metadata")
        );

        local_server.abort();
        external_server.abort();
    }

    #[tokio::test]
    async fn claude_pass_forwards_login_headers_and_preserves_native_request() {
        let local_headers = Arc::new(Mutex::new(None::<HeaderMap>));
        let local_headers_for_handler = local_headers.clone();
        let local_app = Router::new().route(
            "/v1/chat/completions",
            post(move |headers: HeaderMap| {
                let local_headers = local_headers_for_handler.clone();
                async move {
                    *local_headers.lock().unwrap() = Some(headers);
                    Json(pass_completion())
                }
            }),
        );
        let (local_base_url, local_server) = spawn_test_upstream(local_app).await;

        let external_body = Arc::new(Mutex::new(None::<Value>));
        let external_headers = Arc::new(Mutex::new(None::<HeaderMap>));
        let external_body_for_handler = external_body.clone();
        let external_headers_for_handler = external_headers.clone();
        let external_app = Router::new().route(
            "/v1/messages",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let external_body = external_body_for_handler.clone();
                let external_headers = external_headers_for_handler.clone();
                async move {
                    *external_body.lock().unwrap() = Some(body);
                    *external_headers.lock().unwrap() = Some(headers);
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(CONTENT_TYPE, "text/event-stream")
                        .header("request-id", "req-anthropic")
                        .header("anthropic-ratelimit-requests-remaining", "41")
                        .body(Body::from(
                            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-external\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-test\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                        ))
                        .unwrap()
                }
            }),
        );
        let (external_base_url, external_server) = spawn_test_upstream(external_app).await;

        let mut config = Config {
            default_provider: "external".to_string(),
            providers: HashMap::from([
                (
                    "external".to_string(),
                    UpstreamConfig {
                        base_url: external_base_url,
                        ..UpstreamConfig::default()
                    },
                ),
                (
                    "local".to_string(),
                    UpstreamConfig {
                        base_url: local_base_url,
                        ..UpstreamConfig::default()
                    },
                ),
            ]),
            interceptor: local_interceptor_config("local", "local-fast"),
            model_routing: ModelRoutingMode::Passthrough,
            anthropic_mode: AnthropicMode::Passthrough,
            ..Config::default()
        };
        config.model_aliases.insert(
            "claude-sonnet-test".to_string(),
            ModelAlias::Model("must-not-rewrite".to_string()),
        );
        let state = test_state_with_config(config);
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-api-key"),
            HeaderValue::from_static("api-key-from-claude"),
        );
        headers.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static("2023-06-01"),
        );
        headers.insert(
            HeaderName::from_static("anthropic-beta"),
            HeaderValue::from_static("oauth-2025-04-20"),
        );
        headers.insert(
            HeaderName::from_static("x-stainless-runtime"),
            HeaderValue::from_static("node"),
        );
        let original = json!({
            "model": "claude-sonnet-test",
            "stream": true,
            "max_tokens": 1024,
            "system": [{ "type": "text", "text": "Use the repository instructions." }],
            "messages": [{ "role": "user", "content": "complex external task" }],
            "tools": [{
                "name": "Bash",
                "description": "Run a command",
                "input_schema": { "type": "object" }
            }],
            "metadata": { "user_id": "session-test" }
        });

        let response = anthropic_messages(
            State(SharedState::new(state)),
            headers,
            Json(original.clone()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("request-id")
                .and_then(|value| value.to_str().ok()),
            Some("req-anthropic")
        );
        assert_eq!(
            response
                .headers()
                .get("anthropic-ratelimit-requests-remaining")
                .and_then(|value| value.to_str().ok()),
            Some("41")
        );
        assert!(
            collect_response_text(response)
                .await
                .contains("message_stop")
        );

        assert_eq!(*external_body.lock().unwrap(), Some(original));
        let captured_headers = external_headers.lock().unwrap();
        let captured_headers = captured_headers.as_ref().unwrap();
        for (name, expected) in [
            ("x-api-key", "api-key-from-claude"),
            ("anthropic-version", "2023-06-01"),
            ("anthropic-beta", "oauth-2025-04-20"),
            ("x-stainless-runtime", "node"),
        ] {
            assert_eq!(
                captured_headers
                    .get(name)
                    .and_then(|value| value.to_str().ok()),
                Some(expected)
            );
        }
        let local_headers = local_headers.lock().unwrap();
        let local_headers = local_headers.as_ref().unwrap();
        assert!(local_headers.get("x-api-key").is_none());
        assert!(local_headers.get(AUTHORIZATION).is_none());

        local_server.abort();
        external_server.abort();
    }

    #[tokio::test]
    async fn launcher_claude_session_attaches_to_normal_proxy_and_preserves_request() {
        let local_body = Arc::new(Mutex::new(None::<Value>));
        let local_headers = Arc::new(Mutex::new(None::<HeaderMap>));
        let local_body_for_handler = local_body.clone();
        let local_headers_for_handler = local_headers.clone();
        let local_app = Router::new().route(
            "/v1/chat/completions",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let local_body = local_body_for_handler.clone();
                let local_headers = local_headers_for_handler.clone();
                async move {
                    *local_body.lock().unwrap() = Some(body);
                    *local_headers.lock().unwrap() = Some(headers);
                    Json(pass_completion())
                }
            }),
        );
        let (local_base_url, local_server) = spawn_test_upstream(local_app).await;

        let external_body = Arc::new(Mutex::new(None::<Value>));
        let external_body_for_handler = external_body.clone();
        let external_app = Router::new().route(
            "/v1/messages",
            post(move |Json(body): Json<Value>| {
                let external_body = external_body_for_handler.clone();
                async move {
                    *external_body.lock().unwrap() = Some(body);
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(CONTENT_TYPE, "text/event-stream")
                        .body(Body::from(
                            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-external\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-test\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                        ))
                        .unwrap()
                }
            }),
        );
        let (external_base_url, external_server) = spawn_test_upstream(external_app).await;

        let mut interceptor = local_interceptor_config("local", "local-fast");
        interceptor.enabled = false;
        let mut config = Config::default();
        config.providers.insert(
            "local".to_string(),
            UpstreamConfig {
                base_url: local_base_url,
                ..UpstreamConfig::default()
            },
        );
        config.interceptor = interceptor;
        config.model_aliases.insert(
            "claude-sonnet-test".to_string(),
            ModelAlias::Model("must-not-rewrite".to_string()),
        );

        let token = LauncherSession::new(LauncherKind::Claude, external_base_url)
            .unwrap()
            .encode()
            .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-api-key"),
            HeaderValue::from_static("api-key-from-claude"),
        );
        let original = json!({
            "model": "claude-sonnet-test",
            "stream": true,
            "max_tokens": 1024,
            "messages": [{ "role": "user", "content": "complex external task" }]
        });

        let response = launcher_anthropic_messages(
            AxumPath(token),
            State(SharedState::new(test_state_with_config(config))),
            headers,
            Json(original.clone()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            collect_response_text(response)
                .await
                .contains("message_stop")
        );

        assert_eq!(
            local_body.lock().unwrap().as_ref().unwrap()["model"],
            "local-fast"
        );
        assert!(
            local_headers
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .get("x-api-key")
                .is_none()
        );
        assert_eq!(external_body.lock().unwrap().as_ref().unwrap(), &original);

        local_server.abort();
        external_server.abort();
    }

    #[test]
    fn parses_legacy_and_provider_routed_aliases() {
        let config: Config = toml::from_str(
            r#"
                default_provider = "default"

                [upstream]
                base_url = "http://127.0.0.1:8000/v1"

                [providers.embedding]
                base_url = "http://127.0.0.1:18000/v1"

                [model_aliases]
                chat = "upstream-chat"
                embedding = { provider = "embedding", model = "upstream-embedding" }
            "#,
        )
        .unwrap();

        assert_eq!(
            config.model_aliases.get("chat"),
            Some(&ModelAlias::Model("upstream-chat".to_string()))
        );
        assert_eq!(
            config.model_aliases.get("embedding"),
            Some(&ModelAlias::Route {
                provider: "embedding".to_string(),
                model: "upstream-embedding".to_string(),
            })
        );
        assert_eq!(effective_provider_configs(&config).unwrap().len(), 2);
        validate_config(&config).unwrap();
    }

    #[tokio::test]
    async fn routed_embedding_alias_uses_selected_provider_and_model() {
        let seen_body = Arc::new(Mutex::new(None::<Value>));
        let seen_body_for_handler = seen_body.clone();
        let app = Router::new().route(
            "/v1/embeddings",
            post(move |Json(body): Json<Value>| {
                let seen_body = seen_body_for_handler.clone();
                async move {
                    *seen_body.lock().unwrap() = Some(body);
                    Json(json!({ "object": "list", "data": [] }))
                }
            }),
        );
        let (base_url, server) = spawn_test_upstream(app).await;
        let mut config = Config::default();
        config.providers.insert(
            "embedding".to_string(),
            UpstreamConfig {
                base_url,
                ..UpstreamConfig::default()
            },
        );
        config.model_aliases.insert(
            "embedding".to_string(),
            ModelAlias::Route {
                provider: "embedding".to_string(),
                model: "Qwen/Qwen3-Embedding-0.6B".to_string(),
            },
        );
        let state = test_state_with_config(config);
        let mut body = json!({ "model": "embedding", "input": "hello" });

        let provider = rewrite_proxy_request_value(&state, &HeaderMap::new(), &mut body)
            .await
            .unwrap();
        assert_eq!(provider.name, "embedding");
        assert_eq!(body["model"], "Qwen/Qwen3-Embedding-0.6B");

        let response = proxy_json(&provider, &HeaderMap::new(), "embeddings", &body).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            seen_body.lock().unwrap().as_ref().unwrap()["model"],
            "Qwen/Qwen3-Embedding-0.6B"
        );
        server.abort();
    }

    #[test]
    fn rejects_one_request_routed_to_multiple_providers() {
        let mut config = Config::default();
        config.providers.insert(
            "second".to_string(),
            UpstreamConfig {
                base_url: "http://127.0.0.1:18000/v1".to_string(),
                ..UpstreamConfig::default()
            },
        );
        config.model_aliases.insert(
            "first-model".to_string(),
            ModelAlias::Model("upstream-first".to_string()),
        );
        config.model_aliases.insert(
            "second-model".to_string(),
            ModelAlias::Route {
                provider: "second".to_string(),
                model: "upstream-second".to_string(),
            },
        );
        let state = test_state_with_config(config);
        let response = route_request(
            &state,
            &json!({
                "model": "first-model",
                "nested": { "model": "second-model" }
            }),
        )
        .err()
        .expect("mixed-provider request should fail");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn parse_upstream_json_accepts_valid_json_and_rejects_invalid_json() {
        let value = parse_upstream_json(br#"{"data":[{"id":"qwen"}]}"#).unwrap();
        assert_eq!(value["data"][0]["id"], "qwen");

        let large_body = format!(r#"{{"data":"{}"}}"#, "x".repeat(SIMD_JSON_MIN_BYTES));
        let value = parse_upstream_json(large_body.as_bytes()).unwrap();
        assert_eq!(value["data"].as_str().unwrap().len(), SIMD_JSON_MIN_BYTES);

        let error = parse_upstream_json(br#"{"data":["#).unwrap_err();
        assert!(!error.is_empty());
    }

    #[test]
    fn rewrites_model_alias_and_roles_recursively() {
        let mut value = json!({
            "model": "codex-qwen",
            "messages": [
                { "role": "developer", "content": "be concise" },
                { "role": "user", "content": "hi" }
            ],
            "nested": { "role": "developer" }
        });

        let provider = test_provider("http://127.0.0.1:1/v1".to_string());
        rewrite_request_value(&mut value, &test_config(), &provider, None);

        assert_eq!(value["model"], "Qwen/Qwen2.5-Coder-32B-Instruct");
        assert_eq!(value["messages"][0]["role"], "system");
        assert_eq!(value["nested"]["role"], "system");
    }

    #[test]
    fn rewrites_non_alias_models_to_default_upstream_model() {
        let mut value = json!({
            "model": "gpt-5.5",
            "messages": [
                { "role": "developer", "content": "be concise" }
            ],
            "nested": { "model": "codex-qwen" }
        });
        let provider = test_provider("http://127.0.0.1:1/v1".to_string());

        assert!(request_needs_default_model(&value, &test_config()));
        rewrite_request_value(
            &mut value,
            &test_config(),
            &provider,
            Some("Qwen/Qwen3-Coder-30B-A3B-Instruct"),
        );

        assert_eq!(value["model"], "Qwen/Qwen3-Coder-30B-A3B-Instruct");
        assert_eq!(value["messages"][0]["role"], "system");
        assert_eq!(value["nested"]["model"], "Qwen/Qwen2.5-Coder-32B-Instruct");
    }

    #[test]
    fn extracts_upstream_model_ids_in_order() {
        let upstream = json!({
            "object": "list",
            "data": [
                { "id": "first-model" },
                { "id": "second-model" },
                { "id": "first-model" },
                { "object": "model" }
            ]
        });

        assert_eq!(
            extract_model_ids(&upstream),
            vec!["first-model", "second-model"]
        );
    }

    #[test]
    fn model_cache_reports_update_state() {
        let model_cache = ModelCache::default();

        let upstream = json!({ "data": [{ "id": "first-model" }] });
        assert!(
            model_cache.replace_upstream_models(vec!["first-model".to_string()], upstream.clone())
        );
        assert!(!model_cache.replace_upstream_models(vec!["first-model".to_string()], upstream));
        assert_eq!(
            model_cache.first_upstream_model().as_deref(),
            Some("first-model")
        );
    }

    #[tokio::test]
    async fn generation_decode_errors_do_not_invalidate_upstream_model_cache() {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|| async { (StatusCode::OK, [("content-type", "application/json")], "{") }),
        );
        let (base_url, server) = spawn_test_upstream(app).await;
        let state = test_state(base_url);
        state
            .default_provider()
            .model_cache
            .replace_upstream_models(
                vec!["cached-model".to_string()],
                json!({ "data": [{ "id": "cached-model" }] }),
            );

        let response = proxy_responses_once_via_chat(
            state.default_provider(),
            &HeaderMap::new(),
            &json!({
                "model": "cached-model",
                "messages": [{ "role": "user", "content": "hi" }]
            }),
            Some("client-model".to_string()),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            state
                .default_provider()
                .model_cache
                .first_upstream_model()
                .as_deref(),
            Some("cached-model")
        );

        server.abort();
    }

    #[tokio::test]
    async fn upstream_status_errors_preserve_status_and_include_body_detail() {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": {
                            "message": "Stream options can only be defined when stream=True.",
                            "type": "BadRequestError",
                            "param": "stream_options",
                            "code": 400
                        }
                    })),
                )
            }),
        );
        let (base_url, server) = spawn_test_upstream(app).await;
        let state = test_state(base_url);

        let response = proxy_responses_once_via_chat(
            state.default_provider(),
            &HeaderMap::new(),
            &json!({
                "model": "upstream-model",
                "messages": [{ "role": "user", "content": "hi" }]
            }),
            Some("client-model".to_string()),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = collect_response_text(response).await;
        assert!(body.contains("Stream options can only be defined"));
        assert!(body.contains("\"upstream_status\":400"));

        server.abort();
    }

    #[tokio::test]
    async fn alias_only_request_rewrite_skips_upstream_model_lookup() {
        let attempts = Arc::new(AtomicU64::new(0));
        let attempts_for_handler = attempts.clone();
        let app = Router::new().route(
            "/v1/models",
            get(move || {
                let attempts = attempts_for_handler.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Json(json!({ "data": [{ "id": "upstream-model" }] }))
                }
            }),
        );
        let (base_url, server) = spawn_test_upstream(app).await;
        let mut config = test_config();
        config.upstream.base_url = base_url;
        let state = test_state_with_config(config);
        let mut body = json!({
            "model": "codex-qwen",
            "messages": [{ "role": "developer", "content": "hi" }]
        });

        rewrite_proxy_request_value(&state, &HeaderMap::new(), &mut body)
            .await
            .unwrap();

        assert_eq!(attempts.load(Ordering::SeqCst), 0);
        assert_eq!(body["model"], "Qwen/Qwen2.5-Coder-32B-Instruct");
        assert_eq!(body["messages"][0]["role"], "system");
        server.abort();
    }

    #[tokio::test]
    async fn non_alias_request_rewrite_still_loads_default_upstream_model() {
        let attempts = Arc::new(AtomicU64::new(0));
        let attempts_for_handler = attempts.clone();
        let app = Router::new().route(
            "/v1/models",
            get(move || {
                let attempts = attempts_for_handler.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Json(json!({ "data": [{ "id": "default-upstream-model" }] }))
                }
            }),
        );
        let (base_url, server) = spawn_test_upstream(app).await;
        let mut config = test_config();
        config.upstream.base_url = base_url;
        let state = test_state_with_config(config);
        let mut body = json!({
            "model": "unknown-client-model",
            "messages": [{ "role": "user", "content": "hi" }]
        });

        rewrite_proxy_request_value(&state, &HeaderMap::new(), &mut body)
            .await
            .unwrap();

        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(body["model"], "default-upstream-model");
        server.abort();
    }

    #[tokio::test]
    async fn upstream_requests_forward_or_generate_request_ids() {
        let seen_request_id = Arc::new(Mutex::new(None::<String>));
        let seen_request_id_for_handler = seen_request_id.clone();
        let app = Router::new().route(
            "/v1/models",
            get(move |headers: HeaderMap| {
                let seen_request_id = seen_request_id_for_handler.clone();
                async move {
                    let value = headers
                        .get("x-request-id")
                        .and_then(|value| value.to_str().ok())
                        .map(ToOwned::to_owned);
                    *seen_request_id.lock().unwrap() = value;
                    Json(json!({ "data": [{ "id": "upstream-model" }] }))
                }
            }),
        );
        let (base_url, server) = spawn_test_upstream(app).await;
        let state = test_state(base_url);
        let mut headers = HeaderMap::new();
        headers.insert("x-request-id", HeaderValue::from_static("client-request-1"));

        let body = proxy_get(state.default_provider(), &headers, "models")
            .await
            .unwrap();

        assert_eq!(body["data"][0]["id"], "upstream-model");
        assert_eq!(
            seen_request_id.lock().unwrap().as_deref(),
            Some("client-request-1")
        );

        let body = proxy_get(state.default_provider(), &HeaderMap::new(), "models")
            .await
            .unwrap();
        assert_eq!(body["data"][0]["id"], "upstream-model");
        let generated_request_id = seen_request_id
            .lock()
            .unwrap()
            .clone()
            .expect("generated request id forwarded");
        assert!(generated_request_id.starts_with("llmproxy-"));

        server.abort();
    }

    #[tokio::test]
    async fn upstream_models_refresh_retries_safe_decode_failures() {
        let attempts = Arc::new(AtomicU64::new(0));
        let attempts_for_handler = attempts.clone();
        let app = Router::new().route(
            "/v1/models",
            get(move || {
                let attempts = attempts_for_handler.clone();
                async move {
                    let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                    if attempt == 0 {
                        (StatusCode::OK, [("content-type", "application/json")], "{")
                            .into_response()
                    } else {
                        Json(json!({ "data": [{ "id": "retried-model" }] })).into_response()
                    }
                }
            }),
        );
        let (base_url, server) = spawn_test_upstream(app).await;
        let state = test_state(base_url);

        let ids = refresh_upstream_models(state.default_provider(), &HeaderMap::new())
            .await
            .unwrap();

        assert_eq!(ids, vec!["retried-model"]);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(
            state
                .default_provider()
                .model_cache
                .first_upstream_model()
                .as_deref(),
            Some("retried-model")
        );
        server.abort();
    }

    #[tokio::test]
    async fn models_endpoint_uses_cached_upstream_response_by_default() {
        let attempts = Arc::new(AtomicU64::new(0));
        let attempts_for_handler = attempts.clone();
        let app = Router::new().route(
            "/v1/models",
            get(move || {
                let attempts = attempts_for_handler.clone();
                async move {
                    let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                    Json(json!({
                        "data": [{ "id": format!("upstream-model-{attempt}") }]
                    }))
                }
            }),
        );
        let (base_url, server) = spawn_test_upstream(app).await;
        let mut config = Config::default();
        config.upstream.base_url = base_url;
        let state = test_state_with_config(config);

        let first = models(State(SharedState::new(state.clone())), HeaderMap::new()).await;
        let second = models(State(SharedState::new(state.clone())), HeaderMap::new()).await;

        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(
            state
                .default_provider()
                .model_cache
                .first_upstream_model()
                .as_deref(),
            Some("upstream-model-0")
        );
        server.abort();
    }

    #[tokio::test]
    async fn models_endpoint_can_disable_cached_upstream_response() {
        let attempts = Arc::new(AtomicU64::new(0));
        let attempts_for_handler = attempts.clone();
        let app = Router::new().route(
            "/v1/models",
            get(move || {
                let attempts = attempts_for_handler.clone();
                async move {
                    let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                    Json(json!({
                        "data": [{ "id": format!("upstream-model-{attempt}") }]
                    }))
                }
            }),
        );
        let (base_url, server) = spawn_test_upstream(app).await;
        let mut config = Config::default();
        config.upstream.base_url = base_url;
        config.models.cache_ttl_secs = Some(0);
        let state = test_state_with_config(config);

        let first = models(State(SharedState::new(state.clone())), HeaderMap::new()).await;
        let second = models(State(SharedState::new(state.clone())), HeaderMap::new()).await;

        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(
            state
                .default_provider()
                .model_cache
                .first_upstream_model()
                .as_deref(),
            Some("upstream-model-1")
        );
        server.abort();
    }

    #[test]
    fn normalizes_system_messages_to_single_leading_message() {
        let mut value = json!({
            "messages": [
                { "role": "user", "content": "first user turn" },
                { "role": "system", "content": "late system turn" },
                { "role": "assistant", "content": "assistant turn" },
                { "role": "system", "content": [{ "type": "text", "text": "second system turn" }] }
            ]
        });

        normalize_chat_request_system_messages(&mut value);

        assert_eq!(value["messages"][0]["role"], "system");
        assert_eq!(
            value["messages"][0]["content"],
            "late system turn\n\nsecond system turn"
        );
        assert_eq!(value["messages"][1]["role"], "user");
        assert_eq!(value["messages"][2]["role"], "assistant");
        assert!(
            value["messages"]
                .as_array()
                .unwrap()
                .iter()
                .skip(1)
                .all(|message| message["role"] != "system")
        );
    }

    #[test]
    fn normalizes_chat_request_removes_non_streaming_stream_options() {
        let mut non_streaming = json!({
            "stream": false,
            "stream_options": { "include_usage": true },
            "messages": [{ "role": "user", "content": "hi" }]
        });
        normalize_chat_completion_request(&mut non_streaming, &test_config());
        assert!(non_streaming.get("stream_options").is_none());

        let mut omitted_stream = json!({
            "stream_options": { "include_usage": true },
            "messages": [{ "role": "user", "content": "hi" }]
        });
        normalize_chat_completion_request(&mut omitted_stream, &test_config());
        assert!(omitted_stream.get("stream_options").is_none());

        let mut streaming = json!({
            "stream": true,
            "stream_options": { "include_usage": true },
            "messages": [{ "role": "user", "content": "hi" }]
        });
        normalize_chat_completion_request(&mut streaming, &test_config());
        assert_eq!(streaming["stream_options"]["include_usage"], true);
    }

    #[test]
    fn normalizes_chat_request_can_disable_qwen_thinking() {
        let mut config = test_config();
        config.chat.disable_thinking = true;

        let mut request = json!({
            "messages": [{ "role": "user", "content": "hi" }]
        });
        normalize_chat_completion_request(&mut request, &config);
        assert_eq!(request["chat_template_kwargs"]["enable_thinking"], false);

        let mut explicit = json!({
            "chat_template_kwargs": { "enable_thinking": true },
            "messages": [{ "role": "user", "content": "hi" }]
        });
        normalize_chat_completion_request(&mut explicit, &config);
        assert_eq!(explicit["chat_template_kwargs"]["enable_thinking"], true);
    }

    #[tokio::test]
    async fn chat_completions_removes_non_streaming_stream_options_before_forwarding() {
        let seen_body = Arc::new(Mutex::new(None::<Value>));
        let seen_body_for_handler = seen_body.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |Json(body): Json<Value>| {
                let seen_body = seen_body_for_handler.clone();
                async move {
                    *seen_body.lock().unwrap() = Some(body);
                    Json(json!({
                        "id": "chatcmpl_test",
                        "object": "chat.completion",
                        "choices": [{
                            "message": { "role": "assistant", "content": "ok" }
                        }]
                    }))
                }
            }),
        );
        let (base_url, server) = spawn_test_upstream(app).await;
        let mut config = test_config();
        config.upstream.base_url = base_url;
        let state = test_state_with_config(config);

        let response = chat_completions(
            State(SharedState::new(state)),
            HeaderMap::new(),
            Json(json!({
                "model": "codex-qwen",
                "stream": false,
                "stream_options": { "include_usage": true },
                "messages": [{ "role": "user", "content": "hi" }]
            })),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert!(collect_response_text(response).await.contains("ok"));
        let forwarded = seen_body
            .lock()
            .unwrap()
            .clone()
            .expect("upstream request body captured");
        assert_eq!(forwarded["stream"], false);
        assert!(forwarded.get("stream_options").is_none());

        server.abort();
    }

    #[test]
    fn converts_responses_request_to_chat_completion_request() {
        let mut value = json!({
            "model": "codex-qwen",
            "instructions": "You are helpful.",
            "input": [
                {
                    "type": "message",
                    "role": "developer",
                    "content": [{ "type": "input_text", "text": "Use short answers." }]
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": "Hello" }]
                }
            ],
            "max_output_tokens": 64,
            "stream": true
        });
        let provider = test_provider("http://127.0.0.1:1/v1".to_string());
        rewrite_request_value(&mut value, &test_config(), &provider, None);

        let chat = responses_to_chat_request(&value);

        assert_eq!(chat["model"], "Qwen/Qwen2.5-Coder-32B-Instruct");
        assert_eq!(chat["max_tokens"], 64);
        assert_eq!(chat["stream"], true);
        assert_eq!(chat["messages"][0]["role"], "system");
        assert_eq!(
            chat["messages"][0]["content"],
            "You are helpful.\n\nUse short answers."
        );
        assert_eq!(chat["messages"][1]["role"], "user");
        assert_eq!(chat["messages"][1]["content"], "Hello");
    }

    #[test]
    fn converts_responses_function_tools_to_chat_completion_tools() {
        let value = json!({
            "model": "codex-qwen",
            "input": "Use a tool.",
            "tools": [
                {
                    "type": "function",
                    "name": "exec_command",
                    "description": "Run a command.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "cmd": { "type": "string" }
                        },
                        "required": ["cmd"],
                        "additionalProperties": false
                    },
                    "strict": false
                },
                {
                    "type": "web_search_preview"
                }
            ],
            "tool_choice": {
                "type": "function",
                "name": "exec_command"
            },
            "parallel_tool_calls": true
        });

        let chat = responses_to_chat_request(&value);

        assert_eq!(chat["tools"].as_array().unwrap().len(), 1);
        assert_eq!(chat["tools"][0]["type"], "function");
        assert_eq!(chat["tools"][0]["function"]["name"], "exec_command");
        assert_eq!(
            chat["tools"][0]["function"]["description"],
            "Run a command."
        );
        assert_eq!(
            chat["tools"][0]["function"]["parameters"]["required"][0],
            "cmd"
        );
        assert_eq!(chat["tools"][0]["function"]["strict"], false);
        assert!(chat["tools"][0].get("name").is_none());
        assert_eq!(chat["tool_choice"]["function"]["name"], "exec_command");
        assert_eq!(chat["parallel_tool_calls"], true);
    }

    #[test]
    fn converts_responses_function_call_history_to_chat_messages() {
        let value = json!({
            "model": "codex-qwen",
            "input": [
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "exec_command",
                    "arguments": "{\"cmd\":\"pwd\"}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": [{ "type": "input_text", "text": "/tmp/project" }]
                }
            ]
        });

        let chat = responses_to_chat_request(&value);
        let messages = chat["messages"].as_array().unwrap();

        assert_eq!(messages[0]["role"], "assistant");
        assert!(messages[0]["content"].is_null());
        assert_eq!(messages[0]["tool_calls"][0]["id"], "call_1");
        assert_eq!(messages[0]["tool_calls"][0]["type"], "function");
        assert_eq!(
            messages[0]["tool_calls"][0]["function"]["name"],
            "exec_command"
        );
        assert_eq!(
            messages[0]["tool_calls"][0]["function"]["arguments"],
            "{\"cmd\":\"pwd\"}"
        );
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[1]["tool_call_id"], "call_1");
        assert_eq!(messages[1]["content"], "/tmp/project");
    }

    #[test]
    fn exposes_model_aliases_in_model_list() {
        let upstream = json!({
            "object": "list",
            "data": [{
                "id": "Qwen/Qwen2.5-Coder-32B-Instruct",
                "object": "model",
                "owned_by": "vllm"
            }]
        });

        let rewritten = rewrite_models_response(upstream, &test_config());

        assert!(
            rewritten["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|model| model["id"] == "codex-qwen"
                    && model["proxy_target"] == "Qwen/Qwen2.5-Coder-32B-Instruct")
        );
    }

    #[test]
    fn maps_chat_completion_to_response_shape() {
        let upstream = json!({
            "id": "chatcmpl_test",
            "created": 123,
            "model": "Qwen/Qwen2.5-Coder-32B-Instruct",
            "choices": [{
                "message": { "role": "assistant", "content": "hello" }
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        });

        let response = chat_completion_to_response(upstream, Some("codex-qwen".to_string()));

        assert_eq!(response["object"], "response");
        assert_eq!(response["model"], "codex-qwen");
        assert_eq!(response["output_text"], "hello");
        assert_eq!(response["output"][0]["content"][0]["text"], "hello");
        assert_eq!(response["usage"]["total_tokens"], 2);
    }

    #[test]
    fn maps_chat_tool_calls_to_response_function_calls() {
        let upstream = json!({
            "id": "chatcmpl_test",
            "created": 123,
            "model": "Qwen/Qwen2.5-Coder-32B-Instruct",
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "exec_command",
                            "arguments": "{\"cmd\":\"ls\"}"
                        }
                    }]
                }
            }]
        });

        let response = chat_completion_to_response(upstream, Some("codex-qwen".to_string()));

        assert_eq!(response["output_text"], "");
        assert_eq!(response["output"].as_array().unwrap().len(), 1);
        assert_eq!(response["output"][0]["type"], "function_call");
        assert_eq!(response["output"][0]["call_id"], "call_1");
        assert_eq!(response["output"][0]["name"], "exec_command");
        assert_eq!(response["output"][0]["arguments"], "{\"cmd\":\"ls\"}");
    }

    #[test]
    fn normalizes_qwen_xml_tool_arguments_to_json() {
        let arguments = "\n\n<tool_call>\n<function=exec_command>\n<parameter=cmd>\npwd\n</parameter>\n<parameter=timeout_ms>\n1000\n</parameter>\n</function>\n</tool_call>";

        let normalized = normalize_tool_arguments(arguments);
        let parsed = serde_json::from_str::<Value>(&normalized).unwrap();

        assert_eq!(parsed["cmd"], "pwd");
        assert_eq!(parsed["timeout_ms"], 1000);
    }

    #[test]
    fn extracts_chat_stream_tool_call_deltas() {
        let value = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "exec_command",
                            "arguments": "{\"cmd\":\""
                        }
                    }]
                }
            }, {
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": {
                            "arguments": "pwd\"}"
                        }
                    }]
                }
            }]
        });

        let deltas = chat_stream_tool_call_deltas(&value);
        assert_eq!(
            deltas,
            vec![
                ChatStreamToolCallDelta {
                    index: 0,
                    id: Some("call_1".to_string()),
                    name: Some("exec_command".to_string()),
                    arguments_delta: "{\"cmd\":\"".to_string(),
                },
                ChatStreamToolCallDelta {
                    index: 0,
                    id: None,
                    name: None,
                    arguments_delta: "pwd\"}".to_string(),
                },
            ]
        );

        let mut accumulated = Vec::new();
        for delta in deltas {
            apply_stream_tool_call_delta(&mut accumulated, delta, "resp_test");
        }
        assert_eq!(accumulated[0].call_id, "call_1");
        assert_eq!(accumulated[0].name, "exec_command");
        assert_eq!(accumulated[0].arguments, "{\"cmd\":\"pwd\"}");
    }

    #[test]
    fn converts_anthropic_message_to_chat_completion_request() {
        let mut value = json!({
            "model": "codex-qwen",
            "system": "You are concise.",
            "max_tokens": 128,
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": "Run ls" },
                    { "type": "tool_result", "tool_use_id": "toolu_1", "content": "ok" }
                ]
            }],
            "tools": [{
                "name": "Bash",
                "description": "Run shell commands",
                "input_schema": {
                    "type": "object",
                    "properties": { "command": { "type": "string" } }
                }
            }],
            "tool_choice": { "type": "auto" }
        });
        let provider = test_provider("http://127.0.0.1:1/v1".to_string());
        rewrite_request_value(&mut value, &test_config(), &provider, None);

        let chat = anthropic_message_to_chat_request(&value);

        assert_eq!(chat["model"], "Qwen/Qwen2.5-Coder-32B-Instruct");
        assert_eq!(chat["max_tokens"], 128);
        assert_eq!(chat["messages"][0]["role"], "system");
        assert_eq!(chat["messages"][0]["content"], "You are concise.");
        assert_eq!(chat["messages"][1]["role"], "user");
        assert!(
            chat["messages"][1]["content"]
                .as_str()
                .unwrap()
                .contains("[tool_result toolu_1]")
        );
        assert_eq!(chat["tools"][0]["function"]["name"], "Bash");
        assert_eq!(chat["tool_choice"], "auto");
    }

    #[test]
    fn maps_chat_completion_to_anthropic_message_shape() {
        let upstream = json!({
            "id": "chatcmpl_test",
            "model": "Qwen/Qwen2.5-Coder-32B-Instruct",
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": "checking",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "Bash",
                            "arguments": "{\"command\":\"pwd\"}"
                        }
                    }]
                }
            }],
            "usage": { "prompt_tokens": 5, "completion_tokens": 7, "total_tokens": 12 }
        });

        let message = chat_completion_to_anthropic_message(upstream, Some("local".to_string()));

        assert_eq!(message["type"], "message");
        assert_eq!(message["model"], "local");
        assert_eq!(message["stop_reason"], "tool_use");
        assert_eq!(message["content"][0]["type"], "text");
        assert_eq!(message["content"][1]["type"], "tool_use");
        assert_eq!(message["content"][1]["name"], "Bash");
        assert_eq!(message["content"][1]["input"]["command"], "pwd");
        assert_eq!(message["usage"]["input_tokens"], 5);
        assert_eq!(message["usage"]["output_tokens"], 7);
    }

    #[test]
    fn ensure_chat_stream_include_usage_sets_flag_when_streaming() {
        let mut request = json!({ "stream": true });
        ensure_chat_stream_include_usage(&mut request);
        assert_eq!(request["stream_options"]["include_usage"], true);

        let mut existing = json!({
            "stream": true,
            "stream_options": { "include_usage": false }
        });
        ensure_chat_stream_include_usage(&mut existing);
        assert_eq!(existing["stream_options"]["include_usage"], false);

        let mut not_streaming = json!({ "stream": false });
        ensure_chat_stream_include_usage(&mut not_streaming);
        assert!(not_streaming.get("stream_options").is_none());
    }

    #[test]
    fn chat_usage_to_responses_usage_maps_token_fields() {
        let chat_usage = json!({
            "prompt_tokens": 10,
            "completion_tokens": 4,
            "total_tokens": 14,
            "prompt_tokens_details": { "cached_tokens": 3 },
            "completion_tokens_details": { "reasoning_tokens": 1 }
        });
        let mapped = chat_usage_to_responses_usage(&chat_usage);
        assert_eq!(mapped["input_tokens"], 10);
        assert_eq!(mapped["output_tokens"], 4);
        assert_eq!(mapped["total_tokens"], 14);
        assert_eq!(mapped["input_tokens_details"]["cached_tokens"], 3);
        assert_eq!(mapped["output_tokens_details"]["reasoning_tokens"], 1);
    }

    #[test]
    fn take_sse_line_bytes_handles_split_utf8_across_chunks() {
        let mut buffer: Vec<u8> = Vec::new();
        let line1 = "data: {\"choices\":[]}\n";
        let line2_bytes = [b'd', b'a', b't', b'a', b':', b' ', 0xE4, 0xB8, 0xAD, b'\n'];
        buffer.extend_from_slice(line1.as_bytes());
        let first = take_sse_line_bytes(&mut buffer).expect("first line");
        assert_eq!(first, "data: {\"choices\":[]}");
        buffer.extend_from_slice(&line2_bytes);
        let second = take_sse_line_bytes(&mut buffer).expect("second line");
        assert_eq!(second, "data: 中");
        assert!(take_sse_line_bytes(&mut buffer).is_none());
    }

    #[test]
    fn sse_data_parser_supports_multiline_data_events() {
        let mut buffer = concat!(
            "event: ignored\n",
            "data: {\"choices\":[\n",
            "data: {\"delta\":{\"content\":\"hi\"}}]}\n\n",
        )
        .as_bytes()
        .to_vec();
        let mut data_lines = Vec::new();

        let events = take_sse_data_events(&mut buffer, &mut data_lines);

        assert_eq!(events.len(), 1);
        let value: Value = serde_json::from_str(&events[0]).unwrap();
        assert_eq!(value["choices"][0]["delta"]["content"], "hi");
        assert!(buffer.is_empty());
        assert!(data_lines.is_empty());
    }

    #[test]
    fn sse_data_parser_surfaces_incomplete_tail_at_eof() {
        let mut buffer = "data: {\"choices\":[]}".as_bytes().to_vec();
        let mut data_lines = Vec::new();

        let (events, had_incomplete_tail) = finish_sse_data_events(&mut buffer, &mut data_lines);

        assert!(had_incomplete_tail);
        assert_eq!(events, vec!["{\"choices\":[]}"]);
        assert!(buffer.is_empty());
        assert!(data_lines.is_empty());
    }

    #[tokio::test]
    async fn stream_chat_as_responses_emits_lifecycle_events() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}],",
            "\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3,\"total_tokens\":10}}\n\n",
            "data: [DONE]\n\n",
        );
        let response = mock_upstream_response(body);
        let response = stream_chat_as_responses(response, Some("local".to_string()), None);
        let events = collect_sse_events(response).await;

        let kinds: Vec<&str> = events.iter().map(|(event, _)| event.as_str()).collect();
        assert!(kinds.contains(&"response.created"));
        assert!(kinds.contains(&"response.output_text.delta"));
        assert!(kinds.contains(&"response.output_item.done"));
        assert!(kinds.contains(&"response.completed"));

        let completed = events
            .iter()
            .find(|(event, _)| event == "response.completed")
            .map(|(_, payload)| payload.clone())
            .expect("response.completed present");
        assert_eq!(completed["response"]["usage"]["input_tokens"], 7);
        assert_eq!(completed["response"]["usage"]["output_tokens"], 3);
        assert_eq!(completed["response"]["usage"]["total_tokens"], 10);
        assert_eq!(completed["response"]["model"], "local");
        assert_eq!(completed["response"]["end_turn"], true);

        let text_deltas: Vec<String> = events
            .iter()
            .filter(|(event, _)| event == "response.output_text.delta")
            .map(|(_, payload)| payload["delta"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(text_deltas.join(""), "hello");
    }

    #[tokio::test]
    async fn stream_chat_as_responses_reads_multiline_sse_data() {
        let body = concat!(
            "data: {\"choices\":[\n",
            "data: {\"delta\":{\"content\":\"multi\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let response = mock_upstream_response(body);
        let response = stream_chat_as_responses(response, Some("local".to_string()), None);
        let events = collect_sse_events(response).await;

        let text_deltas: Vec<String> = events
            .iter()
            .filter(|(event, _)| event == "response.output_text.delta")
            .map(|(_, payload)| payload["delta"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(text_deltas, vec!["multi"]);
    }

    #[tokio::test]
    async fn stream_chat_as_anthropic_emits_message_lifecycle() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\" there\"},\"finish_reason\":\"stop\"}],",
            "\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2,\"total_tokens\":6}}\n\n",
            "data: [DONE]\n\n",
        );
        let response = mock_upstream_response(body);
        let response = stream_chat_as_anthropic(response, Some("local".to_string()), None);
        let events = collect_sse_events(response).await;

        let kinds: Vec<&str> = events.iter().map(|(event, _)| event.as_str()).collect();
        assert!(kinds.starts_with(&["message_start"]));
        assert!(kinds.contains(&"content_block_start"));
        assert!(kinds.contains(&"content_block_delta"));
        assert!(kinds.contains(&"content_block_stop"));
        assert!(kinds.contains(&"message_delta"));
        assert!(kinds.ends_with(&["message_stop"]));

        let text: String = events
            .iter()
            .filter(|(event, _)| event == "content_block_delta")
            .filter_map(|(_, payload)| payload["delta"]["text"].as_str().map(ToOwned::to_owned))
            .collect();
        assert_eq!(text, "hi there");

        let delta = events
            .iter()
            .find(|(event, _)| event == "message_delta")
            .map(|(_, payload)| payload.clone())
            .expect("message_delta present");
        assert_eq!(delta["delta"]["stop_reason"], "end_turn");
        assert_eq!(delta["usage"]["input_tokens"], 4);
        assert_eq!(delta["usage"]["output_tokens"], 2);
    }

    #[tokio::test]
    async fn stream_chat_as_anthropic_emits_tool_use_block() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",",
            "\"type\":\"function\",\"function\":{\"name\":\"Bash\",\"arguments\":\"{\\\"cmd\\\":\\\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,",
            "\"function\":{\"arguments\":\"pwd\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let response = mock_upstream_response(body);
        let response = stream_chat_as_anthropic(response, Some("local".to_string()), None);
        let events = collect_sse_events(response).await;

        let tool_starts: Vec<&Value> = events
            .iter()
            .filter(|(event, payload)| {
                event == "content_block_start" && payload["content_block"]["type"] == "tool_use"
            })
            .map(|(_, payload)| payload)
            .collect();
        assert_eq!(tool_starts.len(), 1);
        assert_eq!(tool_starts[0]["content_block"]["id"], "call_1");
        assert_eq!(tool_starts[0]["content_block"]["name"], "Bash");

        let partial_jsons: String = events
            .iter()
            .filter(|(event, payload)| {
                event == "content_block_delta" && payload["delta"]["type"] == "input_json_delta"
            })
            .filter_map(|(_, payload)| {
                payload["delta"]["partial_json"]
                    .as_str()
                    .map(ToOwned::to_owned)
            })
            .collect();
        assert_eq!(partial_jsons, "{\"cmd\":\"pwd\"}");

        let delta = events
            .iter()
            .find(|(event, _)| event == "message_delta")
            .map(|(_, payload)| payload.clone())
            .expect("message_delta present");
        assert_eq!(delta["delta"]["stop_reason"], "tool_use");
    }

    #[tokio::test]
    async fn stream_chat_as_anthropic_turns_upstream_body_errors_into_sse_errors() {
        let response = mock_upstream_error_response("stream exploded");
        let meta = UpstreamRequestMeta::new(
            "test",
            &HeaderMap::new(),
            Method::POST,
            "chat/completions",
            UpstreamResponseKind::Stream,
            false,
        );

        let response = stream_chat_as_anthropic(response, Some("local".to_string()), Some(meta));
        let events = collect_sse_events(response).await;

        assert_eq!(
            events.first().map(|(event, _)| event.as_str()),
            Some("message_start")
        );
        let error = events
            .iter()
            .find(|(event, _)| event == "error")
            .map(|(_, payload)| payload)
            .expect("error event present");
        assert_eq!(error["type"], "error");
        assert_eq!(error["error"]["type"], "api_error");
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("stream exploded")
        );
    }

    #[tokio::test]
    async fn stream_chat_as_responses_turns_upstream_body_errors_into_failed_events() {
        let response = mock_upstream_error_response("stream exploded");
        let meta = UpstreamRequestMeta::new(
            "test",
            &HeaderMap::new(),
            Method::POST,
            "chat/completions",
            UpstreamResponseKind::Stream,
            false,
        );

        let response = stream_chat_as_responses(response, Some("local".to_string()), Some(meta));
        let events = collect_sse_events(response).await;

        let failed = events
            .iter()
            .find(|(event, _)| event == "response.failed")
            .map(|(_, payload)| payload)
            .expect("response.failed event present");
        assert_eq!(failed["response"]["status"], "failed");
        assert_eq!(failed["response"]["error"]["type"], "upstream_error");
        assert!(
            failed["response"]["error"]["message"]
                .as_str()
                .unwrap()
                .contains("stream exploded")
        );
    }

    #[tokio::test]
    async fn streaming_proxy_requests_skip_total_upstream_timeout() {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                let stream = async_stream::stream! {
                    sleep(Duration::from_millis(1200)).await;
                    yield Ok::<Bytes, std::io::Error>(Bytes::from_static(b"data: [DONE]\n\n"));
                };

                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(stream))
                    .unwrap()
            }),
        );
        let (base_url, server) = spawn_test_upstream(app).await;
        let mut config = Config::default();
        config.upstream.base_url = base_url;
        config.upstream.timeout_secs = 1;
        let state = test_state_with_config(config);

        let response = proxy_json(
            state.default_provider(),
            &HeaderMap::new(),
            "chat/completions",
            &json!({
                "model": "local",
                "stream": true,
                "messages": [{ "role": "user", "content": "hi" }]
            }),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert!(collect_response_text(response).await.contains("[DONE]"));
        server.abort();
    }

    fn mock_upstream_response(body: &str) -> reqwest::Response {
        let http_response = http::Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(body.to_string())
            .expect("build mock response");
        reqwest::Response::from(http_response)
    }

    fn mock_upstream_error_response(message: &'static str) -> reqwest::Response {
        let stream = futures_util::stream::once(async move {
            Err::<Bytes, std::io::Error>(std::io::Error::other(message))
        });
        let http_response = http::Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(reqwest::Body::wrap_stream(stream))
            .expect("build mock error response");
        reqwest::Response::from(http_response)
    }

    async fn collect_response_text(response: Response) -> String {
        use futures_util::StreamExt;
        let body = response.into_body();
        let mut stream = body.into_data_stream();
        let mut buffer: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.expect("stream chunk");
            buffer.extend_from_slice(&chunk);
        }
        String::from_utf8(buffer).expect("utf8 response body")
    }

    async fn collect_sse_events(response: Response) -> Vec<(String, Value)> {
        use futures_util::StreamExt;
        let body = response.into_body();
        let mut stream = body.into_data_stream();
        let mut buffer: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.expect("stream chunk");
            buffer.extend_from_slice(&chunk);
        }
        let text = String::from_utf8(buffer).expect("utf8 sse body");
        let mut events = Vec::new();
        for block in text.split("\n\n") {
            let mut event_name: Option<String> = None;
            let mut data_line: Option<String> = None;
            for line in block.lines() {
                if let Some(rest) = line.strip_prefix("event:") {
                    event_name = Some(rest.trim().to_string());
                } else if let Some(rest) = line.strip_prefix("data:") {
                    data_line = Some(rest.trim().to_string());
                }
            }
            if let (Some(name), Some(data)) = (event_name, data_line) {
                let value: Value = serde_json::from_str(&data).unwrap_or(Value::String(data));
                events.push((name, value));
            }
        }
        events
    }
}
