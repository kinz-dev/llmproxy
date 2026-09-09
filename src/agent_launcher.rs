use std::env;
#[cfg(any(windows, test))]
use std::ffi::OsStr;
use std::ffi::OsString;
use std::net::SocketAddr;
#[cfg(any(windows, test))]
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::{Child, Command};
#[cfg(windows)]
use tracing::info;

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

#[cfg(any(windows, test))]
const WINDOWS_LAUNCH_EXTENSIONS: [&str; 4] = [".COM", ".EXE", ".BAT", ".CMD"];

pub(super) fn resolve_agent_executable(
    agent: &str,
    configured: PathBuf,
    configuration_hint: &str,
) -> Result<PathBuf> {
    if configured.as_os_str().is_empty() {
        bail!("{agent} executable cannot be empty; set {configuration_hint}");
    }

    #[cfg(not(windows))]
    {
        let _ = (agent, configuration_hint);
        Ok(configured)
    }

    #[cfg(windows)]
    {
        let search_dirs = windows_agent_search_dirs();
        let extensions = windows_launch_extensions(env::var_os("PATHEXT").as_deref());
        let resolved = resolve_windows_executable_with(
            &configured,
            &search_dirs,
            &extensions,
            Path::is_file,
        )
        .with_context(|| {
            format!(
                "{agent} executable {} was not found; install {agent}, add its .exe/.cmd shim to PATH, or set {configuration_hint}",
                configured.display()
            )
        })?;
        if resolved != configured {
            info!(
                agent,
                configured = %configured.display(),
                resolved = %resolved.display(),
                "resolved Windows agent executable"
            );
        }
        Ok(resolved)
    }
}

#[cfg(any(windows, test))]
fn windows_launch_extensions(path_ext: Option<&OsStr>) -> Vec<OsString> {
    let mut extensions = Vec::new();
    if let Some(path_ext) = path_ext {
        for extension in path_ext.to_string_lossy().split(';') {
            push_windows_launch_extension(&mut extensions, extension);
        }
    }
    for extension in WINDOWS_LAUNCH_EXTENSIONS {
        push_windows_launch_extension(&mut extensions, extension);
    }
    extensions
}

#[cfg(any(windows, test))]
fn push_windows_launch_extension(extensions: &mut Vec<OsString>, extension: &str) {
    let extension = extension.trim();
    if extension.is_empty() {
        return;
    }
    let normalized = if extension.starts_with('.') {
        extension.to_ascii_uppercase()
    } else {
        format!(".{}", extension.to_ascii_uppercase())
    };
    if !WINDOWS_LAUNCH_EXTENSIONS.contains(&normalized.as_str())
        || extensions
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(OsStr::new(&normalized)))
    {
        return;
    }
    extensions.push(OsString::from(normalized));
}

#[cfg(windows)]
fn windows_agent_search_dirs() -> Vec<PathBuf> {
    let mut directories = env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .into_iter()
        .collect::<Vec<_>>();
    if let Some(path) = env::var_os("PATH") {
        directories.extend(env::split_paths(&path).filter(|path| !path.as_os_str().is_empty()));
    }
    directories
}

#[cfg(any(windows, test))]
fn resolve_windows_executable_with(
    configured: &Path,
    search_dirs: &[PathBuf],
    extensions: &[OsString],
    is_file: impl Fn(&Path) -> bool,
) -> Result<PathBuf> {
    let has_directory = configured
        .parent()
        .is_some_and(|parent| !parent.as_os_str().is_empty());
    let bases = if has_directory {
        vec![configured.to_path_buf()]
    } else {
        search_dirs
            .iter()
            .map(|directory| directory.join(configured))
            .collect()
    };

    for base in bases {
        if base.extension().is_some() {
            if is_file(&base) {
                return Ok(base);
            }
            continue;
        }
        for extension in extensions {
            let mut candidate = base.as_os_str().to_os_string();
            candidate.push(extension);
            let candidate = PathBuf::from(candidate);
            if is_file(&candidate) {
                return Ok(candidate);
            }
        }
        if has_directory && is_file(&base) {
            return Ok(base);
        }
    }
    bail!("no supported Windows executable candidate exists")
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
        match mode {
            LauncherMode::Intercept => Self::new(kind, upstream_base_url.to_string()),
            LauncherMode::Proxy | LauncherMode::Integrated => Self::local(kind, mode),
        }
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
        Self::for_mode(session.kind, session.mode, session.upstream_base_url)
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
