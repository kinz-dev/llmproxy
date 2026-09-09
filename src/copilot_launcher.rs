use std::env;
use std::ffi::OsString;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use reqwest::Url;
use tokio::process::{Child, Command};
use tracing::info;

use super::Cli;
use crate::INTEGRATED_AUTO_MODEL;
use crate::agent_launcher::{
    LauncherKind, LauncherMode, LauncherSession, configure_proxy_process, contains_arg,
    has_model_arg, home_dir, non_empty_env_value, prepend_args, require_loopback,
    resolve_agent_executable,
};

const COPILOT_API_BASE_URL: &str = "https://api.githubcopilot.com";
const LOCAL_COPILOT_MODEL_ID: &str = "gpt-5.4";
const PROXY_COPILOT_WIRE_MODEL: &str = "llmproxy-auto";
const EXPLICIT_ARGS: &[&str] = &[
    "completion",
    "help",
    "init",
    "login",
    "mcp",
    "plugin",
    "update",
    "version",
    "--connect",
    "--continue",
    "--resume",
    "--session-id",
    "--prompt",
    "-p",
    "--interactive",
    "-i",
    "--worktree",
    "-h",
    "--help",
    "-v",
    "--version",
];
const EXPLICIT_VALUE_PREFIXES: &[&str] = &[
    "--connect=",
    "--resume=",
    "--session-id=",
    "--prompt=",
    "--worktree=",
];
const COPILOT_PROVIDER_ENV: [&str; 10] = [
    "COPILOT_PROVIDER_BASE_URL",
    "COPILOT_PROVIDER_TYPE",
    "COPILOT_PROVIDER_API_KEY",
    "COPILOT_PROVIDER_BEARER_TOKEN",
    "COPILOT_PROVIDER_WIRE_API",
    "COPILOT_PROVIDER_AZURE_API_VERSION",
    "COPILOT_PROVIDER_MODEL_ID",
    "COPILOT_PROVIDER_WIRE_MODEL",
    "COPILOT_PROVIDER_MAX_PROMPT_TOKENS",
    "COPILOT_PROVIDER_MAX_OUTPUT_TOKENS",
];

#[derive(Debug, Clone)]
pub(super) struct CopilotLaunch {
    copilot_bin: PathBuf,
    copilot_args: Vec<OsString>,
    upstream_base_url: String,
    github_mcp_url: String,
    remote_sessions_url: String,
    mode: LauncherMode,
}

impl CopilotLaunch {
    pub(super) async fn discover(cli: &Cli, mode: LauncherMode) -> Result<Self> {
        let copilot_bin = cli
            .copilot_bin
            .clone()
            .or_else(|| env::var_os("LLMPROXY_COPILOT_BIN").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("copilot"));
        let copilot_bin = resolve_agent_executable(
            "Copilot",
            copilot_bin,
            "--copilot-bin or LLMPROXY_COPILOT_BIN",
        )?;
        let upstream_base_url = discover_copilot_upstream(cli.default_provider_url.as_deref())?;
        let github_mcp_url = non_empty_env_value("GITHUB_MCP_URL_OVERRIDE")
            .unwrap_or_else(|| append_path(&upstream_base_url, "mcp"));
        let remote_sessions_url = non_empty_env_value("COPILOT_MC_BASE_URL")
            .unwrap_or_else(|| append_path(&upstream_base_url, "agents"));
        let current_session_exists = copilot_session_exists_for_current_home();

        Ok(Self {
            copilot_bin,
            copilot_args: copilot_launch_args(&cli.agent_args, current_session_exists),
            upstream_base_url,
            github_mcp_url,
            remote_sessions_url,
            mode,
        })
    }

    pub(super) fn spawn(&self, proxy_addr: SocketAddr) -> Result<Child> {
        require_loopback("Copilot", proxy_addr)?;
        if base_url_targets_addr(&self.upstream_base_url, proxy_addr) {
            bail!("Copilot upstream resolves back to this proxy");
        }
        let token = LauncherSession::for_launcher(
            LauncherKind::Copilot,
            self.mode,
            &self.upstream_base_url,
        )?
        .encode()?;
        let proxy_base_url = match self.mode {
            LauncherMode::Intercept => format!("http://{proxy_addr}/_launcher/{token}"),
            LauncherMode::Proxy | LauncherMode::Integrated => {
                format!("http://{proxy_addr}/_launcher/{token}/v1")
            }
        };

        let mut command = Command::new(&self.copilot_bin);
        command
            .args(&self.copilot_args)
            .env("GITHUB_MCP_URL_OVERRIDE", &self.github_mcp_url)
            .env("COPILOT_MC_BASE_URL", &self.remote_sessions_url)
            .env("COPILOT_CLI_DISABLE_WEBSOCKET_RESPONSES", "1")
            .env("WEBSOCKET_RESPONSES", "false")
            .env("COPILOT_EXP_COPILOT_CLI_WEBSOCKET_RESPONSES", "false")
            .env("LLMPROXY_COPILOT_ACTIVE", "1")
            .env("LLMPROXY_COPILOT_PROXY_URL", &proxy_base_url);
        configure_proxy_process(&mut command);
        for name in COPILOT_PROVIDER_ENV {
            command.env_remove(name);
        }
        if self.mode != LauncherMode::Intercept {
            command
                .env_remove("COPILOT_API_URL")
                .env("COPILOT_PROVIDER_BASE_URL", &proxy_base_url)
                .env("COPILOT_PROVIDER_TYPE", "openai")
                .env("COPILOT_PROVIDER_WIRE_API", "responses");
            if !copilot_model_is_configured(&self.copilot_args) {
                let wire_model = copilot_default_wire_model(self.mode)
                    .expect("native interceptor branch handled above");
                command
                    .env("COPILOT_MODEL", LOCAL_COPILOT_MODEL_ID)
                    .env("COPILOT_PROVIDER_WIRE_MODEL", wire_model);
            }
        } else {
            // COPILOT_API_URL is a shipped CAPI compatibility hook. Copilot still
            // owns credential loading and sends its native authorization headers.
            command.env("COPILOT_API_URL", &proxy_base_url);
        }

        info!(
            executable = %self.copilot_bin.display(),
            proxy = %proxy_base_url,
            mode = ?self.mode,
            "launching GitHub Copilot CLI through llmproxy"
        );
        command.spawn().with_context(|| {
            format!(
                "failed to launch Copilot executable {}",
                self.copilot_bin.display()
            )
        })
    }
}

fn copilot_default_wire_model(mode: LauncherMode) -> Option<&'static str> {
    match mode {
        LauncherMode::Proxy => Some(PROXY_COPILOT_WIRE_MODEL),
        LauncherMode::Integrated => Some(INTEGRATED_AUTO_MODEL),
        LauncherMode::Intercept => None,
    }
}

fn copilot_model_is_configured(args: &[OsString]) -> bool {
    copilot_model_is_configured_with(args, &|name| non_empty_env_value(name))
}

fn copilot_model_is_configured_with(
    args: &[OsString],
    env_value: &impl Fn(&str) -> Option<String>,
) -> bool {
    env_value("COPILOT_MODEL").is_some() || has_model_arg(args)
}

fn discover_copilot_upstream(cli_upstream: Option<&str>) -> Result<String> {
    if let Some(base_url) = cli_upstream
        .map(str::to_string)
        .or_else(|| non_empty_env_value("COPILOT_API_URL"))
    {
        return normalize_copilot_base_url(&base_url);
    }

    for name in ["COPILOT_GH_HOST", "GH_HOST"] {
        if let Some(host) = non_empty_env_value(name) {
            return copilot_api_url_for_github_host(&host);
        }
    }
    Ok(COPILOT_API_BASE_URL.to_string())
}

fn normalize_copilot_base_url(base_url: &str) -> Result<String> {
    let mut base_url = base_url.trim().trim_end_matches('/');
    if let Some(without_v1) = base_url.strip_suffix("/v1") {
        base_url = without_v1.trim_end_matches('/');
    }
    if base_url.is_empty() {
        bail!("Copilot upstream base URL cannot be empty");
    }
    let parsed = Url::parse(base_url).context("invalid Copilot upstream base URL")?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        bail!("Copilot upstream base URL must be an HTTP(S) URL");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        bail!("Copilot upstream base URL cannot contain a query or fragment");
    }
    Ok(base_url.to_string())
}

fn copilot_api_url_for_github_host(host: &str) -> Result<String> {
    let host = host
        .trim()
        .trim_end_matches('/')
        .strip_prefix("https://")
        .or_else(|| host.trim().trim_end_matches('/').strip_prefix("http://"))
        .unwrap_or_else(|| host.trim().trim_end_matches('/'))
        .split('/')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if host == "github.com" {
        return Ok(COPILOT_API_BASE_URL.to_string());
    }
    if host.ends_with(".ghe.com") {
        let host = host.strip_prefix("copilot-api.").unwrap_or(&host);
        return Ok(format!("https://copilot-api.{host}"));
    }
    bail!("cannot derive the Copilot API URL for GitHub host {host:?}; pass --upstream explicitly")
}

fn base_url_targets_addr(base_url: &str, addr: SocketAddr) -> bool {
    let Ok(url) = Url::parse(base_url) else {
        return false;
    };
    let host_matches = url
        .host_str()
        .is_some_and(|host| host == addr.ip().to_string() || host == "localhost");
    host_matches && url.port_or_known_default() == Some(addr.port())
}

fn append_path(base_url: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base_url.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

fn copilot_home_path() -> Option<PathBuf> {
    if let Some(path) = env::var_os("COPILOT_HOME").filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(path));
    }
    home_dir().map(|home| home.join(".copilot"))
}

fn copilot_session_exists_for_current_home() -> bool {
    copilot_home_path().is_some_and(|home| copilot_session_exists(&home))
}

fn copilot_session_exists(copilot_home: &Path) -> bool {
    fs::read_dir(copilot_home.join("session-state"))
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .any(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
}

fn copilot_launch_args(args: &[OsString], current_session_exists: bool) -> Vec<OsString> {
    if !current_session_exists || contains_arg(args, EXPLICIT_ARGS, EXPLICIT_VALUE_PREFIXES) {
        return args.to_vec();
    }
    prepend_args(args, &["--continue"])
}

#[cfg(test)]
#[path = "copilot_launcher_tests.rs"]
mod tests;
