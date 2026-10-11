use super::*;

use std::sync::atomic::AtomicU32;

use axum::Json;
use serde_json::json;

use crate::{build_app, build_provider_state, effective_provider_configs, parse_config};

fn temp_key_path(label: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "llmproxy-sharing-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(format!("{label}_ed25519"))
}

fn key_for(path: &Path) -> NodeKey {
    load_or_create_key(&SharingConfig {
        private_key_file: Some(path.to_path_buf()),
        ..SharingConfig::default()
    })
    .unwrap()
}

#[test]
fn local_work_borrows_incoming_slots_but_peers_never_take_local_ones() {
    let slots = SlotManager::new(2, 1, 0);
    let a = slots.try_local().unwrap();
    let b = slots.try_local().unwrap();
    let c = slots.try_local().unwrap();
    assert_eq!(
        [a.kind(), b.kind(), c.kind()],
        [SlotKind::Local, SlotKind::Local, SlotKind::Borrowed]
    );
    assert!(slots.try_local().is_none());
    assert!(slots.try_incoming().is_none(), "lent slot is busy");
    drop(a);
    assert!(
        slots.try_incoming().is_none(),
        "a free local slot is never given to a peer"
    );
    drop(c);
    let incoming = slots.try_incoming().unwrap();
    assert_eq!(incoming.kind(), SlotKind::Incoming);
    let snapshot = slots.snapshot();
    assert_eq!(
        (
            snapshot.local_busy,
            snapshot.incoming_busy,
            snapshot.borrowed
        ),
        (1, 1, 0)
    );
}

#[tokio::test]
async fn released_slots_go_to_the_oldest_waiter_first() {
    let slots = SlotManager::new(1, 0, 4);
    let held = slots.try_local().unwrap();
    let first = tokio::spawn({
        let slots = slots.clone();
        async move { slots.wait_local(Duration::from_secs(5)).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let second = tokio::spawn({
        let slots = slots.clone();
        async move {
            slots
                .wait_local(Duration::from_millis(200))
                .await
                .map(|_| 2)
        }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(slots.snapshot().queued, 2);
    drop(held);
    let first = first.await.unwrap().expect("oldest waiter is served");
    assert_eq!(second.await.unwrap(), Err(WaitError::Timeout));
    assert_eq!(slots.snapshot().queued, 0);
    drop(first);
    assert_eq!(slots.snapshot().local_busy, 0);
}

#[tokio::test]
async fn a_full_queue_rejects_immediately_and_a_peer_slot_release_feeds_local_waiters() {
    let slots = SlotManager::new(1, 1, 1);
    let _local = slots.try_local().unwrap();
    let peer = slots.try_incoming().unwrap();
    let waiter = tokio::spawn({
        let slots = slots.clone();
        async move {
            slots
                .wait_local(Duration::from_secs(5))
                .await
                .map(|p| p.kind())
        }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        slots.wait_local(Duration::from_secs(5)).await.unwrap_err(),
        WaitError::Full
    );
    drop(peer);
    assert_eq!(waiter.await.unwrap(), Ok(SlotKind::Borrowed));
    assert_eq!(slots.snapshot().borrowed, 0, "permit dropped with the task");
}

#[tokio::test]
async fn a_cancelled_waiter_leaves_the_queue() {
    let slots = SlotManager::new(1, 0, 1);
    let held = slots.try_local().unwrap();
    let waiting = tokio::spawn({
        let slots = slots.clone();
        async move { slots.wait_local(Duration::from_secs(30)).await.is_ok() }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(slots.snapshot().queued, 1);
    waiting.abort();
    let _ = waiting.await;
    assert_eq!(slots.snapshot().queued, 0);
    drop(held);
    assert_eq!(slots.snapshot().local_busy, 0);
}

#[tokio::test]
async fn resizing_drains_held_slots_and_grows_into_waiters() {
    let slots = SlotManager::new(2, 0, 2);
    let a = slots.try_local().unwrap();
    let _b = slots.try_local().unwrap();
    slots.resize(1, 0, 2);
    assert!(slots.try_local().is_none());
    drop(a);
    assert_eq!(slots.snapshot().local_busy, 1);
    assert!(slots.try_local().is_none(), "still at the reduced total");
    let waiter = tokio::spawn({
        let slots = slots.clone();
        async move { slots.wait_local(Duration::from_secs(5)).await.is_ok() }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    slots.resize(2, 0, 2);
    assert!(waiter.await.unwrap());
}

#[tokio::test]
async fn a_permit_is_held_until_the_response_body_finishes() {
    let slots = SlotManager::new(1, 0, 0);
    let (sender, receiver) =
        tokio::sync::mpsc::channel::<std::result::Result<Bytes, std::io::Error>>(2);
    let stream = async_stream::stream! {
        let mut receiver = receiver;
        while let Some(item) = receiver.recv().await {
            yield item;
        }
    };
    let response = hold(Response::new(Body::from_stream(stream)), slots.try_local());
    sender.send(Ok(Bytes::from_static(b"part"))).await.unwrap();
    assert_eq!(slots.snapshot().local_busy, 1);
    drop(sender);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&body[..], b"part");
    assert_eq!(slots.snapshot().local_busy, 0);

    let early = hold(Response::new(Body::empty()), slots.try_local());
    assert_eq!(slots.snapshot().local_busy, 1);
    drop(early);
    assert_eq!(slots.snapshot().local_busy, 0, "client disconnect releases");
}

fn auth_config(sender_key: &NodeKey) -> Config {
    let mut config = Config::default();
    config.sharing.mode = SharingMode::Peers;
    config.peers.insert(
        "a".into(),
        PeerConfig {
            url: "http://127.0.0.1:1".into(),
            public_key: sender_key.public_key().to_string(),
            ..PeerConfig::default()
        },
    );
    config
}

#[test]
fn signed_requests_verify_and_every_tamper_fails() {
    let sender = key_for(&temp_key_path("a"));
    let other = key_for(&temp_key_path("x"));
    let receiver_path = temp_key_path("b");
    let receiver = key_for(&receiver_path);
    let mut config = auth_config(&sender);
    config.sharing.private_key_file = Some(receiver_path);
    let runtime = SharingRuntime::new(Arc::new(config.clone())).unwrap();
    let body = br#"{"model":"m"}"#;
    let path = "/v1/messages";
    let sign = || sign_request(&sender, receiver.public_key(), &Method::POST, path, body);
    let verify = |headers: &HeaderMap, path: &str, body: &[u8]| {
        runtime.verify(
            &config,
            &Method::POST,
            path,
            headers,
            body,
            AuthPurpose::Job,
        )
    };

    let headers = sign();
    assert_eq!(verify(&headers, path, body), Ok("a".to_string()));
    assert_eq!(verify(&headers, path, body), Err("replayed nonce"));
    assert_eq!(
        verify(&sign(), path, br#"{"model":"n"}"#),
        Err("body digest mismatch")
    );
    assert_eq!(
        verify(&sign(), "/v1/chat/completions", body),
        Err("signature verification failed")
    );
    let to_c = sign_request(&sender, other.public_key(), &Method::POST, path, body);
    assert_eq!(
        verify(&to_c, path, body),
        Err("signature verification failed"),
        "a request signed for another node cannot be replayed here"
    );
    let mut forged = sign_request(&other, receiver.public_key(), &Method::POST, path, body);
    assert_eq!(verify(&forged, path, body), Err("unknown peer"));
    forged.insert(
        HeaderName::from_static(PEER_HEADER),
        HeaderValue::from_str(sender.public_key()).unwrap(),
    );
    assert_eq!(
        verify(&forged, path, body),
        Err("signature verification failed")
    );

    let mut stale = sign();
    stale.insert(
        HeaderName::from_static(TIMESTAMP_HEADER),
        HeaderValue::from((unix_now() - 3600) as i64),
    );
    assert_eq!(
        verify(&stale, path, body),
        Err("timestamp outside the accepted window")
    );

    let mut disabled = config.clone();
    disabled.peers.get_mut("a").unwrap().accept_incoming = false;
    assert_eq!(
        runtime.verify(
            &disabled,
            &Method::POST,
            path,
            &sign(),
            body,
            AuthPurpose::Job
        ),
        Err("peer not accepted for inbound jobs")
    );
    assert!(
        runtime
            .verify(
                &disabled,
                &Method::POST,
                path,
                &sign(),
                body,
                AuthPurpose::Status
            )
            .is_ok(),
        "status stays readable for an outbound-only registration"
    );
}

#[test]
fn listen_guard_admits_loopback_and_listed_networks_only() {
    let mut config = Config::default();
    config.listen_guard.allowed_sources = vec!["192.168.0.0/16".into(), "10.8.0.7".into()];
    let runtime = SharingRuntime::new(Arc::new(config)).unwrap();
    for allowed in [
        "127.0.0.1",
        "::1",
        "192.168.4.2",
        "10.8.0.7",
        "::ffff:192.168.1.1",
    ] {
        assert!(
            runtime.source_allowed(allowed.parse().unwrap()),
            "{allowed}"
        );
    }
    for denied in ["10.8.0.8", "8.8.8.8", "::ffff:8.8.8.8"] {
        assert!(!runtime.source_allowed(denied.parse().unwrap()), "{denied}");
    }
    let defaults = SharingRuntime::new(Arc::new(Config::default())).unwrap();
    assert!(defaults.source_allowed("192.168.1.9".parse().unwrap()));
    assert!(!defaults.source_allowed("10.0.0.1".parse().unwrap()));
    let mut config = Config::default();
    config.listen_guard.allowed_sources.clear();
    let open = SharingRuntime::new(Arc::new(config)).unwrap();
    assert!(open.source_allowed("8.8.8.8".parse().unwrap()));
}

#[test]
fn validation_rejects_unsafe_sharing_configs() {
    let key = key_for(&temp_key_path("v"));
    let base = |node: &str, local: &str, extra: &str| {
        format!(
            r#"
[upstream]
base_url = "http://127.0.0.1:9/v1"
[providers.cloud]
kind = "openai"
base_url = "https://api.example.com/v1"
[sharing]
mode = "peers"
node_name = "{node}"
onprem_provider = "{local}"
{extra}
[peers.b]
url = "http://10.8.0.2:4040"
public_key = "{}"
"#,
            key.public_key()
        )
    };
    let check = |node: &str, local: &str, extra: &str| {
        let config = parse_config(&base(node, local, extra), "test").unwrap();
        crate::validate_config(&config).map_err(|error| error.to_string())
    };
    assert!(check("a", "default", "").is_ok());
    assert!(check("a", "cloud", "").unwrap_err().contains("private"));
    assert!(
        check("a", "default", "cloud_overflow = \"cloud\"")
            .unwrap_err()
            .contains("provider/model")
    );
    assert!(
        check("a", "default", "cloud_overflow = \"cloud/gpt\"")
            .unwrap_err()
            .contains("proxy-owned credential")
    );
    let keyed = base("a", "default", "cloud_overflow = \"cloud/gpt\"").replace(
        "kind = \"openai\"",
        "kind = \"openai\"\napi_key = \"test-key\"",
    );
    assert!(crate::validate_config(&parse_config(&keyed, "test").unwrap()).is_ok());
    let tailscale = base("a", "default", "").replace("127.0.0.1:9", "100.101.102.103:8000");
    assert!(
        crate::validate_config(&parse_config(&tailscale, "test").unwrap()).is_ok(),
        "a local model on a VPN overlay address is still on-prem"
    );
    let prefixed = base("a", "default", "").replace("10.8.0.2:4040\"", "10.8.0.2:4040/llm\"");
    assert!(
        crate::validate_config(&parse_config(&prefixed, "test").unwrap())
            .unwrap_err()
            .to_string()
            .contains("without a path")
    );
    let bad_key = base("a", "default", "").replace(key.public_key(), "ed25519:AAAA");
    assert!(crate::validate_config(&parse_config(&bad_key, "test").unwrap()).is_err());
}

#[test]
fn a_pre_rename_cluster_config_loads_as_sharing() {
    let config = parse_config(
        r#"
[upstream]
base_url = "http://127.0.0.1:9/v1"
[cluster]
enable = true
node_name = "home"
listen_address = "0.0.0.0:4040"
local_slots = 3
remote_slots = 1
commercial_route = "cloud/m"
[cluster.tiers]
local = true
peer = false
commercial = false
[peers.office]
url = "http://10.8.0.2:4040"
public_key = "k"
accept_inbound = false
"#,
        "test",
    )
    .unwrap();
    let sharing = &config.sharing;
    assert!(sharing.with_peers());
    assert_eq!(
        (
            sharing.local_limit,
            sharing.incoming_limit,
            sharing.send_outgoing
        ),
        (3, 1, false)
    );
    assert_eq!(sharing.peer_listen.as_deref(), Some("0.0.0.0:4040"));
    assert_eq!(sharing.onprem_provider, "default");
    assert_eq!(sharing.cloud_overflow, None, "commercial tier was off");
    assert!(!config.peers["office"].accept_incoming);
}

#[derive(Clone)]
struct Mock {
    name: &'static str,
    hits: Arc<AtomicU64>,
}

async fn mock_upstream(name: &'static str) -> (String, Arc<AtomicU64>) {
    let hits = Arc::new(AtomicU64::new(0));
    let mock = Mock {
        name,
        hits: hits.clone(),
    };
    let app = Router::new()
        .route(
            "/v1/models",
            get(|State(mock): State<Mock>| async move {
                Json(json!({"object": "list", "data": [{"id": format!("{}-model", mock.name)}]}))
            }),
        )
        .route(
            "/v1/chat/completions",
            post(|State(mock): State<Mock>, headers: HeaderMap, Json(body): Json<Value>| async move {
                mock.hits.fetch_add(1, Ordering::SeqCst);
                if headers.contains_key("authorization") || headers.contains_key("x-api-key") {
                    mock.hits.fetch_add(1000, Ordering::SeqCst);
                }
                if body["stream"] == true {
                    let text = format!("from-{}", mock.name);
                    let chunk = |delta: Value, finish: Value| {
                        format!(
                            "data: {}\n\n",
                            json!({"id": "c1", "object": "chat.completion.chunk", "created": 1,
                                "model": body["model"],
                                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
                        )
                    };
                    let sse = format!(
                        "{}{}data: [DONE]\n\n",
                        chunk(json!({"role": "assistant", "content": text}), Value::Null),
                        chunk(json!({}), json!("stop"))
                    );
                    return ([(CONTENT_TYPE, "text/event-stream")], sse).into_response();
                }
                Json(json!({
                    "id": "chatcmpl-1",
                    "object": "chat.completion",
                    "created": 1,
                    "model": body["model"],
                    "choices": [{"index": 0, "finish_reason": "stop",
                        "message": {"role": "assistant", "content": format!("from-{}", mock.name)}}],
                    "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
                }))
                .into_response()
            }),
        )
        .with_state(mock);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/v1"), hits)
}

struct Node {
    shared: SharedState,
    runtime: Arc<SharingRuntime>,
    proxy: String,
    peer: Option<SocketAddr>,
}

async fn start_node(toml: &str) -> Node {
    let config = parse_config(toml, "test").unwrap();
    crate::validate_config(&config).unwrap();
    let config = Arc::new(config);
    let providers = effective_provider_configs(&config)
        .unwrap()
        .into_iter()
        .map(|(name, upstream)| {
            let provider = build_provider_state(name.clone(), upstream).unwrap();
            (name, provider)
        })
        .collect();
    let runtime = SharingRuntime::new(config.clone()).unwrap();
    let shared = SharedState::new(AppState {
        config,
        providers: Arc::new(providers),
        launcher_kind: None,
        launcher_mode: None,
        forward_caller_credentials: true,
        sharing: Some(runtime.clone()),
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = build_app(shared.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let peer = spawn_peer_listener(shared.clone()).await.unwrap();
    Node {
        shared,
        runtime,
        proxy: format!("http://{addr}"),
        peer,
    }
}

impl Node {
    async fn poll(&self, peer: &str) {
        let config = self.runtime.latest();
        let key = self.runtime.key().unwrap();
        poll_peer(&self.runtime, &config, &key, peer, &config.peers[peer]).await;
    }

    async fn chat(&self) -> (StatusCode, Value, HeaderMap) {
        let response = reqwest::Client::new()
            .post(format!("{}/v1/chat/completions", self.proxy))
            .header("authorization", "Bearer caller-secret")
            .json(&json!({"model": "anything", "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await
            .unwrap();
        let status = StatusCode::from_u16(response.status().as_u16()).unwrap();
        let headers = response.headers().clone();
        (
            status,
            response.json().await.unwrap_or(Value::Null),
            headers,
        )
    }
}

fn node_toml(
    name: &str,
    key: &Path,
    upstream: &str,
    sharing: &str,
    peers: &[(&str, &str, &NodeKey, &str)],
) -> String {
    let mut toml = format!(
        r#"
[upstream]
base_url = "{upstream}"
[sharing]
mode = "peers"
node_name = "{name}"
peer_listen = "127.0.0.1:0"
private_key_file = "{}"
onprem_provider = "default"
status_stale_after_secs = 60
{sharing}
"#,
        key.display()
    );
    for (peer, url, peer_key, extra) in peers {
        toml.push_str(&format!(
            "[peers.{peer}]\nurl = \"{url}\"\npublic_key = \"{}\"\n{extra}\n",
            peer_key.public_key()
        ));
    }
    toml
}

fn content(value: &Value) -> &str {
    value["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
}

#[tokio::test]
async fn saturated_node_offloads_to_a_peer_that_runs_it_on_its_own_local_model() {
    let (a_key_path, b_key_path) = (temp_key_path("a"), temp_key_path("b"));
    let (a_key, b_key) = (key_for(&a_key_path), key_for(&b_key_path));
    let (a_upstream, a_hits) = mock_upstream("a").await;
    let (b_upstream, b_hits) = mock_upstream("b").await;

    let b = start_node(&node_toml(
        "b",
        &b_key_path,
        &b_upstream,
        "local_limit = 1\nincoming_limit = 1",
        &[("a", "http://127.0.0.1:1", &a_key, "send_outgoing = false")],
    ))
    .await;
    let b_url = format!("http://{}", b.peer.unwrap());
    let a = start_node(&node_toml(
        "a",
        &a_key_path,
        &a_upstream,
        "local_limit = 1\nincoming_limit = 0\nqueue_depth = 0",
        &[("b", &b_url, &b_key, "accept_incoming = false")],
    ))
    .await;
    assert!(
        a.peer.is_some(),
        "an enabled peer opens the listener for status"
    );

    let (status, body, _) = a.chat().await;
    assert_eq!((status, content(&body)), (StatusCode::OK, "from-a"));

    a.poll("b").await;
    let held = a.runtime.slots.try_local().unwrap();
    let (status, body, _) = a.chat().await;
    assert_eq!((status, content(&body)), (StatusCode::OK, "from-b"));
    assert_eq!(
        body["model"], "b-model",
        "unknown model maps to the peer's first model"
    );
    assert_eq!(
        (a_hits.load(Ordering::SeqCst), b_hits.load(Ordering::SeqCst)),
        (1, 1)
    );
    assert_eq!(
        b.runtime.slots.snapshot().incoming_busy,
        0,
        "peer slot released"
    );

    // With the peer's only incoming slot busy, the peer refuses before running
    // anything and this node falls through to a fast local rejection.
    let peer_busy = b.runtime.slots.try_incoming().unwrap();
    a.poll("b").await;
    let (status, body, headers) = a.chat().await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["error"]["type"], "rate_limit_error");
    assert!(headers.contains_key("retry-after"));
    assert_eq!(b_hits.load(Ordering::SeqCst), 1);
    drop((held, peer_busy));
    let _ = &b.shared;
}

#[tokio::test]
async fn peers_are_identified_by_key_whatever_their_label() {
    let (a_key_path, b_key_path) = (temp_key_path("a"), temp_key_path("b"));
    let (a_key, b_key) = (key_for(&a_key_path), key_for(&b_key_path));
    let (a_upstream, _) = mock_upstream("a").await;
    let (b_upstream, _) = mock_upstream("b").await;
    let b = start_node(&node_toml(
        "b",
        &b_key_path,
        &b_upstream,
        "local_limit = 1\nincoming_limit = 1",
        &[(
            "office",
            "http://127.0.0.1:1",
            &a_key,
            "send_outgoing = false",
        )],
    ))
    .await;
    let b_url = format!("http://{}", b.peer.unwrap());
    let a = start_node(&node_toml(
        "a",
        &a_key_path,
        &a_upstream,
        "local_limit = 1\nincoming_limit = 0\nqueue_depth = 0",
        &[("home", &b_url, &b_key, "accept_incoming = false")],
    ))
    .await;
    a.poll("home").await;
    let _held = a.runtime.slots.try_local().unwrap();
    let (status, body, _) = a.chat().await;
    assert_eq!((status, content(&body)), (StatusCode::OK, "from-b"));
    let _ = &b.shared;
}

#[tokio::test]
async fn without_a_local_limit_requests_stay_local_and_cloud_follows_the_mode() {
    let (a_key_path, b_key_path) = (temp_key_path("a"), temp_key_path("b"));
    let (a_key, b_key) = (key_for(&a_key_path), key_for(&b_key_path));
    let (a_upstream, a_hits) = mock_upstream("a").await;
    let (b_upstream, b_hits) = mock_upstream("b").await;
    let (cloud, cloud_hits) = mock_upstream("cloud").await;
    let b = start_node(&node_toml(
        "b",
        &b_key_path,
        &b_upstream,
        "incoming_limit = 1",
        &[("a", "http://127.0.0.1:1", &a_key, "send_outgoing = false")],
    ))
    .await;
    let b_url = format!("http://{}", b.peer.unwrap());
    let toml = format!(
        "{}\n[providers.cloud]\nbase_url = \"{cloud}\"\n",
        node_toml(
            "a",
            &a_key_path,
            &a_upstream,
            "queue_depth = 0\ncloud_overflow = \"cloud/m\"\ncloud_overflow_modes = { proxy = false }",
            &[("b", &b_url, &b_key, "accept_incoming = false")],
        ),
    );
    let a = start_node(&toml).await;
    a.poll("b").await;
    for _ in 0..3 {
        let (status, body, _) = a.chat().await;
        assert_eq!((status, content(&body)), (StatusCode::OK, "from-a"));
    }
    assert_eq!(a_hits.load(Ordering::SeqCst), 3);
    assert_eq!(b_hits.load(Ordering::SeqCst), 0, "no limit: never full");
    assert_eq!(cloud_hits.load(Ordering::SeqCst), 0);
    assert_eq!(
        a.shared
            .snapshot()
            .config
            .sharing
            .cloud_overflow(crate::RoutingMode::Proxy),
        None,
        "cloud overflow is off in proxy mode"
    );
    assert_eq!(
        a.shared
            .snapshot()
            .config
            .sharing
            .cloud_overflow(crate::RoutingMode::Integrated),
        Some("cloud/m")
    );
}

#[tokio::test]
async fn a_peer_is_tried_before_lending_an_incoming_slot() {
    let (a_key_path, b_key_path) = (temp_key_path("a"), temp_key_path("b"));
    let (a_key, b_key) = (key_for(&a_key_path), key_for(&b_key_path));
    let (a_upstream, a_hits) = mock_upstream("a").await;
    let (b_upstream, _) = mock_upstream("b").await;
    let b = start_node(&node_toml(
        "b",
        &b_key_path,
        &b_upstream,
        "local_limit = 1\nincoming_limit = 1",
        &[("a", "http://127.0.0.1:1", &a_key, "send_outgoing = false")],
    ))
    .await;
    let b_url = format!("http://{}", b.peer.unwrap());
    let a = start_node(&node_toml(
        "a",
        &a_key_path,
        &a_upstream,
        "local_limit = 1\nincoming_limit = 1\nqueue_depth = 0",
        &[("b", &b_url, &b_key, "accept_incoming = false")],
    ))
    .await;
    a.poll("b").await;
    let _held = a.runtime.slots.try_local().unwrap();
    let (status, body, _) = a.chat().await;
    assert_eq!((status, content(&body)), (StatusCode::OK, "from-b"));
    assert_eq!(a_hits.load(Ordering::SeqCst), 0, "incoming slot not lent");

    // Peer full: the request falls back to this node's lent incoming slot.
    let _peer_busy = b.runtime.slots.try_incoming().unwrap();
    a.poll("b").await;
    let (status, body, _) = a.chat().await;
    assert_eq!((status, content(&body)), (StatusCode::OK, "from-a"));
    let _ = &b.shared;
}

#[tokio::test]
async fn an_inbound_job_is_never_forwarded_to_another_peer() {
    let keys: Vec<PathBuf> = ["a", "b", "c"].iter().map(|n| temp_key_path(n)).collect();
    let [a_key, b_key, c_key] = [0, 1, 2].map(|i| key_for(&keys[i]));
    let (a_upstream, _) = mock_upstream("a").await;
    let (b_upstream, b_hits) = mock_upstream("b").await;
    let (c_upstream, c_hits) = mock_upstream("c").await;

    let c = start_node(&node_toml(
        "c",
        &keys[2],
        &c_upstream,
        "local_limit = 1\nincoming_limit = 4",
        &[("b", "http://127.0.0.1:1", &b_key, "send_outgoing = false")],
    ))
    .await;
    let c_url = format!("http://{}", c.peer.unwrap());
    let b = start_node(&node_toml(
        "b",
        &keys[1],
        &b_upstream,
        "local_limit = 1\nincoming_limit = 1\nqueue_depth = 0",
        &[
            ("a", "http://127.0.0.1:1", &a_key, "send_outgoing = false"),
            ("c", &c_url, &c_key, "accept_incoming = false"),
        ],
    ))
    .await;
    let b_url = format!("http://{}", b.peer.unwrap());
    let a = start_node(&node_toml(
        "a",
        &keys[0],
        &a_upstream,
        "local_limit = 1\nincoming_limit = 0\nqueue_depth = 0",
        &[("b", &b_url, &b_key, "accept_incoming = false")],
    ))
    .await;
    b.poll("c").await;
    a.poll("b").await;

    // B's own local slots are saturated: the inbound job still runs on B's
    // incoming slot and B's local model, never on C.
    let _a_local = a.runtime.slots.try_local().unwrap();
    let _b_local = b.runtime.slots.try_local().unwrap();
    let (status, body, _) = a.chat().await;
    assert_eq!((status, content(&body)), (StatusCode::OK, "from-b"));

    // B's incoming slot is busy and C has room: B rejects rather than forwarding.
    let _b_incoming = b.runtime.slots.try_incoming().unwrap();
    a.poll("b").await;
    let (status, _, _) = a.chat().await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(b_hits.load(Ordering::SeqCst), 1);
    assert_eq!(c_hits.load(Ordering::SeqCst), 0, "single hop, always");
    let _ = (&c.shared, &b.shared);
}

#[tokio::test]
async fn overflow_uses_the_cloud_route_then_a_bounded_wait() {
    let key_path = temp_key_path("solo");
    let (local, local_hits) = mock_upstream("local").await;
    let (cloud, cloud_hits) = mock_upstream("cloud").await;
    let toml = format!(
        "{}\n[providers.cloud]\nbase_url = \"{cloud}\"\n",
        node_toml(
            "solo",
            &key_path,
            &local,
            "local_limit = 1\nincoming_limit = 0\nqueue_depth = 1\nqueue_wait_ms = 3000\ncloud_overflow = \"cloud/cloud-model\"",
            &[],
        )
    );
    let node = start_node(&toml).await;
    let held = node.runtime.slots.try_local().unwrap();
    let (status, body, _) = node.chat().await;
    assert_eq!((status, content(&body)), (StatusCode::OK, "from-cloud"));
    assert_eq!(body["model"], "cloud-model");

    // Cloud overflow off: the request waits for the held slot instead.
    let mut config = (*node.shared.snapshot().config).clone();
    config.sharing.cloud_overflow = None;
    let config = Arc::new(config);
    node.runtime.reconfigure(config.clone()).unwrap();
    let mut state = node.shared.snapshot();
    state.config = config;
    node.shared.replace(state);
    let waiting = tokio::spawn({
        let proxy = node.proxy.clone();
        async move {
            reqwest::Client::new()
                .post(format!("{proxy}/v1/chat/completions"))
                .json(&json!({"model": "x", "messages": [{"role": "user", "content": "hi"}]}))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(node.runtime.slots.snapshot().queued, 1);
    drop(held);
    assert_eq!(content(&waiting.await.unwrap()), "from-local");
    assert_eq!(
        (
            local_hits.load(Ordering::SeqCst),
            cloud_hits.load(Ordering::SeqCst)
        ),
        (1, 1)
    );
}

#[tokio::test]
async fn the_peer_port_rejects_unsigned_and_unknown_callers_identically() {
    let (a_key_path, b_key_path) = (temp_key_path("a"), temp_key_path("b"));
    let a_key = key_for(&a_key_path);
    let (b_upstream, b_hits) = mock_upstream("b").await;
    let b = start_node(&node_toml(
        "b",
        &b_key_path,
        &b_upstream,
        "",
        &[("a", "http://127.0.0.1:1", &a_key, "")],
    ))
    .await;
    let url = format!("http://{}/v1/messages", b.peer.unwrap());
    let client = reqwest::Client::new();
    let body = br#"{"model":"m","max_tokens":5,"messages":[]}"#.to_vec();
    let unsigned = client.post(&url).body(body.clone()).send().await.unwrap();
    let stranger = key_for(&temp_key_path("s"));
    let forged = client
        .post(&url)
        .headers(sign_request(
            &stranger,
            key_for(&b_key_path).public_key(),
            &Method::POST,
            "/v1/messages",
            &body,
        ))
        .body(body)
        .send()
        .await
        .unwrap();
    for response in [unsigned, forged] {
        assert_eq!(response.status().as_u16(), 401);
        assert_eq!(response.headers()[REJECT_HEADER], "auth");
        assert_eq!(
            response.text().await.unwrap(),
            r#"{"error":{"message":"unauthorized","type":"authentication_error"}}"#
        );
    }
    let other = client
        .get(format!("http://{}/v1/models", b.peer.unwrap()))
        .send()
        .await
        .unwrap();
    assert_eq!(
        other.status().as_u16(),
        404,
        "only job and status routes exist"
    );
    assert_eq!(b_hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_streamed_anthropic_job_crosses_nodes_as_native_sse() {
    let (a_key_path, b_key_path) = (temp_key_path("a"), temp_key_path("b"));
    let (a_key, b_key) = (key_for(&a_key_path), key_for(&b_key_path));
    let (a_upstream, _) = mock_upstream("a").await;
    let (b_upstream, b_hits) = mock_upstream("b").await;
    let b = start_node(&node_toml(
        "b",
        &b_key_path,
        &b_upstream,
        "incoming_limit = 1",
        &[("a", "http://127.0.0.1:1", &a_key, "send_outgoing = false")],
    ))
    .await;
    let a = start_node(&node_toml(
        "a",
        &a_key_path,
        &a_upstream,
        "local_limit = 1\nincoming_limit = 0\nqueue_depth = 0",
        &[(
            "b",
            &format!("http://{}", b.peer.unwrap()),
            &b_key,
            "accept_incoming = false",
        )],
    ))
    .await;
    a.poll("b").await;
    let _held = a.runtime.slots.try_local().unwrap();
    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", a.proxy))
        .header("x-api-key", "caller-secret")
        .header("anthropic-version", "2023-06-01")
        .json(
            &json!({"model": "claude-x", "max_tokens": 16, "stream": true,
            "messages": [{"role": "user", "content": "hi"}]}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert!(
        response.headers()[CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let text = response.text().await.unwrap();
    assert!(text.contains("event: message_start"), "{text}");
    assert!(text.contains("from-b"), "{text}");
    assert!(text.contains("event: message_stop"), "{text}");
    assert_eq!(
        b_hits.load(Ordering::SeqCst),
        1,
        "served once, and no caller credential reached the peer's provider"
    );
    assert_eq!(b.runtime.slots.snapshot().incoming_busy, 0);
}
