use super::*;

#[test]
fn missing_listen_address_uses_loopback_random_port() {
    let admin = AdminConfig::default();
    assert_eq!(listen_address(&admin), "127.0.0.1:0");
    let admin = AdminConfig {
        listen_address: Some("127.0.0.1:11180".to_string()),
        ..AdminConfig::default()
    };
    assert_eq!(listen_address(&admin), "127.0.0.1:11180");
}

#[test]
fn admin_config_defaults_to_disabled() {
    let config: Config = toml::from_str("").unwrap();
    assert!(!config.admin.enable);
    assert!(config.admin.listen_address.is_none());

    let config: Config = toml::from_str("[admin]\nenable = true\n").unwrap();
    assert!(config.admin.enable);

    // `enabled` is accepted as an alias.
    let config: Config = toml::from_str("[admin]\nenabled = true\n").unwrap();
    assert!(config.admin.enable);
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

    // A non-loopback address is fine while the admin interface stays disabled.
    let config: Config = toml::from_str("[admin]\nlisten_address = \"0.0.0.0:11180\"\n").unwrap();
    validate_config(&config).unwrap();
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

    // Round-trips into a valid config with both alias forms.
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
    ensure_table(&mut doc, "interceptor")
        .unwrap()
        .insert("enabled", value(true));
    let rendered = doc.to_string();
    assert!(rendered.contains("[interceptor]"));
    assert!(rendered.contains("enabled = true"));
    let config: Config = toml::from_str(&rendered).unwrap();
    assert!(config.interceptor.enabled);
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
        base_url: Some("http://127.0.0.1:9000/v1 ".to_string()),
        api_key: None,
        api_key_env: Some(String::new()),
        timeout_secs: Some(120),
        connect_timeout_secs: None,
        pool_max_idle_per_host: None,
        pool_idle_timeout_secs: None,
    };
    body.apply(&mut upstream);
    assert_eq!(upstream.base_url, "http://127.0.0.1:9000/v1");
    assert_eq!(upstream.api_key.as_deref(), Some("placeholder")); // kept
    assert_eq!(upstream.api_key_env, None); // cleared
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
        base_url: Some("http://127.0.0.1:11434/v1".to_string()),
        api_key: None,
        api_key_env: Some("LOCAL_KEY".to_string()),
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
    assert_eq!(provider.timeout_secs, 30);
}
