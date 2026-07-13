use std::env;
use std::ffi::OsString;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio::process::{Child, Command};
use tracing::info;

use super::Cli;
use crate::agent_launcher::{LauncherKind, LauncherSession, no_proxy_value};

const ANTHROPIC_API_BASE_URL: &str = "https://api.anthropic.com";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeAuthStatus {
    logged_in: bool,
    auth_method: String,
    api_provider: String,
}

#[derive(Debug, Clone)]
pub(super) struct ClaudeLaunch {
    claude_bin: PathBuf,
    claude_args: Vec<OsString>,
    upstream_base_url: String,
}

impl ClaudeLaunch {
    pub(super) async fn discover(cli: &Cli) -> Result<Self> {
        reject_third_party_provider_env()?;
        let claude_bin = cli
            .claude_bin
            .clone()
            .or_else(|| env::var_os("LLMPROXY_CLAUDE_BIN").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("claude"));
        detect_auth_method(&claude_bin).await?;
        let upstream_base_url = cli
            .upstream
            .clone()
            .or_else(|| non_empty_env_value("ANTHROPIC_BASE_URL"))
            .unwrap_or_else(|| ANTHROPIC_API_BASE_URL.to_string());
        let upstream_base_url = normalize_anthropic_base_url(&upstream_base_url)?;
        let current_session_exists = claude_session_exists_for_current_dir(&claude_bin);

        Ok(Self {
            claude_bin,
            claude_args: claude_launch_args(&cli.agent_args, current_session_exists),
            upstream_base_url,
        })
    }

    pub(super) fn spawn(&self, proxy_addr: SocketAddr) -> Result<Child> {
        if !proxy_addr.ip().is_loopback() {
            bail!("--intercept --claude requires a loopback listen address, got {proxy_addr}");
        }
        let proxy_root_url = format!("http://{proxy_addr}");
        if normalize_anthropic_base_url(&proxy_root_url)? == self.upstream_base_url {
            bail!("Claude upstream resolves back to this proxy");
        }
        let session = self.launcher_session()?;
        let token = session.encode()?;
        let proxy_base_url = format!("http://{proxy_addr}/_launcher/{token}");

        let mut command = Command::new(&self.claude_bin);
        command
            .args(&self.claude_args)
            .env("ANTHROPIC_BASE_URL", &proxy_base_url)
            .env("LLMPROXY_CLAUDE_ACTIVE", "1")
            .env("LLMPROXY_CLAUDE_PROXY_URL", &proxy_base_url)
            .env("NO_PROXY", no_proxy_value("NO_PROXY"))
            .env("no_proxy", no_proxy_value("no_proxy"))
            .kill_on_drop(true);

        info!(
            executable = %self.claude_bin.display(),
            proxy = %proxy_base_url,
            "launching Claude through local interceptor"
        );
        command.spawn().with_context(|| {
            format!(
                "failed to launch Claude executable {}",
                self.claude_bin.display()
            )
        })
    }

    fn launcher_session(&self) -> Result<LauncherSession> {
        LauncherSession::new(LauncherKind::Claude, self.upstream_base_url.clone())
    }
}

async fn detect_auth_method(claude_bin: &Path) -> Result<String> {
    let output = Command::new(claude_bin)
        .args(["auth", "status", "--json"])
        .output()
        .await
        .with_context(|| {
            format!(
                "failed to inspect Claude login using {}",
                claude_bin.display()
            )
        })?;
    if !output.status.success() {
        bail!("Claude login is unavailable; run `claude auth login`");
    }
    let status: ClaudeAuthStatus = serde_json::from_slice(&output.stdout)
        .context("Claude auth status returned invalid JSON")?;
    validate_auth_status(&status)?;
    Ok(status.auth_method)
}

fn validate_auth_status(status: &ClaudeAuthStatus) -> Result<()> {
    if !status.logged_in {
        bail!("Claude login is unavailable; run `claude auth login`");
    }
    if status.api_provider != "firstParty" {
        bail!(
            "Claude provider {:?} is not supported by --intercept --claude; use first-party Anthropic authentication",
            status.api_provider
        );
    }
    if status.auth_method == "none" {
        bail!("Claude auth status did not report an authentication method");
    }
    Ok(())
}

fn reject_third_party_provider_env() -> Result<()> {
    for name in [
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
    ] {
        if env_truthy(name) {
            bail!("{name} is not supported by --intercept --claude");
        }
    }
    Ok(())
}

fn normalize_anthropic_base_url(base_url: &str) -> Result<String> {
    let base_url = base_url.trim().trim_end_matches('/');
    if base_url.is_empty() {
        bail!("Claude upstream base URL cannot be empty");
    }
    if base_url.ends_with("/v1") {
        Ok(base_url.to_string())
    } else {
        Ok(format!("{base_url}/v1"))
    }
}

fn non_empty_env_value(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn env_truthy(name: &str) -> bool {
    non_empty_env_value(name).is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn claude_launch_args(args: &[OsString], current_session_exists: bool) -> Vec<OsString> {
    if args.iter().any(|arg| {
        matches!(
            arg.to_str(),
            Some(
                "agents"
                    | "auth"
                    | "auto-mode"
                    | "doctor"
                    | "gateway"
                    | "install"
                    | "mcp"
                    | "plugin"
                    | "plugins"
                    | "project"
                    | "setup-token"
                    | "ultrareview"
                    | "update"
                    | "upgrade"
                    | "-c"
                    | "--continue"
                    | "-r"
                    | "--resume"
                    | "--from-pr"
                    | "--session-id"
                    | "-h"
                    | "--help"
                    | "-v"
                    | "--version"
            )
        ) || arg.to_str().is_some_and(|arg| {
            arg.starts_with("--resume=")
                || arg.starts_with("--from-pr=")
                || arg.starts_with("--session-id=")
        })
    }) {
        return args.to_vec();
    }
    if !current_session_exists {
        return args.to_vec();
    }

    std::iter::once(OsString::from("--continue"))
        .chain(args.iter().cloned())
        .collect()
}

fn claude_session_exists_for_current_dir(claude_bin: &Path) -> bool {
    let Some(config_home) = claude_config_home(claude_bin) else {
        return false;
    };
    let Ok(cwd) = env::current_dir() else {
        return false;
    };
    let cwd = fs::canonicalize(&cwd).unwrap_or(cwd);
    claude_session_exists(&config_home, &cwd)
}

fn claude_config_home(claude_bin: &Path) -> Option<PathBuf> {
    let is_openclaude = claude_bin
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.to_ascii_lowercase().contains("openclaude"));
    let override_name = if is_openclaude {
        "OPENCLAUDE_CONFIG_DIR"
    } else {
        "CLAUDE_CONFIG_DIR"
    };
    if let Some(path) = env::var_os(override_name).filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(path));
    }
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .map(|home| {
            home.join(if is_openclaude {
                ".openclaude"
            } else {
                ".claude"
            })
        })
}

fn claude_session_exists(config_home: &Path, cwd: &Path) -> bool {
    let project_name = cwd
        .to_string_lossy()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    let Ok(entries) = fs::read_dir(config_home.join("projects").join(project_name)) else {
        return false;
    };
    entries.filter_map(|entry| entry.ok()).any(|entry| {
        entry
            .path()
            .extension()
            .and_then(|extension| extension.to_str())
            == Some("jsonl")
            && entry.file_type().is_ok_and(|file_type| file_type.is_file())
    })
}

#[cfg(test)]
#[path = "claude_launcher_tests.rs"]
mod tests;
