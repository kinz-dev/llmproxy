use std::collections::{BTreeMap, HashMap};

use axum::http::{HeaderMap, HeaderValue};
use serde_json::{Value, json};

use super::{
    IntegratedConfig, IntegratedTarget, classify_completion_details,
    classify_completion_with_targets, enforce_router_only,
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
fn integrated_mode_rejects_direct_answers_and_normalizes_target_names() {
    let targets = BTreeMap::from([(
        "provider-default".to_string(),
        IntegratedTarget {
            provider: "default".to_string(),
            model: "routine-model".to_string(),
            hint: "deterministic work".to_string(),
            reasoning_effort: None,
        },
    )]);
    let redirect = json!({
        "choices": [{
            "message": {
                "content": Value::Null,
                "tool_calls": [{
                    "function": { "name": "PROVIDER_DEFAULT", "arguments": "{}" }
                }]
            }
        }]
    });
    assert_eq!(
        classify_completion_with_targets(&redirect, &targets),
        CompletionDecision::Redirect("provider-default".to_string())
    );

    let direct = json!({ "choices": [{ "message": { "content": "answer" } }] });
    let classified =
        enforce_router_only(classify_completion_details(&direct, &targets, false), true);
    assert_eq!(classified.decision, CompletionDecision::InvalidControl);
}

#[test]
fn integrated_request_rejects_normalized_caller_tool_collisions() {
    let proxy_config = Config {
        integrated: IntegratedConfig {
            enabled: true,
            targets: BTreeMap::from([(
                "provider-default".to_string(),
                IntegratedTarget {
                    provider: "default".to_string(),
                    model: "routine-model".to_string(),
                    hint: "deterministic work".to_string(),
                    reasoning_effort: None,
                },
            )]),
            ..IntegratedConfig::default()
        },
        ..Config::default()
    };
    let request = json!({
        "messages": [{ "role": "user", "content": "work" }],
        "tools": [{
            "type": "function",
            "function": { "name": "PROVIDER_DEFAULT", "parameters": {} }
        }]
    });
    let error = build_local_request(
        request,
        &InterceptorConfig {
            model: "triage".to_string(),
            ..InterceptorConfig::default()
        },
        InterceptorProtocol::ChatCompletions,
        Some(INTEGRATED_AUTO_MODEL),
        &HashMap::new(),
        &proxy_config,
    )
    .unwrap_err();
    assert!(error.contains("provider-default"));
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
    assert!(prompt.contains("otherwise call PASS"));
    assert!(prompt.contains("greater than 80%"));
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
    assert!(
        local
            .to_string()
            .contains("triage and deterministic-execution provider")
    );
    assert!(!local.to_string().contains("provider-routing tier"));
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
    assert!(stock.contains("provider-routing tier"));
    assert!(!stock.contains("triage and deterministic-execution provider"));
    assert!(!stock.contains("ROUTE_DEFAULT"));
    assert!(stock.contains("PASS"));

    // Intercept-mode overrides stay out of integrated mode; the integrated
    // policy has its own override field.
    let intercept_override = prompt_for(&InterceptorConfig {
        model: "triage".to_string(),
        instructions: "Custom intercept policy.".to_string(),
        ..InterceptorConfig::default()
    });
    assert!(intercept_override.contains("provider-routing tier"));
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
    assert!(!overridden.contains("provider-routing tier"));
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
    )
    .unwrap();
    let plain_prompt = plain["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| text_from_value(&message["content"]))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(plain_prompt.contains("triage and deterministic-execution provider"));
    assert!(!plain_prompt.contains("provider-routing tier"));
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
fn sensitive_rejection_requires_private_triage_provider() {
    let providers = HashMap::from([(
        "triage".to_string(),
        crate::UpstreamConfig {
            base_url: "https://triage.example/v1".to_string(),
            ..crate::UpstreamConfig::default()
        },
    )]);
    let config = InterceptorConfig {
        enabled: true,
        reject_sensitive: true,
        provider: "triage".to_string(),
        model: "screening-model".to_string(),
        ..InterceptorConfig::default()
    };
    let error = crate::interceptor::validate_config(&config, &providers)
        .unwrap_err()
        .to_string();
    assert!(error.contains("would receive the prompt before REJECT"));
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
        "reject".to_string(),
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
