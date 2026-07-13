use std::env;
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::Child;

use super::Cli;
use crate::claude_launcher::ClaudeLaunch;
use crate::codex_launcher::CodexLaunch;
use crate::copilot_launcher::CopilotLaunch;

pub(super) fn no_proxy_value(name: &str) -> String {
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct LauncherSession {
    version: u8,
    kind: LauncherKind,
    upstream_base_url: String,
}

impl LauncherSession {
    const VERSION: u8 = 1;

    pub(super) fn new(kind: LauncherKind, upstream_base_url: String) -> Result<Self> {
        let upstream_base_url = upstream_base_url.trim().trim_end_matches('/').to_string();
        if upstream_base_url.is_empty() {
            bail!("launcher upstream base URL cannot be empty");
        }
        Ok(Self {
            version: Self::VERSION,
            kind,
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
        Self::new(session.kind, session.upstream_base_url)
    }

    pub(super) fn kind(&self) -> LauncherKind {
        self.kind
    }

    pub(super) fn upstream_base_url(&self) -> &str {
        &self.upstream_base_url
    }
}

impl AgentLaunch {
    pub(super) async fn discover(cli: &Cli) -> Result<Option<Self>> {
        if !cli.claude && !cli.codex && !cli.copilot {
            return Ok(None);
        }
        if cli.claude {
            return ClaudeLaunch::discover(cli)
                .await
                .map(Self::Claude)
                .map(Some);
        }
        if cli.codex {
            return CodexLaunch::discover(cli).await.map(Self::Codex).map(Some);
        }
        if cli.copilot {
            return CopilotLaunch::discover(cli)
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
