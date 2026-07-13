use std::ffi::OsString;

use super::{
    ClaudeAuthStatus, claude_launch_args, claude_session_exists, normalize_anthropic_base_url,
    validate_auth_status,
};

#[test]
fn validates_first_party_claude_auth_without_credentials() {
    let status = ClaudeAuthStatus {
        logged_in: true,
        auth_method: "claude.ai".to_string(),
        api_provider: "firstParty".to_string(),
    };
    assert!(validate_auth_status(&status).is_ok());

    let third_party = ClaudeAuthStatus {
        api_provider: "bedrock".to_string(),
        ..status
    };
    assert!(validate_auth_status(&third_party).is_err());
}

#[test]
fn normalizes_anthropic_api_roots() {
    assert_eq!(
        normalize_anthropic_base_url("https://api.anthropic.com").unwrap(),
        "https://api.anthropic.com/v1"
    );
    assert_eq!(
        normalize_anthropic_base_url("https://gateway.example/anthropic/v1/").unwrap(),
        "https://gateway.example/anthropic/v1"
    );
    assert!(normalize_anthropic_base_url("  ").is_err());
}

#[test]
fn claude_auto_continues_unless_the_invocation_is_explicit() {
    assert_eq!(
        claude_launch_args(&[], true),
        ["--continue"].map(OsString::from)
    );
    assert_eq!(
        claude_launch_args(
            &[
                OsString::from("--model"),
                OsString::from("sonnet"),
                OsString::from("continue the work"),
            ],
            true
        ),
        ["--continue", "--model", "sonnet", "continue the work"].map(OsString::from)
    );
    assert_eq!(
        claude_launch_args(
            &[OsString::from("--model"), OsString::from("sonnet")],
            false
        ),
        ["--model", "sonnet"].map(OsString::from)
    );

    for explicit in [
        vec!["--resume", "session-id"],
        vec!["--resume=session-id"],
        vec!["-c"],
        vec!["auth", "status"],
        vec!["--version"],
    ] {
        let explicit = explicit.into_iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(claude_launch_args(&explicit, false), explicit);
    }
}

#[test]
fn detects_a_claude_session_for_the_current_project() {
    let root = std::env::temp_dir().join(format!(
        "llmproxy-claude-session-test-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let cwd = root.join("work dir");
    let project = root.join("config").join("projects").join(
        cwd.to_string_lossy()
            .replace(|character: char| !character.is_ascii_alphanumeric(), "-"),
    );
    std::fs::create_dir_all(&project).unwrap();
    assert!(!claude_session_exists(&root.join("config"), &cwd));
    std::fs::write(project.join("session.jsonl"), "{}\n").unwrap();
    assert!(claude_session_exists(&root.join("config"), &cwd));
    std::fs::remove_dir_all(root).unwrap();
}
