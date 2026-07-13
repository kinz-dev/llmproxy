use std::ffi::OsString;

use super::{
    CodexAuthRoute, classify_login_status, codex_enables_api_key_env, codex_launch_args,
    codex_provider_override, environment_auth_route, openai_base_url_from_config, selected_profile,
    validate_profile_name,
};

#[test]
fn classifies_codex_login_without_reading_credentials() {
    assert_eq!(
        classify_login_status("Logged in using an API key - sk-...1234").unwrap(),
        CodexAuthRoute::ApiKeyManaged
    );
    assert_eq!(
        classify_login_status("Logged in using ChatGPT").unwrap(),
        CodexAuthRoute::CodexBackend
    );
    assert_eq!(
        classify_login_status("Logged in using personal access token").unwrap(),
        CodexAuthRoute::CodexBackend
    );
    assert!(classify_login_status("Logged in using Amazon Bedrock API key").is_err());
}

#[test]
fn finds_profile_in_forwarded_codex_arguments() {
    assert_eq!(
        selected_profile(&[
            OsString::from("exec"),
            OsString::from("--profile"),
            OsString::from("work"),
        ]),
        Some("work".to_string())
    );
    assert_eq!(
        selected_profile(&[OsString::from("--profile=personal")]),
        Some("personal".to_string())
    );
    assert!(validate_profile_name("work").is_ok());
    assert!(validate_profile_name("../auth.json").is_err());
}

#[test]
fn codex_auto_resumes_unless_the_invocation_is_explicit() {
    assert_eq!(
        codex_launch_args(&[]),
        ["resume", "--last"].map(OsString::from)
    );
    assert_eq!(
        codex_launch_args(&[
            OsString::from("--model"),
            OsString::from("gpt-test"),
            OsString::from("continue the work"),
        ]),
        [
            "resume",
            "--last",
            "--model",
            "gpt-test",
            "continue the work",
        ]
        .map(OsString::from)
    );

    for explicit in [
        vec!["resume", "session-id"],
        vec!["--model", "gpt-test", "exec", "prompt"],
        vec!["--version"],
    ] {
        let explicit = explicit.into_iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(codex_launch_args(&explicit), explicit);
    }
}

#[test]
fn codex_api_key_environment_matches_the_selected_surface() {
    for args in [
        vec!["exec", "prompt"],
        vec!["e", "prompt"],
        vec!["--model", "gpt-test", "review"],
    ] {
        let args = args.into_iter().map(OsString::from).collect::<Vec<_>>();
        assert!(codex_enables_api_key_env(&args));
    }

    for args in [
        vec!["resume", "--last"],
        vec!["fork", "session-id"],
        vec!["--model", "gpt-test"],
    ] {
        let args = args.into_iter().map(OsString::from).collect::<Vec<_>>();
        assert!(!codex_enables_api_key_env(&args));
    }
}

#[test]
fn environment_auth_route_matches_codex_auth_precedence() {
    let interactive = ["resume", "--last"].map(OsString::from);
    let exec = ["exec", "prompt"].map(OsString::from);

    assert_eq!(
        environment_auth_route(&interactive, |name| name == "OPENAI_API_KEY"),
        None
    );
    assert_eq!(
        environment_auth_route(&interactive, |name| name == "CODEX_API_KEY"),
        None
    );
    assert_eq!(
        environment_auth_route(&exec, |name| name == "CODEX_API_KEY"),
        Some(CodexAuthRoute::ApiKeyEnvironment)
    );
    assert_eq!(
        environment_auth_route(&interactive, |name| name == "CODEX_ACCESS_TOKEN"),
        Some(CodexAuthRoute::CodexBackend)
    );
}

#[test]
fn reads_openai_base_url_from_codex_config_shapes() {
    let root: toml::Value =
        toml::from_str(r#"openai_base_url = "https://root.example/v1""#).unwrap();
    assert_eq!(
        openai_base_url_from_config(&root),
        Some("https://root.example/v1")
    );

    let provider: toml::Value = toml::from_str(
        r#"
        [model_providers.openai]
        base_url = "https://provider.example/v1"
        "#,
    )
    .unwrap();
    assert_eq!(
        openai_base_url_from_config(&provider),
        Some("https://provider.example/v1")
    );
}

#[test]
fn provider_override_leaves_authentication_to_codex() {
    let provider = codex_provider_override("http://127.0.0.1:8080/v1");
    assert!(!provider.contains("env_key"));
    assert!(provider.contains("requires_openai_auth = true"));
}
