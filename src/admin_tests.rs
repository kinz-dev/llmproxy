use super::*;

#[test]
fn admin_config_defaults_to_disabled() {
    let config: Config = toml::from_str("").unwrap();
    assert!(!config.admin.enable);
    assert!(config.admin.listen_address.is_none());
    assert_eq!(listen_address(&config.admin), "127.0.0.1:0");

    let config: Config = toml::from_str("[admin]\nenable = true\n").unwrap();
    assert!(config.admin.enable);

    let config: Config =
        toml::from_str("[admin]\nenabled = true\nlisten_address = \"127.0.0.1:11180\"\n").unwrap();
    assert!(config.admin.enable);
    assert_eq!(listen_address(&config.admin), "127.0.0.1:11180");
}

#[test]
fn non_loopback_admin_listen_address_is_rejected() {
    let config: Config =
        toml::from_str("[admin]\nenable = true\nlisten_address = \"0.0.0.0:11180\"\n").unwrap();
    let error = validate_config(&config).unwrap_err().to_string();
    assert!(error.contains("loopback"), "unexpected error: {error}");

    let config: Config =
        toml::from_str("[admin]\nenable = true\nlisten_address = \"127.0.0.1:11180\"\n").unwrap();
    validate_config(&config).unwrap();

    let config: Config = toml::from_str("[admin]\nlisten_address = \"0.0.0.0:11180\"\n").unwrap();
    validate_config(&config).unwrap();
}

#[test]
fn loopback_host_check_accepts_only_literal_loopback_ip_port() {
    assert!(loopback_host_matches("127.0.0.1:11180", 11180));
    assert!(loopback_host_matches("[::1]:11180", 11180));
    // Wrong port.
    assert!(!loopback_host_matches("127.0.0.1:9999", 11180));
    // DNS rebinding: attacker hostname (even one resolving to loopback) fails
    // because we require a literal IP, not a name.
    assert!(!loopback_host_matches("attacker.com:11180", 11180));
    assert!(!loopback_host_matches("localhost:11180", 11180));
    // Non-loopback literal.
    assert!(!loopback_host_matches("10.0.0.5:11180", 11180));
    // Missing port / malformed.
    assert!(!loopback_host_matches("127.0.0.1", 11180));
    assert!(!loopback_host_matches("127.0.0.1:notaport", 11180));
}

#[test]
fn same_origin_check_allows_only_own_loopback_origin() {
    assert!(is_same_loopback_origin("http://127.0.0.1:11180", 11180));
    assert!(is_same_loopback_origin("http://[::1]:11180", 11180));
    assert!(!is_same_loopback_origin("http://attacker.com:11180", 11180));
    assert!(!is_same_loopback_origin("https://127.0.0.1:11180", 11180));
    assert!(!is_same_loopback_origin("http://127.0.0.1:2222", 11180));
    assert!(!is_same_loopback_origin("null", 11180));
}

#[test]
fn json_content_type_check_is_strict_on_charset() {
    assert!(is_json_content_type(Some("application/json")));
    assert!(is_json_content_type(Some(
        "application/json; charset=utf-8"
    )));
    assert!(is_json_content_type(Some("APPLICATION/JSON")));
    assert!(!is_json_content_type(Some("text/plain")));
    assert!(!is_json_content_type(Some(
        "application/x-www-form-urlencoded"
    )));
    assert!(!is_json_content_type(None));
}

#[test]
fn ui_paths_are_sanitized() {
    assert_eq!(sanitize_ui_path("/").as_deref(), Some("index.html"));
    assert_eq!(sanitize_ui_path("/app.js").as_deref(), Some("app.js"));
    assert_eq!(
        sanitize_ui_path("/assets/logo.svg").as_deref(),
        Some("assets/logo.svg")
    );
    for rejected in [
        "/../Cargo.toml",
        "/ui/../../secret",
        "/%2e%2e/Cargo.toml",
        "/a\\b.html",
        "/etc//passwd/../x",
        "/\0",
    ] {
        assert_eq!(sanitize_ui_path(rejected), None, "accepted: {rejected:?}");
    }
}

#[test]
fn ui_content_types_cover_web_assets() {
    assert_eq!(ui_content_type("index.html"), "text/html; charset=utf-8");
    assert_eq!(ui_content_type("style.css"), "text/css; charset=utf-8");
    assert_eq!(ui_content_type("app.js"), "text/javascript; charset=utf-8");
    assert_eq!(ui_content_type("icon.svg"), "image/svg+xml");
    assert_eq!(ui_content_type("unknown.bin"), "application/octet-stream");
}

#[test]
fn alias_doc_edits_preserve_comments() {
    let mut doc: DocumentMut = "# my proxy\nlisten_addr = \"127.0.0.1:4000\" # keep\n\n\
         [model_aliases]\n# alias comment\nold = \"target\"\n"
        .parse()
        .unwrap();
    let table = ensure_table(&mut doc, "model_aliases").unwrap();
    table.insert("gpt-5.5", value("Qwen/Qwen3-32B"));
    let mut inline = toml_edit::InlineTable::new();
    inline.insert("provider", "embeddings".into());
    inline.insert("model", "Qwen/Qwen3-Embedding-0.6B".into());
    table.insert("embedding", value(inline));

    let rendered = doc.to_string();
    assert!(rendered.contains("# my proxy"));
    assert!(rendered.contains("# keep"));
    assert!(rendered.contains("# alias comment"));
    assert!(rendered.contains("\"gpt-5.5\" = \"Qwen/Qwen3-32B\""));
    assert!(rendered.contains("provider = \"embeddings\""));

    let config: Config = toml::from_str(&rendered).unwrap();
    assert_eq!(
        config.model_aliases.get("gpt-5.5"),
        Some(&ModelAlias::Model("Qwen/Qwen3-32B".to_string()))
    );
    assert_eq!(
        config.model_aliases.get("embedding"),
        Some(&ModelAlias::Route {
            provider: "embeddings".to_string(),
            model: "Qwen/Qwen3-Embedding-0.6B".to_string(),
        })
    );
}

#[test]
fn ensure_table_creates_missing_sections() {
    let mut doc: DocumentMut = "listen_addr = \"127.0.0.1:4000\"\n".parse().unwrap();
    doc.insert("routing_mode", value("intercept"));
    ensure_table(&mut doc, "interceptor").unwrap();
    let rendered = doc.to_string();
    assert!(rendered.contains("[interceptor]"));
    assert!(rendered.contains("routing_mode = \"intercept\""));
    let config: Config = toml::from_str(&rendered).unwrap();
    assert_eq!(config.routing_mode, crate::RoutingMode::Intercept);
}

#[test]
fn provider_put_body_merges_only_provided_fields() {
    let mut upstream = crate::UpstreamConfig {
        base_url: "http://127.0.0.1:8000/v1".to_string(),
        api_key: Some("placeholder".to_string()),
        api_key_env: Some("KEY_ENV".to_string()),
        ..crate::UpstreamConfig::default()
    };
    let body = ProviderPutBody {
        kind: None,
        base_url: Some("http://127.0.0.1:9000/v1 ".to_string()),
        auth: None,
        api_key: None,
        api_key_env: Some(String::new()),
        reasoning_effort: Some("high".to_string()),
        timeout_secs: Some(120),
        connect_timeout_secs: None,
        pool_max_idle_per_host: None,
        pool_idle_timeout_secs: None,
    };
    body.apply(&mut upstream).unwrap();
    assert_eq!(upstream.base_url, "http://127.0.0.1:9000/v1");
    assert_eq!(upstream.api_key.as_deref(), Some("placeholder")); // kept
    assert_eq!(upstream.api_key_env, None); // cleared
    assert_eq!(
        upstream.reasoning_effort,
        Some(crate::ReasoningEffort::High)
    );
    assert_eq!(upstream.timeout_secs, 120);
    assert_eq!(
        upstream.connect_timeout_secs,
        crate::UpstreamConfig::default().connect_timeout_secs
    );
}

#[test]
fn provider_doc_edit_writes_section_style_tables() {
    let mut doc: DocumentMut = "listen_addr = \"127.0.0.1:4000\"\n".parse().unwrap();
    let body = ProviderPutBody {
        kind: None,
        base_url: Some("http://127.0.0.1:11434/v1".to_string()),
        auth: None,
        api_key: None,
        api_key_env: Some("LOCAL_KEY".to_string()),
        reasoning_effort: Some("max".to_string()),
        timeout_secs: Some(30),
        connect_timeout_secs: Some(2),
        pool_max_idle_per_host: None,
        pool_idle_timeout_secs: None,
    };
    let providers = ensure_table(&mut doc, "providers").unwrap();
    providers.set_implicit(true);
    let item = providers
        .entry("local_interceptor")
        .or_insert(Item::Table(Table::new()));
    let table = item.as_table_mut().unwrap();
    table.set_implicit(false);
    body.apply_to_doc(table);

    let rendered = doc.to_string();
    assert!(
        rendered.contains("[providers.local_interceptor]"),
        "rendered:\n{rendered}"
    );
    assert!(!rendered.contains("[providers]\n"));
    let config: Config = toml::from_str(&rendered).unwrap();
    let provider = &config.providers["local_interceptor"];
    assert_eq!(provider.base_url, "http://127.0.0.1:11434/v1");
    assert_eq!(provider.api_key_env.as_deref(), Some("LOCAL_KEY"));
    assert_eq!(provider.reasoning_effort, Some(crate::ReasoningEffort::Max));
    assert_eq!(provider.timeout_secs, 30);
}

#[test]
fn integrated_doc_edit_writes_valid_target_tables() {
    let mut doc: DocumentMut =
        "# keep\n[interceptor]\nmodel = \"triage\"\n[integrated]\nrespect_explicit_model = false\n"
            .parse()
            .unwrap();
    let body = IntegratedPutBody {
        sticky_ttl_secs: Some(900),
        sticky_capacity: Some(64),
        instructions: Some("Route GUI work to the visual provider.".to_string()),
        targets: Some(BTreeMap::from([(
            "backend".to_string(),
            IntegratedTarget {
                provider: "openai".to_string(),
                model: "gpt-test".to_string(),
                hint: "tool-heavy work".to_string(),
                reasoning_effort: Some(crate::ReasoningEffort::Max),
            },
        )])),
    };
    body.apply_to_doc(&mut doc).unwrap();

    let rendered = doc.to_string();
    assert!(rendered.contains("# keep"));
    assert!(!rendered.contains("respect_explicit_model"));
    assert!(rendered.contains("[integrated.targets.backend]"));
    assert!(rendered.contains("reasoning_effort = \"max\""));
    let config: Config = toml::from_str(&rendered).unwrap();
    assert_eq!(config.integrated.targets["backend"].provider, "openai");
    assert_eq!(
        config.integrated.targets["backend"].reasoning_effort,
        Some(crate::ReasoningEffort::Max)
    );
    assert_eq!(
        config.integrated.instructions,
        "Route GUI work to the visual provider."
    );
}
