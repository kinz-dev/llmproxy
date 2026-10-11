//! Borrowed subscription OAuth for the Codex and Claude backends.
//!
//! llmproxy never runs its own sign-in. Instead it reads the token the
//! official CLI already wrote to disk (`~/.codex/auth.json`,
//! `~/.claude/.credentials.json`), refreshes it against the vendor token
//! endpoint when it is near expiry, and writes the rotated token back to the
//! same file so the CLI and llmproxy stay in sync. Access tokens are cached in
//! memory between requests; only a near-expiry token touches the network.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::Client;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::agent_launcher::home_dir;

/// OpenAI's public Codex CLI OAuth client id (same value the codex CLI and
/// 9router/openclaude use). Only used as the `client_id` on refresh.
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// Anthropic's public Claude Code OAuth client id.
const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const CLAUDE_TOKEN_URL: &str = "https://api.anthropic.com/v1/oauth/token";

/// Refresh once the access token is within this window of expiring.
const REFRESH_SKEW_SECS: u64 = 300;

/// Which borrowed CLI login a provider authenticates with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OAuthProvider {
    Codex,
    Claude,
}

/// A resolved access token plus the identity headers a request needs.
#[derive(Debug, Clone)]
pub(crate) struct OAuthToken {
    pub(crate) access_token: String,
    /// ChatGPT account id for the `ChatGPT-Account-ID` header (Codex only).
    pub(crate) account_id: Option<String>,
}

/// Return a live access token for `provider`, refreshing and persisting it
/// first if it is missing or about to expire.
pub(crate) async fn access_token(provider: OAuthProvider) -> Result<OAuthToken> {
    match provider {
        OAuthProvider::Codex => codex_store().token().await,
        OAuthProvider::Claude => claude_store().token().await,
    }
}

fn http_client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder()
            .no_proxy()
            .build()
            .expect("failed to build oauth http client")
    })
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

/// The `exp` claim (unix seconds) of a JWT access token, if decodable.
fn jwt_exp_secs(token: &str) -> Option<u64> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    claims.get("exp").and_then(Value::as_u64)
}

/// Extract the ChatGPT account id from an OpenAI id/access JWT.
fn chatgpt_account_id(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    claims
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Codex: ~/.codex/auth.json
// {
//   "OPENAI_API_KEY": null,
//   "tokens": { "id_token", "access_token", "refresh_token", "account_id" },
//   "last_refresh": "<rfc3339>"
// }

struct CodexStore {
    lock: Mutex<Option<CachedToken>>,
}

struct CachedToken {
    access_token: String,
    account_id: Option<String>,
    expires_at: u64,
}

fn codex_store() -> &'static CodexStore {
    static STORE: OnceLock<CodexStore> = OnceLock::new();
    STORE.get_or_init(|| CodexStore {
        lock: Mutex::new(None),
    })
}

pub(crate) fn codex_auth_path() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CODEX_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(dir).join("auth.json"));
    }
    home_dir()
        .map(|home| home.join(".codex").join("auth.json"))
        .context("cannot locate ~/.codex/auth.json (no home directory)")
}

impl CodexStore {
    async fn token(&self) -> Result<OAuthToken> {
        let mut cached = self.lock.lock().await;
        if let Some(entry) = cached.as_ref()
            && entry.expires_at > now_secs() + REFRESH_SKEW_SECS
        {
            return Ok(OAuthToken {
                access_token: entry.access_token.clone(),
                account_id: entry.account_id.clone(),
            });
        }

        let path = codex_auth_path()?;
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {} — run `codex login` first", path.display()))?;
        let mut doc: Value = serde_json::from_str(&raw)
            .with_context(|| format!("invalid JSON in {}", path.display()))?;
        let tokens = doc
            .get("tokens")
            .cloned()
            .context("~/.codex/auth.json has no `tokens` (run `codex login`)")?;
        let access = tokens
            .get("access_token")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let refresh = tokens
            .get("refresh_token")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let account_id = tokens
            .get("account_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                tokens
                    .get("id_token")
                    .and_then(Value::as_str)
                    .and_then(chatgpt_account_id)
            })
            .or_else(|| chatgpt_account_id(&access));

        let needs_refresh = access.is_empty()
            || jwt_exp_secs(&access).is_none_or(|exp| exp <= now_secs() + REFRESH_SKEW_SECS);

        let (access, account_id) = if needs_refresh {
            if refresh.is_empty() {
                bail!("~/.codex/auth.json has no refresh_token — run `codex login` again");
            }
            let refreshed = refresh_codex(&refresh).await?;
            let new_access = refreshed.access_token;
            let new_account = account_id
                .or_else(|| refreshed.id_token.as_deref().and_then(chatgpt_account_id))
                .or_else(|| chatgpt_account_id(&new_access));
            // Persist the rotated token so the codex CLI keeps working.
            if let Some(tokens) = doc.get_mut("tokens").and_then(Value::as_object_mut) {
                tokens.insert("access_token".into(), json!(new_access));
                tokens.insert("refresh_token".into(), json!(refreshed.refresh_token));
                if let Some(id_token) = &refreshed.id_token {
                    tokens.insert("id_token".into(), json!(id_token));
                }
            }
            if let Some(root) = doc.as_object_mut() {
                root.insert("last_refresh".into(), json!(rfc3339_now()));
            }
            if let Err(error) = write_json_atomic(&path, &doc) {
                warn!(error = %error, "could not persist refreshed Codex token; CLI may re-login");
            }
            info!("refreshed borrowed Codex access token");
            (new_access, new_account)
        } else {
            (access, account_id)
        };

        let expires_at = jwt_exp_secs(&access).unwrap_or_else(|| now_secs() + 3600);
        *cached = Some(CachedToken {
            access_token: access.clone(),
            account_id: account_id.clone(),
            expires_at,
        });
        Ok(OAuthToken {
            access_token: access,
            account_id,
        })
    }
}

struct RefreshedCodex {
    access_token: String,
    refresh_token: String,
    id_token: Option<String>,
}

async fn refresh_codex(refresh_token: &str) -> Result<RefreshedCodex> {
    let response = http_client()
        .post(CODEX_TOKEN_URL)
        .json(&json!({
            "client_id": CODEX_CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        }))
        .send()
        .await
        .context("Codex token refresh request failed")?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("Codex token refresh failed ({status}): {}", body.trim());
    }
    let body: Value = response
        .json()
        .await
        .context("Codex token refresh returned invalid JSON")?;
    let access_token = body
        .get("access_token")
        .and_then(Value::as_str)
        .context("Codex token refresh response had no access_token")?
        .to_string();
    let refresh_token = body
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or(refresh_token)
        .to_string();
    let id_token = body
        .get("id_token")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(RefreshedCodex {
        access_token,
        refresh_token,
        id_token,
    })
}

// ---------------------------------------------------------------------------
// Claude: ~/.claude/.credentials.json
// { "claudeAiOauth": { "accessToken", "refreshToken", "expiresAt"(ms), "scopes" } }

struct ClaudeStore {
    lock: Mutex<Option<CachedToken>>,
}

fn claude_store() -> &'static ClaudeStore {
    static STORE: OnceLock<ClaudeStore> = OnceLock::new();
    STORE.get_or_init(|| ClaudeStore {
        lock: Mutex::new(None),
    })
}

pub(crate) fn claude_credentials_path() -> Result<PathBuf> {
    if let Some(path) =
        std::env::var_os("CLAUDE_CREDENTIALS_PATH").filter(|value| !value.is_empty())
    {
        return Ok(PathBuf::from(path));
    }
    home_dir()
        .map(|home| home.join(".claude").join(".credentials.json"))
        .context("cannot locate ~/.claude/.credentials.json (no home directory)")
}

impl ClaudeStore {
    async fn token(&self) -> Result<OAuthToken> {
        let mut cached = self.lock.lock().await;
        if let Some(entry) = cached.as_ref()
            && entry.expires_at > now_secs() + REFRESH_SKEW_SECS
        {
            return Ok(OAuthToken {
                access_token: entry.access_token.clone(),
                account_id: None,
            });
        }

        let path = claude_credentials_path()?;
        let raw = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "cannot read {} — run `claude` and sign in first",
                path.display()
            )
        })?;
        let mut doc: Value = serde_json::from_str(&raw)
            .with_context(|| format!("invalid JSON in {}", path.display()))?;
        let oauth = doc
            .get("claudeAiOauth")
            .cloned()
            .context("~/.claude/.credentials.json has no `claudeAiOauth` block")?;
        let access = oauth
            .get("accessToken")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let refresh = oauth
            .get("refreshToken")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // expiresAt is unix milliseconds.
        let expires_at_ms = oauth.get("expiresAt").and_then(Value::as_u64).unwrap_or(0);
        let expires_at = expires_at_ms / 1000;

        let needs_refresh = access.is_empty() || expires_at <= now_secs() + REFRESH_SKEW_SECS;

        let (access, expires_at) = if needs_refresh {
            if refresh.is_empty() {
                bail!(
                    "~/.claude/.credentials.json has no refreshToken — sign in with `claude` again"
                );
            }
            let refreshed = refresh_claude(&refresh).await?;
            let new_expiry = now_secs() + refreshed.expires_in.max(60);
            if let Some(block) = doc.get_mut("claudeAiOauth").and_then(Value::as_object_mut) {
                block.insert("accessToken".into(), json!(refreshed.access_token));
                block.insert("refreshToken".into(), json!(refreshed.refresh_token));
                block.insert("expiresAt".into(), json!(new_expiry * 1000));
            }
            if let Err(error) = write_json_atomic(&path, &doc) {
                warn!(error = %error, "could not persist refreshed Claude token; CLI may re-login");
            }
            info!("refreshed borrowed Claude access token");
            (refreshed.access_token, new_expiry)
        } else {
            (access, expires_at)
        };

        *cached = Some(CachedToken {
            access_token: access.clone(),
            account_id: None,
            expires_at,
        });
        Ok(OAuthToken {
            access_token: access,
            account_id: None,
        })
    }
}

struct RefreshedClaude {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}

async fn refresh_claude(refresh_token: &str) -> Result<RefreshedClaude> {
    let response = http_client()
        .post(CLAUDE_TOKEN_URL)
        .json(&json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLAUDE_CLIENT_ID,
        }))
        .send()
        .await
        .context("Claude token refresh request failed")?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("Claude token refresh failed ({status}): {}", body.trim());
    }
    let body: Value = response
        .json()
        .await
        .context("Claude token refresh returned invalid JSON")?;
    let access_token = body
        .get("access_token")
        .and_then(Value::as_str)
        .context("Claude token refresh response had no access_token")?
        .to_string();
    let refresh_token = body
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or(refresh_token)
        .to_string();
    let expires_in = body
        .get("expires_in")
        .and_then(Value::as_u64)
        .unwrap_or(3600);
    Ok(RefreshedClaude {
        access_token,
        refresh_token,
        expires_in,
    })
}

// ---------------------------------------------------------------------------

fn rfc3339_now() -> String {
    // Minimal RFC3339 UTC stamp; avoids pulling in a date crate.
    let secs = now_secs();
    let days = secs / 86_400;
    let sod = secs % 86_400;
    let (hour, min, sec) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

/// Howard Hinnant's days-from-civil, inverted. `days` is days since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if month <= 2 { y + 1 } else { y }, month, day)
}

/// Write `value` to `path` via a sibling temp file + rename so a crash never
/// leaves a half-written credentials file.
fn write_json_atomic(path: &Path, value: &Value) -> Result<()> {
    let serialized = serde_json::to_vec_pretty(value)?;
    let tmp = path.with_extension("json.llmproxy.tmp");
    std::fs::write(&tmp, &serialized)
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    replace_file(&tmp, path).with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::{iter, os::windows::ffi::OsStrExt};
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect::<Vec<_>>();
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[path = "oauth_tests.rs"]
mod tests;
