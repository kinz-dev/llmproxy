use std::ffi::OsString;

use axum::Json;
use axum::Router;
use axum::routing::get;
use serde_json::json;
use tokio::net::TcpListener;

use super::{
    LauncherKind, LauncherMode, LauncherSession, has_model_arg, probe_existing_llmproxy,
    with_integrated_auto_model,
};
use crate::INTEGRATED_AUTO_MODEL;

#[test]
fn integrated_launchers_default_to_auto_without_overriding_explicit_models() {
    let resumed = ["resume", "--last"].map(OsString::from);
    assert_eq!(
        with_integrated_auto_model(&resumed, LauncherMode::Integrated),
        ["--model", INTEGRATED_AUTO_MODEL, "resume", "--last"].map(OsString::from)
    );
    assert_eq!(
        with_integrated_auto_model(&resumed, LauncherMode::Proxy),
        resumed
    );

    for args in [
        vec!["--model", "provider/model"],
        vec!["--model=provider/model"],
        vec!["-m", "provider/model"],
        vec!["-m=provider/model"],
    ] {
        let args = args.into_iter().map(OsString::from).collect::<Vec<_>>();
        assert!(has_model_arg(&args));
        assert_eq!(
            with_integrated_auto_model(&args, LauncherMode::Integrated),
            args
        );
    }
}

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
fn launcher_sessions_round_trip_through_url_safe_tokens() {
    let session = LauncherSession::new(
        LauncherKind::Codex,
        "https://chatgpt.com/backend-api/codex".to_string(),
    )
    .unwrap();
    let token = session.encode().unwrap();
    let decoded = LauncherSession::decode(&token).unwrap();
    assert_eq!(decoded, session);

    for mode in [LauncherMode::Proxy, LauncherMode::Integrated] {
        let session = LauncherSession::for_launcher(LauncherKind::Codex, mode, "ignored").unwrap();
        let decoded = LauncherSession::decode(&session.encode().unwrap()).unwrap();
        assert_eq!(decoded.mode(), mode);
        assert_eq!(decoded.upstream_base_url(), "");
    }
}
