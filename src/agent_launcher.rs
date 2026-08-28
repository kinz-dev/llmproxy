use std::env;
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::{Child, Command};

use super::Cli;
use crate::INTEGRATED_AUTO_MODEL;
use crate::claude_launcher::ClaudeLaunch;
use crate::codex_launcher::CodexLaunch;
use crate::copilot_launcher::CopilotLaunch;

fn no_proxy_value(name: &str) -> String {
    let existing = env::var(name).unwrap_or_default();
    let mut entries = existing
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    for loopback in ["127.0.0.1", "localhost", "::1"] {
        if !entries.iter().any(|entry| entry == loopback) {
            entries.push(loopback.to_string());
        }
    }
    entries.join(",")
}

pub(super) fn configure_proxy_process(command: &mut Command) {
    command
        .env("NO_PROXY", no_proxy_value("NO_PROXY"))
        .env("no_proxy", no_proxy_value("no_proxy"))
        .kill_on_drop(true);
}

pub(super) fn require_loopback(agent: &str, addr: SocketAddr) -> Result<()> {
    if !addr.ip().is_loopback() {
        bail!("{agent} launcher requires a loopback listen address, got {addr}");
    }
    Ok(())
}

pub(super) fn non_empty_env_value(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

pub(super) fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

pub(super) fn contains_arg(args: &[OsString], exact: &[&str], value_prefixes: &[&str]) -> bool {
    args.iter().any(|arg| {
        arg.to_str().is_some_and(|arg| {
            exact.contains(&arg) || value_prefixes.iter().any(|prefix| arg.starts_with(prefix))
        })
    })
}

pub(super) fn prepend_args(args: &[OsString], prefix: &[&str]) -> Vec<OsString> {
    prefix
        .iter()
        .map(OsString::from)
        .chain(args.iter().cloned())
        .collect()
}

pub(super) fn has_model_arg(args: &[OsString]) -> bool {
    contains_arg(args, &["--model", "-m"], &["--model=", "-m="])
}

pub(super) fn with_integrated_auto_model(args: &[OsString], mode: LauncherMode) -> Vec<OsString> {
    if mode == LauncherMode::Integrated && !has_model_arg(args) {
        prepend_args(args, &["--model", INTEGRATED_AUTO_MODEL])
    } else {
        args.to_vec()
    }
}

pub(super) enum AgentLaunch {
    Claude(ClaudeLaunch),
    Codex(CodexLaunch),
    Copilot(CopilotLaunch),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum LauncherKind {
    Claude,
    Codex,
    Copilot,
}

impl LauncherKind {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Copilot => "copilot",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum LauncherMode {
    Proxy,
    #[default]
    Intercept,
    Integrated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct LauncherSession {
    version: u8,
    kind: LauncherKind,
    #[serde(default)]
    mode: LauncherMode,
    upstream_base_url: String,
    /// Absolute path of the project root whose `.llmproxy.toml` scopes this
    /// session, so one daemon keeps serving every project. The token is
    /// unsigned, so the daemon re-reads that file and re-validates it against
    /// its own providers rather than trusting the token's claim about it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project_root: Option<String>,
}

/// The project the launcher resolved for this process. A launcher process
/// serves exactly one working directory, so it is set once during discovery
/// and read when the session token is minted.
static LAUNCH_PROJECT_ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();

pub(super) fn set_launch_project(root: Option<PathBuf>) {
    let _ = LAUNCH_PROJECT_ROOT.set(root);
}

fn launch_project_root() -> Option<String> {
    LAUNCH_PROJECT_ROOT
        .get()
        .and_then(Option::as_ref)
        .map(|root| root.display().to_string())
}

impl LauncherSession {
    const VERSION: u8 = 1;

    pub(super) fn new(kind: LauncherKind, upstream_base_url: String) -> Result<Self> {
        Self::for_mode(kind, LauncherMode::Intercept, upstream_base_url)
    }

    pub(super) fn local(kind: LauncherKind, mode: LauncherMode) -> Result<Self> {
        Self::for_mode(kind, mode, String::new())
    }

    pub(super) fn for_launcher(
        kind: LauncherKind,
        mode: LauncherMode,
        upstream_base_url: &str,
    ) -> Result<Self> {
        let session = match mode {
            LauncherMode::Intercept => Self::new(kind, upstream_base_url.to_string()),
            LauncherMode::Proxy | LauncherMode::Integrated => Self::local(kind, mode),
        }?;
        Ok(session.with_project_root(launch_project_root()))
    }

    pub(super) fn with_project_root(mut self, project_root: Option<String>) -> Self {
        self.project_root = project_root;
        self
    }

    fn for_mode(kind: LauncherKind, mode: LauncherMode, upstream_base_url: String) -> Result<Self> {
        let upstream_base_url = upstream_base_url.trim().trim_end_matches('/').to_string();
        match mode {
            LauncherMode::Intercept if upstream_base_url.is_empty() => {
                bail!("interceptor launcher upstream base URL cannot be empty");
            }
            LauncherMode::Proxy | LauncherMode::Integrated if !upstream_base_url.is_empty() => {
                bail!("local launcher sessions cannot carry an external upstream URL");
            }
            _ => {}
        }
        Ok(Self {
            version: Self::VERSION,
            kind,
            mode,
            upstream_base_url,
            project_root: None,
        })
    }

    pub(super) fn encode(&self) -> Result<String> {
        let bytes = serde_json::to_vec(self).context("failed to encode launcher session")?;
        Ok(URL_SAFE_NO_PAD.encode(bytes))
    }

    pub(super) fn decode(token: &str) -> Result<Self> {
        let bytes = URL_SAFE_NO_PAD
            .decode(token)
            .context("invalid launcher session token")?;
        let session: Self =
            serde_json::from_slice(&bytes).context("invalid launcher session payload")?;
        if session.version != Self::VERSION {
            bail!(
                "unsupported launcher session version {}; expected {}",
                session.version,
                Self::VERSION
            );
        }
        let project_root = session.project_root.clone();
        Ok(
            Self::for_mode(session.kind, session.mode, session.upstream_base_url)?
                .with_project_root(project_root),
        )
    }

    /// The project root the launcher claimed. Nothing about it is trusted: the
    /// daemon re-reads the file at this path and re-validates its contents.
    pub(super) fn project_root(&self) -> Option<&str> {
        self.project_root.as_deref()
    }

    pub(super) fn kind(&self) -> LauncherKind {
        self.kind
    }

    pub(super) fn mode(&self) -> LauncherMode {
        self.mode
    }

    pub(super) fn upstream_base_url(&self) -> &str {
        &self.upstream_base_url
    }
}

impl AgentLaunch {
    pub(super) async fn discover(cli: &Cli, mode: LauncherMode) -> Result<Option<Self>> {
        if !cli.claude && !cli.codex && !cli.copilot {
            return Ok(None);
        }
        if cli.claude {
            return ClaudeLaunch::discover(cli, mode)
                .await
                .map(Self::Claude)
                .map(Some);
        }
        if cli.codex {
            return CodexLaunch::discover(cli, mode)
                .await
                .map(Self::Codex)
                .map(Some);
        }
        if cli.copilot {
            return CopilotLaunch::discover(cli, mode)
                .await
                .map(Self::Copilot)
                .map(Some);
        }
        unreachable!("launcher selection was checked before discovery")
    }

    pub(super) fn spawn(&self, proxy_addr: SocketAddr) -> Result<Child> {
        match self {
            Self::Claude(launch) => launch.spawn(proxy_addr),
            Self::Codex(launch) => launch.spawn(proxy_addr),
            Self::Copilot(launch) => launch.spawn(proxy_addr),
        }
    }

    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::Claude(_) => "Claude",
            Self::Codex(_) => "Codex",
            Self::Copilot(_) => "Copilot",
        }
    }
}

pub(super) async fn probe_existing_llmproxy(addr: SocketAddr) -> bool {
    let Ok(client) = Client::builder()
        .timeout(Duration::from_millis(500))
        .no_proxy()
        .build()
    else {
        return false;
    };
    let Ok(response) = client.get(format!("http://{addr}/health")).send().await else {
        return false;
    };
    if !response.status().is_success() {
        return false;
    }
    let Ok(body) = response.json::<Value>().await else {
        return false;
    };
    body.get("status").and_then(Value::as_str) == Some("ok")
}

#[cfg(test)]
#[path = "agent_launcher_tests.rs"]
mod tests;
