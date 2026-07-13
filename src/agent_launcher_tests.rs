use axum::Json;
use axum::Router;
use axum::routing::get;
use serde_json::json;
use tokio::net::TcpListener;

use super::{LauncherKind, LauncherSession, probe_existing_llmproxy};

#[tokio::test]
async fn detects_an_existing_llmproxy_health_endpoint() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/health", get(|| async { Json(json!({ "status": "ok" })) })),
        )
        .await
        .unwrap();
    });

    assert!(probe_existing_llmproxy(addr).await);
    server.abort();
}

#[test]
fn launcher_session_round_trips_through_url_safe_token() {
    let session = LauncherSession::new(
        LauncherKind::Codex,
        "https://chatgpt.com/backend-api/codex".to_string(),
    )
    .unwrap();
    let token = session.encode().unwrap();
    let decoded = LauncherSession::decode(&token).unwrap();
    assert_eq!(decoded, session);
}
