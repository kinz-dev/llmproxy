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
fn loopback_host_check_accepts_literal_loopback_and_loopback_names() {
    assert!(loopback_host_matches("127.0.0.1:11180", 11180));
    assert!(loopback_host_matches("[::1]:11180", 11180));
    // Wrong port.
    assert!(!loopback_host_matches("127.0.0.1:9999", 11180));
    // A name is admitted only by resolution, never by spelling: localhost is in
    // every resolver's loopback set, so it must now pass.
    assert!(loopback_host_matches("localhost:11180", 11180));
    assert!(loopback_host_matches("ip6-localhost:11180", 11180));
    // DNS rebinding: an attacker-chosen name does not resolve to loopback on
    // the victim's machine, so it fails on resolution rather than on being a
    // name. Absent names fail the same way, which is the fail-closed direction.
    assert!(!loopback_host_matches("attacker.com:11180", 11180));
    assert!(!loopback_host_matches("rebind.example:11180", 11180));
    // Non-loopback literal.
    assert!(!loopback_host_matches("10.0.0.5:11180", 11180));
    // Missing port / malformed.
    assert!(!loopback_host_matches("127.0.0.1", 11180));
    assert!(!loopback_host_matches("127.0.0.1:notaport", 11180));
    // Not a name at all: the character filter must reject before the resolver.
    assert!(!loopback_host_matches("local host:11180", 11180));
    assert!(!loopback_host_matches(":11180", 11180));
}

#[test]
fn loopback_name_check_fails_closed_on_unresolvable_and_mixed_names() {
    // Unresolvable resolves to nothing, which must not be read as "allowed".
    assert!(!host_is_loopback_by_name("llmproxy-does-not-exist.invalid"));
    // A name resolving to any non-loopback address is rejected outright, even
    // if it also yields a loopback record.
    assert!(!host_is_loopback_by_name("example.com"));
    // Empty and non-name input never reach the resolver.
    assert!(!host_is_loopback_by_name(""));
    assert!(!host_is_loopback_by_name("bad/name"));
    assert!(!host_is_loopback_by_name("a;b"));
    // The loopback names do resolve.
    assert!(host_is_loopback_by_name("localhost"));
}

#[test]
fn same_origin_check_allows_own_loopback_origin_and_loopback_names() {
    assert!(is_same_loopback_origin("http://127.0.0.1:11180", 11180));
    assert!(is_same_loopback_origin("http://[::1]:11180", 11180));
    assert!(is_same_loopback_origin("http://localhost:11180", 11180));
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
        enabled: None,
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
        fallback: None,
        circuit_breaker_threshold: None,
        circuit_breaker_cooldown_secs: None,
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
        enabled: None,
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
        fallback: Some(vec![" cloud/gpt ".to_string(), String::new()]),
        circuit_breaker_threshold: Some(3),
        circuit_breaker_cooldown_secs: Some(45),
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
    assert_eq!(provider.fallback, ["cloud/gpt"]);
    assert_eq!(provider.circuit_breaker_threshold, 3);
    assert_eq!(provider.circuit_breaker_cooldown_secs, 45);
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

#[test]
fn deleting_a_provider_removes_every_reference_to_it() {
    let raw = r#"
[upstream]
base_url = "http://127.0.0.1:9/v1"
fallback = ["cloud", "other/m"]

[providers.cloud]
base_url = "https://api.example.com/v1"
api_key = "k"

[providers.other]
base_url = "http://127.0.0.1:10/v1"
fallback = ["cloud/m"]

[model_aliases]
plain = "local-model"
big = { provider = "cloud", model = "m" }

[integrated.targets.HARD]
provider = "cloud"
model = "m"

[[routing_rules]]
name = "to-target"
route = "HARD"

[[routing_rules]]
name = "to-cloud"
route = "cloud/m"

[[routing_rules]]
name = "keep"
route = "PASS"

[pricing]
baseline_model = "cloud/m"
[pricing.models]
"cloud/m" = { input_per_mtok = 1.0, output_per_mtok = 2.0 }
"other/x" = { input_per_mtok = 1.0, output_per_mtok = 1.0 }
"#;
    let mut config = parse_config(raw, "test").unwrap();
    let removed = purge_provider(&mut config, "cloud").unwrap();
    assert!(!config.providers.contains_key("cloud"));
    assert_eq!(config.upstream.fallback, vec!["other/m".to_string()]);
    assert!(config.providers["other"].fallback.is_empty());
    assert!(
        config.model_aliases.contains_key("plain") && !config.model_aliases.contains_key("big")
    );
    assert!(config.integrated.targets.is_empty());
    assert_eq!(
        config
            .routing_rules
            .iter()
            .map(|rule| rule.name.as_str())
            .collect::<Vec<_>>(),
        ["keep"]
    );
    assert!(config.pricing.baseline_model.is_none());
    assert!(
        removed.iter().any(|item| item == "alias big"),
        "{removed:?}"
    );
    validate_config(&config).unwrap();

    let mut doc: DocumentMut = raw.parse().unwrap();
    purge_provider_doc(&mut doc, "cloud", "default");
    let reparsed = parse_config(&doc.to_string(), "test").unwrap();
    assert!(!reparsed.providers.contains_key("cloud"));
    assert_eq!(reparsed.routing_rules.len(), 1);
    assert!(!reparsed.model_aliases.contains_key("big"));
    assert!(reparsed.integrated.targets.is_empty());
    assert_eq!(reparsed.upstream.fallback, vec!["other/m".to_string()]);
    assert!(doc.to_string().contains("\"other/x\""));

    let mut config = parse_config(raw, "test").unwrap();
    assert!(
        purge_provider(&mut config, "default").is_err(),
        "default stays"
    );
}

#[test]
fn models_missing_from_a_catalog_become_its_first_model_in_config_and_file() {
    let raw = r#"
default_model = "gone"
[upstream]
base_url = "http://127.0.0.1:9/v1"
fallback = ["cloud/old"]

[providers.cloud]
base_url = "https://api.example.com/v1"
api_key = "k"

[model_aliases]
big = { provider = "cloud", model = "old" }
fine = { provider = "cloud", model = "kept" }

[[routing_rules]]
name = "r"
route = "cloud/old"
"#;
    let catalogs = HashMap::from([
        ("default".to_string(), vec!["local-a".to_string()]),
        (
            "cloud".to_string(),
            vec!["new".to_string(), "kept".to_string()],
        ),
    ]);
    let mut config = parse_config(raw, "test").unwrap();
    let fixes = model_sync::plan(&config, &catalogs);
    model_sync::apply(&mut config, &fixes);
    assert!(model_sync::plan(&config, &catalogs).is_empty());
    assert_eq!(config.default_model.as_deref(), Some("local-a"));

    let mut doc: DocumentMut = raw.parse().unwrap();
    model_sync::write(&mut doc, &fixes, "default");
    let saved = parse_config(&doc.to_string(), "test").unwrap();
    assert!(model_sync::plan(&saved, &catalogs).is_empty(), "{doc}");
    assert_eq!(saved.routing_rules[0].route, "cloud/new");
    assert_eq!(saved.upstream.fallback, vec!["cloud/new".to_string()]);
    assert!(
        model_sync::plan(&saved, &HashMap::new()).is_empty(),
        "empty catalogs change nothing"
    );
}

#[test]
fn a_turned_off_provider_is_never_routed_to() {
    let raw = r#"
routing_mode = "integrated"
[upstream]
base_url = "http://127.0.0.1:9/v1"

[providers.cloud]
enabled = false
base_url = "https://api.example.com/v1"
api_key = "k"

[providers.lan]
base_url = "http://10.0.0.5:8000/v1"

[interceptor]
provider = "default"
model = "m"

[integrated.targets.HARD]
provider = "cloud"
model = "m"
hint = "hard"

[integrated.targets.EASY]
provider = "lan"
model = "m"
hint = "easy"
"#;
    let config = parse_config(raw, "test").unwrap();
    validate_config(&config).unwrap();
    assert!(
        !effective_provider_configs(&config)
            .unwrap()
            .contains_key("cloud")
    );
    assert_eq!(config.active_targets().keys().collect::<Vec<_>>(), ["EASY"]);
    let off_default = raw.replace("[upstream]", "[upstream]\nenabled = false");
    assert!(validate_config(&parse_config(&off_default, "test").unwrap()).is_err());
}
