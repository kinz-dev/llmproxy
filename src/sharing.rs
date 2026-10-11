//! Sharing: the local limit, peer offload, and cloud overflow. A job received
//! from a peer is never forwarded again: the inbound path has no peer or cloud
//! tier at all, whatever its headers claim.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header::CONTENT_TYPE},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

use crate::interceptor::{InterceptorProtocol as Wire, RoutingAuditDraft};
use crate::stats::{self, ResponseTracking, route_class_for_base_url, track_response};
use crate::{
    AppState, Config, ProviderState, ResponseResult, SharedState, UpstreamFailure,
    UpstreamRequestMeta, UpstreamResponseKind, request_is_streaming,
};

pub(crate) const PEER_HEADER: &str = "llmproxy-peer";
pub(crate) const TIMESTAMP_HEADER: &str = "llmproxy-timestamp";
pub(crate) const NONCE_HEADER: &str = "llmproxy-nonce";
pub(crate) const DIGEST_HEADER: &str = "content-digest";
pub(crate) const SIGNATURE_HEADER: &str = "llmproxy-signature";
pub(crate) const HOP_HEADER: &str = "llmproxy-hop";
pub(crate) const REJECT_HEADER: &str = "llmproxy-reject";
pub(crate) const SLOTS_HEADER: &str = "llmproxy-slots";
const SIGNING_DOMAIN: &str = "llmproxy-peer-v1";
const PROTOCOL_VERSION: u32 = 1;
const KEY_PREFIX: &str = "ed25519:";
const NONCE_CACHE_CAP: usize = 65_536;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SharingMode {
    #[default]
    Local,
    Peers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ModeSwitches {
    pub(crate) proxy: bool,
    pub(crate) intercept: bool,
    pub(crate) integrated: bool,
}

impl Default for ModeSwitches {
    fn default() -> Self {
        Self {
            proxy: true,
            intercept: true,
            integrated: true,
        }
    }
}

impl ModeSwitches {
    fn get(&self, mode: crate::RoutingMode) -> bool {
        match mode {
            crate::RoutingMode::Proxy => self.proxy,
            crate::RoutingMode::Intercept => self.intercept,
            crate::RoutingMode::Integrated => self.integrated,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct SharingConfig {
    pub(crate) mode: SharingMode,
    pub(crate) node_name: String,
    pub(crate) peer_listen: Option<String>,
    /// Shown as this node's address when `peer_listen` is not what peers reach.
    pub(crate) advertise_url: Option<String>,
    pub(crate) private_key_file: Option<PathBuf>,
    /// Runs inbound peer jobs and is the model the local limit governs.
    pub(crate) onprem_provider: String,
    /// 0 is no limit: this node is then never full.
    pub(crate) local_limit: u32,
    pub(crate) send_outgoing: bool,
    pub(crate) accept_incoming: bool,
    pub(crate) incoming_limit: u32,
    pub(crate) queue_depth: u32,
    pub(crate) queue_wait_ms: u64,
    pub(crate) retry_after_secs: u64,
    pub(crate) status_interval_secs: u64,
    pub(crate) status_stale_after_secs: u64,
    pub(crate) clock_skew_secs: u64,
    pub(crate) peer_timeout_secs: u64,
    /// Cloud `provider/model` for local overflow, in the modes switched on.
    pub(crate) cloud_overflow: Option<String>,
    pub(crate) cloud_overflow_modes: ModeSwitches,
}

impl SharingConfig {
    pub(crate) fn with_peers(&self) -> bool {
        self.mode == SharingMode::Peers
    }

    pub(crate) fn limited(&self) -> bool {
        self.local_limit > 0
    }

    pub(crate) fn cloud_overflow(&self, mode: crate::RoutingMode) -> Option<&str> {
        self.cloud_overflow
            .as_deref()
            .filter(|_| self.cloud_overflow_modes.get(mode))
    }

    pub(crate) fn schedules(&self) -> bool {
        self.limited() || (self.with_peers() && self.send_outgoing) || self.cloud_overflow.is_some()
    }
}

impl Default for SharingConfig {
    fn default() -> Self {
        Self {
            mode: SharingMode::Local,
            node_name: default_node_name(),
            peer_listen: None,
            advertise_url: None,
            private_key_file: None,
            onprem_provider: String::new(),
            local_limit: 0,
            send_outgoing: true,
            accept_incoming: true,
            incoming_limit: 2,
            queue_depth: 8,
            queue_wait_ms: 1_000,
            retry_after_secs: 2,
            status_interval_secs: 5,
            status_stale_after_secs: 15,
            clock_skew_secs: 60,
            peer_timeout_secs: 600,
            cloud_overflow: None,
            cloud_overflow_modes: ModeSwitches::default(),
        }
    }
}

/// Rewrites the pre-rename `[cluster]` table and peer keys into `[sharing]`.
pub(crate) fn migrate_legacy(document: &mut toml::Table) {
    if let Some(toml::Value::Table(peers)) = document.get_mut("peers") {
        for (_, peer) in peers.iter_mut() {
            let Some(peer) = peer.as_table_mut() else {
                continue;
            };
            peer.remove("node_name");
            for (old, new) in [
                ("accept_inbound", "accept_incoming"),
                ("send_outbound", "send_outgoing"),
            ] {
                if let Some(value) = peer.remove(old) {
                    peer.entry(new).or_insert(value);
                }
            }
        }
    }
    let Some(toml::Value::Table(mut legacy)) = document.remove("cluster") else {
        return;
    };
    let tiers = legacy
        .remove("tiers")
        .and_then(|value| value.as_table().cloned())
        .unwrap_or_default();
    let tier = |key: &str, default: bool| {
        tiers
            .get(key)
            .and_then(toml::Value::as_bool)
            .unwrap_or(default)
    };
    let mut sharing = toml::Table::new();
    if legacy.remove("enable").and_then(|value| value.as_bool()) == Some(true) {
        sharing.insert("mode".into(), "peers".into());
    }
    let local_slots = legacy.remove("local_slots");
    if tier("local", false) {
        sharing.insert(
            "local_limit".into(),
            local_slots.unwrap_or(toml::Value::Integer(6)),
        );
    }
    sharing.insert("send_outgoing".into(), tier("peer", true).into());
    if let Some(route) = legacy.remove("commercial_route")
        && tier("commercial", false)
    {
        sharing.insert("cloud_overflow".into(), route);
    }
    for (old, new) in [
        ("listen_address", "peer_listen"),
        ("local_provider", "onprem_provider"),
        ("remote_slots", "incoming_limit"),
        ("accept_inbound", "accept_incoming"),
        ("commercial_modes", "cloud_overflow_modes"),
    ] {
        if let Some(value) = legacy.remove(old) {
            sharing.insert(new.into(), value);
        }
    }
    sharing.extend(legacy);
    document.insert("sharing".into(), toml::Value::Table(sharing));
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct PeerConfig {
    pub(crate) url: String,
    pub(crate) public_key: String,
    pub(crate) enabled: bool,
    pub(crate) accept_incoming: bool,
    pub(crate) send_outgoing: bool,
}

impl Default for PeerConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            public_key: String::new(),
            enabled: true,
            accept_incoming: true,
            send_outgoing: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ListenGuardConfig {
    /// An explicit empty list admits everyone. Loopback is always admitted.
    pub(crate) allowed_sources: Vec<String>,
}

impl Default for ListenGuardConfig {
    fn default() -> Self {
        Self {
            allowed_sources: vec!["192.168.0.0/16".into(), "127.0.0.1".into()],
        }
    }
}

pub(crate) const SHARING_KEYS: &[&str] = &[
    "mode",
    "node_name",
    "peer_listen",
    "advertise_url",
    "private_key_file",
    "onprem_provider",
    "local_limit",
    "send_outgoing",
    "accept_incoming",
    "incoming_limit",
    "queue_depth",
    "queue_wait_ms",
    "retry_after_secs",
    "status_interval_secs",
    "status_stale_after_secs",
    "clock_skew_secs",
    "peer_timeout_secs",
    "cloud_overflow",
    "cloud_overflow_modes",
];
pub(crate) const PEER_KEYS: &[&str] = &[
    "url",
    "public_key",
    "enabled",
    "accept_incoming",
    "send_outgoing",
];
pub(crate) const LISTEN_GUARD_KEYS: &[&str] = &["allowed_sources"];

pub(crate) fn parse_networks(sources: &[String]) -> Result<Vec<ipnetwork::IpNetwork>> {
    sources
        .iter()
        .map(|source| {
            let source = source.trim();
            source
                .parse::<ipnetwork::IpNetwork>()
                .or_else(|_| source.parse::<IpAddr>().map(ipnetwork::IpNetwork::from))
                .with_context(|| format!("invalid allowed source {source:?}"))
        })
        .collect()
}

pub(crate) const DEFAULT_PEER_LISTEN: &str = "0.0.0.0:4040";

/// The URL peers should use for this node: `advertise_url`, else the
/// listener address, with a wildcard host replaced by the address this
/// machine routes from.
pub(crate) fn advertised_url(sharing: &SharingConfig) -> String {
    if let Some(url) = sharing.advertise_url.as_deref() {
        return url.trim_end_matches('/').to_string();
    }
    let address = sharing
        .peer_listen
        .as_deref()
        .unwrap_or(DEFAULT_PEER_LISTEN);
    match address.parse::<SocketAddr>() {
        Ok(addr) if addr.ip().is_unspecified() => {
            let ip = outbound_ip().unwrap_or(addr.ip());
            format!("http://{}", SocketAddr::new(ip, addr.port()))
        }
        _ => format!("http://{address}"),
    }
}

/// Picks the interface the default route uses. `connect` on UDP sends nothing.
fn outbound_ip() -> Option<IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    Some(socket.local_addr().ok()?.ip()).filter(|ip| !ip.is_unspecified())
}

pub(crate) fn default_node_name() -> String {
    let raw = std::fs::read_to_string("/etc/hostname")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_default();
    let name: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    if name.is_empty() { "node".into() } else { name }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['/', ' ', ':'])
}

// The signature covers the request path, so a path prefix (which a receiver
// would never see as signed) is refused up front.
fn valid_origin(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && matches!(url.path(), "" | "/")
            && url.query().is_none()
    })
}

pub(crate) fn validate(config: &Config) -> Result<()> {
    parse_networks(&config.listen_guard.allowed_sources)?;
    let sharing = &config.sharing;
    for (name, peer) in &config.peers {
        if !valid_name(name) {
            bail!("peer name {name:?} must be non-empty without '/', ':' or spaces");
        }
        if config
            .peers
            .iter()
            .any(|(other, other_peer)| other != name && other_peer.public_key == peer.public_key)
        {
            bail!("peers.{name} uses the same public key as another peer");
        }
        parse_public_key(&peer.public_key).with_context(|| format!("peers.{name}.public_key"))?;
        if !valid_origin(&peer.url) {
            bail!("peers.{name}.url must be an http(s) origin without a path");
        }
    }
    if sharing
        .advertise_url
        .as_deref()
        .is_some_and(|url| !valid_origin(url))
    {
        bail!("sharing.advertise_url must be an http(s) origin without a path");
    }
    if !sharing.with_peers() && !sharing.schedules() {
        return Ok(());
    }
    let local = config
        .provider_config(&sharing.onprem_provider)
        .with_context(|| {
            format!(
                "sharing.onprem_provider {:?} is not a configured provider",
                sharing.onprem_provider
            )
        })?;
    if route_class_for_base_url(&local.base_url) != stats::RouteClass::Local
        && !is_vpn_overlay(&local.base_url)
    {
        bail!(
            "sharing.onprem_provider {:?} must resolve to loopback or a private network",
            sharing.onprem_provider
        );
    }
    if let Some(route) = sharing.cloud_overflow.as_deref() {
        let (provider, _) = route
            .split_once('/')
            .filter(|(provider, model)| !provider.is_empty() && !model.is_empty())
            .context("sharing.cloud_overflow must be provider/model")?;
        if provider == sharing.onprem_provider {
            bail!("sharing.cloud_overflow must not target the on-prem provider");
        }
        // Overflow traffic carries no caller credentials to a non-default
        // provider, so a cloud route without its own key could only fail.
        let cloud = config.provider_config(provider).with_context(|| {
            format!("sharing.cloud_overflow provider {provider:?} is not configured")
        })?;
        if route_class_for_base_url(&cloud.base_url) == stats::RouteClass::External
            && cloud.api_key.is_none()
            && cloud.auth == crate::ProviderAuth::None
        {
            bail!(
                "sharing.cloud_overflow provider {provider:?} needs a proxy-owned credential (api_key, api_key_env, or auth)"
            );
        }
    }
    if let Some(listen) = sharing.peer_listen.as_deref() {
        let peer_addr: SocketAddr = listen
            .parse()
            .with_context(|| format!("invalid sharing.peer_listen {listen:?}"))?;
        if config.listen_addr.parse::<SocketAddr>().ok() == Some(peer_addr) {
            bail!("sharing.peer_listen must differ from listen_addr");
        }
        if config
            .admin
            .listen_address
            .as_deref()
            .and_then(|a| a.parse().ok())
            == Some(peer_addr)
        {
            bail!("sharing.peer_listen must differ from the admin listener");
        }
    }
    if sharing.status_interval_secs == 0 || sharing.clock_skew_secs == 0 {
        bail!("sharing.status_interval_secs and sharing.clock_skew_secs must be positive");
    }
    Ok(())
}

/// Carrier-grade NAT space (100.64.0.0/10), which VPN overlays such as
/// Tailscale use for node addresses. Not RFC 1918, but not public either.
fn is_vpn_overlay(base_url: &str) -> bool {
    reqwest::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str()?.parse::<std::net::Ipv4Addr>().ok())
        .is_some_and(|ip| ip.octets()[0] == 100 && (ip.octets()[1] & 0xc0) == 64)
}

pub(crate) struct NodeKey {
    pair: Ed25519KeyPair,
    public: String,
}

impl NodeKey {
    pub(crate) fn public_key(&self) -> &str {
        &self.public
    }

    pub(crate) fn fingerprint(&self) -> String {
        fingerprint(self.pair.public_key().as_ref())
    }
}

pub(crate) fn default_key_path() -> Option<PathBuf> {
    crate::agent_launcher::home_dir().map(|home| home.join(".llmproxy").join("peer_ed25519"))
}

fn key_path(config: &SharingConfig) -> Result<PathBuf> {
    config
        .private_key_file
        .clone()
        .or_else(default_key_path)
        .context(
            "cannot locate the peer key without a home directory; set sharing.private_key_file",
        )
}

pub(crate) fn load_or_create_key(config: &SharingConfig) -> Result<NodeKey> {
    let path = key_path(config)?;
    let pkcs8 = match std::fs::read_to_string(&path) {
        Ok(text) => STANDARD
            .decode(text.trim())
            .with_context(|| format!("peer key {} is not base64", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
                .map_err(|_| anyhow::anyhow!("failed to generate the peer key"))?;
            write_owner_only(&path, STANDARD.encode(document.as_ref()).as_bytes())?;
            info!(path = %path.display(), "generated peer signing key");
            document.as_ref().to_vec()
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read peer key {}", path.display()));
        }
    };
    let pair = Ed25519KeyPair::from_pkcs8(&pkcs8)
        .map_err(|error| anyhow::anyhow!("peer key {} is invalid: {error}", path.display()))?;
    let public = format!(
        "{KEY_PREFIX}{}",
        STANDARD.encode(pair.public_key().as_ref())
    );
    Ok(NodeKey { pair, public })
}

fn write_owner_only(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("failed to create peer key {}", path.display()))?;
    file.write_all(bytes)?;
    Ok(())
}

pub(crate) fn parse_public_key(value: &str) -> Result<[u8; 32]> {
    let encoded = value
        .trim()
        .strip_prefix(KEY_PREFIX)
        .context("public key must start with ed25519:")?;
    let bytes = STANDARD
        .decode(encoded)
        .context("public key is not base64")?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("public key must be 32 bytes"))
}

pub(crate) fn fingerprint(public: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, public);
    format!("SHA256:{}", &URL_SAFE_NO_PAD.encode(digest.as_ref())[..16])
}

pub(crate) fn content_digest(body: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, body);
    format!("sha-256=:{}:", STANDARD.encode(digest.as_ref()))
}

fn signing_string(
    method: &Method,
    path: &str,
    target: &str,
    timestamp: &str,
    nonce: &str,
    digest: &str,
) -> String {
    format!("{SIGNING_DOMAIN}\n{method}\n{path}\n{target}\n{timestamp}\n{nonce}\n{digest}")
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

/// Headers that authenticate one peer call to the node holding `target`'s key.
pub(crate) fn sign_request(
    key: &NodeKey,
    target: &str,
    method: &Method,
    path: &str,
    body: &[u8],
) -> HeaderMap {
    let mut nonce = [0_u8; 16];
    let _ = SystemRandom::new().fill(&mut nonce);
    let nonce = URL_SAFE_NO_PAD.encode(nonce);
    let timestamp = unix_now().to_string();
    let digest = content_digest(body);
    let signature = key
        .pair
        .sign(signing_string(method, path, target, &timestamp, &nonce, &digest).as_bytes());
    let mut headers = HeaderMap::with_capacity(5);
    for (name, value) in [
        (PEER_HEADER, key.public_key().to_string()),
        (TIMESTAMP_HEADER, timestamp),
        (NONCE_HEADER, nonce),
        (DIGEST_HEADER, digest),
        (SIGNATURE_HEADER, STANDARD.encode(signature.as_ref())),
    ] {
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(HeaderName::from_static(name), value);
        }
    }
    headers
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthPurpose {
    Job,
    Status,
}

/// Bounded replay cache. Entries outlive the accepted timestamp window, so a
/// nonce cannot be reused while its signature would still verify.
#[derive(Default)]
struct NonceCache {
    seen: Mutex<HashMap<String, Instant>>,
}

impl NonceCache {
    fn insert(&self, nonce: &str, ttl: Duration) -> bool {
        let now = Instant::now();
        let mut seen = self.seen.lock().expect("nonce cache poisoned");
        if seen.len() >= NONCE_CACHE_CAP / 2 {
            seen.retain(|_, expires| *expires > now);
        }
        if seen.len() >= NONCE_CACHE_CAP {
            return false;
        }
        match seen.get(nonce) {
            Some(expires) if *expires > now => false,
            _ => {
                seen.insert(nonce.to_string(), now + ttl);
                true
            }
        }
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlotKind {
    Local,
    Incoming,
    /// An incoming slot lent to local work.
    Borrowed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct SlotSnapshot {
    pub(crate) local_total: u32,
    pub(crate) local_busy: u32,
    pub(crate) incoming_total: u32,
    pub(crate) incoming_busy: u32,
    pub(crate) borrowed: u32,
    pub(crate) queued: u32,
    pub(crate) queue_depth: u32,
}

impl SlotSnapshot {
    pub(crate) fn incoming_free(&self) -> u32 {
        self.incoming_total.saturating_sub(self.incoming_busy)
    }

    fn header_value(&self) -> String {
        format!(
            "{}/{}/{}/{}",
            self.local_busy, self.local_total, self.incoming_busy, self.incoming_total
        )
    }

    fn from_header(value: &str) -> Option<Self> {
        let mut parts = value.split('/').map(|part| part.trim().parse::<u32>());
        let snapshot = Self {
            local_busy: parts.next()?.ok()?,
            local_total: parts.next()?.ok()?,
            incoming_busy: parts.next()?.ok()?,
            incoming_total: parts.next()?.ok()?,
            ..Self::default()
        };
        parts.next().is_none().then_some(snapshot)
    }
}

#[derive(Default)]
struct SlotState {
    local_total: u32,
    incoming_total: u32,
    local_busy: u32,
    incoming_busy: u32,
    borrowed: u32,
    queue_depth: u32,
    waiters: VecDeque<(u64, oneshot::Sender<SlotPermit>)>,
    next_waiter: u64,
}

impl SlotState {
    fn claim_own_local(&mut self) -> Option<SlotKind> {
        (self.local_busy < self.local_total).then(|| {
            self.local_busy += 1;
            SlotKind::Local
        })
    }

    fn claim_local(&mut self) -> Option<SlotKind> {
        if self.local_busy < self.local_total {
            self.local_busy += 1;
            Some(SlotKind::Local)
        } else if self.incoming_busy < self.incoming_total {
            self.incoming_busy += 1;
            self.borrowed += 1;
            Some(SlotKind::Borrowed)
        } else {
            None
        }
    }

    fn unclaim(&mut self, kind: SlotKind) {
        match kind {
            SlotKind::Local => self.local_busy = self.local_busy.saturating_sub(1),
            SlotKind::Incoming => self.incoming_busy = self.incoming_busy.saturating_sub(1),
            SlotKind::Borrowed => {
                self.incoming_busy = self.incoming_busy.saturating_sub(1);
                self.borrowed = self.borrowed.saturating_sub(1);
            }
        }
    }
}

/// Execution permits. A lock guards O(1) bookkeeping only and is never held
/// across an await. Released slots are handed straight to the oldest waiting
/// local request, so a waiter cannot be overtaken by a newcomer.
#[derive(Default)]
pub(crate) struct SlotManager {
    state: Mutex<SlotState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitError {
    Full,
    Timeout,
}

/// Held for the whole request, including a streamed body. Dropping it releases
/// the slot.
pub(crate) struct SlotPermit {
    manager: Arc<SlotManager>,
    kind: SlotKind,
    armed: bool,
}

impl std::fmt::Debug for SlotPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlotPermit")
            .field("kind", &self.kind)
            .finish()
    }
}

impl SlotPermit {
    pub(crate) fn kind(&self) -> SlotKind {
        self.kind
    }
}

impl Drop for SlotPermit {
    fn drop(&mut self) {
        if self.armed {
            self.manager.release(self.kind);
        }
    }
}

impl SlotManager {
    pub(crate) fn new(local: u32, incoming: u32, queue_depth: u32) -> Arc<Self> {
        let manager = Arc::new(Self::default());
        manager.resize(local, incoming, queue_depth);
        manager
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SlotState> {
        self.state.lock().expect("slot state poisoned")
    }

    fn permit(self: &Arc<Self>, kind: SlotKind) -> SlotPermit {
        SlotPermit {
            manager: self.clone(),
            kind,
            armed: true,
        }
    }

    pub(crate) fn snapshot(&self) -> SlotSnapshot {
        let state = self.lock();
        SlotSnapshot {
            local_total: state.local_total,
            local_busy: state.local_busy,
            incoming_total: state.incoming_total,
            incoming_busy: state.incoming_busy,
            borrowed: state.borrowed,
            queued: state.waiters.len() as u32,
            queue_depth: state.queue_depth,
        }
    }

    /// Slots already held above a reduced total drain naturally; nothing is
    /// revoked.
    pub(crate) fn resize(self: &Arc<Self>, local: u32, incoming: u32, queue_depth: u32) {
        let mut state = self.lock();
        state.local_total = local;
        state.incoming_total = incoming;
        state.queue_depth = queue_depth;
        while !state.waiters.is_empty() {
            let Some(kind) = state.claim_local() else {
                break;
            };
            let (_, sender) = state.waiters.pop_front().expect("waiter present");
            if let Err(mut permit) = sender.send(self.permit(kind)) {
                permit.armed = false;
                state.unclaim(kind);
            }
        }
    }

    /// A local slot only, never a lent incoming slot.
    pub(crate) fn try_own_local(self: &Arc<Self>) -> Option<SlotPermit> {
        let kind = self.lock().claim_own_local()?;
        Some(self.permit(kind))
    }

    /// A local slot, else an idle incoming slot lent to local work.
    pub(crate) fn try_local(self: &Arc<Self>) -> Option<SlotPermit> {
        let kind = self.lock().claim_local()?;
        Some(self.permit(kind))
    }

    pub(crate) fn try_incoming(self: &Arc<Self>) -> Option<SlotPermit> {
        let mut state = self.lock();
        if state.incoming_busy >= state.incoming_total {
            return None;
        }
        state.incoming_busy += 1;
        drop(state);
        Some(self.permit(SlotKind::Incoming))
    }

    pub(crate) async fn wait_local(
        self: &Arc<Self>,
        wait: Duration,
    ) -> std::result::Result<SlotPermit, WaitError> {
        let (receiver, id) = {
            let mut state = self.lock();
            if let Some(kind) = state.claim_local() {
                drop(state);
                return Ok(self.permit(kind));
            }
            if state.waiters.len() >= state.queue_depth as usize {
                return Err(WaitError::Full);
            }
            let id = state.next_waiter;
            state.next_waiter = state.next_waiter.wrapping_add(1);
            let (sender, receiver) = oneshot::channel();
            state.waiters.push_back((id, sender));
            (receiver, id)
        };
        // Removes the waiter if this future is dropped (client disconnect) or
        // times out, so a dead waiter never holds a queue position.
        struct Leave<'a>(&'a SlotManager, u64);
        impl Drop for Leave<'_> {
            fn drop(&mut self) {
                self.0.lock().waiters.retain(|(id, _)| *id != self.1);
            }
        }
        let _leave = Leave(self, id);
        match tokio::time::timeout(wait, receiver).await {
            Ok(Ok(permit)) => Ok(permit),
            _ => Err(WaitError::Timeout),
        }
    }

    fn release(self: &Arc<Self>, kind: SlotKind) {
        let mut state = self.lock();
        let over_capacity = match kind {
            SlotKind::Local => state.local_busy > state.local_total,
            SlotKind::Incoming | SlotKind::Borrowed => state.incoming_busy > state.incoming_total,
        };
        if !over_capacity {
            let handoff = match kind {
                SlotKind::Local => SlotKind::Local,
                SlotKind::Incoming | SlotKind::Borrowed => SlotKind::Borrowed,
            };
            while let Some((_, sender)) = state.waiters.pop_front() {
                match sender.send(self.permit(handoff)) {
                    Ok(()) => {
                        if kind == SlotKind::Incoming {
                            state.borrowed += 1;
                        }
                        return;
                    }
                    Err(mut permit) => permit.armed = false,
                }
            }
        }
        state.unclaim(kind);
    }
}

/// Keeps a permit alive until the response body has been fully sent or the
/// client goes away.
struct PermitBody {
    inner: Body,
    _permit: SlotPermit,
}

impl http_body::Body for PermitBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<std::result::Result<http_body::Frame<Bytes>, Self::Error>>> {
        Pin::new(&mut self.get_mut().inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

pub(crate) fn hold(response: Response, permit: Option<SlotPermit>) -> Response {
    let Some(permit) = permit else {
        return response;
    };
    let (parts, body) = response.into_parts();
    Response::from_parts(
        parts,
        Body::new(PermitBody {
            inner: body,
            _permit: permit,
        }),
    )
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct OnpremProviderStatus {
    pub(crate) healthy: bool,
    pub(crate) breaker: String,
    pub(crate) models: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct NodeStatus {
    pub(crate) node: String,
    pub(crate) version: String,
    pub(crate) protocol: u32,
    pub(crate) time: u64,
    pub(crate) enabled: bool,
    pub(crate) accept_incoming_for_you: bool,
    pub(crate) slots: SlotSnapshot,
    pub(crate) onprem_provider: OnpremProviderStatus,
}

#[derive(Default)]
struct PeerView {
    status: Option<NodeStatus>,
    fetched_at: Option<Instant>,
    last_error: Option<String>,
    /// Set when the peer refused a job for lack of slots; cleared by the next
    /// status poll, so a full peer is not hammered between polls.
    saturated: bool,
}

#[derive(Default)]
pub(crate) struct PeerRuntime {
    view: Mutex<PeerView>,
    sent: AtomicU64,
    inbound_accepted: AtomicU64,
}

impl PeerRuntime {
    fn note_slots(&self, snapshot: SlotSnapshot) {
        let mut view = self.view.lock().expect("peer view poisoned");
        if let Some(status) = view.status.as_mut() {
            let queued = status.slots.queued;
            let depth = status.slots.queue_depth;
            status.slots = SlotSnapshot {
                queued,
                queue_depth: depth,
                ..snapshot
            };
        }
    }
}

#[derive(Default)]
pub(crate) struct SharingMetrics {
    served_local: AtomicU64,
    served_borrowed: AtomicU64,
    served_peer: AtomicU64,
    served_cloud: AtomicU64,
    queued: AtomicU64,
    queue_timeouts: AtomicU64,
    queue_full: AtomicU64,
    rejected: AtomicU64,
    inbound_accepted: AtomicU64,
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

fn load(counter: &AtomicU64) -> u64 {
    counter.load(Ordering::Relaxed)
}

/// Process-lifetime sharing state. It survives runtime config swaps: slot
/// occupancy, peer views, and counters must not reset when a setting changes.
pub(crate) struct SharingRuntime {
    slots: Arc<SlotManager>,
    latest: RwLock<Arc<Config>>,
    key: RwLock<Option<Arc<NodeKey>>>,
    key_error: RwLock<Option<String>>,
    peers: RwLock<BTreeMap<String, Arc<PeerRuntime>>>,
    guard: RwLock<Arc<Vec<ipnetwork::IpNetwork>>>,
    nonces: NonceCache,
    cursor: AtomicUsize,
    client: reqwest::Client,
    metrics: SharingMetrics,
    peer_listener: RwLock<Option<SocketAddr>>,
    poller_started: AtomicBool,
    guard_rejections: AtomicU64,
}

impl SharingRuntime {
    pub(crate) fn new(config: Arc<Config>) -> Result<Arc<Self>> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .context("failed to build the peer HTTP client")?;
        let runtime = Arc::new(Self {
            slots: SlotManager::new(0, 0, 0),
            latest: RwLock::new(config.clone()),
            key: RwLock::new(None),
            key_error: RwLock::new(None),
            peers: RwLock::new(BTreeMap::new()),
            guard: RwLock::new(Arc::new(Vec::new())),
            nonces: NonceCache::default(),
            cursor: AtomicUsize::new(0),
            client,
            metrics: SharingMetrics::default(),
            peer_listener: RwLock::new(None),
            poller_started: AtomicBool::new(false),
            guard_rejections: AtomicU64::new(0),
        });
        runtime.reconfigure(config)?;
        Ok(runtime)
    }

    pub(crate) fn reconfigure(&self, config: Arc<Config>) -> Result<()> {
        let guard = parse_networks(&config.listen_guard.allowed_sources)?;
        // An existing key is always shown, even before sharing mode is on.
        let needs_key = config.sharing.with_peers()
            || !config.peers.is_empty()
            || key_path(&config.sharing).is_ok_and(|path| path.is_file());
        if needs_key && self.key.read().expect("key lock").is_none() {
            match load_or_create_key(&config.sharing) {
                Ok(key) => {
                    *self.key.write().expect("key lock") = Some(Arc::new(key));
                    *self.key_error.write().expect("key lock") = None;
                }
                Err(error) => {
                    *self.key_error.write().expect("key lock") = Some(format!("{error:#}"));
                    if config.sharing.with_peers() {
                        return Err(error);
                    }
                }
            }
        }
        let sharing = &config.sharing;
        let limited = sharing.limited();
        let local = if limited { sharing.local_limit } else { 0 };
        let incoming = if sharing.with_peers() && sharing.accept_incoming {
            sharing.incoming_limit
        } else {
            0
        };
        let depth = if limited { sharing.queue_depth } else { 0 };
        self.slots.resize(local, incoming, depth);
        {
            let mut peers = self.peers.write().expect("peer lock");
            peers.retain(|name, _| config.peers.contains_key(name));
            for name in config.peers.keys() {
                peers.entry(name.clone()).or_default();
            }
        }
        *self.guard.write().expect("guard lock") = Arc::new(guard);
        *self.latest.write().expect("config lock") = config;
        Ok(())
    }

    pub(crate) fn key(&self) -> Option<Arc<NodeKey>> {
        self.key.read().expect("key lock").clone()
    }

    /// The same key file `llmproxy --peer-key` uses.
    pub(crate) fn ensure_key(&self) -> Result<Arc<NodeKey>> {
        if let Some(key) = self.key() {
            return Ok(key);
        }
        let key = Arc::new(load_or_create_key(&self.latest().sharing)?);
        *self.key.write().expect("key lock") = Some(key.clone());
        *self.key_error.write().expect("key lock") = None;
        Ok(key)
    }

    fn granted(&self, permit: SlotPermit) -> SlotPermit {
        bump(match permit.kind() {
            SlotKind::Local => &self.metrics.served_local,
            _ => &self.metrics.served_borrowed,
        });
        permit
    }

    /// Bounded FIFO wait for a local or lent slot; a full queue or timeout is
    /// a native 429.
    async fn wait_for_slot(
        &self,
        sharing: &SharingConfig,
        wire: Wire,
    ) -> ResponseResult<SlotPermit> {
        let waited = Instant::now();
        let result = self
            .slots
            .wait_local(Duration::from_millis(sharing.queue_wait_ms))
            .await;
        let elapsed = waited.elapsed();
        crate::latency::add_queue_wait(elapsed);
        if elapsed > Duration::from_millis(1) {
            bump(&self.metrics.queued);
        }
        let permit = result.map_err(|error| {
            let message = match error {
                WaitError::Full => {
                    bump(&self.metrics.queue_full);
                    "all execution slots are busy and the wait queue is full"
                }
                WaitError::Timeout => {
                    bump(&self.metrics.queue_timeouts);
                    "timed out waiting for an execution slot"
                }
            };
            warn!(reason = message, "sharing rejected a local request");
            Box::new(rejection(wire, sharing.retry_after_secs, message, None))
        })?;
        Ok(self.granted(permit))
    }

    fn latest(&self) -> Arc<Config> {
        self.latest.read().expect("config lock").clone()
    }

    fn peer(&self, name: &str) -> Option<Arc<PeerRuntime>> {
        self.peers.read().expect("peer lock").get(name).cloned()
    }

    pub(crate) fn source_allowed(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        if ip.is_loopback() {
            return true;
        }
        let guard = self.guard.read().expect("guard lock");
        if guard.is_empty() || guard.iter().any(|network| network.contains(ip)) {
            return true;
        }
        drop(guard);
        self.guard_rejections.fetch_add(1, Ordering::Relaxed);
        false
    }

    /// Peers eligible for an outbound job, starting at the round-robin cursor.
    fn candidates(&self, config: &Config) -> Vec<String> {
        let stale = Duration::from_secs(config.sharing.status_stale_after_secs.max(1));
        let peers = self.peers.read().expect("peer lock");
        let mut ready: Vec<String> = config
            .peers
            .iter()
            .filter(|(_, peer)| peer.enabled && peer.send_outgoing)
            .filter(|(name, _)| {
                peers.get(*name).is_some_and(|runtime| {
                    let view = runtime.view.lock().expect("peer view poisoned");
                    !view.saturated
                        && view.fetched_at.is_some_and(|at| at.elapsed() < stale)
                        && view.status.as_ref().is_some_and(|status| {
                            status.enabled
                                && status.accept_incoming_for_you
                                && status.onprem_provider.healthy
                                && status.slots.incoming_free() > 0
                        })
                })
            })
            .map(|(name, _)| name.clone())
            .collect();
        if ready.len() > 1 {
            let start = self.cursor.fetch_add(1, Ordering::Relaxed) % ready.len();
            ready.rotate_left(start);
        }
        ready
    }

    pub(crate) fn set_peer_listener(&self, addr: SocketAddr) {
        *self.peer_listener.write().expect("listener lock") = Some(addr);
    }

    fn local_status(&self, state: &AppState, requester: Option<&str>) -> NodeStatus {
        let config = &state.config;
        let provider = state.providers.get(&config.sharing.onprem_provider);
        let (models, _) = provider
            .map(|provider| provider.model_cache.cached_ids())
            .unwrap_or_default();
        let breaker = provider.map_or("unknown", ProviderState::breaker_status);
        NodeStatus {
            node: config.sharing.node_name.clone(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol: PROTOCOL_VERSION,
            time: unix_now(),
            enabled: config.sharing.with_peers(),
            accept_incoming_for_you: config.sharing.with_peers()
                && config.sharing.accept_incoming
                && requester
                    .and_then(|name| config.peers.get(name))
                    .is_some_and(|peer| peer.enabled && peer.accept_incoming),
            slots: self.slots.snapshot(),
            onprem_provider: OnpremProviderStatus {
                healthy: provider.is_some() && breaker != "open",
                breaker: breaker.to_string(),
                models,
            },
        }
    }

    /// Admin view. Never includes the private key.
    pub(crate) fn admin_json(&self, state: &AppState) -> Value {
        let config = &state.config;
        let key = self.key();
        let peers = self.peers.read().expect("peer lock");
        let peer_rows: Vec<Value> = config
            .peers
            .iter()
            .map(|(name, peer)| {
                let runtime = peers.get(name);
                let (status, age, last_error, saturated) = runtime.map_or(
                    (None, None, None, false),
                    |runtime| {
                        let view = runtime.view.lock().expect("peer view poisoned");
                        (
                            view.status.clone(),
                            view.fetched_at.map(|at| at.elapsed().as_secs()),
                            view.last_error.clone(),
                            view.saturated,
                        )
                    },
                );
                let stale = config.sharing.status_stale_after_secs.max(1);
                // `last_error` is cleared by the next successful poll, so any
                // error still present describes the peer as it is now.
                let state_label = if !peer.enabled {
                    "disabled"
                } else if last_error.as_deref().is_some_and(|e| e.contains("401")) {
                    "unauthorized"
                } else if last_error.is_some() || age.is_none_or(|age| age >= stale) {
                    "unreachable"
                } else if saturated
                    || status
                        .as_ref()
                        .is_some_and(|status| status.slots.incoming_free() == 0)
                {
                    "full"
                } else if status
                    .as_ref()
                    .is_some_and(|status| !status.accept_incoming_for_you)
                {
                    "not accepting"
                } else {
                    "ready"
                };
                let counter = |pick: fn(&PeerRuntime) -> &AtomicU64| {
                    runtime.map_or(0, |runtime| load(pick(runtime)))
                };
                json!({
                    "name": name,
                    "url": peer.url,
                    "class": route_class_for_base_url(&peer.url).as_str(),
                    "public_key": peer.public_key,
                    "fingerprint": parse_public_key(&peer.public_key).ok().map(|key| fingerprint(&key)),
                    "enabled": peer.enabled,
                    "accept_incoming": peer.accept_incoming,
                    "send_outgoing": peer.send_outgoing,
                    "state": state_label,
                    "last_error": last_error,
                    "sent": counter(|r| &r.sent),
                    "inbound_accepted": counter(|r| &r.inbound_accepted),
                })
            })
            .collect();
        let metrics = &self.metrics;
        json!({
            "public_key": key.as_ref().map(|key| key.public_key().to_string()),
            "advertised_url": advertised_url(&config.sharing),
            "advertise_url": config.sharing.advertise_url,
            "node_name": config.sharing.node_name,
            "peer_listen": config.sharing.peer_listen,
            "peer_listener": self.peer_listener.read().expect("listener lock").map(|addr| addr.to_string()),
            "fingerprint": key.as_ref().map(|key| key.fingerprint()),
            "key_error": self.key_error.read().expect("key lock").clone(),
            "mode": config.sharing.mode,
            "onprem_provider": config.sharing.onprem_provider,
            "local_limit": config.sharing.local_limit,
            "incoming_limit": config.sharing.incoming_limit,
            "send_outgoing": config.sharing.send_outgoing,
            "accept_incoming": config.sharing.accept_incoming,
            "queue_depth": config.sharing.queue_depth,
            "queue_wait_ms": config.sharing.queue_wait_ms,
            "retry_after_secs": config.sharing.retry_after_secs,
            "cloud_overflow": config.sharing.cloud_overflow,
            "cloud_overflow_modes": config.sharing.cloud_overflow_modes,
            "cloud_overflow_active": config.sharing.cloud_overflow(config.routing_mode),
            "slots": self.slots.snapshot(),
            "allowed_sources": config.listen_guard.allowed_sources,
            "guard_rejections": self.guard_rejections.load(Ordering::Relaxed),
            "peers": peer_rows,
            "metrics": {
                "served_local": load(&metrics.served_local),
                "served_borrowed": load(&metrics.served_borrowed),
                "served_peer": load(&metrics.served_peer),
                "served_cloud": load(&metrics.served_cloud),
                "queued": load(&metrics.queued),
                "queue_timeouts": load(&metrics.queue_timeouts),
                "queue_full": load(&metrics.queue_full),
                "rejected": load(&metrics.rejected),
                "inbound_accepted": load(&metrics.inbound_accepted),
            },
        })
    }

    /// Verify a signed peer call. Every failure is reported to the caller the
    /// same way; the reason is for this node's log only.
    pub(crate) fn verify(
        &self,
        config: &Config,
        method: &Method,
        path: &str,
        headers: &HeaderMap,
        body: &[u8],
        purpose: AuthPurpose,
    ) -> std::result::Result<String, &'static str> {
        let sender = header_str(headers, PEER_HEADER).ok_or("missing peer header")?;
        let (peer_name, peer) = config
            .peers
            .iter()
            .find(|(_, peer)| peer.public_key == sender)
            .ok_or("unknown peer")?;
        if !peer.enabled {
            return Err("peer disabled");
        }
        if purpose == AuthPurpose::Job
            && !(peer.accept_incoming
                && config.sharing.with_peers()
                && config.sharing.accept_incoming)
        {
            return Err("peer not accepted for inbound jobs");
        }
        let timestamp = header_str(headers, TIMESTAMP_HEADER).ok_or("missing timestamp")?;
        let sent = timestamp.parse::<u64>().map_err(|_| "invalid timestamp")?;
        let skew = config.sharing.clock_skew_secs.max(1);
        if unix_now().abs_diff(sent) > skew {
            return Err("timestamp outside the accepted window");
        }
        let nonce = header_str(headers, NONCE_HEADER)
            .filter(|nonce| (16..=64).contains(&nonce.len()))
            .ok_or("missing or invalid nonce")?;
        let digest = header_str(headers, DIGEST_HEADER).ok_or("missing digest")?;
        if digest != content_digest(body) {
            return Err("body digest mismatch");
        }
        let signature = header_str(headers, SIGNATURE_HEADER)
            .and_then(|value| STANDARD.decode(value).ok())
            .ok_or("missing or invalid signature")?;
        let public = parse_public_key(&peer.public_key).map_err(|_| "invalid registered key")?;
        let own = self.key().ok_or("no node key")?;
        let message = signing_string(method, path, own.public_key(), timestamp, nonce, digest);
        UnparsedPublicKey::new(&ED25519, public)
            .verify(message.as_bytes(), &signature)
            .map_err(|_| "signature verification failed")?;
        // Recorded only after the signature holds, so unauthenticated traffic
        // cannot fill the replay cache.
        if !self
            .nonces
            .insert(nonce, Duration::from_secs(skew.saturating_mul(2)))
        {
            return Err("replayed nonce");
        }
        Ok(peer_name.to_string())
    }
}

/// The proxy listener with the source allowlist applied at accept, before any
/// HTTP parsing. Refused connections are dropped without a response.
pub(crate) struct GuardedListener {
    inner: TcpListener,
    runtime: Arc<SharingRuntime>,
}

impl GuardedListener {
    pub(crate) fn new(inner: TcpListener, runtime: Arc<SharingRuntime>) -> Self {
        Self { inner, runtime }
    }
}

impl axum::serve::Listener for GuardedListener {
    type Io = tokio::net::TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (io, addr) = axum::serve::Listener::accept(&mut self.inner).await;
            if self.runtime.source_allowed(addr.ip()) {
                return (io, addr);
            }
            debug!(source = %addr, "proxy listener refused a connection outside allowed_sources");
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

fn peer_path(wire: Wire) -> &'static str {
    match wire {
        Wire::AnthropicMessages => "/v1/messages",
        Wire::ChatCompletions => "/v1/chat/completions",
        Wire::Responses => "/v1/responses",
    }
}

pub(crate) struct Scheduled {
    pub(crate) provider: ProviderState,
    pub(crate) permit: Option<SlotPermit>,
}

/// An own slot, a peer, a local or lent incoming slot, peers that were only
/// full, cloud overflow, then a bounded wait. Peers come before lending so
/// incoming slots stay free for peer jobs. `Err` carries a finished response
/// when a peer served it or no tier could take it.
pub(crate) async fn schedule(
    state: &AppState,
    headers: &HeaderMap,
    wire: Wire,
    body: &mut Value,
    started: Instant,
    audit: &mut Option<Box<RoutingAuditDraft>>,
) -> ResponseResult<Scheduled> {
    let provider = crate::route_request(state, body)?;
    let sharing = &state.config.sharing;
    let Some(runtime) = state.sharing.as_ref().filter(|_| sharing.schedules()) else {
        return Ok(Scheduled {
            provider,
            permit: None,
        });
    };
    if provider.name != sharing.onprem_provider {
        return Ok(Scheduled {
            provider,
            permit: None,
        });
    }
    // Without a local limit this node is never full, so peers and the cloud
    // take work only while the on-prem provider is down (breaker open).
    let limited = sharing.limited();
    let down = provider.breaker_status() == "open";
    let overflow = limited || down;

    if limited
        && !down
        && let Some(permit) = runtime.slots.try_own_local()
    {
        return Ok(Scheduled {
            provider,
            permit: Some(runtime.granted(permit)),
        });
    }

    let mut busy = Vec::new();
    if sharing.with_peers() && sharing.send_outgoing && overflow {
        let names = runtime.candidates(&state.config);
        if let Some(response) = try_peers(
            runtime, state, names, wire, headers, body, started, audit, &mut busy,
        )
        .await
        {
            return Err(Box::new(response));
        }
    }

    if limited
        && !down
        && let Some(permit) = runtime.slots.try_local()
    {
        return Ok(Scheduled {
            provider,
            permit: Some(runtime.granted(permit)),
        });
    }

    // A refusal happens before anything runs, so asking a full peer again is
    // routing, not a retry.
    if !busy.is_empty() {
        let names = std::mem::take(&mut busy);
        if let Some(response) = try_peers(
            runtime, state, names, wire, headers, body, started, audit, &mut busy,
        )
        .await
        {
            return Err(Box::new(response));
        }
    }

    if overflow && let Some(route) = sharing.cloud_overflow(state.config.routing_mode) {
        if let Some(object) = body.as_object_mut() {
            object.insert("model".to_string(), Value::String(route.to_string()));
        }
        bump(&runtime.metrics.served_cloud);
        info!(route, "local overflow sent to the cloud");
        let provider = crate::route_request(state, body)?;
        return Ok(Scheduled {
            provider,
            permit: None,
        });
    }

    // Unlimited, or down with nowhere else to go: run it normally and let the
    // provider's own breaker and fallback answer.
    if !limited || down {
        return Ok(Scheduled {
            provider,
            permit: None,
        });
    }
    let permit = runtime.wait_for_slot(sharing, wire).await?;
    Ok(Scheduled {
        provider,
        permit: Some(permit),
    })
}

/// One pass over `names`. Returns the response of the peer that served it;
/// peers that refused only for lack of a slot are pushed onto `busy`.
#[allow(clippy::too_many_arguments)]
async fn try_peers(
    runtime: &SharingRuntime,
    state: &AppState,
    names: Vec<String>,
    wire: Wire,
    headers: &HeaderMap,
    body: &Value,
    started: Instant,
    audit: &mut Option<Box<RoutingAuditDraft>>,
    busy: &mut Vec<String>,
) -> Option<Response> {
    for name in names {
        let Some(peer) = state.config.peers.get(&name) else {
            continue;
        };
        // With the confidentiality boundary on, a prompt may only leave this
        // node for a peer inside the approved private networks.
        let privacy = &state.config.privacy;
        if privacy.enabled()
            && crate::privacy::NetworkPolicy::parse(&privacy.private_networks)
                .and_then(|policy| policy.check_url(&peer.url))
                .is_err()
        {
            debug!(peer = %name, "peer skipped: outside privacy.private-networks");
            continue;
        }
        match forward_to_peer(runtime, state, &name, peer, wire, headers, body, started).await {
            Forwarded::Served(response, model) => {
                bump(&runtime.metrics.served_peer);
                if let Some(audit) = audit.take() {
                    audit.record_peer_destination(&name, &peer.url, &model);
                }
                return Some(response);
            }
            Forwarded::Refused { busy: true } => busy.push(name),
            Forwarded::Refused { busy: false } => {}
        }
    }
    None
}

/// Slot admission for an audited destination (an integrated redirect to the
/// on-prem provider). The decision named this node's model, so the request may
/// wait for a local slot but is never moved to a peer or the cloud.
pub(crate) async fn admit_audited(
    state: &AppState,
    provider: &str,
    wire: Wire,
) -> ResponseResult<Option<SlotPermit>> {
    let sharing = &state.config.sharing;
    let Some(runtime) = state.sharing.as_ref() else {
        return Ok(None);
    };
    if provider != sharing.onprem_provider || !sharing.limited() {
        return Ok(None);
    }
    runtime.wait_for_slot(sharing, wire).await.map(Some)
}

fn rejection(wire: Wire, retry_after: u64, message: &str, slots: Option<SlotSnapshot>) -> Response {
    let body = match wire {
        Wire::AnthropicMessages => json!({
            "type": "error",
            "error": {"type": "rate_limit_error", "message": message}
        }),
        Wire::ChatCompletions | Wire::Responses => json!({
            "error": {"message": message, "type": "rate_limit_error", "code": "rate_limit_exceeded"}
        }),
    };
    let mut response = (StatusCode::TOO_MANY_REQUESTS, axum::Json(body)).into_response();
    let headers = response.headers_mut();
    headers.insert(
        HeaderName::from_static("retry-after"),
        HeaderValue::from(retry_after.max(1)),
    );
    headers.insert(
        HeaderName::from_static(REJECT_HEADER),
        HeaderValue::from_static("slots"),
    );
    if let Some(slots) = slots
        && let Ok(value) = HeaderValue::from_str(&slots.header_value())
    {
        headers.insert(HeaderName::from_static(SLOTS_HEADER), value);
    }
    response
}

enum Forwarded {
    Served(Response, String),
    /// The peer refused before running anything; the next tier may try.
    /// `busy` means it only lacked a free slot and may be asked again.
    Refused {
        busy: bool,
    },
}

/// Caller headers a peer needs to render the same protocol. Credentials never
/// cross nodes.
const FORWARDED_HEADERS: &[&str] = &[
    "accept",
    "anthropic-beta",
    "anthropic-version",
    "user-agent",
];

#[allow(clippy::too_many_arguments)]
async fn forward_to_peer(
    runtime: &SharingRuntime,
    state: &AppState,
    name: &str,
    peer: &PeerConfig,
    wire: Wire,
    incoming: &HeaderMap,
    body: &Value,
    started: Instant,
) -> Forwarded {
    let Some(key) = runtime.key() else {
        return Forwarded::Refused { busy: false };
    };
    let peer_runtime = runtime.peer(name);
    let path = peer_path(wire);
    let bytes = match serde_json::to_vec(body) {
        Ok(bytes) => bytes,
        Err(_) => return Forwarded::Refused { busy: false },
    };
    let streaming = request_is_streaming(body);
    let provider_label = format!("peer:{name}");
    let meta = UpstreamRequestMeta::new(
        &provider_label,
        incoming,
        Method::POST,
        path.trim_start_matches('/').to_string(),
        if streaming {
            UpstreamResponseKind::Stream
        } else {
            UpstreamResponseKind::Json
        },
        false,
    )
    .without_body_logging();
    let mut headers = sign_request(&key, &peer.public_key, &Method::POST, path, &bytes);
    for header in FORWARDED_HEADERS {
        if let Some(value) = incoming.get(*header) {
            headers.insert(HeaderName::from_static(header), value.clone());
        }
    }
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        HeaderName::from_static(HOP_HEADER),
        HeaderValue::from_static("1"),
    );
    headers.insert(
        HeaderName::from_static("x-request-id"),
        meta.request_id_header.clone(),
    );
    let url = format!("{}{path}", peer.url.trim_end_matches('/'));
    let timeout = Duration::from_secs(state.config.sharing.peer_timeout_secs.max(1));
    let mut request = runtime.client.post(&url).headers(headers).body(bytes);
    if !streaming {
        request = request.timeout(timeout);
    }
    let model = crate::request_model(body).to_string();
    let input_estimate = crate::estimate_tokens(body);
    let dispatched = Instant::now();
    let result = tokio::time::timeout(timeout, request.send()).await;
    crate::latency::add_upstream_wait(dispatched.elapsed());
    if let Some(peer_runtime) = &peer_runtime {
        bump(&peer_runtime.sent);
    }
    let response = match result {
        Ok(Ok(response)) => response,
        Ok(Err(error)) if error.is_connect() => {
            // Nothing reached the peer, so trying the next tier is routing, not
            // a retry.
            warn!(peer = name, error = %error, "peer refused the connection");
            if let Some(peer_runtime) = &peer_runtime {
                let mut view = peer_runtime.view.lock().expect("peer view poisoned");
                view.saturated = true;
                view.last_error = Some(format!("connect: {error}"));
            }
            return Forwarded::Refused { busy: false };
        }
        Ok(Err(error)) => {
            let failure = UpstreamFailure::from_reqwest(&meta, error);
            crate::log_upstream_failure(&failure, "peer_forward");
            return Forwarded::Served(crate::upstream_error_response(&failure), model);
        }
        Err(_) => {
            let failure = UpstreamFailure::timeout(
                &meta,
                format!("peer {name} did not respond within {}s", timeout.as_secs()),
            );
            crate::log_upstream_failure(&failure, "peer_forward");
            return Forwarded::Served(crate::upstream_error_response(&failure), model);
        }
    };
    if let Some(snapshot) = response
        .headers()
        .get(SLOTS_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(SlotSnapshot::from_header)
        && let Some(peer_runtime) = &peer_runtime
    {
        peer_runtime.note_slots(snapshot);
    }
    let reject = response
        .headers()
        .get(REJECT_HEADER)
        .and_then(|value| value.to_str().ok());
    match (response.status(), reject) {
        (StatusCode::TOO_MANY_REQUESTS, Some("slots")) => {
            debug!(peer = name, "peer had no free incoming slot");
            if let Some(peer_runtime) = &peer_runtime {
                peer_runtime
                    .view
                    .lock()
                    .expect("peer view poisoned")
                    .saturated = true;
            }
            return Forwarded::Refused { busy: true };
        }
        (StatusCode::UNAUTHORIZED, Some("auth")) => {
            error!(
                peer = name,
                "peer rejected this node's signature; check both peer registrations"
            );
            if let Some(peer_runtime) = &peer_runtime {
                let mut view = peer_runtime.view.lock().expect("peer view poisoned");
                view.saturated = true;
                view.last_error = Some("HTTP 401: peer rejected this node's signature".into());
            }
            return Forwarded::Refused { busy: false };
        }
        _ => {}
    }
    info!(peer = name, model = %model, "sharing overflow forwarded to peer");
    let relayed = crate::relay_response(&meta, response).await;
    let tracked = track_response(
        ResponseTracking::new(
            route_class_for_base_url(&peer.url),
            &provider_label,
            &model,
            wire.as_str(),
            started,
            dispatched,
            input_estimate,
        )
        .with_pricing(crate::request_pricing(
            &state.config,
            &provider_label,
            &model,
        )),
        relayed,
    );
    Forwarded::Served(tracked, model)
}

pub(crate) fn spawn_status_poller(runtime: Arc<SharingRuntime>) {
    if runtime.poller_started.swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::spawn(async move {
        loop {
            let config = runtime.latest();
            let interval = Duration::from_secs(config.sharing.status_interval_secs.max(1));
            if config.sharing.with_peers()
                && let Some(key) = runtime.key()
            {
                let polls = config
                    .peers
                    .iter()
                    .filter(|(_, peer)| peer.enabled && peer.send_outgoing)
                    .map(|(name, peer)| poll_peer(&runtime, &config, &key, name, peer));
                futures_util::future::join_all(polls).await;
            }
            tokio::time::sleep(interval).await;
        }
    });
}

async fn poll_peer(
    runtime: &SharingRuntime,
    config: &Config,
    key: &NodeKey,
    name: &str,
    peer: &PeerConfig,
) {
    let Some(peer_runtime) = runtime.peer(name) else {
        return;
    };
    let path = "/sharing/status";
    let headers = sign_request(key, &peer.public_key, &Method::GET, path, b"");
    let url = format!("{}{path}", peer.url.trim_end_matches('/'));
    let result = runtime
        .client
        .get(&url)
        .headers(headers)
        .timeout(Duration::from_secs(
            config.sharing.status_interval_secs.clamp(1, 10),
        ))
        .send()
        .await;
    let outcome = match result {
        Ok(response) if response.status().is_success() => response
            .json::<NodeStatus>()
            .await
            .map_err(|error| format!("invalid status: {error}")),
        Ok(response) => Err(format!("HTTP {}", response.status().as_u16())),
        Err(error) => Err(error.to_string()),
    };
    let mut view = peer_runtime.view.lock().expect("peer view poisoned");
    match outcome {
        Ok(status) => {
            view.status = Some(status);
            view.fetched_at = Some(Instant::now());
            view.last_error = None;
            view.saturated = false;
        }
        Err(error) => {
            debug!(peer = name, error = %error, "peer status poll failed");
            view.last_error = Some(error);
        }
    }
}

pub(crate) async fn spawn_peer_listener(shared: SharedState) -> Result<Option<SocketAddr>> {
    let state = shared.snapshot();
    let config = &state.config;
    let Some(runtime) = state.sharing.clone() else {
        return Ok(None);
    };
    if let Some(bound) = *runtime.peer_listener.read().expect("listener lock") {
        return Ok(Some(bound));
    }
    // Peers poll this listener for status, so it opens with the first enabled
    // peer; whether their jobs run is `accept_incoming`.
    if !config.sharing.with_peers() || !config.peers.values().any(|peer| peer.enabled) {
        return Ok(None);
    }
    let address = config
        .sharing
        .peer_listen
        .as_deref()
        .unwrap_or(DEFAULT_PEER_LISTEN);
    let addr: SocketAddr = address
        .parse()
        .with_context(|| format!("invalid sharing.peer_listen {address:?}"))?;
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind peer listener {addr}"))?;
    let bound = listener.local_addr()?;
    runtime.set_peer_listener(bound);
    let app = Router::new()
        .route("/v1/messages", post(inbound_messages))
        .route("/v1/chat/completions", post(inbound_chat))
        .route("/v1/responses", post(inbound_responses))
        .route("/sharing/status", get(inbound_status))
        .route("/sharing/hello", get(inbound_status))
        .fallback(|| async { StatusCode::NOT_FOUND })
        .with_state(shared)
        .layer(middleware::from_fn(crate::trace_http_request));
    info!(peer_listen_addr = %bound, "llmproxy peer listener ready");
    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            error!(error = %error, "peer listener failed");
        }
    });
    Ok(Some(bound))
}

fn unauthorized() -> Response {
    let mut response = (
        StatusCode::UNAUTHORIZED,
        axum::Json(json!({"error": {"type": "authentication_error", "message": "unauthorized"}})),
    )
        .into_response();
    response.headers_mut().insert(
        HeaderName::from_static(REJECT_HEADER),
        HeaderValue::from_static("auth"),
    );
    response
}

fn authenticate(
    state: &AppState,
    runtime: &SharingRuntime,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &[u8],
    purpose: AuthPurpose,
) -> ResponseResult<String> {
    runtime
        .verify(&state.config, method, uri.path(), headers, body, purpose)
        .map_err(|reason| {
            warn!(
                peer = header_str(headers, PEER_HEADER).unwrap_or("-"),
                path = uri.path(),
                reason,
                "peer authentication failed"
            );
            Box::new(unauthorized())
        })
}

async fn inbound_status(
    State(shared): State<SharedState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let state = shared.snapshot();
    let Some(runtime) = state.sharing.clone() else {
        return unauthorized();
    };
    match authenticate(
        &state,
        &runtime,
        &method,
        &uri,
        &headers,
        b"",
        AuthPurpose::Status,
    ) {
        Ok(peer) => axum::Json(runtime.local_status(&state, Some(&peer))).into_response(),
        Err(response) => *response,
    }
}

async fn inbound_messages(
    State(shared): State<SharedState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    inbound_job(shared, Wire::AnthropicMessages, method, uri, headers, body).await
}

async fn inbound_chat(
    State(shared): State<SharedState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    inbound_job(shared, Wire::ChatCompletions, method, uri, headers, body).await
}

async fn inbound_responses(
    State(shared): State<SharedState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    inbound_job(shared, Wire::Responses, method, uri, headers, body).await
}

/// No peer tier, no cloud tier, no failover, and no triage: an inbound job can
/// only run on this node's on-prem provider or be rejected.
async fn inbound_job(
    shared: SharedState,
    wire: Wire,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let state = shared.snapshot();
    let Some(runtime) = state.sharing.clone() else {
        return unauthorized();
    };
    let peer = match authenticate(
        &state,
        &runtime,
        &method,
        &uri,
        &headers,
        &body,
        AuthPurpose::Job,
    ) {
        Ok(peer) => peer,
        Err(response) => return *response,
    };
    let peer_runtime = runtime.peer(&peer);
    let Some(permit) = runtime.slots.try_incoming() else {
        debug!(peer = %peer, "inbound job rejected: no free incoming slot");
        return rejection(
            wire,
            state.config.sharing.retry_after_secs,
            "no free incoming execution slot on this node",
            Some(runtime.slots.snapshot()),
        );
    };
    let value: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": {"type": "invalid_request_error", "message": error.to_string()}})),
            )
                .into_response();
        }
    };
    let inbound_state = match state.inbound_peer_state() {
        Ok(inbound_state) => inbound_state,
        Err(error) => return crate::internal_error_response(error.to_string()),
    };
    bump(&runtime.metrics.inbound_accepted);
    if let Some(peer_runtime) = &peer_runtime {
        bump(&peer_runtime.inbound_accepted);
    }
    info!(peer = %peer, frontend = wire.as_str(), "running inbound peer job");
    let mut forwarded = HeaderMap::with_capacity(FORWARDED_HEADERS.len() + 1);
    for header in FORWARDED_HEADERS.iter().chain(["x-request-id"].iter()) {
        if let Some(value) = headers.get(*header) {
            forwarded.insert(HeaderName::from_static(header), value.clone());
        }
    }
    let response = match wire {
        Wire::AnthropicMessages => {
            crate::anthropic_messages_with_state(inbound_state, None, forwarded, value).await
        }
        Wire::ChatCompletions => {
            crate::chat_completions_with_state(inbound_state, None, forwarded, value).await
        }
        Wire::Responses => crate::responses_with_state(inbound_state, None, forwarded, value).await,
    };
    let mut response = hold(response, Some(permit));
    if let Ok(value) = HeaderValue::from_str(&runtime.slots.snapshot().header_value()) {
        response
            .headers_mut()
            .insert(HeaderName::from_static(SLOTS_HEADER), value);
    }
    response
}

#[cfg(test)]
#[path = "sharing_tests.rs"]
mod tests;
