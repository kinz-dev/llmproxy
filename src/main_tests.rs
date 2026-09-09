use super::*;

#[test]
fn sensitive_screening_errors_do_not_retain_upstream_body_details() {
    let meta = UpstreamRequestMeta::new(
        "local",
        &HeaderMap::new(),
        Method::POST,
        "chat/completions",
        UpstreamResponseKind::Json,
        false,
    )
    .without_body_logging();
    let failure = UpstreamFailure::status_with_body(
        &meta,
        StatusCode::BAD_REQUEST,
        br#"{"error":{"message":"sensitive-value"}}"#,
    );
    assert!(!failure.message.contains("sensitive-value"));
    assert_eq!(
        failure.message,
        "upstream /chat/completions returned 400 Bad Request"
    );
}

#[test]
fn config_discovery_prefers_overrides_then_local_then_user() {
    let explicit = PathBuf::from("explicit.toml");
    let configured = PathBuf::from("configured.toml");
    let home = PathBuf::from("user-home");
    let local = PathBuf::from("config.toml");
    let user = home.join(".llmproxy").join("config.toml");

    assert_eq!(
        resolve_config_path(
            Some(explicit.clone()),
            Some(configured.clone()),
            Some(&home),
            &|_| true,
        ),
        Some(explicit)
    );
    assert_eq!(
        resolve_config_path(None, Some(configured.clone()), Some(&home), &|_| true,),
        Some(configured)
    );
    assert_eq!(
        resolve_config_path(None, None, Some(&home), &|path| path == local),
        Some(local)
    );
    assert_eq!(
        resolve_config_path(None, None, Some(&home), &|path| path == user),
        Some(user)
    );
    assert_eq!(resolve_config_path(None, None, None, &|_| false), None);
}

#[test]
fn pid_files_reject_zero() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let path = env::temp_dir().join(format!("llmproxy-pid-test-{}-{unique}", std::process::id()));
    fs::write(&path, "0\n").expect("write pid file");
    assert!(read_pid_file(&path).is_err());
    fs::write(&path, "42\n").expect("replace pid file");
    assert_eq!(read_pid_file(&path).expect("read pid file"), Some(42));
    fs::remove_file(path).expect("remove pid file");
}

#[test]
fn auto_model_alias_is_reserved_for_integrated_routing() {
    let mut config = Config::default();
    config.model_aliases.insert(
        INTEGRATED_AUTO_MODEL.to_string(),
        ModelAlias::Model("upstream-model".to_string()),
    );
    assert!(
        validate_config(&config)
            .unwrap_err()
            .to_string()
            .contains("reserved")
    );
}

#[test]
fn startup_banner_shows_proxy_and_admin_destinations() {
    let info = RuntimeInfo {
        pid: 42,
        proxy_url: "http://127.0.0.1:8080/".to_string(),
        admin_url: Some("http://127.0.0.1:18080/".to_string()),
        integrated: false,
        routing_mode: Some(RoutingMode::Proxy),
        log_file: Some("llmproxy-20260719120000.log".to_string()),
    };
    let banner = startup_banner(&info, "started");
    assert!(banner.contains("llmproxy") || banner.contains("| | |_"));
    assert!(banner.contains("Status : started"));
    assert!(banner.contains("Proxy  : http://127.0.0.1:8080/"));
    assert!(banner.contains("Admin  : http://127.0.0.1:18080/"));
    assert!(banner.contains("Log    : ~/.llmproxy/llmproxy-20260719120000.log"));
}

#[test]
fn startup_banner_explains_when_admin_is_disabled() {
    let info = RuntimeInfo {
        pid: 42,
        proxy_url: "http://127.0.0.1:8080/".to_string(),
        admin_url: None,
        integrated: false,
        routing_mode: Some(RoutingMode::Proxy),
        log_file: None,
    };
    assert!(startup_banner(&info, "running").contains("enable = true"));
}

#[test]
fn legacy_runtime_metadata_requires_restart_when_mode_is_ambiguous() {
    let ambiguous = RuntimeInfo {
        pid: 42,
        proxy_url: "http://127.0.0.1:8080/".to_string(),
        admin_url: None,
        integrated: false,
        routing_mode: None,
        log_file: None,
    };
    assert_eq!(ambiguous.declared_routing_mode(), None);
    assert_eq!(ambiguous.routing_mode_label(), "legacy/unknown");
    assert!(startup_banner(&ambiguous, "running").contains("Mode   : legacy/unknown"));

    let integrated = RuntimeInfo {
        integrated: true,
        ..ambiguous
    };
    assert_eq!(
        integrated.declared_routing_mode(),
        Some(RoutingMode::Integrated)
    );
}

#[tokio::test]
async fn unknown_routes_return_a_diagnostic_404_body() {
    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/unknown?secret=not-logged")
        .body(Body::empty())
        .unwrap();
    let response = route_not_found(request).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let text = collect_response_text(response).await;
    assert!(text.contains("no llmproxy endpoint matches POST /v1/unknown"));
    assert!(!text.contains("secret"));
}

#[test]
fn configured_interceptor_providers_do_not_inherit_external_api_keys() {
    let mut config = Config {
        default_provider: "external".to_string(),
        providers: HashMap::from([
            ("external".to_string(), UpstreamConfig::default()),
            ("local".to_string(), UpstreamConfig::default()),
        ]),
        interceptor: InterceptorConfig {
            enabled: false,
            provider: "local".to_string(),
            model: "local-smoke".to_string(),
            ..InterceptorConfig::default()
        },
        ..Config::default()
    };
    let env_value = |name: &str| (name == "OPENAI_API_KEY").then(|| "external-api-key".to_string());

    finalize_config_keys_with_env(&mut config, &env_value);

    assert_eq!(
        config.providers["external"].api_key.as_deref(),
        Some("external-api-key")
    );
    assert_eq!(config.providers["local"].api_key, None);

    config.providers.get_mut("local").unwrap().api_key_env = Some("LOCAL_API_KEY".to_string());
    let env_value = |name: &str| match name {
        "OPENAI_API_KEY" => Some("external-api-key".to_string()),
        "LOCAL_API_KEY" => Some("local-api-key".to_string()),
        _ => None,
    };
    finalize_config_keys_with_env(&mut config, &env_value);
    assert_eq!(
        config.providers["local"].api_key.as_deref(),
        Some("local-api-key")
    );
}

#[test]
fn non_openai_provider_kinds_do_not_inherit_openai_credentials() {
    let env_value = |name: &str| (name == "OPENAI_API_KEY").then(|| "openai-only-key".to_string());
    for kind in [ProviderKind::Anthropic, ProviderKind::Copilot] {
        let provider = UpstreamConfig {
            kind,
            ..UpstreamConfig::default()
        };
        assert_eq!(resolve_api_key_with_env(&provider, true, &env_value), None);
    }
    let provider = UpstreamConfig {
        kind: ProviderKind::OpenAi,
        ..UpstreamConfig::default()
    };
    assert_eq!(
        resolve_api_key_with_env(&provider, true, &env_value).as_deref(),
        Some("openai-only-key")
    );
}

#[test]
fn disabled_legacy_interceptor_upstream_does_not_inherit_external_api_key() {
    let mut config = Config {
        default_provider: "default".to_string(),
        interceptor: InterceptorConfig {
            enabled: false,
            provider: "default".to_string(),
            model: "local-smoke".to_string(),
            ..InterceptorConfig::default()
        },
        ..Config::default()
    };
    let env_value = |name: &str| (name == "OPENAI_API_KEY").then(|| "external-api-key".to_string());

    finalize_config_keys_with_env(&mut config, &env_value);

    assert_eq!(config.upstream.api_key, None);
}

#[test]
fn parses_instance_modes_and_agent_arguments() {
    let cli = Cli::try_parse_from([
        "llmproxy", "--codex", "--", "exec", "--model", "gpt-test", "hello",
    ])
    .unwrap();
    assert!(cli.codex);
    assert!(!cli.claude);
    assert!(!cli.copilot);
    assert_eq!(
        cli.agent_args,
        ["exec", "--model", "gpt-test", "hello"].map(OsString::from)
    );
    assert!(validate_cli(&cli).is_ok());

    for agent in ["--codex", "--claude", "--copilot"] {
        let cli = Cli::try_parse_from(["llmproxy", agent]).unwrap();
        assert_eq!(cli.requested_mode(), None);
        assert!(validate_cli(&cli).is_ok());
    }

    for (argument, mode) in [
        ("--proxy", RoutingMode::Proxy),
        ("--intercept", RoutingMode::Intercept),
        ("--integrated", RoutingMode::Integrated),
    ] {
        let cli = Cli::try_parse_from(["llmproxy", argument]).unwrap();
        assert_eq!(cli.requested_mode(), Some(mode));
        assert!(validate_cli(&cli).is_ok());
    }
    let cli = Cli::try_parse_from(["llmproxy", "--mode", "intercept"]).unwrap();
    assert_eq!(cli.requested_mode(), Some(RoutingMode::Intercept));
    let cli = Cli::try_parse_from([
        "llmproxy",
        "--restart",
        "--restart-from",
        "127.0.0.1:8080",
        "--listen",
        "127.0.0.1:8181",
    ])
    .unwrap();
    assert_eq!(cli.restart_from.as_deref(), Some("127.0.0.1:8080"));
    assert!(Cli::try_parse_from(["llmproxy", "--restart-from", "127.0.0.1:8080"]).is_err());

    let cli = Cli::try_parse_from(["llmproxy", "--mode", "proxy", "--codex"]).unwrap();
    assert!(validate_cli(&cli).is_err());
    let cli = Cli::try_parse_from([
        "llmproxy",
        "--codex",
        "--default-provider-url",
        "http://127.0.0.1:8000/v1",
    ])
    .unwrap();
    assert!(validate_cli(&cli).is_err());

    assert!(Cli::try_parse_from(["llmproxy", "--proxy", "--intercept", "--codex"]).is_err());
    assert!(Cli::try_parse_from(["llmproxy", "--proxy", "--integrated", "--codex"]).is_err());
    assert!(Cli::try_parse_from(["llmproxy", "--codex", "--claude"]).is_err());
    assert!(Cli::try_parse_from(["llmproxy", "--claude", "--copilot"]).is_err());
    assert!(Cli::try_parse_from(["llmproxy", "--", "--model", "sonnet"]).is_err());
}

#[test]
fn caller_credentials_only_forward_to_the_external_default_provider() {
    let mut incoming = HeaderMap::new();
    incoming.insert(
        AUTHORIZATION,
        HeaderValue::from_static("Bearer caller-secret"),
    );
    incoming.insert(
        "chatgpt-account-id",
        HeaderValue::from_static("caller-account"),
    );

    let local = test_state("http://127.0.0.1:8000/v1".to_string());
    assert!(provider_request_headers(&local, &incoming, local.default_provider()).is_empty());

    let external = test_state("https://api.example.test/v1".to_string());
    let forwarded = provider_request_headers(&external, &incoming, external.default_provider());
    assert_eq!(forwarded.get(AUTHORIZATION), incoming.get(AUTHORIZATION));
    assert_eq!(
        forwarded.get("chatgpt-account-id"),
        incoming.get("chatgpt-account-id")
    );

    let config = Config {
        upstream: UpstreamConfig {
            base_url: "https://default.example.test/v1".to_string(),
            ..UpstreamConfig::default()
        },
        providers: HashMap::from([(
            "secondary".to_string(),
            UpstreamConfig {
                base_url: "https://secondary.example.test/v1".to_string(),
                ..UpstreamConfig::default()
            },
        )]),
        ..Config::default()
    };
    let state = test_state_with_config(config);
    assert!(provider_request_headers(&state, &incoming, &state.providers["secondary"]).is_empty());
}

#[test]
fn launcher_runtime_validation_is_mode_specific() {
    let mut config = Config::default();
    assert!(validate_launcher_runtime(&config, LauncherMode::Proxy).is_ok());
    assert!(validate_launcher_runtime(&config, LauncherMode::Intercept).is_err());

    config.providers.insert(
        "local".to_string(),
        UpstreamConfig {
            base_url: "http://127.0.0.1:11434/v1".to_string(),
            ..UpstreamConfig::default()
        },
    );
    config.interceptor = local_interceptor_config("local", "triage-model");
    config.interceptor.enabled = false;
    assert!(validate_launcher_runtime(&config, LauncherMode::Intercept).is_ok());
    assert!(validate_launcher_runtime(&config, LauncherMode::Integrated).is_err());

    config.integrated.targets.insert(
        "DETERMINISTIC".to_string(),
        interceptor::IntegratedTarget {
            provider: "default".to_string(),
            model: "local-model".to_string(),
            hint: "larger local work".to_string(),
            reasoning_effort: None,
        },
    );
    assert!(validate_launcher_runtime(&config, LauncherMode::Integrated).is_ok());

    config.set_routing_mode(RoutingMode::Integrated);
    let state = test_state_with_config(config);
    let session = LauncherSession::local(LauncherKind::Codex, LauncherMode::Integrated).unwrap();
    let scoped = state.with_launcher_session(&session).unwrap();
    assert!(!scoped.config.interceptor.enabled);
    assert!(scoped.config.integrated.enabled);
    assert_eq!(scoped.launcher_kind, None);
    assert_eq!(scoped.launcher_mode, Some(LauncherMode::Integrated));
    assert!(!scoped.forward_caller_credentials);
}

#[test]
fn routing_mode_is_exclusive_and_gates_sensitive_screening() {
    let mut config = Config::default();
    config.interceptor.reject_sensitive = true;

    config.set_routing_mode(RoutingMode::Proxy);
    let state = test_state_with_config(config.clone());
    assert!(!state.config.interceptor.enabled);
    assert!(!state.config.integrated.enabled);
    assert!(!state.triage_enabled());
    assert!(!state.sensitive_rejection_enabled());

    config.set_routing_mode(RoutingMode::Intercept);
    let state = test_state_with_config(config.clone());
    assert!(state.config.interceptor.enabled);
    assert!(!state.config.integrated.enabled);
    assert!(state.triage_enabled());
    assert!(state.sensitive_rejection_enabled());

    config.set_routing_mode(RoutingMode::Integrated);
    let state = test_state_with_config(config);
    assert!(!state.config.interceptor.enabled);
    assert!(state.config.integrated.enabled);
    assert!(state.triage_enabled());
    assert!(state.sensitive_rejection_enabled());
}

#[test]
fn proxy_mode_does_not_validate_dormant_sensitive_screening() {
    let mut config = Config::default();
    config.upstream.base_url = "https://provider.example.test/v1".to_string();
    config.interceptor.provider = "default".to_string();
    config.interceptor.model = "triage".to_string();
    config.interceptor.reject_sensitive = true;
    config.set_routing_mode(RoutingMode::Proxy);
    assert!(validate_config(&config).is_ok());

    config.set_routing_mode(RoutingMode::Intercept);
    assert!(validate_config(&config).is_err());
}

#[test]
fn config_parsing_migrates_legacy_mode_flags() {
    let proxy = parse_config("", "test").unwrap();
    assert_eq!(proxy.routing_mode, RoutingMode::Proxy);

    let intercept = parse_config("[interceptor]\nenabled = true\n", "test").unwrap();
    assert_eq!(intercept.routing_mode, RoutingMode::Intercept);
    assert!(intercept.interceptor.enabled);
    assert!(!intercept.integrated.enabled);

    let integrated = parse_config("routing_mode = \"integrated\"\n", "test").unwrap();
    assert_eq!(integrated.routing_mode, RoutingMode::Integrated);
    assert!(!integrated.interceptor.enabled);
    assert!(integrated.integrated.enabled);

    assert!(
        parse_config(
            "[interceptor]\nenabled = true\n[integrated]\nenabled = true\n",
            "test"
        )
        .is_err()
    );
}

#[test]
fn unknown_config_keys_are_detected_without_failing_parse() {
    // A typo must not fail parsing but must be surfaced as a finding.
    let raw = "\
listen_addr = \"127.0.0.1:8080\"
reasoning_efort = \"high\"

[providers.default]
base_url = \"http://127.0.0.1:8000/v1\"
reasoning_efort = \"high\"

[providers.triage]
base_url = \"http://127.0.0.1:11434/v1\"
baseurl = \"oops\"

[interceptor]
provider = \"triage\"
timout_ms = 60000

[integrated]
sticky_ttl_secs = 1800

[integrated.targets.fast]
provider = \"default\"
model = \"m\"
reen = \"typo\"

[model_aliases.gpt]
provider = \"default\"
modell = \"m\"

[chat]
disable_thnk = true

[responses]
mode = \"passthrough\"

[models]
cache_ttl_sec = 30

[admin]
enable = true
listen_addres = \"127.0.0.1:11180\"

[role_map]
developer = \"system\"
";
    let document: toml::Table = raw.parse().unwrap();
    let mut found = Vec::new();
    warn_on_unknown_config_keys(&document, "test", &mut found);
    assert!(
        found.contains(&"config.reasoning_efort".to_string()),
        "{found:?}"
    );
    assert!(
        found.contains(&"providers.default.reasoning_efort".to_string()),
        "{found:?}"
    );
    assert!(
        found.contains(&"providers.triage.baseurl".to_string()),
        "{found:?}"
    );
    assert!(
        found.contains(&"interceptor.timout_ms".to_string()),
        "{found:?}"
    );
    assert!(
        found.contains(&"integrated.targets.fast.reen".to_string()),
        "{found:?}"
    );
    assert!(
        found.contains(&"model_aliases.gpt.modell".to_string()),
        "{found:?}"
    );
    assert!(
        found.contains(&"chat.disable_thnk".to_string()),
        "{found:?}"
    );
    assert!(
        found.contains(&"models.cache_ttl_sec".to_string()),
        "{found:?}"
    );
    assert!(
        found.contains(&"admin.listen_addres".to_string()),
        "{found:?}"
    );
    // Free-form maps and valid keys must not be reported.
    assert!(
        !found.iter().any(|k| k.starts_with("role_map.")),
        "{found:?}"
    );
    assert!(
        !found.iter().any(|k| k.contains("sticky_ttl_secs")),
        "{found:?}"
    );
    assert!(!found.iter().any(|k| k.contains(".base_url")), "{found:?}");
}

fn test_config() -> Config {
    Config {
        model_aliases: HashMap::from([(
            "codex-qwen".to_string(),
            ModelAlias::Model("Qwen/Qwen2.5-Coder-32B-Instruct".to_string()),
        )]),
        role_map: HashMap::from([("developer".to_string(), "system".to_string())]),
        ..Config::default()
    }
}

fn test_state(upstream_base_url: String) -> AppState {
    let mut config = Config::default();
    config.upstream.base_url = upstream_base_url;
    test_state_with_config(config)
}

fn test_state_with_config(config: Config) -> AppState {
    let forward_caller_credentials = config.routing_mode != RoutingMode::Integrated;
    let provider_configs = effective_provider_configs(&config).unwrap();
    let providers = provider_configs
        .into_iter()
        .map(|(name, upstream)| {
            (
                name.clone(),
                ProviderState {
                    name,
                    config: Arc::new(upstream),
                    client: Client::new(),
                    model_cache: Arc::new(ModelCache::default()),
                    model_refresh: Arc::new(AsyncMutex::new(())),
                },
            )
        })
        .collect();
    AppState {
        config: Arc::new(config),
        providers: Arc::new(providers),
        launcher_kind: None,
        launcher_mode: None,
        forward_caller_credentials,
    }
}

fn test_provider(upstream_base_url: String) -> ProviderState {
    let state = test_state(upstream_base_url);
    state.default_provider().clone()
}

async fn spawn_test_upstream(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/v1"), server)
}

fn local_interceptor_config(provider: &str, model: &str) -> InterceptorConfig {
    InterceptorConfig {
        enabled: true,
        provider: provider.to_string(),
        model: model.to_string(),
        timeout_ms: 2_000,
        fallback_on_error: true,
        ..InterceptorConfig::default()
    }
}

fn chat_completion(model: &str, content: Value, tool_calls: Value) -> Value {
    let has_tool_calls = tool_calls.as_array().is_some_and(|calls| !calls.is_empty());
    json!({
        "id": "chatcmpl-local",
        "object": "chat.completion",
        "created": 1,
        "model": model,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": content,
                "tool_calls": tool_calls
            },
            "finish_reason": if has_tool_calls {
                "tool_calls"
            } else {
                "stop"
            }
        }],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 3,
            "total_tokens": 13
        }
    })
}

fn pass_completion() -> Value {
    chat_completion(
        "local-fast",
        Value::Null,
        json!([{
            "id": "call-pass",
            "type": "function",
            "function": {
                "name": "PASS",
                "arguments": "{}"
            }
        }]),
    )
}

fn route_default_completion() -> Value {
    chat_completion(
        "local-fast",
        Value::Null,
        json!([{
            "id": "call-route-default",
            "type": "function",
            "function": {
                "name": "ROUTE_DEFAULT",
                "arguments": "{}"
            }
        }]),
    )
}

fn reject_completion(reason: &str) -> Value {
    chat_completion(
        "local-fast",
        Value::Null,
        json!([{
            "id": "call-reject",
            "type": "function",
            "function": {
                "name": "REJECT",
                "arguments": json!({ "reason": reason }).to_string()
            }
        }]),
    )
}

#[tokio::test]
async fn interceptor_pass_preserves_external_model_remapping_and_original_request() {
    let local_body = Arc::new(Mutex::new(None::<Value>));
    let local_authorization = Arc::new(Mutex::new(None::<String>));
    let local_body_for_handler = local_body.clone();
    let local_authorization_for_handler = local_authorization.clone();
    let local_app = Router::new().route(
        "/v1/chat/completions",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let local_body = local_body_for_handler.clone();
            let local_authorization = local_authorization_for_handler.clone();
            async move {
                *local_body.lock().unwrap() = Some(body);
                *local_authorization.lock().unwrap() = headers
                    .get(AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    .map(ToOwned::to_owned);
                Json(pass_completion())
            }
        }),
    );
    let (local_base_url, local_server) = spawn_test_upstream(local_app).await;

    let external_body = Arc::new(Mutex::new(None::<Value>));
    let external_body_for_handler = external_body.clone();
    let external_app = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(body): Json<Value>| {
            let external_body = external_body_for_handler.clone();
            async move {
                *external_body.lock().unwrap() = Some(body);
                Json(chat_completion(
                    "destination-model-y",
                    json!("external answer"),
                    json!([]),
                ))
            }
        }),
    );
    let (external_base_url, external_server) = spawn_test_upstream(external_app).await;

    let mut config = Config::default();
    config.upstream.base_url = external_base_url;
    config.providers.insert(
        "local".to_string(),
        UpstreamConfig {
            base_url: local_base_url,
            ..UpstreamConfig::default()
        },
    );
    config.interceptor = local_interceptor_config("local", "local-fast");
    config.model_aliases.insert(
        "agent-openai".to_string(),
        ModelAlias::Model("destination-model-y".to_string()),
    );
    config.set_routing_mode(RoutingMode::Intercept);
    let state = test_state_with_config(config);
    let original = json!({
        "model": "agent-openai",
        "stream": false,
        "messages": [{ "role": "user", "content": "inspect this carefully" }],
        "tools": [{
            "type": "function",
            "function": {
                "name": "exec_command",
                "parameters": { "type": "object" }
            }
        }],
        "metadata": { "preserve": true }
    });
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_static("Bearer placeholder"),
    );

    let response = chat_completions(State(SharedState::new(state)), headers, Json(original)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let response_body: Value =
        serde_json::from_str(&collect_response_text(response).await).unwrap();
    assert_eq!(
        response_body["choices"][0]["message"]["content"],
        "external answer"
    );

    let local = local_body.lock().unwrap().clone().unwrap();
    assert_eq!(local["model"], "local-fast");
    assert_eq!(local["stream"], false);
    assert!(
        local["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| { tool["function"]["name"].as_str() == Some("PASS") })
    );
    assert!(local_authorization.lock().unwrap().is_none());

    let external = external_body.lock().unwrap().clone().unwrap();
    assert_eq!(external["model"], "destination-model-y");
    assert_eq!(external["metadata"]["preserve"], true);
    assert_eq!(external["tools"].as_array().unwrap().len(), 1);
    assert!(
        external["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| { tool["function"]["name"].as_str() != Some("PASS") })
    );
    assert!(
        external["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|message| {
                !text_from_value(&message["content"]).contains("local execution path")
            })
    );

    local_server.abort();
    external_server.abort();
}

#[tokio::test]
async fn integrated_route_default_uses_normal_routing_and_proxy_credentials() {
    let local_capture = Arc::new(Mutex::new(None::<(HeaderMap, Value)>));
    let local_capture_for_handler = local_capture.clone();
    let local_app = Router::new().route(
        "/v1/chat/completions",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let capture = local_capture_for_handler.clone();
            async move {
                *capture.lock().unwrap() = Some((headers, body));
                Json(route_default_completion())
            }
        }),
    );
    let (local_url, local_server) = spawn_test_upstream(local_app).await;

    let external_capture = Arc::new(Mutex::new(None::<(HeaderMap, Value)>));
    let external_capture_for_handler = external_capture.clone();
    let external_app = Router::new().route(
        "/v1/chat/completions",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let capture = external_capture_for_handler.clone();
            async move {
                *capture.lock().unwrap() = Some((headers, body));
                Json(chat_completion(
                    "default-model",
                    json!("default routed"),
                    json!([]),
                ))
            }
        }),
    );
    let (external_url, external_server) = spawn_test_upstream(external_app).await;

    let mut config = Config {
        default_provider: "external".to_string(),
        providers: HashMap::from([
            (
                "local".to_string(),
                UpstreamConfig {
                    base_url: local_url,
                    ..UpstreamConfig::default()
                },
            ),
            (
                "external".to_string(),
                UpstreamConfig {
                    kind: ProviderKind::OpenAi,
                    base_url: external_url,
                    api_key: Some("proxy-owned-credential".to_string()),
                    ..UpstreamConfig::default()
                },
            ),
        ]),
        interceptor: local_interceptor_config("local", "local-fast"),
        integrated: IntegratedConfig {
            enabled: true,
            targets: std::collections::BTreeMap::from([(
                "PROVIDER_EXTERNAL".to_string(),
                interceptor::IntegratedTarget {
                    provider: "external".to_string(),
                    model: "target-model".to_string(),
                    hint: "explicitly selected work".to_string(),
                    reasoning_effort: None,
                },
            )]),
            ..IntegratedConfig::default()
        },
        model_aliases: HashMap::from([(
            "frontend-model".to_string(),
            ModelAlias::Route {
                provider: "external".to_string(),
                model: "default-model".to_string(),
            },
        )]),
        ..Config::default()
    };
    config.set_routing_mode(RoutingMode::Integrated);
    validate_config(&config).unwrap();
    let state = test_state_with_config(config);
    state.providers["external"]
        .model_cache
        .replace_upstream_models(
            vec!["default-model".to_string()],
            json!({ "data": [{ "id": "default-model" }] }),
        );
    let mut caller_headers = HeaderMap::new();
    caller_headers.insert(
        AUTHORIZATION,
        HeaderValue::from_static("Bearer caller-secret"),
    );
    caller_headers.insert("x-api-key", HeaderValue::from_static("caller-key"));

    let response = chat_completions(
        State(SharedState::new(state)),
        caller_headers,
        Json(json!({
            "model": INTEGRATED_AUTO_MODEL,
            "messages": [{ "role": "user", "content": "use normal routing" }],
            "metadata": { "preserve": true }
        })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        collect_response_text(response)
            .await
            .contains("default routed")
    );

    let (local_headers, local_body) = local_capture.lock().unwrap().clone().unwrap();
    assert!(local_headers.get(AUTHORIZATION).is_none());
    assert!(local_headers.get("x-api-key").is_none());
    let local_tools = local_body["tools"].as_array().unwrap();
    assert!(
        local_tools
            .iter()
            .any(|tool| { tool["function"]["name"].as_str() == Some("PASS") })
    );
    assert!(
        local_tools
            .iter()
            .all(|tool| { tool["function"]["name"].as_str() != Some("ROUTE_DEFAULT") })
    );

    let (external_headers, external_body) = external_capture.lock().unwrap().clone().unwrap();
    assert_eq!(
        external_headers.get(AUTHORIZATION).unwrap(),
        "Bearer proxy-owned-credential"
    );
    assert!(external_headers.get("x-api-key").is_none());
    assert_eq!(external_body["model"], "default-model");
    assert_eq!(external_body["metadata"]["preserve"], true);

    local_server.abort();
    external_server.abort();
}

#[tokio::test]
async fn integrated_redirect_uses_target_provider_credentials_only() {
    let local_capture = Arc::new(Mutex::new(None::<(HeaderMap, Value)>));
    let local_capture_for_handler = local_capture.clone();
    let local_app = Router::new().route(
        "/v1/chat/completions",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let capture = local_capture_for_handler.clone();
            async move {
                *capture.lock().unwrap() = Some((headers, body));
                Json(chat_completion(
                    "triage",
                    Value::Null,
                    json!([{
                        "id": "redirect-call",
                        "type": "function",
                        "function": {
                            "name": "PROVIDER_EXTERNAL",
                            "arguments": "{}"
                        }
                    }]),
                ))
            }
        }),
    );
    let (local_url, local_server) = spawn_test_upstream(local_app).await;

    let external_capture = Arc::new(Mutex::new(None::<(HeaderMap, Value)>));
    let external_capture_for_handler = external_capture.clone();
    let external_app = Router::new().route(
        "/v1/chat/completions",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let capture = external_capture_for_handler.clone();
            async move {
                *capture.lock().unwrap() = Some((headers, body));
                Json(chat_completion(
                    "target-model",
                    json!("redirected"),
                    json!([]),
                ))
            }
        }),
    );
    let (external_url, external_server) = spawn_test_upstream(external_app).await;

    let mut config = Config {
        default_provider: "external".to_string(),
        providers: HashMap::from([
            (
                "local".to_string(),
                UpstreamConfig {
                    base_url: local_url,
                    ..UpstreamConfig::default()
                },
            ),
            (
                "external".to_string(),
                UpstreamConfig {
                    kind: ProviderKind::OpenAi,
                    base_url: external_url,
                    api_key: Some("target-credential".to_string()),
                    ..UpstreamConfig::default()
                },
            ),
        ]),
        interceptor: {
            let mut interceptor = local_interceptor_config("local", "triage");
            interceptor.enabled = false;
            interceptor
        },
        integrated: IntegratedConfig {
            enabled: true,
            targets: std::collections::BTreeMap::from([(
                "PROVIDER_EXTERNAL".to_string(),
                interceptor::IntegratedTarget {
                    provider: "external".to_string(),
                    model: "target-model".to_string(),
                    hint: "complex work".to_string(),
                    reasoning_effort: Some(ReasoningEffort::Max),
                },
            )]),
            ..IntegratedConfig::default()
        },
        ..Config::default()
    };
    config.set_routing_mode(RoutingMode::Integrated);
    validate_config(&config).unwrap();
    let state = test_state_with_config(config);
    let mut caller_headers = HeaderMap::new();
    caller_headers.insert(
        AUTHORIZATION,
        HeaderValue::from_static("Bearer caller-secret"),
    );
    caller_headers.insert("x-api-key", HeaderValue::from_static("caller-key"));
    let response = chat_completions(
        State(SharedState::new(state)),
        caller_headers,
        Json(json!({
            "model": INTEGRATED_AUTO_MODEL,
            "messages": [{ "role": "user", "content": "unique integrated task" }]
        })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(collect_response_text(response).await.contains("redirected"));

    let (local_headers, local_body) = local_capture.lock().unwrap().clone().unwrap();
    assert!(local_headers.get(AUTHORIZATION).is_none());
    assert!(local_headers.get("x-api-key").is_none());
    assert!(
        local_body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| { tool["function"]["name"].as_str() == Some("PROVIDER_EXTERNAL") })
    );
    let (external_headers, external_body) = external_capture.lock().unwrap().clone().unwrap();
    assert_eq!(
        external_headers.get(AUTHORIZATION).unwrap(),
        "Bearer target-credential"
    );
    assert!(external_headers.get("x-api-key").is_none());
    assert_eq!(external_body["model"], "target-model");
    assert_eq!(external_body["reasoning_effort"], "max");

    local_server.abort();
    external_server.abort();
}

#[tokio::test]
async fn integrated_explicit_namespace_bypasses_triage_and_uses_proxy_credentials() {
    let local_capture = Arc::new(Mutex::new(None::<Value>));
    let local_capture_for_handler = local_capture.clone();
    let local_app = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(body): Json<Value>| {
            let capture = local_capture_for_handler.clone();
            async move {
                *capture.lock().unwrap() = Some(body);
                Json(pass_completion())
            }
        }),
    );
    let (local_url, local_server) = spawn_test_upstream(local_app).await;

    let external_capture = Arc::new(Mutex::new(None::<(HeaderMap, Value)>));
    let capture_for_handler = external_capture.clone();
    let external_app = Router::new().route(
        "/v1/chat/completions",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let capture = capture_for_handler.clone();
            async move {
                *capture.lock().unwrap() = Some((headers, body));
                Json(chat_completion("pinned-model", json!("pinned"), json!([])))
            }
        }),
    );
    let (external_url, external_server) = spawn_test_upstream(external_app).await;
    let mut config = Config {
        default_provider: "external".to_string(),
        providers: HashMap::from([
            (
                "local".to_string(),
                UpstreamConfig {
                    base_url: local_url,
                    ..UpstreamConfig::default()
                },
            ),
            (
                "external".to_string(),
                UpstreamConfig {
                    base_url: external_url,
                    api_key: Some("proxy-credential".to_string()),
                    ..UpstreamConfig::default()
                },
            ),
        ]),
        interceptor: local_interceptor_config("local", "triage"),
        integrated: IntegratedConfig {
            enabled: true,
            targets: std::collections::BTreeMap::from([(
                "PROVIDER_EXTERNAL".to_string(),
                interceptor::IntegratedTarget {
                    provider: "external".to_string(),
                    model: "fallback-model".to_string(),
                    hint: "complex work".to_string(),
                    reasoning_effort: None,
                },
            )]),
            ..IntegratedConfig::default()
        },
        ..Config::default()
    };
    config.set_routing_mode(RoutingMode::Integrated);
    validate_config(&config).unwrap();
    let mut caller_headers = HeaderMap::new();
    caller_headers.insert(
        AUTHORIZATION,
        HeaderValue::from_static("Bearer caller-secret"),
    );
    let response = chat_completions(
        State(SharedState::new(test_state_with_config(config))),
        caller_headers,
        Json(json!({
            "model": "external/pinned-model",
            "messages": [{ "role": "user", "content": "explicit namespace task" }]
        })),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(local_capture.lock().unwrap().is_none());
    let (headers, body) = external_capture.lock().unwrap().clone().unwrap();
    assert_eq!(
        headers.get(AUTHORIZATION).unwrap(),
        "Bearer proxy-credential"
    );
    assert_eq!(body["model"], "pinned-model");
    local_server.abort();
    external_server.abort();
}

#[tokio::test]
async fn integrated_redirect_failure_does_not_issue_a_second_generation() {
    let local_app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            Json(chat_completion(
                "triage",
                Value::Null,
                json!([{
                    "id": "redirect-call",
                    "type": "function",
                    "function": { "name": "PROVIDER_TARGET", "arguments": "{}" }
                }]),
            ))
        }),
    );
    let (local_url, local_server) = spawn_test_upstream(local_app).await;
    let target_app = Router::new().route(
        "/v1/chat/completions",
        post(|| async { (StatusCode::SERVICE_UNAVAILABLE, "target unavailable") }),
    );
    let (target_url, target_server) = spawn_test_upstream(target_app).await;
    let default_calls = Arc::new(AtomicU64::new(0));
    let default_calls_for_handler = default_calls.clone();
    let default_app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let default_calls = default_calls_for_handler.clone();
            async move {
                default_calls.fetch_add(1, Ordering::Relaxed);
                Json(chat_completion(
                    "default-model",
                    json!("default answer"),
                    json!([]),
                ))
            }
        }),
    );
    let (default_url, default_server) = spawn_test_upstream(default_app).await;
    let mut config = Config {
        default_provider: "default".to_string(),
        providers: HashMap::from([
            (
                "local".to_string(),
                UpstreamConfig {
                    base_url: local_url,
                    ..UpstreamConfig::default()
                },
            ),
            (
                "target".to_string(),
                UpstreamConfig {
                    base_url: target_url,
                    api_key: Some("target-key".to_string()),
                    ..UpstreamConfig::default()
                },
            ),
            (
                "default".to_string(),
                UpstreamConfig {
                    base_url: default_url,
                    api_key: Some("default-key".to_string()),
                    ..UpstreamConfig::default()
                },
            ),
        ]),
        interceptor: local_interceptor_config("local", "triage"),
        integrated: IntegratedConfig {
            enabled: true,
            targets: std::collections::BTreeMap::from([(
                "PROVIDER_TARGET".to_string(),
                interceptor::IntegratedTarget {
                    provider: "target".to_string(),
                    model: "target-model".to_string(),
                    hint: "complex work".to_string(),
                    reasoning_effort: None,
                },
            )]),
            ..IntegratedConfig::default()
        },
        model_aliases: HashMap::from([(
            "frontend-model".to_string(),
            ModelAlias::Route {
                provider: "default".to_string(),
                model: "default-model".to_string(),
            },
        )]),
        ..Config::default()
    };
    config.set_routing_mode(RoutingMode::Integrated);
    validate_config(&config).unwrap();
    let response = chat_completions(
        State(SharedState::new(test_state_with_config(config))),
        HeaderMap::new(),
        Json(json!({
            "model": INTEGRATED_AUTO_MODEL,
            "messages": [{ "role": "user", "content": "unique redirect fallback task" }]
        })),
    )
    .await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        collect_response_text(response)
            .await
            .contains("target unavailable")
    );
    assert_eq!(default_calls.load(Ordering::Relaxed), 0);
    local_server.abort();
    target_server.abort();
    default_server.abort();
}

#[tokio::test]
async fn interceptor_local_tool_call_streams_as_responses_without_external_request() {
    let local_body = Arc::new(Mutex::new(None::<Value>));
    let local_body_for_handler = local_body.clone();
    let local_app = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(body): Json<Value>| {
            let local_body = local_body_for_handler.clone();
            async move {
                *local_body.lock().unwrap() = Some(body);
                Json(chat_completion(
                    "local-fast",
                    Value::Null,
                    json!([{
                        "id": "call-exec",
                        "type": "function",
                        "function": {
                            "name": "exec_command",
                            "arguments": "{\"cmd\":\"pwd\"}"
                        }
                    }]),
                ))
            }
        }),
    );
    let (local_base_url, local_server) = spawn_test_upstream(local_app).await;
    let mut config = Config::default();
    config.providers.insert(
        "local".to_string(),
        UpstreamConfig {
            base_url: local_base_url,
            ..UpstreamConfig::default()
        },
    );
    config.interceptor = local_interceptor_config("local", "local-fast");
    config.set_routing_mode(RoutingMode::Intercept);
    let state = test_state_with_config(config);
    let body = json!({
        "model": "agent-openai",
        "stream": true,
        "instructions": "Act as an agent.",
        "input": "show the current directory",
        "tools": [{
            "type": "function",
            "name": "exec_command",
            "description": "Run a command",
            "parameters": {
                "type": "object",
                "properties": { "cmd": { "type": "string" } },
                "required": ["cmd"]
            }
        }]
    });

    let response = responses(State(SharedState::new(state)), HeaderMap::new(), Json(body)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-llmproxy-route")
            .and_then(|value| value.to_str().ok()),
        Some("local")
    );
    let events = collect_sse_events(response).await;
    assert!(events.iter().any(|(event, value)| {
        event == "response.output_item.done"
            && value["item"]["type"] == "function_call"
            && value["item"]["name"] == "exec_command"
    }));
    assert!(events.iter().any(|(event, value)| {
        event == "response.completed" && value["response"]["model"] == "agent-openai"
    }));

    let local = local_body.lock().unwrap().clone().unwrap();
    assert_eq!(local["model"], "local-fast");
    assert!(local["messages"].as_array().unwrap().iter().any(|message| {
        text_from_value(&message["content"]).contains("show the current directory")
    }));
    local_server.abort();
}

#[tokio::test]
async fn sensitive_rejection_renders_for_all_agent_protocols_without_external_calls() {
    let local_app = Router::new().route(
        "/v1/chat/completions",
        post(|| async { Json(reject_completion("personally identifiable information")) }),
    );
    let external_calls = Arc::new(AtomicU64::new(0));
    let external_calls_for_handler = external_calls.clone();
    let external_app = Router::new().fallback(move || {
        let external_calls = external_calls_for_handler.clone();
        async move {
            external_calls.fetch_add(1, Ordering::Relaxed);
            StatusCode::INTERNAL_SERVER_ERROR
        }
    });
    let (local_base_url, local_server) = spawn_test_upstream(local_app).await;
    let (external_base_url, external_server) = spawn_test_upstream(external_app).await;
    let mut config = Config {
        default_provider: "external".to_string(),
        providers: HashMap::from([
            (
                "local".to_string(),
                UpstreamConfig {
                    base_url: local_base_url,
                    ..UpstreamConfig::default()
                },
            ),
            (
                "external".to_string(),
                UpstreamConfig {
                    base_url: external_base_url,
                    ..UpstreamConfig::default()
                },
            ),
        ]),
        ..Config::default()
    };
    config.interceptor = InterceptorConfig {
        reject_sensitive: true,
        ..local_interceptor_config("local", "local-fast")
    };
    config.set_routing_mode(RoutingMode::Intercept);
    let shared = SharedState::new(test_state_with_config(config));

    let responses = [
        chat_completions(
            State(shared.clone()),
            HeaderMap::new(),
            Json(json!({
                "model": "agent-chat",
                "messages": [{ "role": "user", "content": "private input" }]
            })),
        )
        .await,
        responses(
            State(shared.clone()),
            HeaderMap::new(),
            Json(json!({
                "model": "agent-codex",
                "input": "private input"
            })),
        )
        .await,
        anthropic_messages(
            State(shared),
            HeaderMap::new(),
            Json(json!({
                "model": "agent-claude",
                "max_tokens": 100,
                "messages": [{ "role": "user", "content": "private input" }]
            })),
        )
        .await,
    ];

    for response in responses {
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("x-llmproxy-route")
                .and_then(|value| value.to_str().ok()),
            Some("reject")
        );
        let body = collect_response_text(response).await;
        assert!(body.contains("No action was taken"), "body: {body}");
        assert!(body.contains("personally identifiable information"));
    }
    assert_eq!(external_calls.load(Ordering::Relaxed), 0);
    local_server.abort();
    external_server.abort();
}

#[tokio::test]
async fn sensitive_rejection_fails_closed_when_screening_is_unavailable() {
    let local_app = Router::new().route(
        "/v1/chat/completions",
        post(|| async { StatusCode::SERVICE_UNAVAILABLE }),
    );
    let external_calls = Arc::new(AtomicU64::new(0));
    let external_calls_for_handler = external_calls.clone();
    let external_app = Router::new().fallback(move || {
        let external_calls = external_calls_for_handler.clone();
        async move {
            external_calls.fetch_add(1, Ordering::Relaxed);
            StatusCode::OK
        }
    });
    let (local_base_url, local_server) = spawn_test_upstream(local_app).await;
    let (external_base_url, external_server) = spawn_test_upstream(external_app).await;
    let mut config = Config {
        default_provider: "external".to_string(),
        providers: HashMap::from([
            (
                "local".to_string(),
                UpstreamConfig {
                    base_url: local_base_url,
                    ..UpstreamConfig::default()
                },
            ),
            (
                "external".to_string(),
                UpstreamConfig {
                    base_url: external_base_url,
                    ..UpstreamConfig::default()
                },
            ),
        ]),
        ..Config::default()
    };
    config.interceptor = InterceptorConfig {
        enabled: false,
        reject_sensitive: true,
        ..local_interceptor_config("local", "local-fast")
    };
    config.set_routing_mode(RoutingMode::Intercept);
    let response = chat_completions(
        State(SharedState::new(test_state_with_config(config))),
        HeaderMap::new(),
        Json(json!({
            "model": "explicit-agent-model",
            "messages": [{ "role": "user", "content": "screen this" }]
        })),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-llmproxy-route")
            .and_then(|value| value.to_str().ok()),
        Some("reject")
    );
    assert!(
        collect_response_text(response)
            .await
            .contains("screened safely")
    );
    assert_eq!(external_calls.load(Ordering::Relaxed), 0);
    local_server.abort();
    external_server.abort();
}

#[tokio::test]
async fn interceptor_local_text_streams_as_anthropic_messages() {
    let local_app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            Json(chat_completion(
                "local-fast",
                json!("local answer"),
                json!([]),
            ))
        }),
    );
    let (local_base_url, local_server) = spawn_test_upstream(local_app).await;
    let mut config = Config::default();
    config.providers.insert(
        "local".to_string(),
        UpstreamConfig {
            base_url: local_base_url,
            ..UpstreamConfig::default()
        },
    );
    config.interceptor = local_interceptor_config("local", "local-fast");
    config.set_routing_mode(RoutingMode::Intercept);
    let state = test_state_with_config(config);
    let body = json!({
        "model": "agent-claude",
        "stream": true,
        "max_tokens": 100,
        "messages": [{ "role": "user", "content": "say hello" }]
    });

    let response =
        anthropic_messages(State(SharedState::new(state)), HeaderMap::new(), Json(body)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let events = collect_sse_events(response).await;
    assert!(events.iter().any(|(event, value)| {
        event == "content_block_delta"
            && value["delta"]["type"] == "text_delta"
            && value["delta"]["text"] == "local answer"
    }));
    assert!(events.iter().any(|(event, _)| event == "message_stop"));
    local_server.abort();
}

#[tokio::test]
async fn interceptor_failure_falls_back_through_existing_external_route() {
    let local_app = Router::new().route(
        "/v1/chat/completions",
        post(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "local failed") }),
    );
    let (local_base_url, local_server) = spawn_test_upstream(local_app).await;
    let external_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let external_calls_for_handler = external_calls.clone();
    let external_app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let external_calls = external_calls_for_handler.clone();
            async move {
                external_calls.fetch_add(1, Ordering::Relaxed);
                Json(chat_completion(
                    "destination-model-y",
                    json!("external fallback"),
                    json!([]),
                ))
            }
        }),
    );
    let (external_base_url, external_server) = spawn_test_upstream(external_app).await;
    let mut config = Config::default();
    config.upstream.base_url = external_base_url;
    config.providers.insert(
        "local".to_string(),
        UpstreamConfig {
            base_url: local_base_url,
            ..UpstreamConfig::default()
        },
    );
    config.interceptor = local_interceptor_config("local", "local-fast");
    config.model_aliases.insert(
        "agent-openai".to_string(),
        ModelAlias::Model("destination-model-y".to_string()),
    );
    config.set_routing_mode(RoutingMode::Intercept);
    let state = test_state_with_config(config);
    let body = json!({
        "model": "agent-openai",
        "messages": [{ "role": "user", "content": "hard task" }]
    });

    let response =
        chat_completions(State(SharedState::new(state)), HeaderMap::new(), Json(body)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let response_body: Value =
        serde_json::from_str(&collect_response_text(response).await).unwrap();
    assert_eq!(
        response_body["choices"][0]["message"]["content"],
        "external fallback"
    );
    assert_eq!(external_calls.load(Ordering::Relaxed), 1);
    local_server.abort();
    external_server.abort();
}

#[tokio::test]
async fn codex_pass_forwards_login_headers_and_preserves_model() {
    let local_app = Router::new().route(
        "/v1/chat/completions",
        post(|| async { Json(pass_completion()) }),
    );
    let (local_base_url, local_server) = spawn_test_upstream(local_app).await;

    let external_body = Arc::new(Mutex::new(None::<Value>));
    let external_headers = Arc::new(Mutex::new(None::<HeaderMap>));
    let external_body_for_handler = external_body.clone();
    let external_headers_for_handler = external_headers.clone();
    let external_app = Router::new().route(
            "/v1/responses",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let external_body = external_body_for_handler.clone();
                let external_headers = external_headers_for_handler.clone();
                async move {
                    *external_body.lock().unwrap() = Some(body);
                    *external_headers.lock().unwrap() = Some(headers);
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(CONTENT_TYPE, "text/event-stream")
                        .header("x-codex-turn-state", "sticky-route")
                        .header("x-oai-request-id", "oai-request-123")
                        .header("x-ratelimit-remaining-requests", "42")
                        .header("x-reasoning-included", "true")
                        .body(Body::from(
                            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-external\"}}\n\n",
                        ))
                        .unwrap()
                }
            }),
        );
    let (external_base_url, external_server) = spawn_test_upstream(external_app).await;

    let mut config = Config {
        default_provider: "external".to_string(),
        providers: HashMap::from([
            (
                "external".to_string(),
                UpstreamConfig {
                    base_url: external_base_url,
                    ..UpstreamConfig::default()
                },
            ),
            (
                "local".to_string(),
                UpstreamConfig {
                    base_url: local_base_url,
                    ..UpstreamConfig::default()
                },
            ),
        ]),
        interceptor: local_interceptor_config("local", "local-fast"),
        responses: ResponsesConfig {
            mode: ResponsesMode::Passthrough,
        },
        model_routing: ModelRoutingMode::Passthrough,
        ..Config::default()
    };
    config.model_aliases.insert(
        "gpt-codex".to_string(),
        ModelAlias::Model("must-not-rewrite".to_string()),
    );
    config.set_routing_mode(RoutingMode::Intercept);
    let state = test_state_with_config(config);
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_static("Bearer placeholder"),
    );
    headers.insert(
        HeaderName::from_static("chatgpt-account-id"),
        HeaderValue::from_static("account-123"),
    );
    headers.insert(
        HeaderName::from_static("originator"),
        HeaderValue::from_static("codex_cli_rs"),
    );
    headers.insert(
        HeaderName::from_static("session-id"),
        HeaderValue::from_static("session-123"),
    );
    headers.insert(
        HeaderName::from_static("x-codex-beta-features"),
        HeaderValue::from_static("feature-a"),
    );
    headers.insert(
        HeaderName::from_static("x-client-request-id"),
        HeaderValue::from_static("client-request-123"),
    );
    headers.insert(
        HeaderName::from_static("x-oai-attestation"),
        HeaderValue::from_static("attestation-123"),
    );
    let body = json!({
        "model": "gpt-codex",
        "stream": true,
        "input": "complex external task"
    });

    let response = responses(State(SharedState::new(state)), headers, Json(body)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-codex-turn-state")
            .and_then(|value| value.to_str().ok()),
        Some("sticky-route")
    );
    assert_eq!(
        response
            .headers()
            .get("x-oai-request-id")
            .and_then(|value| value.to_str().ok()),
        Some("oai-request-123")
    );
    assert_eq!(
        response
            .headers()
            .get("x-ratelimit-remaining-requests")
            .and_then(|value| value.to_str().ok()),
        Some("42")
    );
    assert_eq!(
        response
            .headers()
            .get("x-reasoning-included")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert!(
        collect_response_text(response)
            .await
            .contains("response.completed")
    );

    assert_eq!(
        external_body.lock().unwrap().as_ref().unwrap()["model"],
        "gpt-codex"
    );
    let captured_headers = external_headers.lock().unwrap();
    let captured_headers = captured_headers.as_ref().unwrap();
    for (name, expected) in [
        ("authorization", "Bearer placeholder"),
        ("chatgpt-account-id", "account-123"),
        ("originator", "codex_cli_rs"),
        ("session-id", "session-123"),
        ("x-codex-beta-features", "feature-a"),
        ("x-client-request-id", "client-request-123"),
        ("x-oai-attestation", "attestation-123"),
    ] {
        assert_eq!(
            captured_headers
                .get(name)
                .and_then(|value| value.to_str().ok()),
            Some(expected)
        );
    }

    local_server.abort();
    external_server.abort();
}

#[tokio::test]
async fn launcher_codex_session_attaches_to_normal_proxy_and_preserves_model() {
    let local_body = Arc::new(Mutex::new(None::<Value>));
    let local_headers = Arc::new(Mutex::new(None::<HeaderMap>));
    let local_body_for_handler = local_body.clone();
    let local_headers_for_handler = local_headers.clone();
    let local_app = Router::new().route(
        "/v1/chat/completions",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let local_body = local_body_for_handler.clone();
            let local_headers = local_headers_for_handler.clone();
            async move {
                *local_body.lock().unwrap() = Some(body);
                *local_headers.lock().unwrap() = Some(headers);
                Json(pass_completion())
            }
        }),
    );
    let (local_base_url, local_server) = spawn_test_upstream(local_app).await;

    let external_body = Arc::new(Mutex::new(None::<Value>));
    let external_body_for_handler = external_body.clone();
    let external_app = Router::new().route(
            "/v1/responses",
            post(move |Json(body): Json<Value>| {
                let external_body = external_body_for_handler.clone();
                async move {
                    *external_body.lock().unwrap() = Some(body);
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(CONTENT_TYPE, "text/event-stream")
                        .body(Body::from(
                            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-external\"}}\n\n",
                        ))
                        .unwrap()
                }
            }),
        );
    let (external_base_url, external_server) = spawn_test_upstream(external_app).await;

    let mut interceptor = local_interceptor_config("local", "local-fast");
    interceptor.enabled = false;
    let mut config = Config::default();
    config.providers.insert(
        "local".to_string(),
        UpstreamConfig {
            base_url: local_base_url,
            ..UpstreamConfig::default()
        },
    );
    config.interceptor = interceptor;
    config.model_aliases.insert(
        "gpt-codex".to_string(),
        ModelAlias::Model("must-not-rewrite".to_string()),
    );
    config.set_routing_mode(RoutingMode::Intercept);

    let token = LauncherSession::new(LauncherKind::Codex, external_base_url)
        .unwrap()
        .encode()
        .unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_static("Bearer placeholder"),
    );
    let body = json!({
        "model": "gpt-codex",
        "stream": true,
        "input": "complex external task"
    });

    let response = launcher_responses(
        AxumPath(token),
        State(SharedState::new(test_state_with_config(config))),
        headers,
        Json(body),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        collect_response_text(response)
            .await
            .contains("response.completed")
    );

    assert_eq!(
        local_body.lock().unwrap().as_ref().unwrap()["model"],
        "local-fast"
    );
    assert!(
        local_headers
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .get(AUTHORIZATION)
            .is_none()
    );
    assert_eq!(
        external_body.lock().unwrap().as_ref().unwrap()["model"],
        "gpt-codex"
    );

    local_server.abort();
    external_server.abort();
}

#[tokio::test]
async fn proxy_launchers_only_remap_protocols_to_the_default_provider() {
    let captured = Arc::new(Mutex::new(Vec::<(HeaderMap, Value)>::new()));
    let captured_for_handler = captured.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let captured = captured_for_handler.clone();
            async move {
                captured.lock().unwrap().push((headers, body));
                Json(chat_completion(
                    "local-model",
                    json!("local proxy response"),
                    json!([]),
                ))
            }
        }),
    );
    let (local_base_url, local_server) = spawn_test_upstream(app).await;
    let config = Config {
        default_provider: "local".to_string(),
        providers: HashMap::from([
            (
                "local".to_string(),
                UpstreamConfig {
                    base_url: local_base_url,
                    api_key: Some("provider-owned-key".to_string()),
                    ..UpstreamConfig::default()
                },
            ),
            (
                "external".to_string(),
                UpstreamConfig {
                    base_url: "http://external.invalid/v1".to_string(),
                    api_key: Some("must-not-be-used".to_string()),
                    ..UpstreamConfig::default()
                },
            ),
        ]),
        interceptor: InterceptorConfig {
            reject_sensitive: true,
            ..local_interceptor_config("local", "interceptor-model")
        },
        model_aliases: HashMap::from([(
            "agent-model".to_string(),
            ModelAlias::Route {
                provider: "external".to_string(),
                model: "external-model".to_string(),
            },
        )]),
        ..Config::default()
    };
    let shared = SharedState::new(test_state_with_config(config));
    let mut caller_headers = HeaderMap::new();
    caller_headers.insert(
        AUTHORIZATION,
        HeaderValue::from_static("Bearer caller-secret"),
    );
    caller_headers.insert("x-api-key", HeaderValue::from_static("caller-secret"));

    let codex_token = LauncherSession::local(LauncherKind::Codex, LauncherMode::Proxy)
        .unwrap()
        .encode()
        .unwrap();
    let codex_response = launcher_responses(
        AxumPath(codex_token),
        State(shared.clone()),
        caller_headers.clone(),
        Json(json!({
            "model": "local/local-model",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": "from codex" }]
            }]
        })),
    )
    .await;
    assert_eq!(codex_response.status(), StatusCode::OK);
    assert!(
        collect_response_text(codex_response)
            .await
            .contains("local proxy response")
    );

    let claude_token = LauncherSession::local(LauncherKind::Claude, LauncherMode::Proxy)
        .unwrap()
        .encode()
        .unwrap();
    let claude_response = launcher_anthropic_messages(
        AxumPath(claude_token),
        State(shared.clone()),
        caller_headers.clone(),
        Json(json!({
            "model": "local/local-model",
            "max_tokens": 64,
            "messages": [{ "role": "user", "content": "from claude" }]
        })),
    )
    .await;
    assert_eq!(claude_response.status(), StatusCode::OK);
    assert!(
        collect_response_text(claude_response)
            .await
            .contains("local proxy response")
    );

    let copilot_token = LauncherSession::local(LauncherKind::Copilot, LauncherMode::Proxy)
        .unwrap()
        .encode()
        .unwrap();
    let copilot_response = launcher_chat_completions(
        AxumPath(copilot_token),
        State(shared),
        caller_headers,
        Json(json!({
            "model": "local/local-model",
            "messages": [{ "role": "user", "content": "from copilot" }]
        })),
    )
    .await;
    assert_eq!(copilot_response.status(), StatusCode::OK);
    assert!(
        collect_response_text(copilot_response)
            .await
            .contains("local proxy response")
    );

    let captured = captured.lock().unwrap();
    assert_eq!(
        captured.len(),
        3,
        "proxy mode must not make an interceptor call"
    );
    for (headers, body) in captured.iter() {
        assert_eq!(body["model"], "local-model");
        assert!(
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|message| message["role"] != "system")
        );
        let serialized = body.to_string();
        assert!(!serialized.contains("PASS"));
        assert!(!serialized.contains("ROUTE_DEFAULT"));
        assert!(!serialized.contains("PROVIDER_"));
        assert_eq!(
            headers
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer provider-owned-key")
        );
        assert!(headers.get("x-api-key").is_none());
    }
    drop(captured);
    local_server.abort();
}

#[tokio::test]
async fn proxy_codex_control_requests_stay_on_the_default_provider() {
    let captured = Arc::new(Mutex::new(None::<(HeaderMap, Value)>));
    let captured_for_handler = captured.clone();
    let app = Router::new().route(
        "/v1/responses/compact",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let captured = captured_for_handler.clone();
            async move {
                *captured.lock().unwrap() = Some((headers, body));
                Json(json!({ "id": "compact-local", "output": [] }))
            }
        }),
    );
    let (local_base_url, local_server) = spawn_test_upstream(app).await;
    let config = Config {
        upstream: UpstreamConfig {
            base_url: local_base_url,
            api_key: Some("provider-owned-key".to_string()),
            ..UpstreamConfig::default()
        },
        ..Config::default()
    };
    let token = LauncherSession::local(LauncherKind::Codex, LauncherMode::Proxy)
        .unwrap()
        .encode()
        .unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_static("Bearer caller-secret"),
    );
    let response = launcher_codex_responses_compact(
        AxumPath(token),
        State(SharedState::new(test_state_with_config(config))),
        headers,
        Json(json!({ "model": "default/local-model", "input": [] })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let (headers, body) = captured.lock().unwrap().clone().unwrap();
    assert_eq!(body["model"], "local-model");
    assert_eq!(
        headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer provider-owned-key")
    );
    local_server.abort();
}

#[tokio::test]
async fn launcher_copilot_pass_is_credential_blind_and_preserves_native_chat() {
    let local_body = Arc::new(Mutex::new(None::<Value>));
    let local_headers = Arc::new(Mutex::new(None::<HeaderMap>));
    let local_body_for_handler = local_body.clone();
    let local_headers_for_handler = local_headers.clone();
    let local_app = Router::new().route(
        "/v1/chat/completions",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let local_body = local_body_for_handler.clone();
            let local_headers = local_headers_for_handler.clone();
            async move {
                *local_body.lock().unwrap() = Some(body);
                *local_headers.lock().unwrap() = Some(headers);
                Json(pass_completion())
            }
        }),
    );
    let (local_base_url, local_server) = spawn_test_upstream(local_app).await;

    let external_body = Arc::new(Mutex::new(None::<Value>));
    let external_headers = Arc::new(Mutex::new(None::<HeaderMap>));
    let external_body_for_handler = external_body.clone();
    let external_headers_for_handler = external_headers.clone();
    let external_app = Router::new().route(
        "/chat/completions",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let external_body = external_body_for_handler.clone();
            let external_headers = external_headers_for_handler.clone();
            async move {
                *external_body.lock().unwrap() = Some(body);
                *external_headers.lock().unwrap() = Some(headers);
                Response::builder()
                    .status(StatusCode::OK)
                    .header(CONTENT_TYPE, "application/json")
                    .header("x-github-request-id", "req-copilot")
                    .header("x-copilot-model", "gpt-5.4")
                    .body(Body::from(
                        json!({
                            "id": "chatcmpl-copilot",
                            "object": "chat.completion",
                            "choices": [{
                                "index": 0,
                                "message": { "role": "assistant", "content": "external" },
                                "finish_reason": "stop"
                            }]
                        })
                        .to_string(),
                    ))
                    .unwrap()
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let external_addr = listener.local_addr().unwrap();
    let external_server = tokio::spawn(async move {
        axum::serve(listener, external_app).await.unwrap();
    });
    let external_base_url = format!("http://{external_addr}");

    let mut config = Config::default();
    config.providers.insert(
        "local".to_string(),
        UpstreamConfig {
            base_url: local_base_url,
            ..UpstreamConfig::default()
        },
    );
    config.interceptor = local_interceptor_config("local", "local-fast");
    config
        .role_map
        .insert("developer".to_string(), "system-must-not-leak".to_string());
    config.model_aliases.insert(
        "gpt-5.4".to_string(),
        ModelAlias::Model("must-not-rewrite".to_string()),
    );
    config.set_routing_mode(RoutingMode::Intercept);

    let token = LauncherSession::new(LauncherKind::Copilot, external_base_url)
        .unwrap()
        .encode()
        .unwrap();
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("authorization", "Bearer placeholder"),
        ("copilot-integration-id", "copilot-cli"),
        ("openai-intent", "conversation-agent"),
        ("x-github-api-version", "2025-04-01"),
        ("x-initiator", "user"),
        ("x-interaction-id", "interaction-123"),
        ("x-copilot-client-timezone", "Europe/London"),
    ] {
        headers.insert(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    let original = json!({
        "model": "gpt-5.4",
        "stream": false,
        "messages": [
            { "role": "developer", "content": "Use AGENTS.md." },
            { "role": "user", "content": "complex external task" }
        ],
        "tools": [{
            "type": "function",
            "function": {
                "name": "shell",
                "description": "Run a command",
                "parameters": { "type": "object" }
            }
        }],
        "copilot_thread_id": "thread-123"
    });

    let response = launcher_chat_completions(
        AxumPath(token),
        State(SharedState::new(test_state_with_config(config))),
        headers,
        Json(original.clone()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-github-request-id")
            .and_then(|value| value.to_str().ok()),
        Some("req-copilot")
    );
    assert_eq!(
        response
            .headers()
            .get("x-copilot-model")
            .and_then(|value| value.to_str().ok()),
        Some("gpt-5.4")
    );

    assert_eq!(*external_body.lock().unwrap(), Some(original));
    let captured_external_headers = external_headers.lock().unwrap();
    let captured_external_headers = captured_external_headers.as_ref().unwrap();
    for (name, expected) in [
        ("authorization", "Bearer placeholder"),
        ("copilot-integration-id", "copilot-cli"),
        ("openai-intent", "conversation-agent"),
        ("x-github-api-version", "2025-04-01"),
        ("x-initiator", "user"),
        ("x-interaction-id", "interaction-123"),
        ("x-copilot-client-timezone", "Europe/London"),
    ] {
        assert_eq!(
            captured_external_headers
                .get(name)
                .and_then(|value| value.to_str().ok()),
            Some(expected)
        );
    }

    assert_eq!(
        local_body.lock().unwrap().as_ref().unwrap()["model"],
        "local-fast"
    );
    let captured_local_headers = local_headers.lock().unwrap();
    let captured_local_headers = captured_local_headers.as_ref().unwrap();
    for name in [
        "authorization",
        "copilot-integration-id",
        "openai-intent",
        "x-github-api-version",
        "x-interaction-id",
        "x-copilot-client-timezone",
    ] {
        assert!(captured_local_headers.get(name).is_none(), "leaked {name}");
    }

    local_server.abort();
    external_server.abort();
}

#[tokio::test]
async fn launcher_copilot_models_relays_native_status_and_headers() {
    let captured_headers = Arc::new(Mutex::new(None::<HeaderMap>));
    let captured_headers_for_handler = captured_headers.clone();
    let external_app = Router::new().route(
        "/models",
        get(move |headers: HeaderMap| {
            let captured_headers = captured_headers_for_handler.clone();
            async move {
                *captured_headers.lock().unwrap() = Some(headers);
                Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .header(CONTENT_TYPE, "application/json")
                    .header("x-github-request-id", "req-models")
                    .body(Body::from(r#"{"error":"authentication failed"}"#))
                    .unwrap()
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let external_addr = listener.local_addr().unwrap();
    let external_server = tokio::spawn(async move {
        axum::serve(listener, external_app).await.unwrap();
    });

    let token = LauncherSession::new(LauncherKind::Copilot, format!("http://{external_addr}"))
        .unwrap()
        .encode()
        .unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_static("Bearer placeholder"),
    );
    headers.insert(
        HeaderName::from_static("copilot-integration-id"),
        HeaderValue::from_static("copilot-cli"),
    );

    let mut config = Config::default();
    config.upstream.base_url = "http://unused.invalid/v1".to_string();
    config.interceptor = local_interceptor_config("default", "local-fast");
    config.set_routing_mode(RoutingMode::Intercept);
    let response = launcher_copilot_models(
        AxumPath(token),
        State(SharedState::new(test_state_with_config(config))),
        headers,
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response
            .headers()
            .get("x-github-request-id")
            .and_then(|value| value.to_str().ok()),
        Some("req-models")
    );
    let captured_headers = captured_headers.lock().unwrap();
    let captured_headers = captured_headers.as_ref().unwrap();
    assert_eq!(
        captured_headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer placeholder")
    );
    assert_eq!(
        captured_headers
            .get("copilot-integration-id")
            .and_then(|value| value.to_str().ok()),
        Some("copilot-cli")
    );

    external_server.abort();
}

#[tokio::test]
async fn launcher_copilot_preserves_native_anthropic_capi_path() {
    let local_app = Router::new().route(
        "/v1/chat/completions",
        post(|| async { Json(pass_completion()) }),
    );
    let (local_base_url, local_server) = spawn_test_upstream(local_app).await;

    let external_calls = Arc::new(AtomicU64::new(0));
    let external_calls_for_handler = external_calls.clone();
    let external_app = Router::new().route(
        "/v1/messages",
        post(move || {
            let external_calls = external_calls_for_handler.clone();
            async move {
                external_calls.fetch_add(1, Ordering::Relaxed);
                Response::builder()
                    .status(StatusCode::OK)
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({
                            "id": "msg-copilot",
                            "type": "message",
                            "role": "assistant",
                            "content": [{ "type": "text", "text": "external" }],
                            "model": "claude-sonnet-4.5",
                            "stop_reason": "end_turn",
                            "usage": { "input_tokens": 1, "output_tokens": 1 }
                        })
                        .to_string(),
                    ))
                    .unwrap()
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let external_addr = listener.local_addr().unwrap();
    let external_server = tokio::spawn(async move {
        axum::serve(listener, external_app).await.unwrap();
    });

    let mut config = Config::default();
    config.providers.insert(
        "local".to_string(),
        UpstreamConfig {
            base_url: local_base_url,
            ..UpstreamConfig::default()
        },
    );
    config.interceptor = local_interceptor_config("local", "local-fast");
    config.set_routing_mode(RoutingMode::Intercept);
    let token = LauncherSession::new(LauncherKind::Copilot, format!("http://{external_addr}"))
        .unwrap()
        .encode()
        .unwrap();
    let original = json!({
        "model": "claude-sonnet-4.5",
        "max_tokens": 100,
        "messages": [{ "role": "user", "content": "complex external task" }]
    });

    let response = launcher_anthropic_messages(
        AxumPath(token),
        State(SharedState::new(test_state_with_config(config))),
        HeaderMap::new(),
        Json(original),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(external_calls.load(Ordering::Relaxed), 1);

    local_server.abort();
    external_server.abort();
}

#[tokio::test]
async fn launcher_codex_control_request_bypasses_local_interceptor() {
    let local_calls = Arc::new(AtomicU64::new(0));
    let local_calls_for_handler = local_calls.clone();
    let local_app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let local_calls = local_calls_for_handler.clone();
            async move {
                local_calls.fetch_add(1, Ordering::Relaxed);
                Json(chat_completion(
                    "local-fast",
                    json!("incorrect local response"),
                    json!([]),
                ))
            }
        }),
    );
    let (local_base_url, local_server) = spawn_test_upstream(local_app).await;

    let external_calls = Arc::new(AtomicU64::new(0));
    let external_body = Arc::new(Mutex::new(None::<Value>));
    let external_headers = Arc::new(Mutex::new(None::<HeaderMap>));
    let external_calls_for_handler = external_calls.clone();
    let external_body_for_handler = external_body.clone();
    let external_headers_for_handler = external_headers.clone();
    let external_app = Router::new().route(
            "/v1/responses",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let external_calls = external_calls_for_handler.clone();
                let external_body = external_body_for_handler.clone();
                let external_headers = external_headers_for_handler.clone();
                async move {
                    external_calls.fetch_add(1, Ordering::Relaxed);
                    *external_body.lock().unwrap() = Some(body);
                    *external_headers.lock().unwrap() = Some(headers);
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(CONTENT_TYPE, "text/event-stream")
                        .body(Body::from(
                            "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"cmp-1\",\"type\":\"compaction\",\"encrypted_content\":\"ciphertext\"}}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-compact\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
                        ))
                        .unwrap()
                }
            }),
        );
    let (external_base_url, external_server) = spawn_test_upstream(external_app).await;

    let mut interceptor = local_interceptor_config("local", "local-fast");
    interceptor.enabled = false;
    let mut config = Config::default();
    config.providers.insert(
        "local".to_string(),
        UpstreamConfig {
            base_url: local_base_url,
            ..UpstreamConfig::default()
        },
    );
    config.interceptor = interceptor;
    config.set_routing_mode(RoutingMode::Intercept);

    let token = LauncherSession::new(LauncherKind::Codex, external_base_url)
        .unwrap()
        .encode()
        .unwrap();
    let turn_metadata = json!({
        "request_kind": "compaction",
        "compaction": { "implementation": "responses_compaction_v2" }
    })
    .to_string();
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_static("Bearer placeholder"),
    );
    headers.insert(
        HeaderName::from_static("x-codex-turn-metadata"),
        HeaderValue::from_str(&turn_metadata).unwrap(),
    );
    let body = json!({
        "model": "gpt-codex",
        "stream": true,
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": "history" }]
            },
            { "type": "compaction_trigger" }
        ],
        "client_metadata": {
            "x-codex-turn-metadata": turn_metadata
        }
    });

    let response = launcher_responses(
        AxumPath(token),
        State(SharedState::new(test_state_with_config(config))),
        headers,
        Json(body.clone()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response_body = collect_response_text(response).await;
    assert!(response_body.contains("\"type\":\"compaction\""));
    assert!(response_body.contains("response.completed"));

    assert_eq!(local_calls.load(Ordering::Relaxed), 0);
    assert_eq!(external_calls.load(Ordering::Relaxed), 1);
    assert_eq!(external_body.lock().unwrap().as_ref(), Some(&body));
    assert!(
        external_headers
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .contains_key("x-codex-turn-metadata")
    );

    local_server.abort();
    external_server.abort();
}

#[tokio::test]
async fn claude_pass_forwards_login_headers_and_preserves_native_request() {
    let local_headers = Arc::new(Mutex::new(None::<HeaderMap>));
    let local_headers_for_handler = local_headers.clone();
    let local_app = Router::new().route(
        "/v1/chat/completions",
        post(move |headers: HeaderMap| {
            let local_headers = local_headers_for_handler.clone();
            async move {
                *local_headers.lock().unwrap() = Some(headers);
                Json(pass_completion())
            }
        }),
    );
    let (local_base_url, local_server) = spawn_test_upstream(local_app).await;

    let external_body = Arc::new(Mutex::new(None::<Value>));
    let external_headers = Arc::new(Mutex::new(None::<HeaderMap>));
    let external_body_for_handler = external_body.clone();
    let external_headers_for_handler = external_headers.clone();
    let external_app = Router::new().route(
            "/v1/messages",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let external_body = external_body_for_handler.clone();
                let external_headers = external_headers_for_handler.clone();
                async move {
                    *external_body.lock().unwrap() = Some(body);
                    *external_headers.lock().unwrap() = Some(headers);
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(CONTENT_TYPE, "text/event-stream")
                        .header("request-id", "req-anthropic")
                        .header("anthropic-ratelimit-requests-remaining", "41")
                        .body(Body::from(
                            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-external\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-test\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                        ))
                        .unwrap()
                }
            }),
        );
    let (external_base_url, external_server) = spawn_test_upstream(external_app).await;

    let mut config = Config {
        default_provider: "external".to_string(),
        providers: HashMap::from([
            (
                "external".to_string(),
                UpstreamConfig {
                    base_url: external_base_url,
                    ..UpstreamConfig::default()
                },
            ),
            (
                "local".to_string(),
                UpstreamConfig {
                    base_url: local_base_url,
                    ..UpstreamConfig::default()
                },
            ),
        ]),
        interceptor: local_interceptor_config("local", "local-fast"),
        model_routing: ModelRoutingMode::Passthrough,
        anthropic_mode: AnthropicMode::Passthrough,
        ..Config::default()
    };
    config.model_aliases.insert(
        "claude-sonnet-test".to_string(),
        ModelAlias::Model("must-not-rewrite".to_string()),
    );
    config.set_routing_mode(RoutingMode::Intercept);
    let state = test_state_with_config(config);
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("x-api-key"),
        HeaderValue::from_static("api-key-from-claude"),
    );
    headers.insert(
        HeaderName::from_static("anthropic-version"),
        HeaderValue::from_static("2023-06-01"),
    );
    headers.insert(
        HeaderName::from_static("anthropic-beta"),
        HeaderValue::from_static("oauth-2025-04-20"),
    );
    headers.insert(
        HeaderName::from_static("x-stainless-runtime"),
        HeaderValue::from_static("node"),
    );
    let original = json!({
        "model": "claude-sonnet-test",
        "stream": true,
        "max_tokens": 1024,
        "system": [{ "type": "text", "text": "Use the repository instructions." }],
        "messages": [{ "role": "user", "content": "complex external task" }],
        "tools": [{
            "name": "Bash",
            "description": "Run a command",
            "input_schema": { "type": "object" }
        }],
        "metadata": { "user_id": "session-test" }
    });

    let response = anthropic_messages(
        State(SharedState::new(state)),
        headers,
        Json(original.clone()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("request-id")
            .and_then(|value| value.to_str().ok()),
        Some("req-anthropic")
    );
    assert_eq!(
        response
            .headers()
            .get("anthropic-ratelimit-requests-remaining")
            .and_then(|value| value.to_str().ok()),
        Some("41")
    );
    assert!(
        collect_response_text(response)
            .await
            .contains("message_stop")
    );

    assert_eq!(*external_body.lock().unwrap(), Some(original));
    let captured_headers = external_headers.lock().unwrap();
    let captured_headers = captured_headers.as_ref().unwrap();
    for (name, expected) in [
        ("x-api-key", "api-key-from-claude"),
        ("anthropic-version", "2023-06-01"),
        ("anthropic-beta", "oauth-2025-04-20"),
        ("x-stainless-runtime", "node"),
    ] {
        assert_eq!(
            captured_headers
                .get(name)
                .and_then(|value| value.to_str().ok()),
            Some(expected)
        );
    }
    let local_headers = local_headers.lock().unwrap();
    let local_headers = local_headers.as_ref().unwrap();
    assert!(local_headers.get("x-api-key").is_none());
    assert!(local_headers.get(AUTHORIZATION).is_none());

    local_server.abort();
    external_server.abort();
}

#[tokio::test]
async fn launcher_claude_session_attaches_to_normal_proxy_and_preserves_request() {
    let local_body = Arc::new(Mutex::new(None::<Value>));
    let local_headers = Arc::new(Mutex::new(None::<HeaderMap>));
    let local_body_for_handler = local_body.clone();
    let local_headers_for_handler = local_headers.clone();
    let local_app = Router::new().route(
        "/v1/chat/completions",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let local_body = local_body_for_handler.clone();
            let local_headers = local_headers_for_handler.clone();
            async move {
                *local_body.lock().unwrap() = Some(body);
                *local_headers.lock().unwrap() = Some(headers);
                Json(pass_completion())
            }
        }),
    );
    let (local_base_url, local_server) = spawn_test_upstream(local_app).await;

    let external_body = Arc::new(Mutex::new(None::<Value>));
    let external_body_for_handler = external_body.clone();
    let external_app = Router::new().route(
            "/v1/messages",
            post(move |Json(body): Json<Value>| {
                let external_body = external_body_for_handler.clone();
                async move {
                    *external_body.lock().unwrap() = Some(body);
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(CONTENT_TYPE, "text/event-stream")
                        .body(Body::from(
                            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-external\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-test\",\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                        ))
                        .unwrap()
                }
            }),
        );
    let (external_base_url, external_server) = spawn_test_upstream(external_app).await;

    let mut interceptor = local_interceptor_config("local", "local-fast");
    interceptor.enabled = false;
    let mut config = Config::default();
    config.providers.insert(
        "local".to_string(),
        UpstreamConfig {
            base_url: local_base_url,
            ..UpstreamConfig::default()
        },
    );
    config.interceptor = interceptor;
    config.model_aliases.insert(
        "claude-sonnet-test".to_string(),
        ModelAlias::Model("must-not-rewrite".to_string()),
    );
    config.set_routing_mode(RoutingMode::Intercept);

    let token = LauncherSession::new(LauncherKind::Claude, external_base_url)
        .unwrap()
        .encode()
        .unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("x-api-key"),
        HeaderValue::from_static("api-key-from-claude"),
    );
    let original = json!({
        "model": "claude-sonnet-test",
        "stream": true,
        "max_tokens": 1024,
        "messages": [{ "role": "user", "content": "complex external task" }]
    });

    let response = launcher_anthropic_messages(
        AxumPath(token),
        State(SharedState::new(test_state_with_config(config))),
        headers,
        Json(original.clone()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        collect_response_text(response)
            .await
            .contains("message_stop")
    );

    assert_eq!(
        local_body.lock().unwrap().as_ref().unwrap()["model"],
        "local-fast"
    );
    assert!(
        local_headers
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .get("x-api-key")
            .is_none()
    );
    assert_eq!(external_body.lock().unwrap().as_ref().unwrap(), &original);

    local_server.abort();
    external_server.abort();
}

#[test]
fn parses_legacy_and_provider_routed_aliases() {
    let config: Config = toml::from_str(
        r#"
                default_provider = "default"

                [upstream]
                base_url = "http://127.0.0.1:8000/v1"

                [providers.embedding]
                base_url = "http://127.0.0.1:18000/v1"

                [model_aliases]
                chat = "upstream-chat"
                embedding = { provider = "embedding", model = "upstream-embedding" }
            "#,
    )
    .unwrap();

    assert_eq!(
        config.model_aliases.get("chat"),
        Some(&ModelAlias::Model("upstream-chat".to_string()))
    );
    assert_eq!(
        config.model_aliases.get("embedding"),
        Some(&ModelAlias::Route {
            provider: "embedding".to_string(),
            model: "upstream-embedding".to_string(),
        })
    );
    assert_eq!(effective_provider_configs(&config).unwrap().len(), 2);
    validate_config(&config).unwrap();
}

#[test]
fn parses_documented_and_legacy_provider_kind_spellings() {
    for (spelling, expected) in [
        ("openai-compatible", ProviderKind::OpenAiCompatible),
        ("open-ai-compatible", ProviderKind::OpenAiCompatible),
        ("openai", ProviderKind::OpenAi),
        ("open-ai", ProviderKind::OpenAi),
        ("anthropic", ProviderKind::Anthropic),
        ("copilot", ProviderKind::Copilot),
    ] {
        let provider: UpstreamConfig = toml::from_str(&format!("kind = {spelling:?}")).unwrap();
        assert_eq!(provider.kind, expected);
    }
}

#[test]
fn validates_provider_reasoning_effort_by_protocol() {
    let high: UpstreamConfig = toml::from_str("reasoning_effort = \"high\"").unwrap();
    assert_eq!(high.reasoning_effort, Some(ReasoningEffort::High));

    let anthropic: Config =
        toml::from_str("[upstream]\nkind = \"anthropic\"\nreasoning_effort = \"none\"\n").unwrap();
    assert!(validate_config(&anthropic).is_err());

    let copilot: Config =
        toml::from_str("[upstream]\nkind = \"copilot\"\nreasoning_effort = \"high\"\n").unwrap();
    assert!(validate_config(&copilot).is_err());
}

#[test]
fn example_config_parses_and_validates() {
    let config = parse_config(include_str!("../llmproxy.example.toml"), "example").unwrap();
    assert_eq!(config.routing_mode, RoutingMode::Integrated);
    assert!(!config.interceptor.enabled);
    assert!(config.integrated.enabled);
    validate_config(&config).unwrap();
}

#[tokio::test]
async fn routed_embedding_alias_uses_selected_provider_and_model() {
    let seen_body = Arc::new(Mutex::new(None::<Value>));
    let seen_body_for_handler = seen_body.clone();
    let app = Router::new().route(
        "/v1/embeddings",
        post(move |Json(body): Json<Value>| {
            let seen_body = seen_body_for_handler.clone();
            async move {
                *seen_body.lock().unwrap() = Some(body);
                Json(json!({ "object": "list", "data": [] }))
            }
        }),
    );
    let (base_url, server) = spawn_test_upstream(app).await;
    let mut config = Config::default();
    config.providers.insert(
        "embedding".to_string(),
        UpstreamConfig {
            base_url,
            ..UpstreamConfig::default()
        },
    );
    config.model_aliases.insert(
        "embedding".to_string(),
        ModelAlias::Route {
            provider: "embedding".to_string(),
            model: "Qwen/Qwen3-Embedding-0.6B".to_string(),
        },
    );
    let state = test_state_with_config(config);
    let mut body = json!({ "model": "embedding", "input": "hello" });

    let provider = rewrite_proxy_request_value(&state, &HeaderMap::new(), &mut body)
        .await
        .unwrap();
    assert_eq!(provider.name, "embedding");
    assert_eq!(body["model"], "Qwen/Qwen3-Embedding-0.6B");

    let response = proxy_json(&provider, &HeaderMap::new(), "embeddings", &body).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        seen_body.lock().unwrap().as_ref().unwrap()["model"],
        "Qwen/Qwen3-Embedding-0.6B"
    );
    server.abort();
}

#[test]
fn rejects_one_request_routed_to_multiple_providers() {
    let mut config = Config::default();
    config.providers.insert(
        "second".to_string(),
        UpstreamConfig {
            base_url: "http://127.0.0.1:18000/v1".to_string(),
            ..UpstreamConfig::default()
        },
    );
    config.model_aliases.insert(
        "first-model".to_string(),
        ModelAlias::Model("upstream-first".to_string()),
    );
    config.model_aliases.insert(
        "second-model".to_string(),
        ModelAlias::Route {
            provider: "second".to_string(),
            model: "upstream-second".to_string(),
        },
    );
    let state = test_state_with_config(config);
    let response = route_request(
        &state,
        &json!({
            "model": "first-model",
            "nested": { "model": "second-model" }
        }),
    )
    .err()
    .expect("mixed-provider request should fail");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[test]
fn parse_upstream_json_accepts_valid_json_and_rejects_invalid_json() {
    let value = parse_upstream_json(br#"{"data":[{"id":"qwen"}]}"#).unwrap();
    assert_eq!(value["data"][0]["id"], "qwen");

    let large_body = format!(r#"{{"data":"{}"}}"#, "x".repeat(SIMD_JSON_MIN_BYTES));
    let value = parse_upstream_json(large_body.as_bytes()).unwrap();
    assert_eq!(value["data"].as_str().unwrap().len(), SIMD_JSON_MIN_BYTES);

    let error = parse_upstream_json(br#"{"data":["#).unwrap_err();
    assert!(!error.is_empty());
}

#[test]
fn rewrites_model_alias_and_roles_recursively() {
    let mut value = json!({
        "model": "codex-qwen",
        "messages": [
            { "role": "developer", "content": "be concise" },
            { "role": "user", "content": "hi" }
        ],
        "nested": { "role": "developer" }
    });

    let provider = test_provider("http://127.0.0.1:1/v1".to_string());
    rewrite_request_value(&mut value, &test_config(), &provider, None);

    assert_eq!(value["model"], "Qwen/Qwen2.5-Coder-32B-Instruct");
    assert_eq!(value["messages"][0]["role"], "system");
    assert_eq!(value["nested"]["role"], "system");
}

#[test]
fn rewrites_non_alias_models_to_default_upstream_model() {
    let mut value = json!({
        "model": "gpt-5.5",
        "messages": [
            { "role": "developer", "content": "be concise" }
        ],
        "nested": { "model": "codex-qwen" }
    });
    let provider = test_provider("http://127.0.0.1:1/v1".to_string());

    assert!(request_needs_default_model(&value, &test_config()));
    rewrite_request_value(
        &mut value,
        &test_config(),
        &provider,
        Some("Qwen/Qwen3-Coder-30B-A3B-Instruct"),
    );

    assert_eq!(value["model"], "Qwen/Qwen3-Coder-30B-A3B-Instruct");
    assert_eq!(value["messages"][0]["role"], "system");
    assert_eq!(value["nested"]["model"], "Qwen/Qwen2.5-Coder-32B-Instruct");
}

#[tokio::test]
async fn namespaced_models_select_provider_and_strip_only_the_prefix() {
    let mut config = Config {
        default_provider: "primary".to_string(),
        providers: HashMap::from([
            ("primary".to_string(), UpstreamConfig::default()),
            ("anthropic".to_string(), UpstreamConfig::default()),
        ]),
        ..Config::default()
    };
    config.model_aliases.insert(
        "anthropic/pinned".to_string(),
        ModelAlias::Route {
            provider: "primary".to_string(),
            model: "alias-wins".to_string(),
        },
    );
    let state = test_state_with_config(config);

    let mut dynamic = json!({ "model": "anthropic/claude/test" });
    let provider = rewrite_proxy_request_value(&state, &HeaderMap::new(), &mut dynamic)
        .await
        .unwrap();
    assert_eq!(provider.name, "anthropic");
    assert_eq!(dynamic["model"], "claude/test");

    let mut exact_alias = json!({ "model": "anthropic/pinned" });
    let provider = rewrite_proxy_request_value(&state, &HeaderMap::new(), &mut exact_alias)
        .await
        .unwrap();
    assert_eq!(provider.name, "primary");
    assert_eq!(exact_alias["model"], "alias-wins");
}

#[tokio::test]
async fn integrated_explicit_raw_model_is_preserved_on_default_provider() {
    let mut config = test_config();
    config.set_routing_mode(RoutingMode::Integrated);
    let state = test_state_with_config(config);
    state
        .default_provider()
        .model_cache
        .replace_upstream_models(
            vec!["fallback-model".to_string()],
            json!({ "data": [{ "id": "fallback-model" }] }),
        );
    let mut request = json!({ "model": "selected-model" });

    let provider = rewrite_proxy_request_value(&state, &HeaderMap::new(), &mut request)
        .await
        .unwrap();

    assert_eq!(provider.name, state.config.default_provider);
    assert_eq!(request["model"], "selected-model");
}

#[test]
fn provider_models_include_typed_namespaced_ids() {
    let mut data = Vec::new();
    append_provider_models(
        &mut data,
        json!({ "data": [{ "id": "model-a", "object": "model" }] }),
        "provider",
        true,
    );
    assert!(data.iter().any(|model| model["id"] == "model-a"));
    let namespaced = data
        .iter()
        .find(|model| model["id"] == "provider/model-a")
        .unwrap();
    assert_eq!(namespaced["proxy_target"], "model-a");
    assert_eq!(namespaced["proxy_provider"], "provider");
    assert_eq!(namespaced["proxy_route"], "namespaced");

    let mut namespaced_only = Vec::new();
    append_provider_models(
        &mut namespaced_only,
        json!({ "data": [{ "id": "model-a", "object": "model" }] }),
        "provider",
        false,
    );
    assert_eq!(namespaced_only.len(), 1);
    assert_eq!(namespaced_only[0]["id"], "provider/model-a");
}

#[test]
fn extracts_upstream_model_ids_in_order() {
    let upstream = json!({
        "object": "list",
        "data": [
            { "id": "first-model" },
            { "id": "second-model" },
            { "id": "first-model" },
            { "object": "model" }
        ]
    });

    assert_eq!(
        extract_model_ids(&upstream),
        vec!["first-model", "second-model"]
    );
}

#[tokio::test]
async fn anthropic_model_discovery_collects_paginated_results() {
    let calls = Arc::new(AtomicU64::new(0));
    let calls_for_handler = calls.clone();
    let app = Router::new().route(
        "/v1/models",
        get(
            move |axum::extract::Query(query): axum::extract::Query<HashMap<String, String>>| {
                let calls = calls_for_handler.clone();
                async move {
                    calls.fetch_add(1, Ordering::Relaxed);
                    if query.get("after_id").map(String::as_str) == Some("model/one") {
                        Json(json!({
                            "data": [{ "id": "model-two" }],
                            "first_id": "model-two",
                            "last_id": "model-two",
                            "has_more": false
                        }))
                    } else {
                        Json(json!({
                            "data": [{ "id": "model/one" }],
                            "first_id": "model/one",
                            "last_id": "model/one",
                            "has_more": true
                        }))
                    }
                }
            },
        ),
    );
    let (base_url, server) = spawn_test_upstream(app).await;
    let provider = build_provider_state(
        "anthropic".to_string(),
        UpstreamConfig {
            kind: ProviderKind::Anthropic,
            base_url,
            api_key: Some("provider-key".to_string()),
            ..UpstreamConfig::default()
        },
    )
    .unwrap();

    let models = fetch_upstream_models(&provider, &HeaderMap::new())
        .await
        .unwrap();
    assert_eq!(extract_model_ids(&models), ["model/one", "model-two"]);
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    server.abort();
}

#[test]
fn model_cache_reports_update_state() {
    let model_cache = ModelCache::default();

    let upstream = json!({ "data": [{ "id": "first-model" }] });
    assert!(model_cache.replace_upstream_models(vec!["first-model".to_string()], upstream.clone()));
    assert!(!model_cache.replace_upstream_models(vec!["first-model".to_string()], upstream));
    assert_eq!(
        model_cache.first_upstream_model().as_deref(),
        Some("first-model")
    );
}

#[tokio::test]
async fn generation_decode_errors_do_not_invalidate_upstream_model_cache() {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async { (StatusCode::OK, [("content-type", "application/json")], "{") }),
    );
    let (base_url, server) = spawn_test_upstream(app).await;
    let state = test_state(base_url);
    state
        .default_provider()
        .model_cache
        .replace_upstream_models(
            vec!["cached-model".to_string()],
            json!({ "data": [{ "id": "cached-model" }] }),
        );

    let response = proxy_responses_once_via_chat(
        state.default_provider(),
        &HeaderMap::new(),
        &json!({
            "model": "cached-model",
            "messages": [{ "role": "user", "content": "hi" }]
        }),
        Some("client-model".to_string()),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        state
            .default_provider()
            .model_cache
            .first_upstream_model()
            .as_deref(),
        Some("cached-model")
    );

    server.abort();
}

#[tokio::test]
async fn anthropic_backend_adapts_chat_and_isolates_caller_credentials() {
    let captured = Arc::new(Mutex::new(None::<(HeaderMap, Value)>));
    let captured_for_handler = captured.clone();
    let app = Router::new().route(
        "/v1/messages",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let captured = captured_for_handler.clone();
            async move {
                *captured.lock().unwrap() = Some((headers, body));
                Json(json!({
                    "id": "msg_test",
                    "type": "message",
                    "model": "claude-test",
                    "content": [{ "type": "text", "text": "adapted" }],
                    "stop_reason": "end_turn",
                    "usage": { "input_tokens": 5, "output_tokens": 2 }
                }))
            }
        }),
    );
    let (base_url, server) = spawn_test_upstream(app).await;
    let config = Config {
        upstream: UpstreamConfig {
            kind: ProviderKind::Anthropic,
            base_url,
            api_key: Some("provider-key".to_string()),
            ..UpstreamConfig::default()
        },
        ..Config::default()
    };
    let state = test_state_with_config(config);
    let mut caller_headers = HeaderMap::new();
    caller_headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer caller"));
    caller_headers.insert("x-api-key", HeaderValue::from_static("caller-key"));

    let response = chat_completions(
        State(SharedState::new(state)),
        caller_headers,
        Json(json!({
            "model": "default/claude-test",
            "messages": [{ "role": "user", "content": "hello" }]
        })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response: Value = serde_json::from_str(&collect_response_text(response).await).unwrap();
    assert_eq!(response["choices"][0]["message"]["content"], "adapted");
    assert_eq!(response["usage"]["total_tokens"], 7);

    let (headers, body) = captured.lock().unwrap().clone().unwrap();
    assert_eq!(headers.get("x-api-key").unwrap(), "provider-key");
    assert!(headers.get(AUTHORIZATION).is_none());
    assert_eq!(headers.get("anthropic-version").unwrap(), "2023-06-01");
    assert_eq!(body["model"], "claude-test");
    assert_eq!(body["messages"][0]["content"][0]["text"], "hello");
    server.abort();
}

#[tokio::test]
async fn anthropic_backend_streams_as_openai_chat() {
    let sse = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_stream\",\"model\":\"claude-test\"}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );
    let app = Router::new().route(
        "/v1/messages",
        post(move || async move { (StatusCode::OK, [(CONTENT_TYPE, "text/event-stream")], sse) }),
    );
    let (base_url, server) = spawn_test_upstream(app).await;
    let config = Config {
        upstream: UpstreamConfig {
            kind: ProviderKind::Anthropic,
            base_url,
            api_key: Some("provider-key".to_string()),
            ..UpstreamConfig::default()
        },
        ..Config::default()
    };
    let state = test_state_with_config(config);
    let response = chat_completions(
        State(SharedState::new(state)),
        HeaderMap::new(),
        Json(json!({
            "model": "default/claude-test",
            "stream": true,
            "messages": [{ "role": "user", "content": "hello" }]
        })),
    )
    .await;
    let body = collect_response_text(response).await;
    assert!(body.contains("chat.completion.chunk"), "body: {body}");
    assert!(body.contains("hello"), "body: {body}");
    assert!(body.contains("[DONE]"), "body: {body}");
    server.abort();
}

#[tokio::test]
async fn copilot_backend_adds_identity_headers_and_sanitizes_content() {
    let captured = Arc::new(Mutex::new(None::<(HeaderMap, Value)>));
    let captured_for_handler = captured.clone();
    let app = Router::new().route(
        "/chat/completions",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let captured = captured_for_handler.clone();
            async move {
                *captured.lock().unwrap() = Some((headers, body));
                Json(chat_completion("gpt-test", json!("ok"), json!([])))
            }
        }),
    );
    let (base_url, server) = spawn_test_upstream(app).await;
    let config = Config {
        upstream: UpstreamConfig {
            kind: ProviderKind::Copilot,
            base_url: base_url.trim_end_matches("/v1").to_string(),
            api_key: Some("copilot-token".to_string()),
            ..UpstreamConfig::default()
        },
        ..Config::default()
    };
    let state = test_state_with_config(config);
    let response = proxy_chat(
        state.default_provider(),
        &HeaderMap::new(),
        &json!({
            "model": "gpt-test",
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "input_text", "text": "hello" },
                    { "type": "thinking", "thinking": "private", "signature": "signed" }
                ]
            }]
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let (headers, body) = captured.lock().unwrap().clone().unwrap();
    assert_eq!(
        headers.get("copilot-integration-id").unwrap(),
        "vscode-chat"
    );
    assert_eq!(headers.get("anthropic-version").unwrap(), "2023-06-01");
    assert_eq!(headers.get(AUTHORIZATION).unwrap(), "Bearer copilot-token");
    assert_eq!(body["messages"][0]["content"].as_array().unwrap().len(), 1);
    assert!(!body.to_string().contains("signed"));
    server.abort();
}

#[tokio::test]
async fn all_frontends_route_through_each_provider_kind() {
    let openai_capture = Arc::new(Mutex::new(Vec::<(HeaderMap, Value)>::new()));
    let openai_capture_for_handler = openai_capture.clone();
    let openai_app = Router::new().route(
        "/v1/chat/completions",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let capture = openai_capture_for_handler.clone();
            async move {
                capture.lock().unwrap().push((headers, body));
                Json(chat_completion("gpt-test", json!("openai-ok"), json!([])))
            }
        }),
    );
    let (openai_url, openai_server) = spawn_test_upstream(openai_app).await;

    let anthropic_capture = Arc::new(Mutex::new(Vec::<(HeaderMap, Value)>::new()));
    let anthropic_capture_for_handler = anthropic_capture.clone();
    let anthropic_app = Router::new().route(
        "/v1/messages",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let capture = anthropic_capture_for_handler.clone();
            async move {
                capture.lock().unwrap().push((headers, body));
                Json(json!({
                    "id": "msg_matrix",
                    "type": "message",
                    "role": "assistant",
                    "model": "claude-test",
                    "content": [{ "type": "text", "text": "anthropic-ok" }],
                    "stop_reason": "end_turn",
                    "usage": { "input_tokens": 3, "output_tokens": 1 }
                }))
            }
        }),
    );
    let (anthropic_url, anthropic_server) = spawn_test_upstream(anthropic_app).await;

    let copilot_capture = Arc::new(Mutex::new(Vec::<(HeaderMap, Value)>::new()));
    let copilot_capture_for_handler = copilot_capture.clone();
    let copilot_app = Router::new().route(
        "/chat/completions",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let capture = copilot_capture_for_handler.clone();
            async move {
                capture.lock().unwrap().push((headers, body));
                Json(chat_completion(
                    "copilot-test",
                    json!("copilot-ok"),
                    json!([]),
                ))
            }
        }),
    );
    let (copilot_url, copilot_server) = spawn_test_upstream(copilot_app).await;

    let config = Config {
        default_provider: "openai".to_string(),
        providers: HashMap::from([
            (
                "openai".to_string(),
                UpstreamConfig {
                    kind: ProviderKind::OpenAi,
                    base_url: openai_url,
                    api_key: Some("openai-provider-key".to_string()),
                    reasoning_effort: Some(ReasoningEffort::High),
                    ..UpstreamConfig::default()
                },
            ),
            (
                "anthropic".to_string(),
                UpstreamConfig {
                    kind: ProviderKind::Anthropic,
                    base_url: anthropic_url,
                    api_key: Some("anthropic-provider-key".to_string()),
                    reasoning_effort: Some(ReasoningEffort::Max),
                    ..UpstreamConfig::default()
                },
            ),
            (
                "copilot".to_string(),
                UpstreamConfig {
                    kind: ProviderKind::Copilot,
                    base_url: copilot_url.trim_end_matches("/v1").to_string(),
                    api_key: Some("copilot-provider-key".to_string()),
                    ..UpstreamConfig::default()
                },
            ),
        ]),
        ..Config::default()
    };
    let shared = SharedState::new(test_state_with_config(config));

    for (provider, model, expected) in [
        ("openai", "gpt-test", "openai-ok"),
        ("anthropic", "claude-test", "anthropic-ok"),
        ("copilot", "copilot-test", "copilot-ok"),
    ] {
        for frontend in ["chat", "responses", "anthropic"] {
            let mut headers = HeaderMap::new();
            headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer caller-key"));
            headers.insert("x-api-key", HeaderValue::from_static("caller-key"));
            let namespaced_model = format!("{provider}/{model}");
            let response = match frontend {
                "chat" => {
                    chat_completions(
                        State(shared.clone()),
                        headers,
                        Json(json!({
                            "model": namespaced_model,
                            "messages": [{ "role": "user", "content": "hello" }]
                        })),
                    )
                    .await
                }
                "responses" => {
                    responses(
                        State(shared.clone()),
                        headers,
                        Json(json!({ "model": namespaced_model, "input": "hello" })),
                    )
                    .await
                }
                "anthropic" => {
                    anthropic_messages(
                        State(shared.clone()),
                        headers,
                        Json(json!({
                            "model": namespaced_model,
                            "max_tokens": 64,
                            "messages": [{ "role": "user", "content": "hello" }]
                        })),
                    )
                    .await
                }
                _ => unreachable!(),
            };
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "{frontend} -> {provider}"
            );
            assert!(
                collect_response_text(response).await.contains(expected),
                "{frontend} -> {provider} did not preserve the provider response"
            );
        }
    }

    let openai_requests = openai_capture.lock().unwrap();
    assert_eq!(openai_requests.len(), 3);
    assert!(openai_requests.iter().all(|(headers, body)| {
        headers
            .get(AUTHORIZATION)
            .is_some_and(|value| value.as_bytes() == b"Bearer openai-provider-key")
            && headers.get("x-api-key").is_none()
            && body["model"] == "gpt-test"
            && body["reasoning_effort"] == "high"
    }));
    let anthropic_requests = anthropic_capture.lock().unwrap();
    assert_eq!(anthropic_requests.len(), 3);
    assert!(anthropic_requests.iter().all(|(headers, body)| {
        headers
            .get("x-api-key")
            .is_some_and(|value| value.as_bytes() == b"anthropic-provider-key")
            && headers.get(AUTHORIZATION).is_none()
            && headers
                .get("anthropic-version")
                .is_some_and(|value| value.as_bytes() == b"2023-06-01")
            && body["model"] == "claude-test"
            && body["output_config"]["effort"] == "max"
    }));
    let copilot_requests = copilot_capture.lock().unwrap();
    assert_eq!(copilot_requests.len(), 3);
    assert!(copilot_requests.iter().all(|(headers, body)| {
        headers
            .get(AUTHORIZATION)
            .is_some_and(|value| value.as_bytes() == b"Bearer copilot-provider-key")
            && headers
                .get("copilot-integration-id")
                .is_some_and(|value| value.as_bytes() == b"vscode-chat")
            && body["model"] == "copilot-test"
    }));

    drop(openai_requests);
    drop(anthropic_requests);
    drop(copilot_requests);
    openai_server.abort();
    anthropic_server.abort();
    copilot_server.abort();
}

#[test]
fn copilot_claude_models_use_native_messages_wire() {
    let upstream = UpstreamConfig {
        kind: ProviderKind::Copilot,
        ..UpstreamConfig::default()
    };
    let provider = build_provider_state("copilot".to_string(), upstream).unwrap();
    let prepared = prepare_provider_chat_request(
        &provider,
        &json!({
            "model": "claude-sonnet-test",
            "messages": [{ "role": "user", "content": "hello" }]
        }),
    );
    assert_eq!(prepared.path, "v1/messages");
    assert!(prepared.anthropic_wire);
    assert_eq!(prepared.body["messages"][0]["content"][0]["text"], "hello");
}

#[test]
fn provider_reasoning_effort_overrides_openai_chat_wire() {
    let provider = build_provider_state(
        "openai".to_string(),
        UpstreamConfig {
            kind: ProviderKind::OpenAi,
            reasoning_effort: Some(ReasoningEffort::High),
            ..UpstreamConfig::default()
        },
    )
    .unwrap();
    let request = json!({
        "model": "gpt-test",
        "reasoning_effort": "low",
        "messages": [{ "role": "user", "content": "hello" }]
    });

    let prepared = prepare_provider_chat_request(&provider, &request);

    assert_eq!(prepared.path, "chat/completions");
    assert_eq!(prepared.body["reasoning_effort"], "high");
    assert_eq!(request["reasoning_effort"], "low");
}

#[test]
fn provider_reasoning_effort_maps_to_anthropic_messages_wire() {
    let provider = build_provider_state(
        "anthropic".to_string(),
        UpstreamConfig {
            kind: ProviderKind::Anthropic,
            reasoning_effort: Some(ReasoningEffort::Max),
            ..UpstreamConfig::default()
        },
    )
    .unwrap();

    let prepared = prepare_provider_chat_request(
        &provider,
        &json!({
            "model": "claude-test",
            "messages": [{ "role": "user", "content": "hello" }]
        }),
    );

    assert_eq!(prepared.path, "messages");
    assert_eq!(prepared.body["output_config"]["effort"], "max");
}

#[test]
fn provider_reasoning_effort_overrides_responses_wire_without_losing_options() {
    let provider = build_provider_state(
        "openai".to_string(),
        UpstreamConfig {
            kind: ProviderKind::OpenAi,
            reasoning_effort: Some(ReasoningEffort::High),
            ..UpstreamConfig::default()
        },
    )
    .unwrap();
    let mut request = json!({
        "model": "gpt-test",
        "reasoning": { "effort": "low", "summary": "auto" },
        "input": "hello"
    });

    apply_provider_reasoning_effort(&provider, ReasoningWire::Responses, &mut request);

    assert_eq!(request["reasoning"]["effort"], "high");
    assert_eq!(request["reasoning"]["summary"], "auto");
}

#[tokio::test]
async fn responses_passthrough_enforces_provider_reasoning_effort() {
    let captured = Arc::new(Mutex::new(None));
    let capture = captured.clone();
    let app = Router::new().route(
        "/v1/responses",
        post(move |Json(body): Json<Value>| {
            let capture = capture.clone();
            async move {
                *capture.lock().unwrap() = Some(body);
                Json(json!({ "id": "resp_test", "object": "response", "output": [] }))
            }
        }),
    );
    let (base_url, server) = spawn_test_upstream(app).await;
    let config = Config {
        upstream: UpstreamConfig {
            kind: ProviderKind::OpenAi,
            base_url,
            reasoning_effort: Some(ReasoningEffort::High),
            ..UpstreamConfig::default()
        },
        model_aliases: HashMap::from([(
            "gpt-test".to_string(),
            ModelAlias::Model("gpt-test".to_string()),
        )]),
        responses: ResponsesConfig {
            mode: ResponsesMode::Passthrough,
        },
        ..Config::default()
    };

    let response = responses(
        State(SharedState::new(test_state_with_config(config))),
        HeaderMap::new(),
        Json(json!({
            "model": "gpt-test",
            "input": "hello",
            "reasoning": { "effort": "low", "summary": "auto" }
        })),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let request = captured.lock().unwrap().clone().unwrap();
    assert_eq!(request["reasoning"]["effort"], "high");
    assert_eq!(request["reasoning"]["summary"], "auto");
    server.abort();
}

#[test]
fn native_interceptor_session_does_not_inherit_reasoning_policy() {
    let state = test_state_with_config(Config {
        upstream: UpstreamConfig {
            reasoning_effort: Some(ReasoningEffort::High),
            ..UpstreamConfig::default()
        },
        ..Config::default()
    });
    let session = LauncherSession::new(
        LauncherKind::Codex,
        "https://example.invalid/v1".to_string(),
    )
    .unwrap();

    let session_state = state.with_native_interceptor_session(&session).unwrap();

    assert_eq!(
        session_state.default_provider().config.reasoning_effort,
        None
    );
}

#[tokio::test]
async fn upstream_status_errors_preserve_status_and_include_body_detail() {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "message": "Stream options can only be defined when stream=True.",
                        "type": "BadRequestError",
                        "param": "stream_options",
                        "code": 400
                    }
                })),
            )
        }),
    );
    let (base_url, server) = spawn_test_upstream(app).await;
    let state = test_state(base_url);

    let response = proxy_responses_once_via_chat(
        state.default_provider(),
        &HeaderMap::new(),
        &json!({
            "model": "upstream-model",
            "messages": [{ "role": "user", "content": "hi" }]
        }),
        Some("client-model".to_string()),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = collect_response_text(response).await;
    assert!(body.contains("Stream options can only be defined"));
    assert!(body.contains("\"upstream_status\":400"));

    server.abort();
}

#[tokio::test]
async fn alias_only_request_rewrite_skips_upstream_model_lookup() {
    let attempts = Arc::new(AtomicU64::new(0));
    let attempts_for_handler = attempts.clone();
    let app = Router::new().route(
        "/v1/models",
        get(move || {
            let attempts = attempts_for_handler.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                Json(json!({ "data": [{ "id": "upstream-model" }] }))
            }
        }),
    );
    let (base_url, server) = spawn_test_upstream(app).await;
    let mut config = test_config();
    config.upstream.base_url = base_url;
    let state = test_state_with_config(config);
    let mut body = json!({
        "model": "codex-qwen",
        "messages": [{ "role": "developer", "content": "hi" }]
    });

    rewrite_proxy_request_value(&state, &HeaderMap::new(), &mut body)
        .await
        .unwrap();

    assert_eq!(attempts.load(Ordering::SeqCst), 0);
    assert_eq!(body["model"], "Qwen/Qwen2.5-Coder-32B-Instruct");
    assert_eq!(body["messages"][0]["role"], "system");
    server.abort();
}

#[tokio::test]
async fn non_alias_request_rewrite_still_loads_default_upstream_model() {
    let attempts = Arc::new(AtomicU64::new(0));
    let attempts_for_handler = attempts.clone();
    let app = Router::new().route(
        "/v1/models",
        get(move || {
            let attempts = attempts_for_handler.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                Json(json!({ "data": [{ "id": "default-upstream-model" }] }))
            }
        }),
    );
    let (base_url, server) = spawn_test_upstream(app).await;
    let mut config = test_config();
    config.upstream.base_url = base_url;
    let state = test_state_with_config(config);
    let mut body = json!({
        "model": "unknown-client-model",
        "messages": [{ "role": "user", "content": "hi" }]
    });

    rewrite_proxy_request_value(&state, &HeaderMap::new(), &mut body)
        .await
        .unwrap();

    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(body["model"], "default-upstream-model");
    server.abort();
}

#[tokio::test]
async fn upstream_requests_forward_or_generate_request_ids() {
    let seen_request_id = Arc::new(Mutex::new(None::<String>));
    let seen_request_id_for_handler = seen_request_id.clone();
    let app = Router::new().route(
        "/v1/models",
        get(move |headers: HeaderMap| {
            let seen_request_id = seen_request_id_for_handler.clone();
            async move {
                let value = headers
                    .get("x-request-id")
                    .and_then(|value| value.to_str().ok())
                    .map(ToOwned::to_owned);
                *seen_request_id.lock().unwrap() = value;
                Json(json!({ "data": [{ "id": "upstream-model" }] }))
            }
        }),
    );
    let (base_url, server) = spawn_test_upstream(app).await;
    let state = test_state(base_url);
    let mut headers = HeaderMap::new();
    headers.insert("x-request-id", HeaderValue::from_static("client-request-1"));

    let body = proxy_get(state.default_provider(), &headers, "models")
        .await
        .unwrap();

    assert_eq!(body["data"][0]["id"], "upstream-model");
    assert_eq!(
        seen_request_id.lock().unwrap().as_deref(),
        Some("client-request-1")
    );

    let body = proxy_get(state.default_provider(), &HeaderMap::new(), "models")
        .await
        .unwrap();
    assert_eq!(body["data"][0]["id"], "upstream-model");
    let generated_request_id = seen_request_id
        .lock()
        .unwrap()
        .clone()
        .expect("generated request id forwarded");
    assert!(generated_request_id.starts_with("llmproxy-"));

    server.abort();
}

#[tokio::test]
async fn upstream_models_refresh_retries_safe_decode_failures() {
    let attempts = Arc::new(AtomicU64::new(0));
    let attempts_for_handler = attempts.clone();
    let app = Router::new().route(
        "/v1/models",
        get(move || {
            let attempts = attempts_for_handler.clone();
            async move {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    (StatusCode::OK, [("content-type", "application/json")], "{").into_response()
                } else {
                    Json(json!({ "data": [{ "id": "retried-model" }] })).into_response()
                }
            }
        }),
    );
    let (base_url, server) = spawn_test_upstream(app).await;
    let mut config = Config::default();
    config.upstream.base_url = base_url;
    // Discovery caps each call well below the generation timeout; give it
    // enough room to cover the retry backoff.
    config.upstream.timeout_secs = 30;
    let state = test_state_with_config(config);

    let ids = refresh_upstream_models(state.default_provider(), &HeaderMap::new())
        .await
        .unwrap();

    assert_eq!(ids, vec!["retried-model"]);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(
        state
            .default_provider()
            .model_cache
            .first_upstream_model()
            .as_deref(),
        Some("retried-model")
    );
    server.abort();
}

#[tokio::test]
async fn models_endpoint_uses_cached_upstream_response_by_default() {
    let attempts = Arc::new(AtomicU64::new(0));
    let attempts_for_handler = attempts.clone();
    let app = Router::new().route(
        "/v1/models",
        get(move || {
            let attempts = attempts_for_handler.clone();
            async move {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                Json(json!({
                    "data": [{ "id": format!("upstream-model-{attempt}") }]
                }))
            }
        }),
    );
    let (base_url, server) = spawn_test_upstream(app).await;
    let mut config = Config::default();
    config.upstream.base_url = base_url;
    let state = test_state_with_config(config);

    let first = models(State(SharedState::new(state.clone())), HeaderMap::new()).await;
    let second = models(State(SharedState::new(state.clone())), HeaderMap::new()).await;

    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(
        state
            .default_provider()
            .model_cache
            .first_upstream_model()
            .as_deref(),
        Some("upstream-model-0")
    );
    server.abort();
}

#[tokio::test]
async fn models_endpoint_can_disable_cached_upstream_response() {
    let attempts = Arc::new(AtomicU64::new(0));
    let attempts_for_handler = attempts.clone();
    let app = Router::new().route(
        "/v1/models",
        get(move || {
            let attempts = attempts_for_handler.clone();
            async move {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                Json(json!({
                    "data": [{ "id": format!("upstream-model-{attempt}") }]
                }))
            }
        }),
    );
    let (base_url, server) = spawn_test_upstream(app).await;
    let mut config = Config::default();
    config.upstream.base_url = base_url;
    config.models.cache_ttl_secs = Some(0);
    let state = test_state_with_config(config);

    let first = models(State(SharedState::new(state.clone())), HeaderMap::new()).await;
    let second = models(State(SharedState::new(state.clone())), HeaderMap::new()).await;

    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(
        state
            .default_provider()
            .model_cache
            .first_upstream_model()
            .as_deref(),
        Some("upstream-model-1")
    );
    server.abort();
}

#[tokio::test]
async fn model_discovery_runs_providers_concurrently() {
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let barrier_for_handler = barrier.clone();
    let app = Router::new().route(
        "/v1/models",
        get(move || {
            let barrier = barrier_for_handler.clone();
            async move {
                // Both providers must be in-flight at the same time or this
                // await never completes, proving the loop is concurrent.
                barrier.wait().await;
                Json(json!({ "data": [{ "id": "concurrent-model" }] }))
            }
        }),
    );
    let (base_url, server) = spawn_test_upstream(app).await;
    let mut config = Config::default();
    config.upstream.base_url = base_url.clone();
    config.providers.insert(
        "second".to_string(),
        UpstreamConfig {
            base_url,
            ..UpstreamConfig::default()
        },
    );
    let state = test_state_with_config(config);

    let outcome = tokio::time::timeout(Duration::from_secs(5), models_catalog_json(&state)).await;
    let catalog = outcome.expect("providers were fetched concurrently");
    let ids = catalog["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m.get("id").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert!(ids.contains(&"concurrent-model"), "{ids:?}");
    assert!(
        ids.contains(&"second/concurrent-model"),
        "namespaced entry missing: {ids:?}"
    );
    server.abort();
}

#[tokio::test]
async fn model_discovery_times_out_a_hung_provider_without_stalling_others() {
    let mut config = Config::default();
    // A short (but retry-tolerant) discovery budget keeps the test fast;
    // the hung provider's generation timeout stays large to prove the
    // discovery cap, not `timeout_secs`, is what bounds the call.
    config.upstream.timeout_secs = 15;
    let hung = spawn_test_upstream(Router::new().route(
        "/v1/models",
        get(|| async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Json(json!({ "data": [] }))
        }),
    ))
    .await;
    let healthy = spawn_test_upstream(Router::new().route(
        "/v1/models",
        get(|| async { Json(json!({ "data": [{ "id": "healthy-model" }] })) }),
    ))
    .await;
    config.default_provider = "hung".to_string();
    config.providers.insert(
        "hung".to_string(),
        UpstreamConfig {
            base_url: hung.0,
            timeout_secs: 600,
            ..UpstreamConfig::default()
        },
    );
    config.providers.insert(
        "healthy".to_string(),
        UpstreamConfig {
            base_url: healthy.0.clone(),
            timeout_secs: 600,
            ..UpstreamConfig::default()
        },
    );
    config.upstream.base_url = healthy.0;
    let state = test_state_with_config(config);

    let started = Instant::now();
    // Well under the 600s generation timeout even with retry backoff;
    // bounded by the 15s discovery cap plus retries.
    let catalog = tokio::time::timeout(Duration::from_secs(60), models_catalog_json(&state))
        .await
        .expect("hung provider must not stall the catalog");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(55),
        "discovery took too long: {elapsed:?}"
    );
    let ids = catalog["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m.get("id").and_then(Value::as_str))
        .collect::<Vec<_>>();
    // The healthy provider survives; the hung provider is omitted. The
    // default provider is "hung", so healthy entries appear namespaced.
    assert!(ids.contains(&"healthy/healthy-model"), "{ids:?}");
    hung.1.abort();
    healthy.1.abort();
}

#[test]
fn model_discovery_timeout_caps_the_generation_timeout() {
    let make = |timeout_secs| ProviderState {
        name: "p".to_string(),
        config: Arc::new(UpstreamConfig {
            timeout_secs,
            ..UpstreamConfig::default()
        }),
        client: Client::new(),
        model_cache: Arc::new(ModelCache::default()),
        model_refresh: Arc::new(AsyncMutex::new(())),
    };
    assert_eq!(model_discovery_timeout(&make(2)), Duration::from_secs(2));
    assert_eq!(model_discovery_timeout(&make(600)), Duration::from_secs(15));
}

#[test]
fn normalizes_system_messages_to_single_leading_message() {
    let mut value = json!({
        "messages": [
            { "role": "user", "content": "first user turn" },
            { "role": "system", "content": "late system turn" },
            { "role": "assistant", "content": "assistant turn" },
            { "role": "system", "content": [{ "type": "text", "text": "second system turn" }] }
        ]
    });

    normalize_chat_request_system_messages(&mut value);

    assert_eq!(value["messages"][0]["role"], "system");
    assert_eq!(
        value["messages"][0]["content"],
        "late system turn\n\nsecond system turn"
    );
    assert_eq!(value["messages"][1]["role"], "user");
    assert_eq!(value["messages"][2]["role"], "assistant");
    assert!(
        value["messages"]
            .as_array()
            .unwrap()
            .iter()
            .skip(1)
            .all(|message| message["role"] != "system")
    );
}

#[test]
fn normalizes_chat_request_removes_non_streaming_stream_options() {
    let mut non_streaming = json!({
        "stream": false,
        "stream_options": { "include_usage": true },
        "messages": [{ "role": "user", "content": "hi" }]
    });
    normalize_chat_completion_request(&mut non_streaming, &test_config());
    assert!(non_streaming.get("stream_options").is_none());

    let mut omitted_stream = json!({
        "stream_options": { "include_usage": true },
        "messages": [{ "role": "user", "content": "hi" }]
    });
    normalize_chat_completion_request(&mut omitted_stream, &test_config());
    assert!(omitted_stream.get("stream_options").is_none());

    let mut streaming = json!({
        "stream": true,
        "stream_options": { "include_usage": true },
        "messages": [{ "role": "user", "content": "hi" }]
    });
    normalize_chat_completion_request(&mut streaming, &test_config());
    assert_eq!(streaming["stream_options"]["include_usage"], true);
}

#[test]
fn normalizes_chat_request_can_disable_qwen_thinking() {
    let mut config = test_config();
    config.chat.disable_thinking = true;

    let mut request = json!({
        "messages": [{ "role": "user", "content": "hi" }]
    });
    normalize_chat_completion_request(&mut request, &config);
    assert_eq!(request["chat_template_kwargs"]["enable_thinking"], false);

    let mut explicit = json!({
        "chat_template_kwargs": { "enable_thinking": true },
        "messages": [{ "role": "user", "content": "hi" }]
    });
    normalize_chat_completion_request(&mut explicit, &config);
    assert_eq!(explicit["chat_template_kwargs"]["enable_thinking"], true);
}

#[tokio::test]
async fn chat_completions_removes_non_streaming_stream_options_before_forwarding() {
    let seen_body = Arc::new(Mutex::new(None::<Value>));
    let seen_body_for_handler = seen_body.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(body): Json<Value>| {
            let seen_body = seen_body_for_handler.clone();
            async move {
                *seen_body.lock().unwrap() = Some(body);
                Json(json!({
                    "id": "chatcmpl_test",
                    "object": "chat.completion",
                    "choices": [{
                        "message": { "role": "assistant", "content": "ok" }
                    }]
                }))
            }
        }),
    );
    let (base_url, server) = spawn_test_upstream(app).await;
    let mut config = test_config();
    config.upstream.base_url = base_url;
    let state = test_state_with_config(config);

    let response = chat_completions(
        State(SharedState::new(state)),
        HeaderMap::new(),
        Json(json!({
            "model": "codex-qwen",
            "stream": false,
            "stream_options": { "include_usage": true },
            "messages": [{ "role": "user", "content": "hi" }]
        })),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(collect_response_text(response).await.contains("ok"));
    let forwarded = seen_body
        .lock()
        .unwrap()
        .clone()
        .expect("upstream request body captured");
    assert_eq!(forwarded["stream"], false);
    assert!(forwarded.get("stream_options").is_none());

    server.abort();
}

#[test]
fn converts_responses_request_to_chat_completion_request() {
    let mut value = json!({
        "model": "codex-qwen",
        "instructions": "You are helpful.",
        "input": [
            {
                "type": "message",
                "role": "developer",
                "content": [{ "type": "input_text", "text": "Use short answers." }]
            },
            {
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": "Hello" }]
            }
        ],
        "max_output_tokens": 64,
        "stream": true
    });
    let provider = test_provider("http://127.0.0.1:1/v1".to_string());
    rewrite_request_value(&mut value, &test_config(), &provider, None);

    let chat = responses_to_chat_request(&value);

    assert_eq!(chat["model"], "Qwen/Qwen2.5-Coder-32B-Instruct");
    assert_eq!(chat["max_tokens"], 64);
    assert_eq!(chat["stream"], true);
    assert_eq!(chat["messages"][0]["role"], "system");
    assert_eq!(
        chat["messages"][0]["content"],
        "You are helpful.\n\nUse short answers."
    );
    assert_eq!(chat["messages"][1]["role"], "user");
    assert_eq!(chat["messages"][1]["content"], "Hello");
}

#[test]
fn responses_adapter_preserves_image_inputs() {
    let chat = responses_to_chat_request(&json!({
        "model": "test",
        "input": [{
            "type": "message",
            "role": "user",
            "content": [
                { "type": "input_text", "text": "inspect this" },
                { "type": "input_image", "image_url": "data:image/png;base64,dGVzdA==", "detail": "high" }
            ]
        }]
    }));

    let content = chat["messages"][0]["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["type"], "image_url");
    assert_eq!(
        content[1]["image_url"]["url"],
        "data:image/png;base64,dGVzdA=="
    );
    assert_eq!(content[1]["image_url"]["detail"], "high");
}

#[test]
fn converts_responses_function_tools_to_chat_completion_tools() {
    let value = json!({
        "model": "codex-qwen",
        "input": "Use a tool.",
        "tools": [
            {
                "type": "function",
                "name": "exec_command",
                "description": "Run a command.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "cmd": { "type": "string" }
                    },
                    "required": ["cmd"],
                    "additionalProperties": false
                },
                "strict": false
            },
            {
                "type": "web_search_preview"
            }
        ],
        "tool_choice": {
            "type": "function",
            "name": "exec_command"
        },
        "parallel_tool_calls": true
    });

    let chat = responses_to_chat_request(&value);

    assert_eq!(chat["tools"].as_array().unwrap().len(), 1);
    assert_eq!(chat["tools"][0]["type"], "function");
    assert_eq!(chat["tools"][0]["function"]["name"], "exec_command");
    assert_eq!(
        chat["tools"][0]["function"]["description"],
        "Run a command."
    );
    assert_eq!(
        chat["tools"][0]["function"]["parameters"]["required"][0],
        "cmd"
    );
    assert_eq!(chat["tools"][0]["function"]["strict"], false);
    assert!(chat["tools"][0].get("name").is_none());
    assert_eq!(chat["tool_choice"]["function"]["name"], "exec_command");
    assert_eq!(chat["parallel_tool_calls"], true);
}

#[test]
fn converts_responses_function_call_history_to_chat_messages() {
    let value = json!({
        "model": "codex-qwen",
        "input": [
            {
                "type": "function_call",
                "call_id": "call_1",
                "name": "exec_command",
                "arguments": "{\"cmd\":\"pwd\"}"
            },
            {
                "type": "function_call_output",
                "call_id": "call_1",
                "output": [{ "type": "input_text", "text": "/tmp/project" }]
            }
        ]
    });

    let chat = responses_to_chat_request(&value);
    let messages = chat["messages"].as_array().unwrap();

    assert_eq!(messages[0]["role"], "assistant");
    assert!(messages[0]["content"].is_null());
    assert_eq!(messages[0]["tool_calls"][0]["id"], "call_1");
    assert_eq!(messages[0]["tool_calls"][0]["type"], "function");
    assert_eq!(
        messages[0]["tool_calls"][0]["function"]["name"],
        "exec_command"
    );
    assert_eq!(
        messages[0]["tool_calls"][0]["function"]["arguments"],
        "{\"cmd\":\"pwd\"}"
    );
    assert_eq!(messages[1]["role"], "tool");
    assert_eq!(messages[1]["tool_call_id"], "call_1");
    assert_eq!(messages[1]["content"], "/tmp/project");
}

#[test]
fn responses_adapter_drops_provider_signed_reasoning_history() {
    let value = json!({
        "input": [
            {
                "type": "reasoning",
                "encrypted_content": "provider-signed-state",
                "summary": []
            },
            {
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": "continue" }]
            }
        ]
    });

    let chat = responses_to_chat_request(&value);
    assert_eq!(chat["messages"].as_array().unwrap().len(), 1);
    assert_eq!(chat["messages"][0]["content"], "continue");
    assert!(!chat.to_string().contains("provider-signed-state"));
}

#[test]
fn exposes_model_aliases_in_model_list() {
    let upstream = json!({
        "object": "list",
        "data": [{
            "id": "Qwen/Qwen2.5-Coder-32B-Instruct",
            "object": "model",
            "owned_by": "vllm"
        }]
    });

    let rewritten = rewrite_models_response(upstream, &test_config());

    assert!(
        rewritten["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["id"] == "codex-qwen"
                && model["proxy_target"] == "Qwen/Qwen2.5-Coder-32B-Instruct")
    );
}

#[test]
fn exposes_auto_and_integrated_targets_in_model_list() {
    let mut config = test_config();
    config.set_routing_mode(RoutingMode::Integrated);
    config.providers.insert(
        "openai".to_string(),
        UpstreamConfig {
            base_url: "https://api.example.test/v1".to_string(),
            ..UpstreamConfig::default()
        },
    );
    config.integrated.targets.insert(
        "PROVIDER_OPENAI".to_string(),
        interceptor::IntegratedTarget {
            provider: "openai".to_string(),
            model: "gpt-test".to_string(),
            hint: "everything else".to_string(),
            reasoning_effort: None,
        },
    );
    config.integrated.targets.insert(
        "PROVIDER_MISSING".to_string(),
        interceptor::IntegratedTarget {
            provider: "absent".to_string(),
            model: "ghost".to_string(),
            hint: "unreachable".to_string(),
            reasoning_effort: None,
        },
    );

    // The provider fetch failed, so no openai models are listed; the
    // integrated target must still be advertised as a namespaced id.
    let rewritten = rewrite_models_response(json!({ "object": "list", "data": [] }), &config);
    let data = rewritten["data"].as_array().unwrap().clone();
    let auto = data
        .iter()
        .find(|model| model["id"] == INTEGRATED_AUTO_MODEL)
        .expect("Auto routing model listed");
    assert_eq!(auto["proxy_route"], "integrated");
    let entry = data
        .iter()
        .find(|model| model["id"] == "openai/gpt-test")
        .expect("integrated target listed");
    assert_eq!(entry["proxy_route"], "namespaced");
    assert!(entry.get("proxy_tool").is_none());
    assert_eq!(entry["proxy_provider"], "openai");
    assert!(!data.iter().any(|model| model["id"] == "absent/ghost"));

    // A listed upstream model that a target routes to is not duplicated.
    let listed = json!({
        "object": "list",
        "data": [{
            "id": "openai/gpt-test",
            "object": "model",
            "proxy_provider": "openai"
        }]
    });
    let rewritten = rewrite_models_response(listed, &config);
    let data = rewritten["data"].as_array().unwrap();
    assert_eq!(
        data.iter()
            .filter(|model| model["id"] == "openai/gpt-test")
            .count(),
        1
    );
    assert!(data[0].get("proxy_tool").is_none());

    config.set_routing_mode(RoutingMode::Proxy);
    let rewritten = rewrite_models_response(json!({ "object": "list", "data": [] }), &config);
    let data = rewritten["data"].as_array().unwrap();
    assert!(
        data.iter()
            .all(|model| model["id"] != INTEGRATED_AUTO_MODEL)
    );
    assert!(data.iter().all(|model| model["id"] != "openai/gpt-test"));
}

#[test]
fn maps_chat_completion_to_response_shape() {
    let upstream = json!({
        "id": "chatcmpl_test",
        "created": 123,
        "model": "Qwen/Qwen2.5-Coder-32B-Instruct",
        "choices": [{
            "message": { "role": "assistant", "content": "hello" }
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
    });

    let response = chat_completion_to_response(upstream, Some("codex-qwen".to_string()));

    assert_eq!(response["object"], "response");
    assert_eq!(response["model"], "codex-qwen");
    assert_eq!(response["output_text"], "hello");
    assert_eq!(response["output"][0]["content"][0]["text"], "hello");
    assert_eq!(response["usage"]["total_tokens"], 2);
}

#[test]
fn maps_chat_tool_calls_to_response_function_calls() {
    let upstream = json!({
        "id": "chatcmpl_test",
        "created": 123,
        "model": "Qwen/Qwen2.5-Coder-32B-Instruct",
        "choices": [{
            "finish_reason": "tool_calls",
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "exec_command",
                        "arguments": "{\"cmd\":\"ls\"}"
                    }
                }]
            }
        }]
    });

    let response = chat_completion_to_response(upstream, Some("codex-qwen".to_string()));

    assert_eq!(response["output_text"], "");
    assert_eq!(response["output"].as_array().unwrap().len(), 1);
    assert_eq!(response["output"][0]["type"], "function_call");
    assert_eq!(response["output"][0]["call_id"], "call_1");
    assert_eq!(response["output"][0]["name"], "exec_command");
    assert_eq!(response["output"][0]["arguments"], "{\"cmd\":\"ls\"}");
}

#[test]
fn normalizes_qwen_xml_tool_arguments_to_json() {
    let arguments = "\n\n<tool_call>\n<function=exec_command>\n<parameter=cmd>\npwd\n</parameter>\n<parameter=timeout_ms>\n1000\n</parameter>\n</function>\n</tool_call>";

    let normalized = normalize_tool_arguments(arguments);
    let parsed = serde_json::from_str::<Value>(&normalized).unwrap();

    assert_eq!(parsed["cmd"], "pwd");
    assert_eq!(parsed["timeout_ms"], 1000);
}

#[test]
fn extracts_chat_stream_tool_call_deltas() {
    let value = json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "exec_command",
                        "arguments": "{\"cmd\":\""
                    }
                }]
            }
        }, {
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "function": {
                        "arguments": "pwd\"}"
                    }
                }]
            }
        }]
    });

    let deltas = chat_stream_tool_call_deltas(&value);
    assert_eq!(
        deltas,
        vec![
            ChatStreamToolCallDelta {
                index: 0,
                id: Some("call_1".to_string()),
                name: Some("exec_command".to_string()),
                arguments_delta: "{\"cmd\":\"".to_string(),
            },
            ChatStreamToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments_delta: "pwd\"}".to_string(),
            },
        ]
    );

    let mut accumulated = Vec::new();
    for delta in deltas {
        apply_stream_tool_call_delta(&mut accumulated, delta, "resp_test");
    }
    assert_eq!(accumulated[0].call_id, "call_1");
    assert_eq!(accumulated[0].name, "exec_command");
    assert_eq!(accumulated[0].arguments, "{\"cmd\":\"pwd\"}");
}

#[test]
fn converts_anthropic_message_to_chat_completion_request() {
    let mut value = json!({
        "model": "codex-qwen",
        "system": "You are concise.",
        "max_tokens": 128,
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": "Run ls" },
                { "type": "tool_result", "tool_use_id": "toolu_1", "content": "ok" }
            ]
        }],
        "tools": [{
            "name": "Bash",
            "description": "Run shell commands",
            "input_schema": {
                "type": "object",
                "properties": { "command": { "type": "string" } }
            }
        }],
        "tool_choice": { "type": "auto" }
    });
    let provider = test_provider("http://127.0.0.1:1/v1".to_string());
    rewrite_request_value(&mut value, &test_config(), &provider, None);

    let chat = anthropic_message_to_chat_request(&value);

    assert_eq!(chat["model"], "Qwen/Qwen2.5-Coder-32B-Instruct");
    assert_eq!(chat["max_tokens"], 128);
    assert_eq!(chat["messages"][0]["role"], "system");
    assert_eq!(chat["messages"][0]["content"], "You are concise.");
    assert_eq!(chat["messages"][1]["role"], "user");
    assert_eq!(chat["messages"][1]["content"], "Run ls");
    assert_eq!(chat["messages"][2]["role"], "tool");
    assert_eq!(chat["messages"][2]["tool_call_id"], "toolu_1");
    assert_eq!(chat["messages"][2]["content"], "ok");
    assert_eq!(chat["tools"][0]["function"]["name"], "Bash");
    assert_eq!(chat["tool_choice"], "auto");
}

#[test]
fn anthropic_adapter_preserves_base64_image_inputs() {
    let chat = anthropic_message_to_chat_request(&json!({
        "model": "test",
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": "inspect this" },
                {
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": "dGVzdA=="
                    }
                }
            ]
        }]
    }));

    let content = chat["messages"][0]["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["type"], "image_url");
    assert_eq!(
        content[1]["image_url"]["url"],
        "data:image/png;base64,dGVzdA=="
    );
}

#[test]
fn maps_chat_completion_to_anthropic_message_shape() {
    let upstream = json!({
        "id": "chatcmpl_test",
        "model": "Qwen/Qwen2.5-Coder-32B-Instruct",
        "choices": [{
            "finish_reason": "tool_calls",
            "message": {
                "role": "assistant",
                "content": "checking",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "Bash",
                        "arguments": "{\"command\":\"pwd\"}"
                    }
                }]
            }
        }],
        "usage": { "prompt_tokens": 5, "completion_tokens": 7, "total_tokens": 12 }
    });

    let message = chat_completion_to_anthropic_message(upstream, Some("local".to_string()));

    assert_eq!(message["type"], "message");
    assert_eq!(message["model"], "local");
    assert_eq!(message["stop_reason"], "tool_use");
    assert_eq!(message["content"][0]["type"], "text");
    assert_eq!(message["content"][1]["type"], "tool_use");
    assert_eq!(message["content"][1]["name"], "Bash");
    assert_eq!(message["content"][1]["input"]["command"], "pwd");
    assert_eq!(message["usage"]["input_tokens"], 5);
    assert_eq!(message["usage"]["output_tokens"], 7);
}

#[test]
fn ensure_chat_stream_include_usage_sets_flag_when_streaming() {
    let mut request = json!({ "stream": true });
    ensure_chat_stream_include_usage(&mut request);
    assert_eq!(request["stream_options"]["include_usage"], true);

    let mut existing = json!({
        "stream": true,
        "stream_options": { "include_usage": false }
    });
    ensure_chat_stream_include_usage(&mut existing);
    assert_eq!(existing["stream_options"]["include_usage"], false);

    let mut not_streaming = json!({ "stream": false });
    ensure_chat_stream_include_usage(&mut not_streaming);
    assert!(not_streaming.get("stream_options").is_none());
}

#[test]
fn chat_usage_to_responses_usage_maps_token_fields() {
    let chat_usage = json!({
        "prompt_tokens": 10,
        "completion_tokens": 4,
        "total_tokens": 14,
        "prompt_tokens_details": { "cached_tokens": 3 },
        "completion_tokens_details": { "reasoning_tokens": 1 }
    });
    let mapped = chat_usage_to_responses_usage(&chat_usage);
    assert_eq!(mapped["input_tokens"], 10);
    assert_eq!(mapped["output_tokens"], 4);
    assert_eq!(mapped["total_tokens"], 14);
    assert_eq!(mapped["input_tokens_details"]["cached_tokens"], 3);
    assert_eq!(mapped["output_tokens_details"]["reasoning_tokens"], 1);
}

#[test]
fn take_sse_line_bytes_handles_split_utf8_across_chunks() {
    let mut buffer: Vec<u8> = Vec::new();
    let line1 = "data: {\"choices\":[]}\n";
    let line2_bytes = [b'd', b'a', b't', b'a', b':', b' ', 0xE4, 0xB8, 0xAD, b'\n'];
    buffer.extend_from_slice(line1.as_bytes());
    let first = take_sse_line_bytes(&mut buffer).expect("first line");
    assert_eq!(first, "data: {\"choices\":[]}");
    buffer.extend_from_slice(&line2_bytes);
    let second = take_sse_line_bytes(&mut buffer).expect("second line");
    assert_eq!(second, "data: 中");
    assert!(take_sse_line_bytes(&mut buffer).is_none());
}

#[test]
fn sse_data_parser_supports_multiline_data_events() {
    let mut buffer = concat!(
        "event: ignored\n",
        "data: {\"choices\":[\n",
        "data: {\"delta\":{\"content\":\"hi\"}}]}\n\n",
    )
    .as_bytes()
    .to_vec();
    let mut data_lines = Vec::new();

    let events = take_sse_data_events(&mut buffer, &mut data_lines);

    assert_eq!(events.len(), 1);
    let value: Value = serde_json::from_str(&events[0]).unwrap();
    assert_eq!(value["choices"][0]["delta"]["content"], "hi");
    assert!(buffer.is_empty());
    assert!(data_lines.is_empty());
}

#[test]
fn sse_data_parser_surfaces_incomplete_tail_at_eof() {
    let mut buffer = "data: {\"choices\":[]}".as_bytes().to_vec();
    let mut data_lines = Vec::new();

    let (events, had_incomplete_tail) = finish_sse_data_events(&mut buffer, &mut data_lines);

    assert!(had_incomplete_tail);
    assert_eq!(events, vec!["{\"choices\":[]}"]);
    assert!(buffer.is_empty());
    assert!(data_lines.is_empty());
}

#[tokio::test]
async fn stream_chat_as_responses_emits_lifecycle_events() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}],",
        "\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3,\"total_tokens\":10}}\n\n",
        "data: [DONE]\n\n",
    );
    let response = mock_upstream_response(body);
    let response = stream_chat_as_responses(response, Some("local".to_string()), None);
    let events = collect_sse_events(response).await;

    let kinds: Vec<&str> = events.iter().map(|(event, _)| event.as_str()).collect();
    assert!(kinds.contains(&"response.created"));
    assert!(kinds.contains(&"response.output_text.delta"));
    assert!(kinds.contains(&"response.output_item.done"));
    assert!(kinds.contains(&"response.completed"));

    let completed = events
        .iter()
        .find(|(event, _)| event == "response.completed")
        .map(|(_, payload)| payload.clone())
        .expect("response.completed present");
    assert_eq!(completed["response"]["usage"]["input_tokens"], 7);
    assert_eq!(completed["response"]["usage"]["output_tokens"], 3);
    assert_eq!(completed["response"]["usage"]["total_tokens"], 10);
    assert_eq!(completed["response"]["model"], "local");
    assert_eq!(completed["response"]["end_turn"], true);

    let text_deltas: Vec<String> = events
        .iter()
        .filter(|(event, _)| event == "response.output_text.delta")
        .map(|(_, payload)| payload["delta"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(text_deltas.join(""), "hello");
}

#[tokio::test]
async fn stream_chat_as_responses_reads_multiline_sse_data() {
    let body = concat!(
        "data: {\"choices\":[\n",
        "data: {\"delta\":{\"content\":\"multi\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let response = mock_upstream_response(body);
    let response = stream_chat_as_responses(response, Some("local".to_string()), None);
    let events = collect_sse_events(response).await;

    let text_deltas: Vec<String> = events
        .iter()
        .filter(|(event, _)| event == "response.output_text.delta")
        .map(|(_, payload)| payload["delta"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(text_deltas, vec!["multi"]);
}

#[tokio::test]
async fn stream_chat_as_anthropic_emits_message_lifecycle() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\" there\"},\"finish_reason\":\"stop\"}],",
        "\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2,\"total_tokens\":6}}\n\n",
        "data: [DONE]\n\n",
    );
    let response = mock_upstream_response(body);
    let response = stream_chat_as_anthropic(response, Some("local".to_string()), None);
    let events = collect_sse_events(response).await;

    let kinds: Vec<&str> = events.iter().map(|(event, _)| event.as_str()).collect();
    assert!(kinds.starts_with(&["message_start"]));
    assert!(kinds.contains(&"content_block_start"));
    assert!(kinds.contains(&"content_block_delta"));
    assert!(kinds.contains(&"content_block_stop"));
    assert!(kinds.contains(&"message_delta"));
    assert!(kinds.ends_with(&["message_stop"]));

    let text: String = events
        .iter()
        .filter(|(event, _)| event == "content_block_delta")
        .filter_map(|(_, payload)| payload["delta"]["text"].as_str().map(ToOwned::to_owned))
        .collect();
    assert_eq!(text, "hi there");

    let delta = events
        .iter()
        .find(|(event, _)| event == "message_delta")
        .map(|(_, payload)| payload.clone())
        .expect("message_delta present");
    assert_eq!(delta["delta"]["stop_reason"], "end_turn");
    assert_eq!(delta["usage"]["input_tokens"], 4);
    assert_eq!(delta["usage"]["output_tokens"], 2);
}

#[tokio::test]
async fn stream_chat_as_anthropic_emits_tool_use_block() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",",
        "\"type\":\"function\",\"function\":{\"name\":\"Bash\",\"arguments\":\"{\\\"cmd\\\":\\\"\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,",
        "\"function\":{\"arguments\":\"pwd\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let response = mock_upstream_response(body);
    let response = stream_chat_as_anthropic(response, Some("local".to_string()), None);
    let events = collect_sse_events(response).await;

    let tool_starts: Vec<&Value> = events
        .iter()
        .filter(|(event, payload)| {
            event == "content_block_start" && payload["content_block"]["type"] == "tool_use"
        })
        .map(|(_, payload)| payload)
        .collect();
    assert_eq!(tool_starts.len(), 1);
    assert_eq!(tool_starts[0]["content_block"]["id"], "call_1");
    assert_eq!(tool_starts[0]["content_block"]["name"], "Bash");

    let partial_jsons: String = events
        .iter()
        .filter(|(event, payload)| {
            event == "content_block_delta" && payload["delta"]["type"] == "input_json_delta"
        })
        .filter_map(|(_, payload)| {
            payload["delta"]["partial_json"]
                .as_str()
                .map(ToOwned::to_owned)
        })
        .collect();
    assert_eq!(partial_jsons, "{\"cmd\":\"pwd\"}");

    let delta = events
        .iter()
        .find(|(event, _)| event == "message_delta")
        .map(|(_, payload)| payload.clone())
        .expect("message_delta present");
    assert_eq!(delta["delta"]["stop_reason"], "tool_use");
}

#[tokio::test]
async fn stream_chat_as_anthropic_turns_upstream_body_errors_into_sse_errors() {
    let response = mock_upstream_error_response("stream exploded");
    let meta = UpstreamRequestMeta::new(
        "test",
        &HeaderMap::new(),
        Method::POST,
        "chat/completions",
        UpstreamResponseKind::Stream,
        false,
    );

    let response = stream_chat_as_anthropic(response, Some("local".to_string()), Some(meta));
    let events = collect_sse_events(response).await;

    assert_eq!(
        events.first().map(|(event, _)| event.as_str()),
        Some("message_start")
    );
    let error = events
        .iter()
        .find(|(event, _)| event == "error")
        .map(|(_, payload)| payload)
        .expect("error event present");
    assert_eq!(error["type"], "error");
    assert_eq!(error["error"]["type"], "api_error");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("stream exploded")
    );
}

#[tokio::test]
async fn stream_chat_as_responses_turns_upstream_body_errors_into_failed_events() {
    let response = mock_upstream_error_response("stream exploded");
    let meta = UpstreamRequestMeta::new(
        "test",
        &HeaderMap::new(),
        Method::POST,
        "chat/completions",
        UpstreamResponseKind::Stream,
        false,
    );

    let response = stream_chat_as_responses(response, Some("local".to_string()), Some(meta));
    let events = collect_sse_events(response).await;

    let failed = events
        .iter()
        .find(|(event, _)| event == "response.failed")
        .map(|(_, payload)| payload)
        .expect("response.failed event present");
    assert_eq!(failed["response"]["status"], "failed");
    assert_eq!(failed["response"]["error"]["type"], "upstream_error");
    assert!(
        failed["response"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("stream exploded")
    );
}

#[tokio::test]
async fn streaming_proxy_requests_skip_total_upstream_timeout() {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            let stream = async_stream::stream! {
                sleep(Duration::from_millis(1200)).await;
                yield Ok::<Bytes, std::io::Error>(Bytes::from_static(b"data: [DONE]\n\n"));
            };

            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        }),
    );
    let (base_url, server) = spawn_test_upstream(app).await;
    let mut config = Config::default();
    config.upstream.base_url = base_url;
    config.upstream.timeout_secs = 1;
    let state = test_state_with_config(config);

    let response = proxy_json(
        state.default_provider(),
        &HeaderMap::new(),
        "chat/completions",
        &json!({
            "model": "local",
            "stream": true,
            "messages": [{ "role": "user", "content": "hi" }]
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(collect_response_text(response).await.contains("[DONE]"));
    server.abort();
}

fn mock_upstream_response(body: &str) -> reqwest::Response {
    let http_response = http::Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(body.to_string())
        .expect("build mock response");
    reqwest::Response::from(http_response)
}

fn mock_upstream_error_response(message: &'static str) -> reqwest::Response {
    let stream = futures_util::stream::once(async move {
        Err::<Bytes, std::io::Error>(std::io::Error::other(message))
    });
    let http_response = http::Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(reqwest::Body::wrap_stream(stream))
        .expect("build mock error response");
    reqwest::Response::from(http_response)
}

async fn collect_response_text(response: Response) -> String {
    use futures_util::StreamExt;
    let body = response.into_body();
    let mut stream = body.into_data_stream();
    let mut buffer: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("stream chunk");
        buffer.extend_from_slice(&chunk);
    }
    String::from_utf8(buffer).expect("utf8 response body")
}

async fn collect_sse_events(response: Response) -> Vec<(String, Value)> {
    use futures_util::StreamExt;
    let body = response.into_body();
    let mut stream = body.into_data_stream();
    let mut buffer: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("stream chunk");
        buffer.extend_from_slice(&chunk);
    }
    let text = String::from_utf8(buffer).expect("utf8 sse body");
    let mut events = Vec::new();
    for block in text.split("\n\n") {
        let mut event_name: Option<String> = None;
        let mut data_line: Option<String> = None;
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("event:") {
                event_name = Some(rest.trim().to_string());
            } else if let Some(rest) = line.strip_prefix("data:") {
                data_line = Some(rest.trim().to_string());
            }
        }
        if let (Some(name), Some(data)) = (event_name, data_line) {
            let value: Value = serde_json::from_str(&data).unwrap_or(Value::String(data));
            events.push((name, value));
        }
    }
    events
}
