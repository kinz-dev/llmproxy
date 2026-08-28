use std::collections::{BTreeMap, HashMap};

use axum::http::{HeaderMap, HeaderValue};
use serde_json::{Value, json};

use std::time::Duration;

use super::{
    IntegratedConfig, IntegratedTarget, StickyBounds, StickyDecision, StickyPolicy, StickyRoutes,
    apply_escalation_floor, cacheable_decision, classify_completion_details,
    classify_completion_with_targets, failover_chain, forced_route_for, routable_targets,
    trim_triage_messages, validate_integrated_config,
};
use crate::Config;
use crate::INTEGRATED_AUTO_MODEL;
use crate::interceptor::CompletionDecision;
use crate::interceptor::InterceptionPolicy;
use crate::interceptor::InterceptorConfig;
use crate::interceptor::InterceptorProtocol;
use crate::interceptor::build_local_request;
use crate::interceptor::classify_completion;
use crate::interceptor::is_pass_tool;
use crate::route_cache::CachedDecision;
use crate::routing_verbs::RoutingVerb;
use crate::text_from_value;

#[test]
fn codex_control_responses_bypass_local_interception() {
    let headers = HeaderMap::new();
    let metadata = json!({ "request_kind": "compaction" }).to_string();
    let request = json!({
        "client_metadata": {
            "x-codex-turn-metadata": metadata
        }
    });
    assert_eq!(
        InterceptionPolicy::for_responses(&headers, &request),
        InterceptionPolicy::Bypass {
            request_kind: "compaction".to_string()
        }
    );

    let mut headers = HeaderMap::new();
    headers.insert(
        "x-codex-turn-metadata",
        HeaderValue::from_static(r#"{"request_kind":"prewarm"}"#),
    );
    assert_eq!(
        InterceptionPolicy::for_responses(&headers, &json!({ "input": "warm up" })),
        InterceptionPolicy::Bypass {
            request_kind: "prewarm".to_string()
        }
    );

    let request = json!({
        "client_metadata": {
            "x-codex-turn-metadata": json!({ "request_kind": "turn" }).to_string()
        },
        "input": [{ "type": "compaction_trigger" }]
    });
    assert_eq!(
        InterceptionPolicy::for_responses(&HeaderMap::new(), &request),
        InterceptionPolicy::Bypass {
            request_kind: "compaction".to_string()
        }
    );
}

#[test]
fn ordinary_responses_remain_eligible_for_local_interception() {
    let turn = json!({
        "client_metadata": {
            "x-codex-turn-metadata": json!({ "request_kind": "turn" }).to_string()
        },
        "input": [{
            "type": "compaction",
            "encrypted_content": "prior-history-is-not-a-trigger"
        }]
    });
    assert_eq!(
        InterceptionPolicy::for_responses(&HeaderMap::new(), &turn),
        InterceptionPolicy::Eligible
    );
    assert_eq!(
        InterceptionPolicy::for_responses(
            &HeaderMap::new(),
            &json!({ "input": "ordinary non-Codex request" }),
        ),
        InterceptionPolicy::Eligible
    );
}

#[test]
fn default_route_control_must_be_the_only_local_output() {
    let pass = json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": Value::Null,
                "tool_calls": [{
                    "type": "function",
                    "function": { "name": "PASS", "arguments": "{}" }
                }]
            }
        }]
    });
    assert_eq!(classify_completion(&pass), CompletionDecision::DefaultRoute);

    let mut reasoned = pass.clone();
    reasoned["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"] =
        json!(r#"{"reason":"requires current external research"}"#);
    let classified = classify_completion_details(&reasoned, &BTreeMap::new(), false);
    assert_eq!(classified.decision, CompletionDecision::DefaultRoute);
    assert_eq!(
        classified.reason.as_deref(),
        Some("requires current external research")
    );

    let mut route_default = pass.clone();
    route_default["choices"][0]["message"]["tool_calls"][0]["function"]["name"] =
        json!("ROUTE_DEFAULT");
    assert_eq!(
        classify_completion(&route_default),
        CompletionDecision::DefaultRoute
    );

    let mixed = json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "also do this",
                "tool_calls": [{
                    "type": "function",
                    "function": { "name": "PASS", "arguments": "{}" }
                }]
            }
        }]
    });
    assert_eq!(
        classify_completion(&mixed),
        CompletionDecision::InvalidControl
    );

    let mut arguments = pass;
    arguments["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"] =
        json!(r#"{"unexpected":true}"#);
    assert_eq!(
        classify_completion(&arguments),
        CompletionDecision::InvalidControl
    );
}

#[test]
fn sensitive_rejection_is_optional_and_uses_safe_reason_categories() {
    let config = Config::default();
    let interceptor = InterceptorConfig {
        reject_sensitive: true,
        model: "triage".to_string(),
        ..InterceptorConfig::default()
    };
    let local = build_local_request(
        json!({ "messages": [{ "role": "user", "content": "sensitive request" }] }),
        &interceptor,
        InterceptorProtocol::ChatCompletions,
        Some("agent-model"),
        &HashMap::new(),
        &config,
        false,
    )
    .unwrap();
    let reject = local["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["function"]["name"] == "REJECT")
        .unwrap();
    assert_eq!(
        reject["function"]["parameters"]["properties"]["reason"]["enum"],
        json!([
            "credential or secret",
            "personally identifiable information",
            "other sensitive information"
        ])
    );
    assert!(local.to_string().contains("Mandatory pre-routing rule"));
    assert!(local.to_string().contains("rejection-screening only"));

    let completion = json!({
        "choices": [{
            "message": {
                "content": Value::Null,
                "tool_calls": [{
                    "function": {
                        "name": "REJECT",
                        "arguments": r#"{"reason":"personally identifiable information"}"#
                    }
                }]
            }
        }]
    });
    let classified = classify_completion_details(&completion, &BTreeMap::new(), true);
    assert_eq!(classified.decision, CompletionDecision::Reject);
    assert_eq!(
        classified.reason.as_deref(),
        Some("personally identifiable information")
    );
    assert_eq!(
        classify_completion_details(&completion, &BTreeMap::new(), false).decision,
        CompletionDecision::Local
    );

    let mut unsafe_reason = completion;
    unsafe_reason["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"] =
        json!(r#"{"reason":"the secret was abc123"}"#);
    assert_eq!(
        classify_completion_details(&unsafe_reason, &BTreeMap::new(), true)
            .reason
            .as_deref(),
        Some("other sensitive information")
    );
}

#[test]
fn integrated_redirect_must_be_the_only_local_output() {
    let targets = BTreeMap::from([(
        "PROVIDER_OPENAI".to_string(),
        IntegratedTarget {
            provider: "openai".to_string(),
            model: "gpt-test".to_string(),
            hint: "complex work".to_string(),
            reasoning_effort: None,
            ..IntegratedTarget::default()
        },
    )]);
    let redirect = json!({
        "choices": [{
            "message": {
                "content": Value::Null,
                "tool_calls": [{
                    "function": { "name": "PROVIDER-OPENAI", "arguments": "{}" }
                }]
            }
        }]
    });
    assert_eq!(
        classify_completion_with_targets(&redirect, &targets),
        CompletionDecision::Redirect("PROVIDER_OPENAI".to_string())
    );

    let mut mixed = redirect;
    mixed["choices"][0]["message"]["content"] = json!("partial answer");
    assert_eq!(
        classify_completion_with_targets(&mixed, &targets),
        CompletionDecision::InvalidControl
    );

    let mut arguments = mixed;
    arguments["choices"][0]["message"]["content"] = Value::Null;
    arguments["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"] =
        json!(r#"{"model":"other"}"#);
    assert_eq!(
        classify_completion_with_targets(&arguments, &targets),
        CompletionDecision::InvalidControl
    );
}

#[test]
fn integrated_request_adds_target_tools_and_prompt_hints() {
    let proxy_config = Config {
        integrated: IntegratedConfig {
            enabled: true,
            targets: BTreeMap::from([(
                "backend".to_string(),
                IntegratedTarget {
                    provider: "openai".to_string(),
                    model: "gpt-test".to_string(),
                    hint: "complex coding".to_string(),
                    reasoning_effort: Some(crate::ReasoningEffort::Max),
                    ..IntegratedTarget::default()
                },
            )]),
            ..IntegratedConfig::default()
        },
        ..Config::default()
    };
    let local = build_local_request(
        json!({ "messages": [{ "role": "user", "content": "work" }] }),
        &InterceptorConfig {
            model: "triage".to_string(),
            ..InterceptorConfig::default()
        },
        InterceptorProtocol::ChatCompletions,
        Some(INTEGRATED_AUTO_MODEL),
        &HashMap::new(),
        &proxy_config,
        false,
    )
    .unwrap();

    let tool_names = local["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect::<Vec<_>>();
    assert!(tool_names.contains(&"ROUTE_DEFAULT"));
    assert!(!tool_names.contains(&"PASS"));
    assert!(tool_names.contains(&"backend"));
    for tool in local["tools"].as_array().unwrap() {
        assert_eq!(
            tool["function"]["parameters"]["required"],
            json!(["reason"])
        );
        assert_eq!(
            tool["function"]["parameters"]["additionalProperties"],
            false
        );
    }
    let prompt = local["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| text_from_value(&message["content"]))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(prompt.contains("Call backend when: complex coding."));
    assert!(prompt.contains("Destination: openai/gpt-test with max reasoning effort."));
    assert!(prompt.contains("ROUTE_DEFAULT"));
}

#[test]
fn explicit_model_in_integrated_config_uses_pass_only_prompt() {
    let proxy_config = Config {
        integrated: IntegratedConfig {
            enabled: true,
            targets: BTreeMap::from([(
                "backend".to_string(),
                IntegratedTarget {
                    provider: "openai".to_string(),
                    model: "gpt-test".to_string(),
                    hint: "complex coding".to_string(),
                    reasoning_effort: None,
                    ..IntegratedTarget::default()
                },
            )]),
            ..IntegratedConfig::default()
        },
        ..Config::default()
    };
    let local = build_local_request(
        json!({ "messages": [{ "role": "user", "content": "work" }] }),
        &InterceptorConfig {
            model: "triage".to_string(),
            ..InterceptorConfig::default()
        },
        InterceptorProtocol::ChatCompletions,
        Some("openai/gpt-test"),
        &HashMap::new(),
        &proxy_config,
        false,
    )
    .unwrap();

    let tool_names = local["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect::<Vec<_>>();
    assert!(tool_names.contains(&"PASS"));
    assert!(!tool_names.contains(&"ROUTE_DEFAULT"));
    assert!(!tool_names.contains(&"backend"));
    assert!(local.to_string().contains("local fast path"));
    assert!(!local.to_string().contains("routing tier"));
}

#[test]
fn integrated_mode_swaps_in_the_routing_tier_prompt_unless_overridden() {
    let proxy_config = Config {
        integrated: IntegratedConfig {
            enabled: true,
            targets: BTreeMap::from([(
                "PROVIDER_OPENAI".to_string(),
                IntegratedTarget {
                    provider: "openai".to_string(),
                    model: "gpt-test".to_string(),
                    hint: "everything else (default route)".to_string(),
                    reasoning_effort: None,
                    ..IntegratedTarget::default()
                },
            )]),
            ..IntegratedConfig::default()
        },
        ..Config::default()
    };
    let request = json!({ "messages": [{ "role": "user", "content": "work" }] });
    let prompt_for = |config: &InterceptorConfig| {
        let local = build_local_request(
            request.clone(),
            config,
            InterceptorProtocol::ChatCompletions,
            Some(INTEGRATED_AUTO_MODEL),
            &HashMap::new(),
            &proxy_config,
            false,
        )
        .unwrap();
        local["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| text_from_value(&message["content"]))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let stock = prompt_for(&InterceptorConfig {
        model: "triage".to_string(),
        ..InterceptorConfig::default()
    });
    assert!(stock.contains("routing tier"));
    assert!(!stock.contains("local fast path"));
    assert!(stock.contains("ROUTE_DEFAULT"));
    assert!(!stock.contains("PASS"));

    // Intercept-mode overrides stay out of integrated mode; the integrated
    // policy has its own override field.
    let intercept_override = prompt_for(&InterceptorConfig {
        model: "triage".to_string(),
        instructions: "Custom intercept policy.".to_string(),
        ..InterceptorConfig::default()
    });
    assert!(intercept_override.contains("routing tier"));
    assert!(!intercept_override.contains("Custom intercept policy."));

    let mut config_with_override = proxy_config.clone();
    config_with_override.integrated.instructions = "Custom integrated policy.".to_string();
    let overridden = build_local_request(
        request.clone(),
        &InterceptorConfig {
            model: "triage".to_string(),
            ..InterceptorConfig::default()
        },
        InterceptorProtocol::ChatCompletions,
        Some(INTEGRATED_AUTO_MODEL),
        &HashMap::new(),
        &config_with_override,
        false,
    )
    .unwrap();
    let overridden = overridden["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| text_from_value(&message["content"]))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(overridden.contains("Custom integrated policy."));
    assert!(!overridden.contains("routing tier"));
    // Route hints are still appended around a custom policy.
    assert!(overridden.contains("Call PROVIDER_OPENAI when: everything else (default route)."));

    // Without integrated mode the stock intercept prompt is untouched.
    let plain = build_local_request(
        request,
        &InterceptorConfig {
            model: "triage".to_string(),
            ..InterceptorConfig::default()
        },
        InterceptorProtocol::ChatCompletions,
        None,
        &HashMap::new(),
        &Config::default(),
        false,
    )
    .unwrap();
    let plain_prompt = plain["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| text_from_value(&message["content"]))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(plain_prompt.contains("local fast path"));
    assert!(!plain_prompt.contains("routing tier"));
}

#[test]
fn local_request_is_dynamic_without_mutating_original() {
    let original = json!({
        "model": "external-model",
        "stream": true,
        "messages": [{ "role": "user", "content": "run pwd" }],
        "tools": [{
            "type": "function",
            "function": {
                "name": "shell",
                "parameters": { "type": "object" }
            }
        }]
    });
    let config = InterceptorConfig {
        enabled: true,
        provider: "local".to_string(),
        model: "local-model".to_string(),
        ..InterceptorConfig::default()
    };
    let local = build_local_request(
        original.clone(),
        &config,
        InterceptorProtocol::ChatCompletions,
        Some("external-model"),
        &HashMap::new(),
        &Config::default(),
        false,
    )
    .unwrap();

    assert_eq!(original["model"], "external-model");
    assert_eq!(original["stream"], true);
    assert_eq!(local["model"], "local-model");
    assert_eq!(local["stream"], false);
    assert!(local["tools"].as_array().unwrap().iter().any(is_pass_tool));
    let messages = local["messages"].as_array().unwrap();
    assert!(
        messages
            .iter()
            .any(|message| text_from_value(&message["content"]).contains("run pwd"))
    );
    assert!(text_from_value(&messages[0]["content"]).contains("PASS"));
}

#[test]
fn local_request_applies_routing_decision_params() {
    let original = json!({
        "model": "external-model",
        "max_completion_tokens": 32000,
        "messages": [{ "role": "user", "content": "run pwd" }]
    });
    let config = InterceptorConfig {
        enabled: true,
        provider: "local".to_string(),
        model: "local-model".to_string(),
        ..InterceptorConfig::default()
    };
    let local = build_local_request(
        original,
        &config,
        InterceptorProtocol::ChatCompletions,
        None,
        &HashMap::new(),
        &Config::default(),
        false,
    )
    .unwrap();

    assert_eq!(local["temperature"], 0.0);
    assert_eq!(local["max_tokens"], 512);
    assert_eq!(local["parallel_tool_calls"], false);
    assert!(local.get("max_completion_tokens").is_none());
}

#[test]
fn zero_max_tokens_leaves_client_limits_alone() {
    let original = json!({
        "max_completion_tokens": 32000,
        "messages": [{ "role": "user", "content": "hi" }]
    });
    let config = InterceptorConfig {
        max_tokens: 0,
        ..InterceptorConfig::default()
    };
    let local = build_local_request(
        original,
        &config,
        InterceptorProtocol::ChatCompletions,
        None,
        &HashMap::new(),
        &Config::default(),
        false,
    )
    .unwrap();

    assert!(local.get("max_tokens").is_none());
    assert_eq!(local["max_completion_tokens"], 32000);
}

#[test]
fn truncated_completions_are_detected() {
    let clipped = json!({
        "choices": [{ "finish_reason": "length", "message": { "content": "partial" } }]
    });
    assert!(crate::interceptor::completion_truncated(&clipped));
    let complete = json!({
        "choices": [{ "finish_reason": "stop", "message": { "content": "done" } }]
    });
    assert!(!crate::interceptor::completion_truncated(&complete));
    assert!(!crate::interceptor::completion_truncated(&json!({})));
}

#[test]
fn temperature_out_of_range_is_rejected() {
    let mut providers = HashMap::new();
    providers.insert("local".to_string(), crate::UpstreamConfig::default());
    let config = InterceptorConfig {
        enabled: true,
        provider: "local".to_string(),
        model: "m".to_string(),
        temperature: 2.5,
        ..InterceptorConfig::default()
    };
    let error = crate::interceptor::validate_config(&config, &providers)
        .unwrap_err()
        .to_string();
    assert!(error.contains("temperature"), "unexpected error: {error}");
}

#[test]
fn integrated_external_targets_require_proxy_credentials() {
    let mut provider = crate::UpstreamConfig {
        base_url: "https://provider.example/v1".to_string(),
        ..crate::UpstreamConfig::default()
    };
    let mut providers = HashMap::from([
        ("local".to_string(), crate::UpstreamConfig::default()),
        ("external".to_string(), provider.clone()),
    ]);
    let config = IntegratedConfig {
        enabled: true,
        targets: BTreeMap::from([(
            "PROVIDER_EXTERNAL".to_string(),
            IntegratedTarget {
                provider: "external".to_string(),
                model: "model".to_string(),
                hint: "complex work".to_string(),
                reasoning_effort: None,
                ..IntegratedTarget::default()
            },
        )]),
        ..IntegratedConfig::default()
    };
    let interceptor = InterceptorConfig {
        provider: "local".to_string(),
        ..InterceptorConfig::default()
    };
    assert!(
        crate::interceptor::validate_integrated_config(&config, &interceptor, &providers)
            .unwrap_err()
            .to_string()
            .contains("proxy-owned")
    );

    provider.api_key = Some("placeholder".to_string());
    providers.insert("external".to_string(), provider);
    crate::interceptor::validate_integrated_config(&config, &interceptor, &providers).unwrap();
}

#[test]
fn integrated_target_names_are_generic_tool_names() {
    let providers = HashMap::from([
        ("local".to_string(), crate::UpstreamConfig::default()),
        ("external".to_string(), crate::UpstreamConfig::default()),
    ]);
    let interceptor = InterceptorConfig {
        provider: "local".to_string(),
        ..InterceptorConfig::default()
    };
    let target = IntegratedTarget {
        provider: "external".to_string(),
        model: "model".to_string(),
        hint: "complex work".to_string(),
        reasoning_effort: None,
        ..IntegratedTarget::default()
    };
    let config_for = |name: String| IntegratedConfig {
        enabled: true,
        targets: BTreeMap::from([(name, target.clone())]),
        ..IntegratedConfig::default()
    };

    for name in ["backend", "visual-route", "ROUTE_2"] {
        crate::interceptor::validate_integrated_config(
            &config_for(name.to_string()),
            &interceptor,
            &providers,
        )
        .unwrap();
    }
    for name in [
        "PASS".to_string(),
        "ROUTE_DEFAULT".to_string(),
        "bad name".to_string(),
        "x".repeat(65),
    ] {
        assert!(
            crate::interceptor::validate_integrated_config(
                &config_for(name),
                &interceptor,
                &providers,
            )
            .is_err()
        );
    }
}

fn target_named(provider: &str, strength: Option<u32>) -> IntegratedTarget {
    IntegratedTarget {
        provider: provider.to_string(),
        model: "model-x".to_string(),
        hint: "work".to_string(),
        reasoning_effort: None,
        strength,
        ..IntegratedTarget::default()
    }
}

/// Back-date a sticky entry so the absolute bound can be exercised without
/// making the test wait for wall-clock time to pass.
fn age_sticky_entry(routes: &StickyRoutes, key: u64, by: Duration) {
    let mut entries = routes.entries.lock().expect("sticky store poisoned");
    let route = entries.0.get_mut(&key).expect("entry present");
    route.created_at -= by;
}

#[test]
fn a_pinned_route_is_reused_while_it_is_inside_every_bound() {
    let routes = StickyRoutes::default();
    routes.insert(1, "backend".to_string(), 600, 16, None);
    assert_eq!(
        routes.lookup(1, StickyBounds::default(), None),
        StickyDecision::Reuse("backend".to_string())
    );
}

#[test]
fn the_absolute_bound_survives_a_refresh_that_slides_the_idle_ttl() {
    let routes = StickyRoutes::default();
    let bounds = StickyBounds {
        absolute_ttl: Some(Duration::from_secs(60)),
        max_turns: None,
        drift_below: 0.0,
    };
    routes.insert(7, "cheap".to_string(), 600, 16, None);
    // Continuous use: every turn refreshes the sliding TTL, which is exactly
    // the case a sliding-only TTL never bounds.
    for _ in 0..5 {
        assert!(matches!(
            routes.lookup(7, bounds, None),
            StickyDecision::Reuse(_)
        ));
        routes.insert(7, "cheap".to_string(), 600, 16, None);
    }
    age_sticky_entry(&routes, 7, Duration::from_secs(61));
    assert_eq!(
        routes.lookup(7, bounds, None),
        StickyDecision::Retriage {
            previous: "cheap".to_string()
        }
    );
}

#[test]
fn the_turn_bound_reconsiders_routing_without_dropping_the_previous_route() {
    let routes = StickyRoutes::default();
    let bounds = StickyBounds {
        absolute_ttl: None,
        max_turns: Some(3),
        drift_below: 0.0,
    };
    for _ in 0..3 {
        routes.insert(9, "cheap".to_string(), 600, 16, None);
    }
    assert_eq!(
        routes.lookup(9, bounds, None),
        StickyDecision::Retriage {
            previous: "cheap".to_string()
        }
    );
}

#[test]
fn re_pinning_a_different_route_restarts_the_decision_clock() {
    let routes = StickyRoutes::default();
    let bounds = StickyBounds {
        absolute_ttl: None,
        max_turns: Some(2),
        drift_below: 0.0,
    };
    routes.insert(11, "cheap".to_string(), 600, 16, None);
    routes.insert(11, "cheap".to_string(), 600, 16, None);
    assert!(matches!(
        routes.lookup(11, bounds, None),
        StickyDecision::Retriage { .. }
    ));
    routes.insert(11, "strong".to_string(), 600, 16, None);
    assert_eq!(
        routes.lookup(11, bounds, None),
        StickyDecision::Reuse("strong".to_string())
    );
}

#[test]
fn an_idle_gap_drops_the_entry_and_carries_nothing_forward() {
    let routes = StickyRoutes::default();
    routes.insert(13, "cheap".to_string(), 0, 16, None);
    assert_eq!(
        routes.lookup(13, StickyBounds::default(), None),
        StickyDecision::Fresh
    );
    assert_eq!(
        routes.lookup(13, StickyBounds::default(), None),
        StickyDecision::Fresh
    );
}

#[test]
fn escalate_refuses_a_demotion_but_allows_an_upgrade() {
    let config = IntegratedConfig {
        sticky_policy: StickyPolicy::Escalate,
        targets: BTreeMap::from([
            ("cheap".to_string(), target_named("openai", Some(1))),
            ("strong".to_string(), target_named("openai", Some(5))),
        ]),
        ..IntegratedConfig::default()
    };
    // A session already on the strong route must not be moved down.
    assert_eq!(
        apply_escalation_floor(&config, "cheap", Some("strong")),
        Some("strong".to_string())
    );
    // Moving up is exactly what the policy is for.
    assert_eq!(
        apply_escalation_floor(&config, "strong", Some("cheap")),
        None
    );
    // Same route, and an unknown floor, change nothing.
    assert_eq!(
        apply_escalation_floor(&config, "cheap", Some("cheap")),
        None
    );
    assert_eq!(
        apply_escalation_floor(&config, "cheap", Some("ghost")),
        None
    );
    assert_eq!(apply_escalation_floor(&config, "cheap", None), None);
}

#[test]
fn an_unranked_configuration_never_blocks_a_route_change() {
    let config = IntegratedConfig {
        targets: BTreeMap::from([
            ("a".to_string(), target_named("openai", None)),
            ("b".to_string(), target_named("openai", None)),
        ]),
        ..IntegratedConfig::default()
    };
    assert_eq!(apply_escalation_floor(&config, "a", Some("b")), None);
}

#[test]
fn a_target_named_after_a_verb_is_rejected_while_verbs_are_on() {
    let providers = HashMap::from([
        ("local".to_string(), crate::UpstreamConfig::default()),
        (
            "openai".to_string(),
            crate::UpstreamConfig {
                base_url: "https://api.openai.invalid/v1".to_string(),
                api_key: Some("k".to_string()),
                ..crate::UpstreamConfig::default()
            },
        ),
    ]);
    let interceptor = InterceptorConfig {
        enabled: true,
        provider: "local".to_string(),
        model: "triage".to_string(),
        ..InterceptorConfig::default()
    };
    let mut config = IntegratedConfig {
        enabled: true,
        targets: BTreeMap::from([("triage".to_string(), target_named("openai", None))]),
        ..IntegratedConfig::default()
    };
    let error = validate_integrated_config(&config, &interceptor, &providers)
        .expect_err("verb collision is rejected");
    assert!(format!("{error:#}").contains("in-band routing verb"));

    config.verbs = false;
    validate_integrated_config(&config, &interceptor, &providers)
        .expect("turning verbs off resolves the collision");
}

#[test]
fn exactly_one_target_may_be_the_catch_all_after_a_merge() {
    let providers = HashMap::from([
        ("local".to_string(), crate::UpstreamConfig::default()),
        (
            "openai".to_string(),
            crate::UpstreamConfig {
                base_url: "https://api.openai.invalid/v1".to_string(),
                api_key: Some("k".to_string()),
                ..crate::UpstreamConfig::default()
            },
        ),
    ]);
    let interceptor = InterceptorConfig {
        enabled: true,
        provider: "local".to_string(),
        model: "triage".to_string(),
        ..InterceptorConfig::default()
    };
    let mut first = target_named("openai", None);
    first.is_default = true;
    let mut second = target_named("openai", None);
    second.is_default = true;
    let config = IntegratedConfig {
        enabled: true,
        targets: BTreeMap::from([("one".to_string(), first), ("two".to_string(), second)]),
        ..IntegratedConfig::default()
    };
    let error = validate_integrated_config(&config, &interceptor, &providers)
        .expect_err("two catch-alls are rejected");
    assert!(format!("{error:#}").contains("default = true"));
}

#[test]
fn triage_trimming_keeps_system_messages_and_starts_on_a_user_turn() {
    let mut request = json!({
        "messages": [
            { "role": "system", "content": "policy" },
            { "role": "user", "content": "one" },
            { "role": "assistant", "content": "a", "tool_calls": [] },
            { "role": "tool", "content": "t" },
            { "role": "user", "content": "two" },
            { "role": "assistant", "content": "b" },
            { "role": "user", "content": "three" }
        ]
    });
    trim_triage_messages(&mut request, 3);
    let roles = request["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .map(|message| message["role"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    // A tool result never arrives without the assistant call that produced it.
    assert_eq!(roles, vec!["system", "user", "assistant", "user"]);
    assert_eq!(request["messages"][1]["content"], "two");
}

#[test]
fn triage_trimming_is_a_no_op_when_disabled_or_already_short() {
    let original = json!({
        "messages": [
            { "role": "user", "content": "one" },
            { "role": "assistant", "content": "a" }
        ]
    });
    let mut request = original.clone();
    trim_triage_messages(&mut request, 0);
    assert_eq!(request, original);

    let mut request = original.clone();
    trim_triage_messages(&mut request, 10);
    assert_eq!(request, original);
}

#[test]
fn a_local_directive_withholds_every_routing_tool() {
    let proxy_config = crate::Config {
        integrated: IntegratedConfig {
            enabled: true,
            targets: BTreeMap::from([("backend".to_string(), target_named("openai", None))]),
            ..IntegratedConfig::default()
        },
        ..crate::Config::default()
    };
    let interceptor = InterceptorConfig {
        model: "triage".to_string(),
        ..InterceptorConfig::default()
    };
    let local = build_local_request(
        json!({ "messages": [{ "role": "user", "content": "answer here" }] }),
        &interceptor,
        InterceptorProtocol::ChatCompletions,
        Some(crate::INTEGRATED_AUTO_MODEL),
        &proxy_config.role_map,
        &proxy_config,
        true,
    )
    .expect("local request builds");

    let tools = local["tools"].as_array().expect("tools array");
    assert!(
        tools.is_empty(),
        "a forced-local turn offers no routing controls: {tools:?}"
    );
    let carries_directive = local["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter_map(|message| message["content"].as_str())
        .any(|text| text.contains("Routing is unavailable"));
    assert!(carries_directive, "forced-local policy reaches the model");
}

#[test]
fn a_forced_verb_route_resolves_targets_and_the_catch_all() {
    let mut catch_all = target_named("openai", None);
    catch_all.is_default = true;
    let config = IntegratedConfig {
        targets: BTreeMap::from([
            ("backend".to_string(), target_named("openai", None)),
            ("fallback".to_string(), catch_all),
        ]),
        ..IntegratedConfig::default()
    };
    assert_eq!(
        forced_route_for(&config, Some(&RoutingVerb::Target("backend".to_string()))),
        Some("backend".to_string())
    );
    assert_eq!(
        forced_route_for(&config, Some(&RoutingVerb::Default)),
        Some("fallback".to_string())
    );
    // Verbs that only change how routing runs are not routes themselves.
    assert_eq!(forced_route_for(&config, Some(&RoutingVerb::Triage)), None);
    assert_eq!(forced_route_for(&config, Some(&RoutingVerb::Local)), None);
    assert_eq!(
        forced_route_for(&config, Some(&RoutingVerb::Target("ghost".to_string()))),
        None
    );
    assert_eq!(forced_route_for(&config, None), None);
}

fn failover_config(entries: &[(&str, &[&str])], shared: &[&str]) -> IntegratedConfig {
    IntegratedConfig {
        failover: shared.iter().map(|name| (*name).to_string()).collect(),
        targets: entries
            .iter()
            .map(|(name, failover)| {
                let mut target = target_named("openai", None);
                target.failover = failover.iter().map(|name| (*name).to_string()).collect();
                ((*name).to_string(), target)
            })
            .collect(),
        ..IntegratedConfig::default()
    }
}

#[test]
fn a_target_chain_wins_over_the_shared_chain() {
    let config = failover_config(&[("a", &["b"]), ("b", &[]), ("c", &[])], &["c"]);
    assert_eq!(failover_chain(&config, "a"), vec!["a", "b"]);
    // A target with no chain of its own inherits the shared one.
    assert_eq!(failover_chain(&config, "b"), vec!["b", "c"]);
}

#[test]
fn the_failover_chain_is_deduplicated_and_bounded() {
    let config = failover_config(
        &[
            ("a", &["b", "b", "a", "c", "d", "e", "f"]),
            ("b", &[]),
            ("c", &[]),
            ("d", &[]),
            ("e", &[]),
            ("f", &[]),
        ],
        &[],
    );
    let chain = failover_chain(&config, "a");
    assert_eq!(chain, vec!["a", "b", "c", "d"]);
}

#[test]
fn an_unknown_failover_candidate_is_dropped_from_the_chain() {
    let config = failover_config(&[("a", &["ghost", "b"]), ("b", &[])], &[]);
    assert_eq!(failover_chain(&config, "a"), vec!["a", "b"]);
}

#[test]
fn failover_validation_rejects_self_reference_and_unknown_targets() {
    let providers = HashMap::from([
        ("local".to_string(), crate::UpstreamConfig::default()),
        (
            "openai".to_string(),
            crate::UpstreamConfig {
                base_url: "https://api.openai.invalid/v1".to_string(),
                api_key: Some("k".to_string()),
                ..crate::UpstreamConfig::default()
            },
        ),
    ]);
    let interceptor = InterceptorConfig {
        enabled: true,
        provider: "local".to_string(),
        model: "triage".to_string(),
        ..InterceptorConfig::default()
    };

    let mut config = failover_config(&[("a", &["a"]), ("b", &[])], &[]);
    config.enabled = true;
    let error = validate_integrated_config(&config, &interceptor, &providers)
        .expect_err("self-reference is rejected");
    assert!(format!("{error:#}").contains("itself"));

    let mut config = failover_config(&[("a", &["ghost"])], &[]);
    config.enabled = true;
    let error = validate_integrated_config(&config, &interceptor, &providers)
        .expect_err("unknown target is rejected");
    assert!(format!("{error:#}").contains("unknown target"));

    let mut config = failover_config(&[("a", &["b"]), ("b", &[])], &[]);
    config.enabled = true;
    validate_integrated_config(&config, &interceptor, &providers)
        .expect("a valid chain is accepted");
}

#[test]
fn speculation_requires_a_catch_all_to_speculate_on() {
    let providers = HashMap::from([
        ("local".to_string(), crate::UpstreamConfig::default()),
        (
            "openai".to_string(),
            crate::UpstreamConfig {
                base_url: "https://api.openai.invalid/v1".to_string(),
                api_key: Some("k".to_string()),
                ..crate::UpstreamConfig::default()
            },
        ),
    ]);
    let interceptor = InterceptorConfig {
        enabled: true,
        provider: "local".to_string(),
        model: "triage".to_string(),
        ..InterceptorConfig::default()
    };
    let mut config = IntegratedConfig {
        enabled: true,
        speculative: true,
        targets: BTreeMap::from([("a".to_string(), target_named("openai", None))]),
        ..IntegratedConfig::default()
    };
    let error = validate_integrated_config(&config, &interceptor, &providers)
        .expect_err("speculation needs a catch-all");
    assert!(format!("{error:#}").contains("default = true"));

    config.targets.get_mut("a").expect("target").is_default = true;
    validate_integrated_config(&config, &interceptor, &providers)
        .expect("a catch-all makes speculation valid");
}

#[test]
fn unhealthy_routes_are_withheld_from_triage_but_never_all_of_them() {
    let health = crate::health::ProviderHealth::default();
    let config = IntegratedConfig {
        targets: BTreeMap::from([
            ("good".to_string(), target_named("healthy", None)),
            ("bad".to_string(), target_named("broken", None)),
        ]),
        ..IntegratedConfig::default()
    };
    assert_eq!(routable_targets(&config, &health), vec!["bad", "good"]);

    health.record_rate_limit("broken", Some(Duration::from_secs(60)));
    assert_eq!(routable_targets(&config, &health), vec!["good"]);

    // With nothing healthy left, an optimistic list beats an empty one.
    health.record_rate_limit("healthy", Some(Duration::from_secs(60)));
    assert_eq!(routable_targets(&config, &health), vec!["bad", "good"]);
}

#[test]
fn material_drift_reconsiders_a_route_that_is_otherwise_still_in_bounds() {
    let routes = StickyRoutes::default();
    let pinned = crate::route_cache::normalize(vec![1.0, 0.0]).expect("vector");
    routes.insert(21, "cheap".to_string(), 600, 16, Some(pinned.clone()));
    let bounds = StickyBounds {
        absolute_ttl: None,
        max_turns: None,
        drift_below: 0.5,
    };

    // The same subject keeps the pin.
    let similar = crate::route_cache::normalize(vec![0.98, 0.2]).expect("vector");
    assert_eq!(
        routes.lookup(21, bounds, Some(&similar)),
        StickyDecision::Reuse("cheap".to_string())
    );

    // A different subject reconsiders, carrying the previous route forward.
    let different = crate::route_cache::normalize(vec![0.0, 1.0]).expect("vector");
    assert_eq!(
        routes.lookup(21, bounds, Some(&different)),
        StickyDecision::Retriage {
            previous: "cheap".to_string()
        }
    );

    // Without a vector, or with drift checking off, nothing changes.
    assert_eq!(
        routes.lookup(21, bounds, None),
        StickyDecision::Reuse("cheap".to_string())
    );
    assert_eq!(
        routes.lookup(
            21,
            StickyBounds {
                drift_below: 0.0,
                ..bounds
            },
            Some(&different)
        ),
        StickyDecision::Reuse("cheap".to_string())
    );
}

#[test]
fn escalation_picks_the_weakest_route_that_is_still_stronger() {
    let mut targets = BTreeMap::new();
    for (name, strength) in [("cheap", 1), ("mid", 5), ("strong", 9)] {
        targets.insert(name.to_string(), target_named("openai", Some(strength)));
    }
    targets.insert("unranked".to_string(), target_named("openai", None));
    let config = IntegratedConfig {
        targets,
        ..IntegratedConfig::default()
    };
    assert_eq!(config.stronger_than("cheap"), Some("mid".to_string()));
    assert_eq!(config.stronger_than("mid"), Some("strong".to_string()));
    // Nothing is stronger than the strongest route, so a re-ask stays put.
    assert_eq!(config.stronger_than("strong"), None);
    // An unranked route counts as the weakest.
    assert_eq!(config.stronger_than("unranked"), Some("cheap".to_string()));
}

#[test]
fn only_replayable_decisions_are_cached() {
    assert_eq!(
        cacheable_decision(&CompletionDecision::Local),
        Some(CachedDecision::Local)
    );
    assert_eq!(
        cacheable_decision(&CompletionDecision::DefaultRoute),
        Some(CachedDecision::DefaultRoute)
    );
    assert_eq!(
        cacheable_decision(&CompletionDecision::Redirect("backend".to_string())),
        Some(CachedDecision::Redirect("backend".to_string()))
    );
    // A rejection or a malformed control must always be re-derived.
    assert_eq!(cacheable_decision(&CompletionDecision::Reject), None);
    assert_eq!(
        cacheable_decision(&CompletionDecision::InvalidControl),
        None
    );
}
