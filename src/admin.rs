//! Optional loopback-only admin web interface.
//!
//! Serves the monitoring/configuration UI from the `ui/` directory (read per
//! request in debug builds, read once at startup and cached in release
//! builds) plus a JSON API. Runtime config edits swap a fully built
//! [`AppState`] into [`SharedState`] and are persisted back to the loaded
//! TOML file with `toml_edit` so user comments survive.

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::{Arc, OnceLock};
use std::{env, fs};

use anyhow::{Context, Result, bail};
use axum::Json;
use axum::Router;
use axum::extract::{Path as AxumPath, Query, Request, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, HOST, ORIGIN};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use bytes::Bytes;
use serde::Deserialize;
use serde_json::{Value, json};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio::sync::Mutex as AsyncMutex;
use toml_edit::{DocumentMut, Item, Table, value};
use tracing::{error, info, warn};

use crate::interceptor::{IntegratedTarget, InterceptorConfig};
use crate::stats::{proxy_stats, route_class_for_base_url, routing_decision_log, traffic_log};
use crate::{
    AdminConfig, AnthropicMode, AppState, Config, INTEGRATED_AUTO_MODEL, ModelAlias,
    ModelRoutingMode, ResponsesMode, RoutingMode, SharedState, attach_privacy_boundary,
    build_provider_state, configure_daemon_process, effective_provider_configs,
    finalize_config_keys, parse_config, refresh_upstream_models, update_runtime_mode,
    validate_config,
};

/// Effective bind address: missing config means loopback on a random port.
pub(crate) fn listen_address(admin: &AdminConfig) -> String {
    admin
        .listen_address
        .clone()
        .unwrap_or_else(|| "127.0.0.1:0".to_string())
}

pub(crate) struct AdminContext {
    config_path: Option<PathBuf>,
    launcher: Option<&'static str>,
    proxy_addr: OnceLock<SocketAddr>,
    admin_addr: OnceLock<SocketAddr>,
    ui: UiSource,
    /// Serializes runtime config edits (read-modify-swap plus file write).
    apply_lock: AsyncMutex<()>,
}

impl AdminContext {
    pub(crate) fn new(
        config_path: Option<PathBuf>,
        launcher: Option<&'static str>,
        admin: &AdminConfig,
    ) -> Self {
        Self {
            config_path,
            launcher,
            proxy_addr: OnceLock::new(),
            admin_addr: OnceLock::new(),
            ui: UiSource::build(admin),
            apply_lock: AsyncMutex::new(()),
        }
    }

    pub(crate) fn set_proxy_addr(&self, addr: SocketAddr) {
        let _ = self.proxy_addr.set(addr);
    }
}

#[derive(Clone)]
struct AdminState {
    shared: SharedState,
    context: Arc<AdminContext>,
}

/// Bind the admin listener and serve it as a background task.
pub(crate) async fn spawn(
    shared: SharedState,
    context: Arc<AdminContext>,
    admin: &AdminConfig,
) -> Result<SocketAddr> {
    let address = listen_address(admin);
    let addr: SocketAddr = address
        .parse()
        .with_context(|| format!("invalid admin.listen_address: {address:?}"))?;
    if !addr.ip().is_loopback() {
        bail!("admin.listen_address {address:?} must be a loopback address");
    }
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind admin interface on {addr}"))?;
    let bound = listener
        .local_addr()
        .context("failed to read admin listen address")?;
    let _ = context.admin_addr.set(bound);
    match &context.ui {
        UiSource::Disk(Some(dir)) => {
            info!(ui_dir = %dir.display(), "admin UI served from disk per request")
        }
        UiSource::Disk(None) => {
            warn!("admin UI directory not found; only the JSON API is available")
        }
        UiSource::Cached(files) if files.is_empty() => {
            warn!("admin UI directory not found at startup; only the JSON API is available")
        }
        UiSource::Cached(files) => info!(files = files.len(), "admin UI cached at startup"),
    }
    info!(admin_url = %format!("http://{bound}/"), "admin interface listening");

    let app = router(AdminState { shared, context });
    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            error!(error = %error, "admin interface server exited");
        }
    });
    Ok(bound)
}

fn router(state: AdminState) -> Router {
    Router::new()
        .route("/api/overview", get(api_overview))
        .route("/api/models", get(api_models))
        .route("/api/model-list", get(api_model_list))
        .route("/api/stats", get(api_stats))
        .route("/api/stats/reset", post(api_stats_reset))
        .route("/api/logs", get(api_logs))
        .route("/api/logs/clear", post(api_logs_clear))
        .route("/api/errors", get(api_errors))
        .route("/api/errors/clear", post(api_errors_clear))
        .route("/api/decisions", get(api_decisions))
        .route("/api/decisions/clear", post(api_decisions_clear))
        .route("/api/launch", get(api_launch))
        .route("/api/config", get(api_config))
        .route("/api/routing-mode", put(api_routing_mode_put))
        .route("/api/restart", post(api_restart))
        .route(
            "/api/config/raw",
            get(api_config_raw_get).put(api_config_raw_put),
        )
        .route(
            "/api/aliases/{alias}",
            put(api_alias_put).delete(api_alias_delete),
        )
        .route("/api/interceptor", put(api_interceptor_put))
        .route("/api/integrated", put(api_integrated_put))
        .route("/api/interceptor/test", post(api_interceptor_test))
        .route("/api/interceptor/revise", post(api_interceptor_revise))
        .route(
            "/api/providers/{name}",
            put(api_provider_put).delete(api_provider_delete),
        )
        .route("/api/settings", put(api_settings_put))
        .fallback(get(ui_asset))
        .with_state(state.clone())
        .route_layer(middleware::from_fn_with_state(state, require_local_origin))
}

/// True when the `Host` header names this listener: the port must equal the
/// bound port and the host must resolve to a loopback address.
///
/// Literal IPs pass as before. A *name* now also passes when it really does
/// resolve to loopback, which is what allows `http://localhost:PORT` while
/// keeping DNS rebinding closed. The distinction still has teeth: this API is
/// unauthenticated and serves the raw config (plaintext provider keys), so a
/// short-TTL domain pointed at 127.0.0.1 makes a browser treat the attacker's
/// page as same-origin and the Origin check stops helping. Whether the name
/// resolves to loopback is then the only thing that says whose name it was,
/// and an attacker-chosen name does not resolve to loopback on the victim's
/// machine.
fn loopback_host_matches(host: &str, bound_port: u16) -> bool {
    let Some((host_part, port_part)) = host.rsplit_once(':') else {
        return false;
    };
    let Ok(port) = port_part.parse::<u16>() else {
        return false;
    };
    if port != bound_port {
        return false;
    }
    let host_ip = host_part
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host_part);
    if let Ok(ip) = host_ip.parse::<std::net::IpAddr>() {
        return ip.is_loopback();
    }
    host_is_loopback_by_name(host_ip)
}

/// Resolve a bare host name and require every address it yields to be loopback.
///
/// All-or-nothing deliberately: a name resolving to both a loopback and a
/// routable address must fail, so a rebinding zone with one good and one bad
/// record cannot be admitted on the strength of the good one. A name that does
/// not resolve to loopback could not have been served by this socket in the
/// first place (the listener is loopback-bound), so failing closed here costs
/// nothing legitimate.
///
/// Resolution is synchronous, which is acceptable because loopback names come
/// from `/etc/hosts` under the usual `files dns` order (~0.1-0.5 ms warm here)
/// and the admin is polled by one local browser. A deployment whose
/// `nsswitch.order` puts DNS first would pay a lookup per admin request and
/// should memoize rather than move this onto a blocking pool.
fn host_is_loopback_by_name(host: &str) -> bool {
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
    {
        return false;
    }
    let Ok(addrs) = std::net::ToSocketAddrs::to_socket_addrs(&(host.to_owned(), 0u16)) else {
        return false;
    };
    let mut saw_any = false;
    for addr in addrs {
        if !addr.ip().is_loopback() {
            return false;
        }
        saw_any = true;
    }
    saw_any
}

fn is_same_loopback_origin(origin: &str, bound_port: u16) -> bool {
    let Some(rest) = origin.strip_prefix("http://") else {
        return false;
    };
    loopback_host_matches(rest, bound_port)
}

fn is_json_content_type(content_type: Option<&str>) -> bool {
    let Some(ct) = content_type else {
        return false;
    };
    let ct = ct.split(';').next().unwrap_or("").trim();
    ct.eq_ignore_ascii_case("application/json")
}

/// Rejects cross-site and DNS-rebinding requests before any handler runs.
///
/// Loopback binding alone does not stop a web page the user visits: a simple
/// bodyless POST from any origin reaches the clear/reset routes, and rebinding
/// a short-TTL domain to 127.0.0.1 makes the browser treat the admin as
/// same-origin, exposing the raw config (plaintext keys) and its writer. The
/// Host check is the one that bites under rebinding, because the header then
/// carries the attacker's hostname.
async fn require_local_origin(
    State(state): State<AdminState>,
    request: Request,
    next: Next,
) -> Response {
    let headers = request.headers();

    let bound_port = state
        .context
        .admin_addr
        .get()
        .map(|addr| addr.port())
        .unwrap_or(0);
    let host_ok = headers
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|host| loopback_host_matches(host, bound_port));
    if !host_ok {
        return api_error(
            StatusCode::BAD_REQUEST,
            "request Host must be the loopback admin address",
        );
    }

    if let Some(origin) = headers.get(ORIGIN).and_then(|v| v.to_str().ok())
        && !is_same_loopback_origin(origin, bound_port)
    {
        return api_error(
            StatusCode::FORBIDDEN,
            "cross-origin admin requests are not allowed",
        );
    }

    // A bodyless, non-simple request could still be forged once Host/Origin
    // checks pass, so require a JSON content type on every mutating method.
    // The bundled UI already sends it on all mutating calls.
    let method = request.method().clone();
    if (method != Method::GET && method != Method::HEAD)
        && !is_json_content_type(headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()))
    {
        return api_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "mutating admin requests require Content-Type: application/json",
        );
    }

    next.run(request).await
}

// Static UI serving

enum UiSource {
    /// Debug builds: resolve and read from disk on every request.
    Disk(Option<PathBuf>),
    /// Release builds: read once at startup, cached until restart.
    Cached(HashMap<String, Bytes>),
}

impl UiSource {
    fn build(admin: &AdminConfig) -> Self {
        let dir = resolve_ui_dir(admin);
        if cfg!(debug_assertions) {
            Self::Disk(dir)
        } else {
            let files = dir.as_deref().map(load_ui_files).unwrap_or_default();
            Self::Cached(files)
        }
    }
}

/// The UI ships as plain files next to the binary; no directory is embedded.
fn resolve_ui_dir(admin: &AdminConfig) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(dir) = env::var_os("LLMPROXY_UI_DIR") {
        candidates.push(PathBuf::from(dir));
    }
    if let Some(dir) = &admin.ui_dir {
        candidates.push(dir.clone());
    }
    if let Ok(exe) = env::current_exe()
        && let Some(exe_dir) = exe.parent()
    {
        candidates.push(exe_dir.join("ui"));
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ui"));
    candidates.push(PathBuf::from("ui"));
    candidates.into_iter().find(|dir| dir.is_dir())
}

fn load_ui_files(dir: &Path) -> HashMap<String, Bytes> {
    let mut files = HashMap::new();
    collect_ui_files(dir, String::new(), &mut files);
    files
}

fn collect_ui_files(dir: &Path, prefix: String, files: &mut HashMap<String, Bytes>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with('.') {
            continue;
        }
        let key = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        let path = entry.path();
        if path.is_dir() {
            collect_ui_files(&path, key, files);
        } else if let Ok(contents) = fs::read(&path) {
            files.insert(key, Bytes::from(contents));
        }
    }
}

/// Normalize a request path to a safe relative UI path. Anything that is not
/// a plain forward-slash relative path of normal components is rejected.
fn sanitize_ui_path(raw_path: &str) -> Option<String> {
    let trimmed = raw_path.trim_start_matches('/');
    let candidate = if trimmed.is_empty() {
        "index.html"
    } else {
        trimmed
    };
    if candidate.contains('%') || candidate.contains('\\') || candidate.contains('\0') {
        return None;
    }
    let path = Path::new(candidate);
    if !path
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        return None;
    }
    Some(candidate.to_string())
}

fn ui_content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or_default() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "webmanifest" => "application/manifest+json",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

const UI_MISSING_HINT: &str = "<!doctype html><meta charset=\"utf-8\">\
<title>llmproxy admin</title>\
<body style=\"font-family:system-ui;margin:4rem auto;max-width:34rem;line-height:1.5\">\
<h1>Admin UI files not found</h1>\
<p>The JSON API under <code>/api/</code> is available, but no <code>ui/</code> \
directory was found next to the binary, in the working directory, or via \
<code>LLMPROXY_UI_DIR</code> / <code>[admin] ui_dir</code>.</p>\
<p>Release builds read the UI once at startup; restart llmproxy after \
installing the files.</p></body>";

async fn ui_asset(State(state): State<AdminState>, uri: Uri) -> Response {
    if uri.path().starts_with("/api/") {
        return api_error(StatusCode::NOT_FOUND, "unknown admin API endpoint");
    }
    let Some(rel_path) = sanitize_ui_path(uri.path()) else {
        return api_error(StatusCode::NOT_FOUND, "invalid path");
    };
    let body = match &state.context.ui {
        UiSource::Disk(dir) => dir.as_ref().and_then(|dir| {
            let path = dir.join(&rel_path);
            path.is_file().then(|| fs::read(&path).ok()).flatten()
        }),
        UiSource::Cached(files) => files.get(&rel_path).map(|bytes| bytes.to_vec()),
    };
    let cache_control = if cfg!(debug_assertions) {
        "no-store"
    } else {
        "no-cache"
    };
    match body {
        Some(bytes) => (
            [
                (CONTENT_TYPE, ui_content_type(&rel_path)),
                (CACHE_CONTROL, cache_control),
            ],
            bytes,
        )
            .into_response(),
        None if rel_path == "index.html" => (
            [
                (CONTENT_TYPE, "text/html; charset=utf-8"),
                (CACHE_CONTROL, "no-store"),
            ],
            UI_MISSING_HINT,
        )
            .into_response(),
        None => api_error(StatusCode::NOT_FOUND, "file not found"),
    }
}

// Read-only monitoring APIs

fn api_error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({ "error": { "message": message.into() } })),
    )
        .into_response()
}

async fn api_overview(State(state): State<AdminState>) -> Response {
    let snapshot = state.shared.snapshot();
    let config = &snapshot.config;
    let context = &state.context;
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "proxy_addr": context.proxy_addr.get().map(ToString::to_string),
        "admin_addr": context.admin_addr.get().map(ToString::to_string),
        "listen_addr": config.listen_addr,
        "config_path": context.config_path.as_ref().map(|path| path.display().to_string()),
        "config_persistable": context.config_path.is_some(),
        "launcher": context.launcher,
        "default_provider": config.default_provider,
        "default_model": config.default_model,
        "routing_mode": config.routing_mode.as_str(),
        "providers": snapshot.providers.len(),
        "model_routing": match config.model_routing {
            ModelRoutingMode::Remap => "remap",
            ModelRoutingMode::Passthrough => "passthrough",
        },
        "anthropic_mode": match config.anthropic_mode {
            AnthropicMode::ChatCompletions => "chat_completions",
            AnthropicMode::Passthrough => "passthrough",
        },
        "responses_mode": match config.responses.mode {
            ResponsesMode::ChatCompletions => "chat_completions",
            ResponsesMode::Passthrough => "passthrough",
        },
        "interceptor": {
            "enabled": config.interceptor.enabled,
            "reject_sensitive": config.interceptor.reject_sensitive,
            "provider": config.interceptor.provider,
            "model": config.interceptor.model,
            "timeout_ms": config.interceptor.timeout_ms,
            "fallback_on_error": config.interceptor.fallback_on_error,
        },
        "integrated": {
            "enabled": config.integrated.enabled,
            "targets": config.integrated.targets.len(),
        },
    }))
    .into_response()
}

async fn api_models(State(state): State<AdminState>) -> Response {
    let snapshot = state.shared.snapshot();
    let config = &snapshot.config;

    let mut provider_names = snapshot.providers.keys().cloned().collect::<Vec<_>>();
    provider_names.sort();
    let providers = provider_names
        .iter()
        .map(|name| {
            let provider = &snapshot.providers[name];
            let (models, fetched) = provider.model_cache.cached_ids();
            json!({
                "name": name,
                "kind": provider.config.kind.as_str(),
                "base_url": provider.config.base_url,
                "class": class_str(&provider.config.base_url),
                "is_default": *name == config.default_provider,
                "is_interceptor": (config.interceptor.enabled || config.integrated.enabled)
                    && *name == config.interceptor.provider,
                "models": models,
                "models_fetched": fetched,
            })
        })
        .collect::<Vec<_>>();

    let mut alias_names = config.model_aliases.keys().cloned().collect::<Vec<_>>();
    alias_names.sort();
    let aliases = alias_names
        .iter()
        .map(|alias| {
            let route = &config.model_aliases[alias];
            json!({
                "alias": alias,
                "provider": route.provider(&config.default_provider),
                "model": route.model(),
                "routed": matches!(route, ModelAlias::Route { .. }),
            })
        })
        .collect::<Vec<_>>();

    let discovered_default_model = snapshot
        .providers
        .get(&config.default_provider)
        .and_then(|provider| provider.model_cache.first_upstream_model());
    let default_auto_model = config
        .default_model
        .as_deref()
        .filter(|model| !model.trim().is_empty())
        .or(discovered_default_model.as_deref());

    Json(json!({
        "providers": providers,
        "aliases": aliases,
        "role_map": config.role_map,
        "auto_mapping": config.model_routing == ModelRoutingMode::Remap,
        "default_auto_model": default_auto_model,
    }))
    .into_response()
}

fn class_str(base_url: &str) -> &'static str {
    match route_class_for_base_url(base_url) {
        crate::stats::RouteClass::Local => "local",
        crate::stats::RouteClass::External => "external",
    }
}

async fn api_stats() -> Response {
    Json(proxy_stats().snapshot_json()).into_response()
}

async fn api_stats_reset() -> Response {
    proxy_stats().reset();
    Json(json!({ "ok": true })).into_response()
}

#[derive(Deserialize)]
struct LogsQuery {
    /// Only return entries with an id greater than this (incremental polling).
    #[serde(default)]
    after: u64,
}

async fn api_logs(Query(query): Query<LogsQuery>) -> Response {
    Json(traffic_log().snapshot_after(query.after)).into_response()
}

async fn api_logs_clear() -> Response {
    traffic_log().clear();
    Json(json!({ "ok": true })).into_response()
}

/// The catalog a client of this proxy sees from `GET /v1/models`.
async fn api_model_list(State(state): State<AdminState>) -> Response {
    let snapshot = state.shared.snapshot();
    let catalog = crate::models_catalog_json(&snapshot).await;
    Json(json!({
        "model_mapping": match snapshot.config.model_routing {
            ModelRoutingMode::Remap => "remap",
            ModelRoutingMode::Passthrough => "passthrough",
        },
        "expose_aliases": snapshot.config.models.expose_aliases,
        "expose_upstream": snapshot.config.models.expose_upstream,
        "models": catalog.get("data").cloned().unwrap_or_else(|| json!([])),
    }))
    .into_response()
}

async fn api_errors(Query(query): Query<LogsQuery>) -> Response {
    Json(crate::stats::error_log().snapshot_after(query.after)).into_response()
}

async fn api_errors_clear() -> Response {
    crate::stats::error_log().clear();
    Json(json!({ "ok": true })).into_response()
}

async fn api_decisions(Query(query): Query<LogsQuery>) -> Response {
    Json(routing_decision_log().snapshot_after(query.after)).into_response()
}

async fn api_decisions_clear() -> Response {
    routing_decision_log().clear();
    Json(json!({ "ok": true })).into_response()
}

#[derive(Deserialize)]
struct InterceptorTestBody {
    prompt: String,
    /// Test with these routing instructions instead of the saved ones;
    /// empty string means the built-in default policy for the mode.
    #[serde(default)]
    instructions: Option<String>,
    /// "intercept" (default) or "integrated".
    #[serde(default)]
    mode: Option<String>,
}

async fn api_interceptor_test(
    State(state): State<AdminState>,
    Json(body): Json<InterceptorTestBody>,
) -> Response {
    let prompt = body.prompt.trim();
    if prompt.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "test prompt cannot be empty");
    }
    let mode = match crate::interceptor::RouteTestMode::parse(body.mode.as_deref()) {
        Ok(mode) => mode,
        Err(message) => return api_error(StatusCode::BAD_REQUEST, &message),
    };
    let snapshot = state.shared.snapshot();
    let verdict =
        crate::interceptor::test_route(&snapshot, prompt, body.instructions.as_deref(), mode).await;
    Json(verdict).into_response()
}

#[derive(Deserialize)]
struct InterceptorReviseBody {
    /// The requested change, e.g. "raise the deterministic execution threshold".
    request: String,
    /// The editor's current policy text; empty means the built-in default
    /// for the mode.
    #[serde(default)]
    instructions: String,
    /// "intercept" (default) or "integrated".
    #[serde(default)]
    mode: Option<String>,
}

async fn api_interceptor_revise(
    State(state): State<AdminState>,
    Json(body): Json<InterceptorReviseBody>,
) -> Response {
    let request = body.request.trim();
    if request.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "describe the change to make");
    }
    let mode = match crate::interceptor::RouteTestMode::parse(body.mode.as_deref()) {
        Ok(mode) => mode,
        Err(message) => return api_error(StatusCode::BAD_REQUEST, &message),
    };
    let snapshot = state.shared.snapshot();
    let result =
        crate::interceptor::revise_instructions(&snapshot, &body.instructions, request, mode).await;
    if let Some(error) = result.get("error").and_then(Value::as_str) {
        return api_error(StatusCode::BAD_GATEWAY, error);
    }
    Json(result).into_response()
}

async fn api_launch(State(state): State<AdminState>) -> Response {
    let snapshot = state.shared.snapshot();
    let config = &snapshot.config;
    let context = &state.context;
    let proxy_addr = context
        .proxy_addr
        .get()
        .map(ToString::to_string)
        .unwrap_or_else(|| config.listen_addr.clone());
    let config_flag = context
        .config_path
        .as_ref()
        .map(|path| format!(" --config {}", path.display()))
        .unwrap_or_default();

    let claude_model = INTEGRATED_AUTO_MODEL;

    let codex_env = format!(
        "# ~/.codex/config.toml\n\
         [model_providers.llmproxy]\n\
         name = \"llmproxy\"\n\
         base_url = \"http://{proxy_addr}/v1\"\n\
         env_key = \"OPENAI_API_KEY\"\n\
         wire_api = \"responses\"\n\
         \n\
         [profiles.llmproxy]\n\
         model_provider = \"llmproxy\"\n\
         model = \"gpt-5.5\"\n\
         \n\
         # then start Codex with:\n\
         codex --profile llmproxy"
    );
    let claude_env = format!(
        "ANTHROPIC_BASE_URL=\"http://{proxy_addr}\" \\\n\
         ANTHROPIC_API_KEY=\"dummy\" \\\n\
         claude --model {claude_model}"
    );

    Json(json!({
        "proxy_addr": proxy_addr,
        "default_provider": config.default_provider,
        "default_model": config.default_model,
        "routing_mode": config.routing_mode.as_str(),
        "config_path": context.config_path.as_ref().map(|path| path.display().to_string()),
        "interceptor_enabled": config.interceptor.enabled,
        "interceptor_configured": !config.interceptor.provider.trim().is_empty()
            && !config.interceptor.model.trim().is_empty(),
        "interceptor_model": config.interceptor.model,
        "integrated_enabled": config.integrated.enabled,
        "integrated_targets": config.integrated.targets.len(),
        "commands": {
            "codex": format!("llmproxy{config_flag} --codex"),
            "claude": format!("llmproxy{config_flag} --claude"),
            "copilot": format!("llmproxy{config_flag} --copilot"),
            "codex_env": codex_env,
            "claude_env": claude_env,
            "copilot_env": "Native Copilot CAPI routing requires the managed launcher",
        },
    }))
    .into_response()
}

async fn api_config(State(state): State<AdminState>) -> Response {
    let snapshot = state.shared.snapshot();
    Json(config_view(&snapshot, &state.context)).into_response()
}

#[derive(Deserialize)]
struct RoutingModePutBody {
    mode: RoutingMode,
}

async fn api_routing_mode_put(
    State(state): State<AdminState>,
    Json(body): Json<RoutingModePutBody>,
) -> Response {
    apply_config_change(
        &state,
        move |config| {
            config.set_routing_mode(body.mode);
            Ok(())
        },
        Persist::Edit(Box::new(move |doc| {
            doc.insert("routing_mode", value(body.mode.as_str()));
            for section in ["interceptor", "integrated"] {
                if let Some(table) = doc.get_mut(section).and_then(Item::as_table_mut) {
                    table.remove("enabled");
                }
            }
            Ok(())
        })),
    )
    .await
}

async fn api_restart(State(state): State<AdminState>, Json(_body): Json<Value>) -> Response {
    let Some(proxy_addr) = state.context.proxy_addr.get().copied() else {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "proxy listener is not ready",
        );
    };
    let snapshot = state.shared.snapshot();
    let target_addr = match snapshot.config.listen_addr.parse::<SocketAddr>() {
        Ok(addr) => addr,
        Err(error) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                format!("configured listen address is invalid: {error}"),
            );
        }
    };
    if target_addr.port() == 0 {
        return api_error(
            StatusCode::BAD_REQUEST,
            "a managed restart requires a fixed configured listen port",
        );
    }
    let executable = match env::current_exe() {
        Ok(executable) => executable,
        Err(error) => {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to locate llmproxy executable: {error}"),
            );
        }
    };
    let config_path = state.context.config_path.clone();
    let routing_mode = snapshot.config.routing_mode;
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let mut command = Command::new(executable);
        command
            .arg("--restart")
            .arg("--restart-from")
            .arg(proxy_addr.to_string())
            .arg("--listen")
            .arg(target_addr.to_string())
            .arg("--mode")
            .arg(routing_mode.as_str())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(config_path) = config_path {
            command.arg("--config").arg(config_path);
        }
        configure_daemon_process(&mut command);
        if let Err(error) = command.spawn() {
            error!(error = %error, "failed to launch managed restart helper");
        }
    });
    (
        StatusCode::ACCEPTED,
        Json(json!({ "accepted": true, "message": "llmproxy restart scheduled" })),
    )
        .into_response()
}

fn config_view(snapshot: &AppState, context: &AdminContext) -> Value {
    let config = &snapshot.config;
    let provider_configs = effective_provider_configs(config).unwrap_or_default();
    let mut provider_names = provider_configs.keys().cloned().collect::<Vec<_>>();
    provider_names.sort();
    let providers = provider_names
        .iter()
        .map(|name| {
            let upstream = &provider_configs[name];
            let from_upstream_section = !config.providers.contains_key(name);
            let mut header_names = upstream.headers.keys().cloned().collect::<Vec<_>>();
            header_names.sort();
            (
                name.clone(),
                json!({
                    "kind": upstream.kind.as_str(),
                    "base_url": upstream.base_url,
                    "auth": upstream.auth.as_str(),
                    "api_key_set": upstream.api_key.is_some(),
                    "api_key_env": upstream.api_key_env,
                    "reasoning_effort": upstream.reasoning_effort.map(|effort| effort.as_str()),
                    "timeout_secs": upstream.timeout_secs,
                    "connect_timeout_secs": upstream.connect_timeout_secs,
                    "pool_max_idle_per_host": upstream.pool_max_idle_per_host,
                    "pool_idle_timeout_secs": upstream.pool_idle_timeout_secs,
                    "header_names": header_names,
                    "class": class_str(&upstream.base_url),
                    "is_default": *name == config.default_provider,
                    "from_upstream_section": from_upstream_section,
                    "fallback": upstream.fallback,
                    "circuit_breaker_threshold": upstream.circuit_breaker_threshold,
                    "circuit_breaker_cooldown_secs": upstream.circuit_breaker_cooldown_secs,
                    // Live breaker state, so an operator can see which
                    // providers are currently being skipped and why.
                    "breaker_status": snapshot
                        .providers
                        .get(name)
                        .map_or("unknown", |provider| provider.breaker_status()),
                    "breaker_failures": snapshot
                        .providers
                        .get(name)
                        .map_or(0, |provider| provider.breaker_failures()),
                    "models": snapshot.providers.get(name).map(|provider| {
                        provider.model_cache.cached_ids().0
                    }).unwrap_or_default(),
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();

    let routing_rules = config
        .routing_rules
        .iter()
        .map(|rule| {
            json!({
                "name": rule.name(),
                "route": rule.route(),
                "min_input_tokens": rule.min_input_tokens(),
                "max_input_tokens": rule.max_input_tokens(),
                "has_tools": rule.has_tools(),
                "contains_any": rule.contains_any(),
                "requested_model": rule.requested_model(),
            })
        })
        .collect::<Vec<_>>();

    let aliases = config
        .model_aliases
        .iter()
        .map(|(alias, route)| {
            (
                alias.clone(),
                json!({
                    "provider": match route {
                        ModelAlias::Model(_) => Value::Null,
                        ModelAlias::Route { provider, .. } => Value::String(provider.clone()),
                    },
                    "model": route.model(),
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let integrated_targets = config
        .integrated
        .targets
        .iter()
        .map(|(name, target)| {
            (
                name.clone(),
                json!({
                    "provider": target.provider,
                    "model": target.model,
                    "hint": target.hint,
                    "reasoning_effort": target.reasoning_effort.map(crate::ReasoningEffort::as_str),
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();

    json!({
        "config_path": context.config_path.as_ref().map(|path| path.display().to_string()),
        "config_persistable": context.config_path.is_some(),
        "listen_addr": config.listen_addr,
        "max_logging_retention_days": config.max_logging_retention_days,
        "default_provider": config.default_provider,
        "default_model": config.default_model,
        "routing_mode": config.routing_mode.as_str(),
        "providers": providers,
        "model_aliases": aliases,
        "routing_rules": routing_rules,
        "pricing": {
            "baseline_model": config.pricing_baseline_model(),
            "models": config.pricing_models(),
        },
        "interceptor": {
            "enabled": config.interceptor.enabled,
            "reject_sensitive": config.interceptor.reject_sensitive,
            "provider": config.interceptor.provider,
            "model": config.interceptor.model,
            "timeout_ms": config.interceptor.timeout_ms,
            "fallback_on_error": config.interceptor.fallback_on_error,
            "instructions": crate::interceptor::intercept_instructions(&config.interceptor),
            "instructions_are_default": crate::interceptor::intercept_instructions_are_default(
                &config.interceptor,
            ),
            "temperature": config.interceptor.temperature,
            "max_tokens": config.interceptor.max_tokens,
            "parallel_tool_calls": config.interceptor.parallel_tool_calls,
        },
        "integrated": {
            "enabled": config.integrated.enabled,
            "sticky_ttl_secs": config.integrated.sticky_ttl_secs,
            "sticky_capacity": config.integrated.sticky_capacity,
            "instructions": crate::interceptor::integrated_instructions(&config.integrated),
            "instructions_are_default": crate::interceptor::integrated_instructions_are_default(
                &config.integrated,
            ),
            "targets": integrated_targets,
        },
        "role_map": config.role_map,
        "chat": { "disable_thinking": config.chat.disable_thinking },
        "responses": {
            "mode": match config.responses.mode {
                ResponsesMode::ChatCompletions => "chat_completions",
                ResponsesMode::Passthrough => "passthrough",
            }
        },
        "models": {
            "expose_aliases": config.models.expose_aliases,
            "expose_upstream": config.models.expose_upstream,
            "cache_ttl_secs": config.models.cache_ttl_secs,
        },
        "admin": {
            "enable": config.admin.enable,
            "listen_address": config.admin.listen_address,
        },
    })
}

// Runtime config editing

/// How an accepted change is written back to the config file.
type ConfigEdit = Box<dyn FnOnce(&mut DocumentMut) -> Result<(), String> + Send>;

enum Persist {
    /// Surgical `toml_edit` update that preserves comments and layout.
    Edit(ConfigEdit),
    /// Verbatim replacement (raw TOML editor).
    Raw(String),
}

/// Clone the running config, apply `mutate`, validate, rebuild the provider
/// set (reusing untouched providers and their model caches), swap the new
/// state in, then persist. Returns the API response body.
async fn apply_config_change(
    state: &AdminState,
    mutate: impl FnOnce(&mut Config) -> Result<(), String>,
    persist: Persist,
) -> Response {
    let _guard = state.context.apply_lock.lock().await;
    let current = state.shared.snapshot();
    let mut config = (*current.config).clone();
    if let Err(message) = mutate(&mut config) {
        return api_error(StatusCode::BAD_REQUEST, message);
    }
    finalize_config_keys(&mut config);
    if let Err(error) = validate_config(&config) {
        return api_error(StatusCode::BAD_REQUEST, error.to_string());
    }
    let provider_configs = match effective_provider_configs(&config) {
        Ok(provider_configs) => provider_configs,
        Err(error) => return api_error(StatusCode::BAD_REQUEST, error.to_string()),
    };

    let mut providers = HashMap::new();
    let mut rebuilt = Vec::new();
    for (name, upstream) in provider_configs {
        match current.providers.get(&name) {
            Some(existing) if *existing.config == upstream => {
                providers.insert(name, existing.clone());
            }
            _ => {
                let provider = match build_provider_state(name.clone(), upstream) {
                    Ok(provider) => provider,
                    Err(error) => {
                        return api_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
                    }
                };
                rebuilt.push(provider.clone());
                providers.insert(name, provider);
            }
        }
    }

    let auto_mapping = config.model_routing == ModelRoutingMode::Remap;
    // The post-swap providers must carry the new policy snapshot, or the
    // transport gate would stay disarmed on a rebuilt provider until a restart:
    // a runtime privacy toggle would not take effect for dispatch verification
    // while routing already honored the new policy.
    let new_state = attach_privacy_boundary(AppState {
        forward_caller_credentials: config.routing_mode != RoutingMode::Integrated,
        config: Arc::new(config),
        providers: Arc::new(providers),
        launcher_kind: None,
        launcher_mode: None,
    });
    state.shared.replace(new_state.clone());
    if let Some(addr) = state.context.proxy_addr.get()
        && let Err(error) = update_runtime_mode(*addr, new_state.config.routing_mode)
    {
        error!(error = %error, "failed to update daemon mode metadata");
    }
    info!(
        rebuilt_providers = rebuilt.len(),
        "runtime config change applied"
    );

    if auto_mapping {
        for provider in rebuilt {
            tokio::spawn(async move {
                let _refresh_guard = provider.model_refresh.lock().await;
                if let Err(error) = refresh_upstream_models(&provider, &HeaderMap::new()).await {
                    error!(
                        error = %error,
                        provider = %provider.name,
                        "failed to load upstream models after config change; will retry on request"
                    );
                }
            });
        }
    }

    let (persisted, warning) = persist_change(state, persist);
    Json(json!({
        "applied": true,
        "persisted": persisted,
        "warning": warning,
        "config": config_view(&new_state, &state.context),
    }))
    .into_response()
}

fn persist_change(state: &AdminState, persist: Persist) -> (bool, Option<String>) {
    let Some(path) = &state.context.config_path else {
        return (
            false,
            Some("no config file was loaded; this change lasts until restart".to_string()),
        );
    };
    let result = match persist {
        Persist::Raw(text) => fs::write(path, text).map_err(|error| error.to_string()),
        Persist::Edit(edit) => {
            let text = fs::read_to_string(path).unwrap_or_default();
            match DocumentMut::from_str(&text) {
                Err(error) => Err(format!("config file is not editable TOML: {error}")),
                Ok(mut doc) => edit(&mut doc)
                    .and_then(|()| fs::write(path, doc.to_string()).map_err(|e| e.to_string())),
            }
        }
    };
    match result {
        Ok(()) => (true, None),
        Err(error) => {
            error!(error = %error, path = %path.display(), "config change applied but not persisted");
            (
                false,
                Some(format!(
                    "applied at runtime, but writing {} failed: {error}",
                    path.display()
                )),
            )
        }
    }
}

/// Fetch a mutable `[section]` table, creating it as an explicit table.
fn ensure_table<'a>(doc: &'a mut DocumentMut, section: &str) -> Result<&'a mut Table, String> {
    let item = doc.entry(section).or_insert(Item::Table(Table::new()));
    let table = item
        .as_table_mut()
        .ok_or_else(|| format!("config key {section:?} is not a table"))?;
    table.set_implicit(false);
    Ok(table)
}

#[derive(Deserialize)]
struct AliasPutBody {
    model: String,
    #[serde(default)]
    provider: Option<String>,
}

async fn api_alias_put(
    State(state): State<AdminState>,
    AxumPath(alias): AxumPath<String>,
    Json(body): Json<AliasPutBody>,
) -> Response {
    let alias_name = alias.trim().to_string();
    if alias_name.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "alias name cannot be empty");
    }
    let model = body.model.trim().to_string();
    if model.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "alias model cannot be empty");
    }
    let provider = body
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|provider| !provider.is_empty())
        .map(ToOwned::to_owned);

    let route = match &provider {
        Some(provider) => ModelAlias::Route {
            provider: provider.clone(),
            model: model.clone(),
        },
        None => ModelAlias::Model(model.clone()),
    };
    let (alias_for_mutate, alias_for_doc) = (alias_name.clone(), alias_name);
    apply_config_change(
        &state,
        move |config| {
            config.model_aliases.insert(alias_for_mutate, route);
            Ok(())
        },
        Persist::Edit(Box::new(move |doc| {
            let table = ensure_table(doc, "model_aliases")?;
            match provider {
                Some(provider) => {
                    let mut inline = toml_edit::InlineTable::new();
                    inline.insert("provider", provider.into());
                    inline.insert("model", model.into());
                    table.insert(&alias_for_doc, value(inline));
                }
                None => {
                    table.insert(&alias_for_doc, value(model));
                }
            }
            Ok(())
        })),
    )
    .await
}

async fn api_alias_delete(
    State(state): State<AdminState>,
    AxumPath(alias): AxumPath<String>,
) -> Response {
    let alias_for_mutate = alias.clone();
    apply_config_change(
        &state,
        move |config| {
            if config.model_aliases.remove(&alias_for_mutate).is_none() {
                return Err(format!("unknown alias {alias_for_mutate:?}"));
            }
            Ok(())
        },
        Persist::Edit(Box::new(move |doc| {
            if let Some(table) = doc.get_mut("model_aliases").and_then(Item::as_table_mut) {
                table.remove(&alias);
            }
            Ok(())
        })),
    )
    .await
}

#[derive(Deserialize)]
struct InterceptorPutBody {
    reject_sensitive: Option<bool>,
    provider: Option<String>,
    model: Option<String>,
    timeout_ms: Option<u64>,
    fallback_on_error: Option<bool>,
    /// Empty string resets to the built-in routing instructions.
    instructions: Option<String>,
    temperature: Option<f64>,
    /// 0 removes the cap (client limits apply).
    max_tokens: Option<u64>,
    parallel_tool_calls: Option<bool>,
}

async fn api_interceptor_put(
    State(state): State<AdminState>,
    Json(body): Json<InterceptorPutBody>,
) -> Response {
    if let Some(temperature) = body.temperature
        && (!temperature.is_finite() || !(0.0..=2.0).contains(&temperature))
    {
        return api_error(
            StatusCode::BAD_REQUEST,
            "temperature must be between 0.0 and 2.0",
        );
    }
    let doc_body = InterceptorPutBody {
        reject_sensitive: body.reject_sensitive,
        provider: body.provider.clone(),
        model: body.model.clone(),
        timeout_ms: body.timeout_ms,
        fallback_on_error: body.fallback_on_error,
        instructions: body.instructions.clone(),
        temperature: body.temperature,
        max_tokens: body.max_tokens,
        parallel_tool_calls: body.parallel_tool_calls,
    };
    apply_config_change(
        &state,
        move |config| {
            let interceptor = &mut config.interceptor;
            if let Some(reject_sensitive) = body.reject_sensitive {
                interceptor.reject_sensitive = reject_sensitive;
            }
            if let Some(provider) = body.provider {
                interceptor.provider = provider.trim().to_string();
            }
            if let Some(model) = body.model {
                interceptor.model = model.trim().to_string();
            }
            if let Some(timeout_ms) = body.timeout_ms {
                interceptor.timeout_ms = timeout_ms;
            }
            if let Some(fallback) = body.fallback_on_error {
                interceptor.fallback_on_error = fallback;
            }
            if let Some(instructions) = body.instructions {
                interceptor.instructions = if instructions.trim().is_empty() {
                    InterceptorConfig::default().instructions
                } else {
                    instructions
                };
            }
            if let Some(temperature) = body.temperature {
                interceptor.temperature = temperature;
            }
            if let Some(max_tokens) = body.max_tokens {
                interceptor.max_tokens = max_tokens;
            }
            if let Some(parallel_tool_calls) = body.parallel_tool_calls {
                interceptor.parallel_tool_calls = parallel_tool_calls;
            }
            Ok(())
        },
        Persist::Edit(Box::new(move |doc| {
            let table = ensure_table(doc, "interceptor")?;
            if let Some(reject_sensitive) = doc_body.reject_sensitive {
                table.insert("reject_sensitive", value(reject_sensitive));
            }
            if let Some(provider) = doc_body.provider {
                table.insert("provider", value(provider.trim()));
            }
            if let Some(model) = doc_body.model {
                table.insert("model", value(model.trim()));
            }
            if let Some(timeout_ms) = doc_body.timeout_ms {
                table.insert("timeout_ms", value(timeout_ms as i64));
            }
            if let Some(fallback) = doc_body.fallback_on_error {
                table.insert("fallback_on_error", value(fallback));
            }
            match doc_body.instructions {
                Some(instructions) if instructions.trim().is_empty() => {
                    table.remove("instructions");
                }
                Some(instructions) => {
                    table.insert("instructions", value(instructions));
                }
                None => {}
            }
            if let Some(temperature) = doc_body.temperature {
                table.insert("temperature", value(temperature));
            }
            if let Some(max_tokens) = doc_body.max_tokens {
                table.insert("max_tokens", value(max_tokens as i64));
            }
            if let Some(parallel_tool_calls) = doc_body.parallel_tool_calls {
                table.insert("parallel_tool_calls", value(parallel_tool_calls));
            }
            Ok(())
        })),
    )
    .await
}

#[derive(Clone, Deserialize)]
struct IntegratedPutBody {
    sticky_ttl_secs: Option<u64>,
    sticky_capacity: Option<usize>,
    /// Empty string resets to the built-in integrated routing policy.
    instructions: Option<String>,
    targets: Option<BTreeMap<String, IntegratedTarget>>,
}

impl IntegratedPutBody {
    fn apply(&self, config: &mut Config) {
        if let Some(ttl) = self.sticky_ttl_secs {
            config.integrated.sticky_ttl_secs = ttl;
        }
        if let Some(capacity) = self.sticky_capacity {
            config.integrated.sticky_capacity = capacity;
        }
        if let Some(instructions) = &self.instructions {
            config.integrated.instructions = if instructions.trim().is_empty() {
                String::new()
            } else {
                instructions.clone()
            };
        }
        if let Some(targets) = &self.targets {
            config.integrated.targets = targets.clone();
        }
    }

    fn apply_to_doc(&self, doc: &mut DocumentMut) -> Result<(), String> {
        let table = ensure_table(doc, "integrated")?;
        table.remove("respect_explicit_model");
        if let Some(ttl) = self.sticky_ttl_secs {
            table.insert("sticky_ttl_secs", value(ttl as i64));
        }
        if let Some(capacity) = self.sticky_capacity {
            table.insert("sticky_capacity", value(capacity as i64));
        }
        match self.instructions.as_deref() {
            Some(instructions) if instructions.trim().is_empty() => {
                table.remove("instructions");
            }
            Some(instructions) => {
                table.insert("instructions", value(instructions));
            }
            None => {}
        }
        if let Some(targets) = &self.targets {
            let mut target_table = Table::new();
            target_table.set_implicit(true);
            for (name, target) in targets {
                let mut entry = Table::new();
                entry.insert("provider", value(target.provider.as_str()));
                entry.insert("model", value(target.model.as_str()));
                entry.insert("hint", value(target.hint.as_str()));
                if let Some(effort) = target.reasoning_effort {
                    entry.insert("reasoning_effort", value(effort.as_str()));
                }
                target_table.insert(name, Item::Table(entry));
            }
            table.insert("targets", Item::Table(target_table));
        }
        Ok(())
    }
}

async fn api_integrated_put(
    State(state): State<AdminState>,
    Json(body): Json<IntegratedPutBody>,
) -> Response {
    let doc_body = body.clone();
    apply_config_change(
        &state,
        move |config| {
            body.apply(config);
            Ok(())
        },
        Persist::Edit(Box::new(move |doc| doc_body.apply_to_doc(doc))),
    )
    .await
}

#[derive(Clone, Deserialize)]
struct ProviderPutBody {
    kind: Option<crate::ProviderKind>,
    base_url: Option<String>,
    /// Borrowed-OAuth mode: "none", "codex-oauth", or "claude-oauth".
    auth: Option<crate::ProviderAuth>,
    /// Empty string clears the stored key; missing keeps it.
    api_key: Option<String>,
    /// Empty string clears; missing keeps.
    api_key_env: Option<String>,
    /// Empty string clears the enforced outbound reasoning effort.
    reasoning_effort: Option<String>,
    timeout_secs: Option<u64>,
    connect_timeout_secs: Option<u64>,
    pool_max_idle_per_host: Option<usize>,
    pool_idle_timeout_secs: Option<u64>,
}

impl ProviderPutBody {
    fn apply(&self, upstream: &mut crate::UpstreamConfig) -> Result<(), String> {
        if let Some(kind) = self.kind {
            upstream.kind = kind;
        }
        if let Some(base_url) = &self.base_url {
            upstream.base_url = base_url.trim().to_string();
        }
        if let Some(auth) = self.auth {
            upstream.auth = auth;
        }
        match self.api_key.as_deref() {
            Some("") => upstream.api_key = None,
            Some(key) => upstream.api_key = Some(key.to_string()),
            None => {}
        }
        match self.api_key_env.as_deref() {
            Some("") => upstream.api_key_env = None,
            Some(name) => upstream.api_key_env = Some(name.to_string()),
            None => {}
        }
        if let Some(value) = &self.reasoning_effort {
            upstream.reasoning_effort = if value.trim().is_empty() {
                None
            } else {
                Some(value.parse::<crate::ReasoningEffort>()?)
            };
        }
        if let Some(timeout_secs) = self.timeout_secs {
            upstream.timeout_secs = timeout_secs;
        }
        if let Some(connect_timeout_secs) = self.connect_timeout_secs {
            upstream.connect_timeout_secs = connect_timeout_secs;
        }
        if let Some(pool_max_idle_per_host) = self.pool_max_idle_per_host {
            upstream.pool_max_idle_per_host = pool_max_idle_per_host;
        }
        if let Some(pool_idle_timeout_secs) = self.pool_idle_timeout_secs {
            upstream.pool_idle_timeout_secs = pool_idle_timeout_secs;
        }
        Ok(())
    }

    fn apply_to_doc(&self, table: &mut Table) {
        if let Some(kind) = self.kind {
            table.insert("kind", value(kind.as_str()));
        }
        if let Some(base_url) = &self.base_url {
            table.insert("base_url", value(base_url.trim()));
        }
        if let Some(auth) = self.auth {
            if auth == crate::ProviderAuth::None {
                table.remove("auth");
            } else {
                table.insert("auth", value(auth.as_str()));
            }
        }
        match self.api_key.as_deref() {
            Some("") => {
                table.remove("api_key");
            }
            Some(key) => {
                table.insert("api_key", value(key));
            }
            None => {}
        }
        match self.api_key_env.as_deref() {
            Some("") => {
                table.remove("api_key_env");
            }
            Some(name) => {
                table.insert("api_key_env", value(name));
            }
            None => {}
        }
        match self.reasoning_effort.as_deref().map(str::trim) {
            Some("") => {
                table.remove("reasoning_effort");
            }
            Some(effort) => {
                table.insert("reasoning_effort", value(effort.to_ascii_lowercase()));
            }
            None => {}
        }
        if let Some(timeout_secs) = self.timeout_secs {
            table.insert("timeout_secs", value(timeout_secs as i64));
        }
        if let Some(connect_timeout_secs) = self.connect_timeout_secs {
            table.insert("connect_timeout_secs", value(connect_timeout_secs as i64));
        }
        if let Some(pool_max_idle_per_host) = self.pool_max_idle_per_host {
            table.insert(
                "pool_max_idle_per_host",
                value(pool_max_idle_per_host as i64),
            );
        }
        if let Some(pool_idle_timeout_secs) = self.pool_idle_timeout_secs {
            table.insert(
                "pool_idle_timeout_secs",
                value(pool_idle_timeout_secs as i64),
            );
        }
    }
}

async fn api_provider_put(
    State(state): State<AdminState>,
    AxumPath(name): AxumPath<String>,
    Json(body): Json<ProviderPutBody>,
) -> Response {
    let name = name.trim().to_string();
    if name.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "provider name cannot be empty");
    }
    // The provider named by `default_provider` without an entry under
    // [providers] lives in the legacy [upstream] section.
    let snapshot = state.shared.snapshot();
    let edits_upstream_section =
        name == snapshot.config.default_provider && !snapshot.config.providers.contains_key(&name);
    let mutate_body = body.clone();
    let mutate_name = name.clone();
    apply_config_change(
        &state,
        move |config| {
            if edits_upstream_section {
                mutate_body.apply(&mut config.upstream)?;
            } else {
                let mut upstream = config
                    .providers
                    .get(&mutate_name)
                    .cloned()
                    .unwrap_or_default();
                mutate_body.apply(&mut upstream)?;
                config.providers.insert(mutate_name, upstream);
            }
            Ok(())
        },
        Persist::Edit(Box::new(move |doc| {
            let table = if edits_upstream_section {
                ensure_table(doc, "upstream")?
            } else {
                let providers = ensure_table(doc, "providers")?;
                providers.set_implicit(true);
                let item = providers.entry(&name).or_insert(Item::Table(Table::new()));
                let table = item
                    .as_table_mut()
                    .ok_or_else(|| format!("[providers.{name}] is not a table"))?;
                table.set_implicit(false);
                table
            };
            body.apply_to_doc(table);
            Ok(())
        })),
    )
    .await
}

async fn api_provider_delete(
    State(state): State<AdminState>,
    AxumPath(name): AxumPath<String>,
) -> Response {
    let mutate_name = name.clone();
    apply_config_change(
        &state,
        move |config| {
            if config.providers.remove(&mutate_name).is_none() {
                return Err(format!(
                    "unknown provider {mutate_name:?} (the [upstream] default cannot be deleted)"
                ));
            }
            Ok(())
        },
        Persist::Edit(Box::new(move |doc| {
            if let Some(providers) = doc.get_mut("providers").and_then(Item::as_table_mut) {
                providers.remove(&name);
            }
            Ok(())
        })),
    )
    .await
}

#[derive(Clone, Deserialize)]
struct SettingsPutBody {
    default_provider: Option<String>,
    default_model: Option<String>,
    max_logging_retention_days: Option<u64>,
    disable_thinking: Option<bool>,
    responses_mode: Option<String>,
    expose_aliases: Option<bool>,
    expose_upstream: Option<bool>,
    /// Negative clears the TTL (cache indefinitely).
    models_cache_ttl_secs: Option<i64>,
    role_map: Option<HashMap<String, String>>,
}

async fn api_settings_put(
    State(state): State<AdminState>,
    Json(body): Json<SettingsPutBody>,
) -> Response {
    if body
        .max_logging_retention_days
        .is_some_and(|days| days > i64::MAX as u64)
    {
        return api_error(
            StatusCode::BAD_REQUEST,
            "max_logging_retention_days is too large",
        );
    }
    let responses_mode = match body.responses_mode.as_deref() {
        None => None,
        Some("chat_completions") => Some(ResponsesMode::ChatCompletions),
        Some("passthrough") => Some(ResponsesMode::Passthrough),
        Some(other) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                format!("unknown responses mode {other:?}"),
            );
        }
    };
    let doc_body = body.clone();
    apply_config_change(
        &state,
        move |config| {
            if let Some(default_provider) = body.default_provider {
                config.default_provider = default_provider.trim().to_string();
            }
            if let Some(default_model) = body.default_model {
                let model = default_model.trim().to_string();
                config.default_model = (!model.is_empty()).then_some(model);
            }
            if let Some(days) = body.max_logging_retention_days {
                config.max_logging_retention_days = days;
            }
            if let Some(disable_thinking) = body.disable_thinking {
                config.chat.disable_thinking = disable_thinking;
            }
            if let Some(mode) = responses_mode {
                config.responses.mode = mode;
            }
            if let Some(expose_aliases) = body.expose_aliases {
                config.models.expose_aliases = expose_aliases;
            }
            if let Some(expose_upstream) = body.expose_upstream {
                config.models.expose_upstream = expose_upstream;
            }
            if let Some(ttl) = body.models_cache_ttl_secs {
                config.models.cache_ttl_secs = u64::try_from(ttl).ok();
            }
            if let Some(role_map) = body.role_map {
                config.role_map = role_map;
            }
            Ok(())
        },
        Persist::Edit(Box::new(move |doc| {
            if let Some(default_provider) = doc_body.default_provider {
                doc.insert("default_provider", value(default_provider.trim()));
            }
            if let Some(default_model) = doc_body.default_model {
                let model = default_model.trim();
                if model.is_empty() {
                    doc.remove("default_model");
                } else {
                    doc.insert("default_model", value(model));
                }
            }
            if let Some(days) = doc_body.max_logging_retention_days {
                doc.insert("max_logging_retention_days", value(days as i64));
            }
            if let Some(disable_thinking) = doc_body.disable_thinking {
                ensure_table(doc, "chat")?.insert("disable_thinking", value(disable_thinking));
            }
            if let Some(mode) = doc_body.responses_mode {
                ensure_table(doc, "responses")?.insert("mode", value(mode));
            }
            if let Some(expose_aliases) = doc_body.expose_aliases {
                ensure_table(doc, "models")?.insert("expose_aliases", value(expose_aliases));
            }
            if let Some(expose_upstream) = doc_body.expose_upstream {
                ensure_table(doc, "models")?.insert("expose_upstream", value(expose_upstream));
            }
            if let Some(ttl) = doc_body.models_cache_ttl_secs {
                let models = ensure_table(doc, "models")?;
                if ttl < 0 {
                    models.remove("cache_ttl_secs");
                } else {
                    models.insert("cache_ttl_secs", value(ttl));
                }
            }
            if let Some(role_map) = doc_body.role_map {
                let table = ensure_table(doc, "role_map")?;
                let existing = table
                    .iter()
                    .map(|(key, _)| key.to_string())
                    .collect::<Vec<_>>();
                for key in existing {
                    table.remove(&key);
                }
                let mut roles = role_map.into_iter().collect::<Vec<_>>();
                roles.sort();
                for (from, to) in roles {
                    table.insert(&from, value(to));
                }
            }
            Ok(())
        })),
    )
    .await
}

async fn api_config_raw_get(State(state): State<AdminState>) -> Response {
    let Some(path) = &state.context.config_path else {
        return Json(json!({
            "toml": Value::Null,
            "config_path": Value::Null,
            "editable": false,
        }))
        .into_response();
    };
    match fs::read_to_string(path) {
        Ok(toml) => Json(json!({
            "toml": toml,
            "config_path": path.display().to_string(),
            "editable": true,
        }))
        .into_response(),
        Err(error) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to read {}: {error}", path.display()),
        ),
    }
}

#[derive(Deserialize)]
struct RawConfigPutBody {
    toml: String,
}

async fn api_config_raw_put(
    State(state): State<AdminState>,
    Json(body): Json<RawConfigPutBody>,
) -> Response {
    if state.context.config_path.is_none() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "no config file was loaded at startup; the raw editor is unavailable",
        );
    }
    let parsed = match parse_config(&body.toml, "admin editor") {
        Ok(parsed) => parsed,
        Err(error) => return api_error(StatusCode::BAD_REQUEST, error.to_string()),
    };
    apply_config_change(
        &state,
        move |config| {
            // Bound listeners and wire adapters are process state; carry them
            // over while applying the routing mode and editable configuration.
            let model_routing = config.model_routing;
            let anthropic_mode = config.anthropic_mode;
            *config = parsed;
            config.model_routing = model_routing;
            config.anthropic_mode = anthropic_mode;
            Ok(())
        },
        Persist::Raw(body.toml),
    )
    .await
}

#[cfg(test)]
#[path = "admin_tests.rs"]
mod tests;
