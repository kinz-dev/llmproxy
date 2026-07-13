use std::ffi::OsString;

use super::{
    copilot_api_url_for_github_host, copilot_launch_args, copilot_session_exists,
    normalize_copilot_base_url,
};

#[test]
fn normalizes_native_copilot_api_roots() {
    assert_eq!(
        normalize_copilot_base_url("https://api.githubcopilot.com/v1/").unwrap(),
        "https://api.githubcopilot.com"
    );
    assert_eq!(
        normalize_copilot_base_url("https://gateway.example/copilot/").unwrap(),
        "https://gateway.example/copilot"
    );
    assert!(normalize_copilot_base_url("file:///tmp/copilot").is_err());
    assert!(normalize_copilot_base_url("  ").is_err());
}

#[test]
fn derives_dotcom_and_ghe_cloud_copilot_api_roots() {
    assert_eq!(
        copilot_api_url_for_github_host("github.com").unwrap(),
        "https://api.githubcopilot.com"
    );
    assert_eq!(
        copilot_api_url_for_github_host("https://octo.ghe.com/").unwrap(),
        "https://copilot-api.octo.ghe.com"
    );
    assert!(copilot_api_url_for_github_host("github.internal.example").is_err());
}

#[test]
fn copilot_auto_continues_only_when_a_session_exists() {
    assert_eq!(
        copilot_launch_args(&[], true),
        ["--continue"].map(OsString::from)
    );
    assert_eq!(
        copilot_launch_args(
            &[OsString::from("--model"), OsString::from("gpt-5.4")],
            true
        ),
        ["--continue", "--model", "gpt-5.4"].map(OsString::from)
    );
    assert!(copilot_launch_args(&[], false).is_empty());

    for explicit in [
        vec!["--resume", "session-id"],
        vec!["--prompt", "do work"],
        vec!["login"],
        vec!["--version"],
    ] {
        let explicit = explicit.into_iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(copilot_launch_args(&explicit, true), explicit);
    }
}

#[test]
fn detects_persisted_copilot_sessions_without_reading_them() {
    let root = std::env::temp_dir().join(format!(
        "llmproxy-copilot-session-test-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    assert!(!copilot_session_exists(&root));
    std::fs::create_dir_all(root.join("session-state").join("session-id")).unwrap();
    assert!(copilot_session_exists(&root));
    std::fs::remove_dir_all(root).unwrap();
}
