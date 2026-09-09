use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use axum::Json;
use axum::Router;
use axum::routing::get;
use serde_json::json;
use tokio::net::TcpListener;

use super::{
    LauncherKind, LauncherMode, LauncherSession, has_model_arg, probe_existing_llmproxy,
    resolve_windows_executable_with, windows_launch_extensions, with_integrated_auto_model,
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

#[test]
fn windows_executable_resolution_finds_cmd_shims_and_prefers_pathext_order() {
    let search_dirs = [PathBuf::from("tools"), PathBuf::from("npm")];
    let extensions = windows_launch_extensions(Some(OsStr::new(".EXE;.CMD;.PS1")));
    let resolved = resolve_windows_executable_with(
        Path::new("codex"),
        &search_dirs,
        &extensions,
        |candidate| candidate == Path::new("npm/codex.CMD"),
    )
    .unwrap();
    assert_eq!(resolved, Path::new("npm/codex.CMD"));

    let resolved = resolve_windows_executable_with(
        Path::new("codex.cmd"),
        &search_dirs,
        &extensions,
        |candidate| candidate == Path::new("tools/codex.cmd"),
    )
    .unwrap();
    assert_eq!(resolved, Path::new("tools/codex.cmd"));
    assert!(!extensions.iter().any(|extension| extension == ".PS1"));
}

#[test]
fn windows_executable_resolution_honors_explicit_paths() {
    let extensions = windows_launch_extensions(None);
    let resolved = resolve_windows_executable_with(
        Path::new("custom/codex"),
        &[PathBuf::from("ignored")],
        &extensions,
        |candidate| candidate == Path::new("custom/codex.EXE"),
    )
    .unwrap();
    assert_eq!(resolved, Path::new("custom/codex.EXE"));

    assert!(
        resolve_windows_executable_with(
            Path::new("codex"),
            &[PathBuf::from("missing")],
            &extensions,
            |_| false,
        )
        .is_err()
    );
}

#[cfg(windows)]
#[test]
fn resolved_cmd_shim_can_be_spawned() {
    use std::fs;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "llmproxy-cmd-launch-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&directory).unwrap();
    let shim = directory.join("codex.cmd");
    fs::write(
        &shim,
        "@echo off\r\nif \"%~1\"==\"argument with spaces\" exit /b 0\r\nexit /b 17\r\n",
    )
    .unwrap();

    let resolved = resolve_windows_executable_with(
        Path::new("codex"),
        std::slice::from_ref(&directory),
        &windows_launch_extensions(None),
        Path::is_file,
    )
    .unwrap();
    let status = Command::new(resolved)
        .arg("argument with spaces")
        .status()
        .unwrap();
    let _ = fs::remove_dir_all(directory);
    assert!(status.success());
}
