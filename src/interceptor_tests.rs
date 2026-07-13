use std::collections::HashMap;

use axum::http::{HeaderMap, HeaderValue};
use serde_json::{Value, json};

use crate::Config;
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
fn pass_must_be_the_only_local_output() {
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
    assert_eq!(classify_completion(&pass), CompletionDecision::Pass);

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
    assert_eq!(classify_completion(&mixed), CompletionDecision::InvalidPass);
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
    // Our cap replaces the client's larger one.
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
