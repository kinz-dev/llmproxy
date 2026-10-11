//! Admin API for sharing, peers, the listener guard, and pricing.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, value};

use super::{AdminState, Persist, api_error, apply_config_change, ensure_table, with_field};
use crate::sharing::{self, ModeSwitches, PeerConfig, SharingConfig, SharingMode};

fn optional(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

pub(super) async fn get_sharing(State(state): State<AdminState>) -> Response {
    let snapshot = state.shared.snapshot();
    match snapshot.sharing.as_ref() {
        Some(runtime) => Json(runtime.admin_json(&snapshot)).into_response(),
        None => api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "sharing runtime unavailable",
        ),
    }
}

/// Rewrites a pre-rename `[cluster]` table into `[sharing]` before any edit.
pub(super) fn migrate_legacy_doc(doc: &mut DocumentMut) {
    let legacy_peer_keys = doc
        .get("peers")
        .and_then(Item::as_table_like)
        .is_some_and(|peers| {
            peers.iter().any(|(_, peer)| {
                ["accept_inbound", "send_outbound", "node_name"]
                    .iter()
                    .any(|key| peer.get(key).is_some())
            })
        });
    if doc.get("cluster").is_none() && !legacy_peer_keys {
        return;
    }
    let Ok(mut table) = doc.to_string().parse::<toml::Table>() else {
        return;
    };
    sharing::migrate_legacy(&mut table);
    let migrated = toml::Table::from_iter(
        ["sharing", "peers"]
            .into_iter()
            .filter_map(|key| table.remove(key).map(|value| (key.to_string(), value))),
    );
    let Ok(text) = toml::to_string(&migrated) else {
        return;
    };
    let Ok(fresh) = text.parse::<DocumentMut>() else {
        return;
    };
    doc.remove("cluster");
    for key in ["sharing", "peers"] {
        if let Some(item) = fresh.get(key) {
            doc.insert(key, item.clone());
        }
    }
}

fn write_sharing(doc: &mut DocumentMut, sharing: &SharingConfig) -> Result<(), String> {
    let defaults = SharingConfig::default();
    let table = ensure_table(doc, "sharing")?;
    let mode = match sharing.mode {
        SharingMode::Local => "local",
        SharingMode::Peers => "peers",
    };
    table.insert("mode", value(mode));
    table.insert("node_name", value(sharing.node_name.as_str()));
    for (key, text) in [
        ("peer_listen", &sharing.peer_listen),
        ("advertise_url", &sharing.advertise_url),
        ("cloud_overflow", &sharing.cloud_overflow),
    ] {
        match text {
            Some(text) => {
                table.insert(key, value(text.as_str()));
            }
            None => {
                table.remove(key);
            }
        }
    }
    if !sharing.onprem_provider.is_empty() {
        table.insert("onprem_provider", value(sharing.onprem_provider.as_str()));
    }
    for (key, number, default) in [
        (
            "local_limit",
            sharing.local_limit.into(),
            defaults.local_limit.into(),
        ),
        (
            "incoming_limit",
            sharing.incoming_limit.into(),
            defaults.incoming_limit.into(),
        ),
        (
            "queue_depth",
            sharing.queue_depth.into(),
            defaults.queue_depth.into(),
        ),
        (
            "queue_wait_ms",
            sharing.queue_wait_ms,
            defaults.queue_wait_ms,
        ),
        (
            "retry_after_secs",
            sharing.retry_after_secs,
            defaults.retry_after_secs,
        ),
    ] {
        if number != default || table.contains_key(key) {
            table.insert(key, value(number as i64));
        }
    }
    table.insert("send_outgoing", value(sharing.send_outgoing));
    table.insert("accept_incoming", value(sharing.accept_incoming));
    let modes = sharing.cloud_overflow_modes;
    if modes != ModeSwitches::default() || table.contains_key("cloud_overflow_modes") {
        let mut inline = InlineTable::new();
        inline.insert("proxy", modes.proxy.into());
        inline.insert("intercept", modes.intercept.into());
        inline.insert("integrated", modes.integrated.into());
        table.insert("cloud_overflow_modes", value(inline));
    }
    Ok(())
}

fn write_peer(doc: &mut DocumentMut, name: &str, peer: Option<&PeerConfig>) -> Result<(), String> {
    let peers = ensure_table(doc, "peers")?;
    peers.set_implicit(true);
    let Some(peer) = peer else {
        peers.remove(name);
        return Ok(());
    };
    let item = peers.entry(name).or_insert(Item::Table(Table::new()));
    let table = item
        .as_table_mut()
        .ok_or_else(|| format!("[peers.{name}] is not a table"))?;
    table.set_implicit(false);
    table.remove("node_name");
    table.insert("url", value(peer.url.as_str()));
    table.insert("public_key", value(peer.public_key.as_str()));
    table.insert("enabled", value(peer.enabled));
    table.insert("accept_incoming", value(peer.accept_incoming));
    table.insert("send_outgoing", value(peer.send_outgoing));
    Ok(())
}

fn fill_identity(sharing: &mut SharingConfig) {
    if sharing.node_name.is_empty() {
        sharing.node_name = sharing::default_node_name();
    }
}

type PeerChange = Option<(String, Option<PeerConfig>)>;

/// Applies a whole sharing section (and optionally one peer), opens the peer
/// port when it becomes needed, and answers with the sharing view.
async fn save(state: &AdminState, sharing: SharingConfig, peer: PeerChange) -> Response {
    let Some(runtime) = state.shared.snapshot().sharing.clone() else {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "sharing runtime unavailable",
        );
    };
    if sharing.with_peers()
        && let Err(error) = runtime.ensure_key()
    {
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}"));
    }
    let (doc_sharing, doc_peer) = (sharing.clone(), peer.clone());
    let response = apply_config_change(
        state,
        move |config| {
            config.sharing = sharing;
            match peer {
                Some((name, Some(peer))) => {
                    config.peers.insert(name, peer);
                }
                Some((name, None)) => {
                    config
                        .peers
                        .remove(&name)
                        .ok_or_else(|| format!("unknown peer {name:?}"))?;
                }
                None => {}
            }
            Ok(())
        },
        Persist::Edit(Box::new(move |doc| {
            write_sharing(doc, &doc_sharing)?;
            if let Some((name, peer)) = &doc_peer {
                write_peer(doc, name, peer.as_ref())?;
            }
            Ok(())
        })),
    )
    .await;
    if !response.status().is_success() {
        return response;
    }
    if let Err(error) = sharing::spawn_peer_listener(state.shared.clone()).await {
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}"));
    }
    with_field(
        response,
        "sharing",
        runtime.admin_json(&state.shared.snapshot()),
    )
    .await
}

#[derive(Deserialize)]
pub(super) struct SharingPutBody {
    mode: Option<SharingMode>,
    /// 0 means no limit.
    local_limit: Option<u32>,
    send_outgoing: Option<bool>,
    accept_incoming: Option<bool>,
    incoming_limit: Option<u32>,
    node_name: Option<String>,
    /// Empty resets it.
    peer_listen: Option<String>,
    /// Empty resets it.
    advertise_url: Option<String>,
    onprem_provider: Option<String>,
    queue_depth: Option<u32>,
    queue_wait_ms: Option<u64>,
    retry_after_secs: Option<u64>,
}

pub(super) async fn put_sharing(
    State(state): State<AdminState>,
    Json(body): Json<SharingPutBody>,
) -> Response {
    let config = state.shared.snapshot().config.clone();
    let mut sharing = config.sharing.clone();
    if let Some(mode) = body.mode {
        sharing.mode = mode;
    }
    if let Some(limit) = body.local_limit {
        sharing.local_limit = limit;
    }
    if let Some(send) = body.send_outgoing {
        sharing.send_outgoing = send;
    }
    if let Some(accept) = body.accept_incoming {
        sharing.accept_incoming = accept;
    }
    if let Some(limit) = body.incoming_limit {
        sharing.incoming_limit = limit;
    }
    if let Some(name) = body.node_name {
        sharing.node_name = name.trim().to_string();
    }
    if let Some(listen) = body.peer_listen {
        sharing.peer_listen = optional(&listen);
    }
    if let Some(url) = body.advertise_url {
        sharing.advertise_url = optional(&url).map(|url| url.trim_end_matches('/').to_string());
    }
    if let Some(provider) = body.onprem_provider {
        sharing.onprem_provider = provider.trim().to_string();
    }
    if let Some(depth) = body.queue_depth {
        sharing.queue_depth = depth;
    }
    if let Some(wait) = body.queue_wait_ms {
        sharing.queue_wait_ms = wait;
    }
    if let Some(retry) = body.retry_after_secs {
        sharing.retry_after_secs = retry;
    }
    fill_identity(&mut sharing);
    save(&state, sharing, None).await
}

#[derive(Deserialize)]
pub(super) struct CloudOverflowPutBody {
    /// `provider/model`; empty turns it off.
    route: Option<String>,
    modes: Option<ModeSwitches>,
}

pub(super) async fn put_cloud_overflow(
    State(state): State<AdminState>,
    Json(body): Json<CloudOverflowPutBody>,
) -> Response {
    let config = state.shared.snapshot().config.clone();
    let mut sharing = config.sharing.clone();
    if let Some(route) = body.route.as_deref() {
        sharing.cloud_overflow = optional(route);
    }
    if let Some(modes) = body.modes {
        sharing.cloud_overflow_modes = modes;
    }
    fill_identity(&mut sharing);
    save(&state, sharing, None).await
}

pub(super) async fn post_key(State(state): State<AdminState>) -> Response {
    let config = state.shared.snapshot().config.clone();
    let mut sharing = config.sharing.clone();
    fill_identity(&mut sharing);
    if let Some(runtime) = state.shared.snapshot().sharing.as_ref()
        && let Err(error) = runtime.ensure_key()
    {
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}"));
    }
    save(&state, sharing, None).await
}

#[derive(Deserialize)]
pub(super) struct AddPeerBody {
    name: String,
    url: String,
    public_key: String,
}

pub(super) async fn post_peer(
    State(state): State<AdminState>,
    Json(body): Json<AddPeerBody>,
) -> Response {
    let public_key = body.public_key.trim().to_string();
    if let Err(error) = sharing::parse_public_key(&public_key) {
        return api_error(StatusCode::BAD_REQUEST, format!("{error:#}"));
    }
    let snapshot = state.shared.snapshot();
    let own_key = snapshot
        .sharing
        .as_ref()
        .and_then(|runtime| runtime.key())
        .is_some_and(|key| key.public_key() == public_key);
    if own_key {
        return api_error(
            StatusCode::BAD_REQUEST,
            "that is this node's own key; paste the key from the other node",
        );
    }
    let mut sharing = snapshot.config.sharing.clone();
    fill_identity(&mut sharing);
    sharing.mode = SharingMode::Peers;
    let name = body.name.trim().replace([' ', '/', ':'], "-");
    let peer = PeerConfig {
        url: body.url.trim().trim_end_matches('/').to_string(),
        public_key,
        ..PeerConfig::default()
    };
    save(&state, sharing, Some((name, Some(peer)))).await
}

pub(super) async fn put_peer(
    State(state): State<AdminState>,
    AxumPath(name): AxumPath<String>,
    Json(peer): Json<PeerConfig>,
) -> Response {
    let name = name.trim().to_string();
    let peer = PeerConfig {
        url: peer.url.trim().to_string(),
        public_key: peer.public_key.trim().to_string(),
        ..peer
    };
    let config = state.shared.snapshot().config.clone();
    let mut sharing = config.sharing.clone();
    fill_identity(&mut sharing);
    save(&state, sharing, Some((name, Some(peer)))).await
}

pub(super) async fn delete_peer(
    State(state): State<AdminState>,
    AxumPath(name): AxumPath<String>,
) -> Response {
    let sharing = state.shared.snapshot().config.sharing.clone();
    save(&state, sharing, Some((name, None))).await
}

#[derive(Deserialize)]
pub(super) struct ListenGuardPutBody {
    allowed_sources: Vec<String>,
}

pub(super) async fn put_listen_guard(
    State(state): State<AdminState>,
    Json(body): Json<ListenGuardPutBody>,
) -> Response {
    let sources: Vec<String> = body
        .allowed_sources
        .iter()
        .map(|source| source.trim().to_string())
        .filter(|source| !source.is_empty())
        .collect();
    let doc_sources = sources.clone();
    apply_config_change(
        &state,
        move |config| {
            config.listen_guard.allowed_sources = sources;
            Ok(())
        },
        Persist::Edit(Box::new(move |doc| {
            let table = ensure_table(doc, "listen_guard")?;
            table.insert(
                "allowed_sources",
                value(doc_sources.iter().map(String::as_str).collect::<Array>()),
            );
            Ok(())
        })),
    )
    .await
}

#[derive(Clone, Deserialize)]
pub(super) struct PriceBody {
    input_per_mtok: f64,
    output_per_mtok: f64,
}

#[derive(Deserialize)]
pub(super) struct PricingPutBody {
    /// Empty clears it.
    baseline_model: Option<String>,
    /// Replaces the whole rate table.
    models: BTreeMap<String, PriceBody>,
}

pub(super) async fn put_pricing(
    State(state): State<AdminState>,
    Json(body): Json<PricingPutBody>,
) -> Response {
    if body
        .models
        .values()
        .any(|price| !price.input_per_mtok.is_finite() || !price.output_per_mtok.is_finite())
    {
        return api_error(StatusCode::BAD_REQUEST, "rates must be finite numbers");
    }
    let baseline = body.baseline_model.as_deref().and_then(optional);
    let models: BTreeMap<String, PriceBody> = body
        .models
        .into_iter()
        .map(|(key, price)| (key.trim().to_string(), price))
        .filter(|(key, _)| !key.is_empty())
        .collect();
    let (doc_baseline, doc_models) = (baseline.clone(), models.clone());
    apply_config_change(
        &state,
        move |config| {
            config.set_pricing(
                baseline,
                models
                    .into_iter()
                    .map(|(key, price)| (key, (price.input_per_mtok, price.output_per_mtok)))
                    .collect(),
            );
            Ok(())
        },
        Persist::Edit(Box::new(move |doc| {
            let pricing = ensure_table(doc, "pricing")?;
            match doc_baseline {
                Some(baseline) => {
                    pricing.insert("baseline_model", value(baseline));
                }
                None => {
                    pricing.remove("baseline_model");
                }
            }
            let mut table = Table::new();
            table.set_implicit(true);
            for (key, price) in doc_models {
                let mut inline = InlineTable::new();
                inline.insert("input_per_mtok", price.input_per_mtok.into());
                inline.insert("output_per_mtok", price.output_per_mtok.into());
                table.insert(&key, value(inline));
            }
            pricing.insert("models", Item::Table(table));
            Ok(())
        })),
    )
    .await
}
