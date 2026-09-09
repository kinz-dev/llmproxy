use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tokio::process::{Child, Command};
use tracing::info;

use super::Cli;
use crate::agent_launcher::{
    LauncherKind, LauncherMode, LauncherSession, configure_proxy_process, contains_arg, home_dir,
    non_empty_env_value, prepend_args, require_loopback, resolve_agent_executable,
    with_integrated_auto_model,
};

const CODEX_PROXY_PROVIDER: &str = "llmproxy_intercept";
const OPENAI_API_BASE_URL: &str = "https://api.openai.com/v1";
const CHATGPT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const EXPLICIT_ARGS: &[&str] = &[
    "exec",
    "e",
    "review",
    "login",
    "logout",
    "mcp",
    "plugin",
    "mcp-server",
    "app-server",
    "remote-control",
    "completion",
    "update",
    "doctor",
    "sandbox",
    "debug",
    "apply",
    "a",
    "resume",
    "archive",
    "delete",
    "unarchive",
    "fork",
    "cloud",
    "exec-server",
    "features",
    "help",
    "-h",
    "--help",
    "-V",
    "--version",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodexAuthRoute {
    ApiKeyEnvironment,
    ApiKeyManaged,
    CodexBackend,
}

impl CodexAuthRoute {
    fn default_base_url(self) -> &'static str {
        match self {
            Self::ApiKeyEnvironment | Self::ApiKeyManaged => OPENAI_API_BASE_URL,
            Self::CodexBackend => CHATGPT_CODEX_BASE_URL,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct CodexLaunch {
    codex_bin: PathBuf,
    codex_args: Vec<OsString>,
    upstream_base_url: String,
    mode: LauncherMode,
}

impl CodexLaunch {
    pub(super) async fn discover(cli: &Cli, mode: LauncherMode) -> Result<Self> {
        let codex_bin = cli
            .codex_bin
            .clone()
            .or_else(|| env::var_os("LLMPROXY_CODEX_BIN").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("codex"));
        let codex_bin =
            resolve_agent_executable("Codex", codex_bin, "--codex-bin or LLMPROXY_CODEX_BIN")?;
        let codex_args = with_integrated_auto_model(&codex_launch_args(&cli.agent_args), mode);
        let upstream_base_url = if mode == LauncherMode::Intercept {
            let auth_route = detect_auth_route(&codex_bin, &codex_args).await?;
            let configured_base_url = configured_openai_base_url(&cli.agent_args)?;
            let upstream_base_url = cli
                .default_provider_url
                .clone()
                .or(configured_base_url)
                .unwrap_or_else(|| auth_route.default_base_url().to_string())
                .trim_end_matches('/')
                .to_string();
            if upstream_base_url.is_empty() {
                bail!("Codex upstream base URL cannot be empty");
            }
            upstream_base_url
        } else {
            String::new()
        };

        Ok(Self {
            codex_bin,
            codex_args,
            upstream_base_url,
            mode,
        })
    }

    pub(super) fn spawn(&self, proxy_addr: SocketAddr) -> Result<Child> {
        require_loopback("Codex", proxy_addr)?;
        let proxy_root_url = format!("http://{proxy_addr}/v1");
        if self.mode == LauncherMode::Intercept
            && same_base_url(&proxy_root_url, &self.upstream_base_url)
        {
            bail!("Codex upstream resolves back to this proxy");
        }
        let token =
            LauncherSession::for_launcher(LauncherKind::Codex, self.mode, &self.upstream_base_url)?
                .encode()?;
        let proxy_base_url = format!("http://{proxy_addr}/_launcher/{token}/v1");

        let provider_override =
            codex_provider_override(&proxy_base_url, self.mode == LauncherMode::Intercept);
        let mut command = Command::new(&self.codex_bin);
        command
            .arg("-c")
            .arg(provider_override)
            .arg("-c")
            .arg(format!(
                "model_provider={}",
                toml_string(CODEX_PROXY_PROVIDER)
            ))
            .arg("-c")
            .arg("features.enable_request_compression=false")
            .args(&self.codex_args)
            .env("LLMPROXY_CODEX_ACTIVE", "1")
            .env("LLMPROXY_CODEX_PROXY_URL", &proxy_base_url);
        configure_proxy_process(&mut command);

        info!(
            executable = %self.codex_bin.display(),
            proxy = %proxy_base_url,
            mode = ?self.mode,
            "launching Codex through llmproxy"
        );
        command.spawn().with_context(|| {
            format!(
                "failed to launch Codex executable {}",
                self.codex_bin.display()
            )
        })
    }
}

async fn detect_auth_route(codex_bin: &Path, codex_args: &[OsString]) -> Result<CodexAuthRoute> {
    if let Some(route) =
        environment_auth_route(codex_args, |name| non_empty_env_value(name).is_some())
    {
        return Ok(route);
    }

    let output = Command::new(codex_bin)
        .args(["login", "status"])
        .output()
        .await
        .with_context(|| {
            format!(
                "failed to inspect Codex login using {}",
                codex_bin.display()
            )
        })?;
    let status = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() {
        bail!("Codex login is unavailable: {}", status.trim());
    }
    classify_login_status(&status)
}

fn classify_login_status(status: &str) -> Result<CodexAuthRoute> {
    let normalized = status.to_ascii_lowercase();
    if normalized.contains("amazon bedrock") {
        bail!("Codex Amazon Bedrock login is not supported by intercept-mode launchers");
    }
    if normalized.contains("api key") {
        return Ok(CodexAuthRoute::ApiKeyManaged);
    }
    if normalized.contains("chatgpt")
        || normalized.contains("access token")
        || normalized.contains("personal access token")
    {
        return Ok(CodexAuthRoute::CodexBackend);
    }
    bail!("could not determine Codex login type from `codex login status`");
}

fn configured_openai_base_url(codex_args: &[OsString]) -> Result<Option<String>> {
    let Some(codex_home) = codex_home_path() else {
        return Ok(None);
    };
    let config_path = codex_home.join("config.toml");
    let config = read_optional_toml(&config_path)?.unwrap_or_else(empty_toml_table);
    let profile = selected_profile(codex_args);
    let profile_base_url = if let Some(profile) = profile.as_deref() {
        validate_profile_name(profile)?;
        let profile_path = codex_home.join(format!("{profile}.config.toml"));
        read_optional_toml(&profile_path)?
            .as_ref()
            .and_then(openai_base_url_from_config)
            .map(str::to_string)
    } else {
        None
    };
    let legacy_profile_base_url = profile.as_deref().and_then(|profile| {
        config
            .get("profiles")
            .and_then(|profiles| profiles.get(profile))
            .and_then(|profile| profile.get("openai_base_url"))
            .and_then(toml::Value::as_str)
    });
    Ok(profile_base_url
        .or_else(|| legacy_profile_base_url.map(str::to_string))
        .or_else(|| openai_base_url_from_config(&config).map(str::to_string)))
}

fn openai_base_url_from_config(config: &toml::Value) -> Option<&str> {
    config
        .get("openai_base_url")
        .and_then(toml::Value::as_str)
        .or_else(|| {
            config
                .get("model_providers")
                .and_then(|providers| providers.get("openai"))
                .and_then(|provider| provider.get("base_url"))
                .and_then(toml::Value::as_str)
        })
}

fn read_optional_toml(path: &Path) -> Result<Option<toml::Value>> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read Codex config {}", path.display()));
        }
    };
    toml::from_str(&raw)
        .map(Some)
        .with_context(|| format!("invalid Codex config {}", path.display()))
}

fn empty_toml_table() -> toml::Value {
    toml::Value::Table(toml::map::Map::new())
}

fn validate_profile_name(profile: &str) -> Result<()> {
    if profile.is_empty() || profile == "." || profile == ".." || profile.contains(['/', '\\']) {
        bail!("invalid Codex profile name {profile:?}");
    }
    Ok(())
}

fn codex_home_path() -> Option<PathBuf> {
    if let Some(codex_home) = env::var_os("CODEX_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(codex_home));
    }
    home_dir().map(|home| home.join(".codex"))
}

fn selected_profile(args: &[OsString]) -> Option<String> {
    for (index, arg) in args.iter().enumerate() {
        if arg == OsStr::new("--profile") || arg == OsStr::new("-p") {
            return args
                .get(index + 1)
                .and_then(|value| value.to_str())
                .map(str::to_string);
        }
        if let Some(arg) = arg.to_str()
            && let Some(profile) = arg.strip_prefix("--profile=")
        {
            return Some(profile.to_string());
        }
    }
    None
}

fn codex_launch_args(args: &[OsString]) -> Vec<OsString> {
    if contains_arg(args, EXPLICIT_ARGS, &[]) {
        return args.to_vec();
    }
    prepend_args(args, &["resume", "--last"])
}

fn codex_enables_api_key_env(args: &[OsString]) -> bool {
    // Codex enables CODEX_API_KEY for non-interactive exec/review runs, but not for the TUI.
    args.iter()
        .any(|arg| matches!(arg.to_str(), Some("exec" | "e" | "review")))
}

fn environment_auth_route(
    args: &[OsString],
    non_empty: impl Fn(&str) -> bool,
) -> Option<CodexAuthRoute> {
    if codex_enables_api_key_env(args) && non_empty("CODEX_API_KEY") {
        Some(CodexAuthRoute::ApiKeyEnvironment)
    } else if non_empty("CODEX_ACCESS_TOKEN") {
        Some(CodexAuthRoute::CodexBackend)
    } else {
        None
    }
}

fn codex_provider_override(proxy_base_url: &str, requires_openai_auth: bool) -> String {
    format!(
        "model_providers.{CODEX_PROXY_PROVIDER}={{ name = \"OpenAI\", base_url = {}, wire_api = \"responses\", requires_openai_auth = {requires_openai_auth}, supports_websockets = false }}",
        toml_string(proxy_base_url),
    )
}

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).expect("strings are always JSON serializable")
}

fn same_base_url(left: &str, right: &str) -> bool {
    left.trim_end_matches('/') == right.trim_end_matches('/')
}

#[cfg(test)]
#[path = "codex_launcher_tests.rs"]
mod tests;
