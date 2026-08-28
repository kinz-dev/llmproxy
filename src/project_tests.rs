use super::*;

fn sample_target(provider: &str) -> IntegratedTarget {
    IntegratedTarget {
        provider: provider.to_string(),
        model: "model-x".to_string(),
        hint: "frontend work".to_string(),
        reasoning_effort: None,
        ..IntegratedTarget::default()
    }
}

#[test]
fn project_file_accepts_targets_and_aliases() {
    let file = ProjectFile::parse(
        r#"
[model_aliases]
fast = { provider = "openai", model = "gpt-mini" }

[integrated.targets.frontend]
provider = "anthropic"
model = "claude-test"
hint = "GUI and frontend work"

[integrated.targets.everything]
provider = "openai"
model = "gpt-test"
hint = "everything else"
default = true
"#,
    )
    .expect("project file parses");
    assert_eq!(file.integrated.targets.len(), 2);
    assert!(file.integrated.targets["everything"].is_default);
    assert!(!file.integrated.targets["frontend"].is_default);
    assert_eq!(file.model_aliases.len(), 1);
}

#[test]
fn project_file_cannot_define_providers_or_credentials() {
    // The whole point of the overlay: cloning an untrusted repository must not
    // be able to decide where prompts are sent or which key pays for them.
    for forbidden in [
        "[providers.evil]\nbase_url = \"https://evil.invalid/v1\"\napi_key = \"sk-x\"\n",
        "listen_addr = \"0.0.0.0:8080\"\n",
        "[interceptor]\nprovider = \"evil\"\n",
        "default_provider = \"evil\"\n",
        "[admin]\nenable = true\n",
    ] {
        let error = ProjectFile::parse(forbidden).expect_err("forbidden key is rejected");
        assert!(
            format!("{error:#}").contains("unknown field"),
            "expected an unknown-field rejection for {forbidden:?}, got {error:#}"
        );
    }
}

#[test]
fn project_target_cannot_smuggle_an_unknown_field() {
    let error = ProjectFile::parse(
        r#"
[integrated.targets.frontend]
provider = "anthropic"
model = "claude-test"
hint = "GUI work"
base_url = "https://evil.invalid/v1"
"#,
    )
    .expect_err("unknown target field is rejected");
    assert!(format!("{error:#}").contains("unknown field"));
}

#[test]
fn overlay_validation_requires_user_level_providers() {
    let mut file = ProjectFile::default();
    file.integrated
        .targets
        .insert("frontend".to_string(), sample_target("anthropic"));

    let error = validate_overlay(&file, |name| name == "openai", "openai")
        .expect_err("unknown provider is rejected");
    assert!(format!("{error:#}").contains("not defined in the user-level configuration"));

    validate_overlay(&file, |name| name == "anthropic", "openai")
        .expect("known provider is accepted");
}

#[test]
fn overlay_validation_covers_alias_providers() {
    let mut file = ProjectFile::default();
    file.model_aliases.insert(
        "fast".to_string(),
        ModelAlias::Route {
            provider: "ghost".to_string(),
            model: "gpt-mini".to_string(),
        },
    );
    assert!(validate_overlay(&file, |name| name == "openai", "openai").is_err());

    // A bare-string alias inherits the default provider, which must exist.
    let mut file = ProjectFile::default();
    file.model_aliases.insert(
        "fast".to_string(),
        ModelAlias::Model("gpt-mini".to_string()),
    );
    assert!(validate_overlay(&file, |name| name == "openai", "openai").is_ok());
    assert!(validate_overlay(&file, |_| false, "openai").is_err());
}

#[test]
fn overlay_rejects_the_reserved_auto_alias() {
    let mut file = ProjectFile::default();
    file.model_aliases.insert(
        crate::INTEGRATED_AUTO_MODEL.to_string(),
        ModelAlias::Model("gpt-mini".to_string()),
    );
    let error = validate_overlay(&file, |_| true, "openai").expect_err("Auto is reserved");
    assert!(format!("{error:#}").contains("reserved"));
}

#[test]
fn project_targets_win_on_a_name_collision_and_never_remove_user_routes() {
    let mut config = crate::Config::default();
    config
        .integrated
        .targets
        .insert("frontend".to_string(), sample_target("user-provider"));
    config
        .integrated
        .targets
        .insert("backend".to_string(), sample_target("user-provider"));
    config.model_aliases.insert(
        "fast".to_string(),
        ModelAlias::Model("user-model".to_string()),
    );

    let mut file = ProjectFile::default();
    file.integrated
        .targets
        .insert("frontend".to_string(), sample_target("project-provider"));
    file.model_aliases.insert(
        "fast".to_string(),
        ModelAlias::Model("project-model".to_string()),
    );
    let scope = ProjectScope {
        root: PathBuf::from("/tmp/demo"),
        name: "demo".to_string(),
        file,
    };
    scope.apply(&mut config);

    assert_eq!(
        config.integrated.targets["frontend"].provider,
        "project-provider"
    );
    assert_eq!(
        config.integrated.targets["backend"].provider,
        "user-provider"
    );
    assert_eq!(config.model_aliases["fast"].model(), "project-model");
    assert_eq!(config.integrated.targets.len(), 2);
}

#[test]
fn discovery_walks_up_to_the_nearest_project_root() {
    let base = std::env::temp_dir().join(format!(
        "llmproxy-project-{}-{}",
        std::process::id(),
        line!()
    ));
    let nested = base.join("crates").join("inner");
    std::fs::create_dir_all(&nested).expect("temp tree");
    std::fs::write(base.join(PROJECT_FILE_NAME), "").expect("project file");

    assert_eq!(discover_from(&nested).as_deref(), Some(base.as_path()));
    assert_eq!(discover_from(&base).as_deref(), Some(base.as_path()));

    let scope = ProjectScope::load(&base).expect("empty project file loads");
    assert!(scope.file().is_empty());
    assert_eq!(scope.name(), base.file_name().unwrap().to_str().unwrap());

    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn explicit_project_path_must_contain_a_project_file() {
    let base = std::env::temp_dir().join(format!(
        "llmproxy-project-{}-{}",
        std::process::id(),
        line!()
    ));
    std::fs::create_dir_all(&base).expect("temp dir");
    let error = resolve(Some(&base), &base).expect_err("missing project file is an error");
    assert!(format!("{error:#}").contains(PROJECT_FILE_NAME));
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn cached_scope_reloads_after_the_ttl_and_stays_bounded() {
    clear_cache();
    let base = std::env::temp_dir().join(format!(
        "llmproxy-project-{}-{}",
        std::process::id(),
        line!()
    ));
    std::fs::create_dir_all(&base).expect("temp dir");
    std::fs::write(
        base.join(PROJECT_FILE_NAME),
        "[integrated.targets.a]\nprovider = \"p\"\nmodel = \"m\"\nhint = \"h\"\n",
    )
    .expect("project file");

    let first = cached_scope(&base).expect("loads");
    let second = cached_scope(&base).expect("cached");
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(first.file().integrated.targets.len(), 1);

    std::fs::remove_dir_all(&base).ok();
    clear_cache();
}
