use super::*;

use serde_json::json;

fn config() -> RedactConfig {
    RedactConfig {
        enabled: true,
        rehydrate: true,
        extra_literals: Vec::new(),
    }
}

fn rewrite(text: &str) -> (String, Redaction) {
    let mut builder = Builder::new(&CONFIG);
    let rewritten = builder.rewrite(text).unwrap_or_else(|| text.to_string());
    (rewritten, builder.finish())
}

static CONFIG: RedactConfig = RedactConfig {
    enabled: true,
    rehydrate: true,
    extra_literals: Vec::new(),
};

#[test]
fn vendor_prefixed_keys_are_replaced() {
    let (text, redaction) = rewrite("export OPENAI_API_KEY=sk-proj-abcdefghijklmnopqrstuvwx");
    assert!(!text.contains("sk-proj-abcdefghijklmnopqrstuvwx"));
    assert!(text.contains("LLMPROXY_REDACTED_KEY_1"));
    assert_eq!(redaction.issued(), 1);
    assert_eq!(redaction.restorable(), 1);
}

#[test]
fn a_prefix_inside_a_longer_word_is_not_a_key() {
    let (text, redaction) = rewrite("the task-sk-value is unrelated and short");
    assert_eq!(text, "the task-sk-value is unrelated and short");
    assert!(redaction.is_empty());
}

#[test]
fn short_prefixed_runs_are_left_alone() {
    // Too short to be a credential; redacting it would damage the prompt.
    let (text, redaction) = rewrite("branch ghp_short");
    assert_eq!(text, "branch ghp_short");
    assert!(redaction.is_empty());
}

#[test]
fn jwts_and_bearer_tokens_are_replaced() {
    let (text, _) = rewrite(
        "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r_wW1gFWFOEjXk",
    );
    assert!(text.contains("LLMPROXY_REDACTED"));
    assert!(!text.contains("eyJhbGciOiJIUzI1NiJ9"));

    let (text, _) = rewrite("Authorization: Bearer abcdefghijklmnopqrstuvwxyz012345");
    assert!(text.contains("Bearer LLMPROXY_REDACTED_TOKEN_1"));
}

#[test]
fn assigned_secrets_are_replaced_whatever_they_look_like() {
    let (text, _) = rewrite("password = \"correct horse battery\"");
    assert!(text.contains("LLMPROXY_REDACTED_TOKEN_1"));
    assert!(!text.contains("correct horse battery"));

    let (text, _) = rewrite("{\"client_secret\":\"9f3a1c77aa21\"}");
    assert!(!text.contains("9f3a1c77aa21"));

    // A short value is not worth the false-positive risk.
    let (text, redaction) = rewrite("password = \"abc\"");
    assert_eq!(text, "password = \"abc\"");
    assert!(redaction.is_empty());
}

#[test]
fn email_addresses_are_replaced_without_eating_the_sentence() {
    let (text, _) = rewrite("mail alice.smith@example.com. then wait");
    assert_eq!(text, "mail LLMPROXY_REDACTED_EMAIL_1. then wait");
}

#[test]
fn a_private_key_block_is_redacted_but_never_put_back() {
    let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIBOgIBAAJBAK\n-----END RSA PRIVATE KEY-----\n";
    let (text, redaction) = rewrite(&format!("here it is:\n{pem}done"));
    assert!(!text.contains("MIIBOgIBAAJBAK"));
    assert_eq!(redaction.issued(), 1);
    // Multi-line values would corrupt a JSON response body if spliced back.
    assert_eq!(redaction.restorable(), 0);
}

#[test]
fn the_same_secret_always_gets_the_same_placeholder() {
    let (text, redaction) =
        rewrite("key sk-proj-abcdefghijklmnopqrstuvwx and again sk-proj-abcdefghijklmnopqrstuvwx");
    assert_eq!(text.matches("LLMPROXY_REDACTED_KEY_1").count(), 2);
    assert_eq!(redaction.issued(), 1);
}

#[test]
fn overlapping_detections_do_not_double_redact() {
    let (text, redaction) = rewrite("api_key = \"sk-proj-abcdefghijklmnopqrstuvwx\"");
    assert_eq!(redaction.issued(), 1, "one span, not two: {text}");
    assert!(!text.contains("sk-proj-abcdefghijklmnopqrstuvwx"));
}

#[test]
fn every_conversation_turn_is_rewritten_including_system_and_parts() {
    let mut body = json!({
        "messages": [
            { "role": "system", "content": "deploy with sk-proj-abcdefghijklmnopqrstuvwx" },
            {
                "role": "user",
                "content": [
                    { "type": "text", "text": "and ghp_abcdefghijklmnopqrstuvwxyz" },
                    { "type": "image_url", "image_url": { "url": "https://example.invalid/a.png" } }
                ]
            }
        ]
    });
    let redaction = redact_request(&config(), &mut body);
    assert_eq!(redaction.issued(), 2);
    assert!(
        !body
            .to_string()
            .contains("sk-proj-abcdefghijklmnopqrstuvwx")
    );
    assert!(!body.to_string().contains("ghp_abcdefghijklmnopqrstuvwxyz"));
    // Non-text fields are untouched.
    assert_eq!(
        body["messages"][1]["content"][1]["image_url"]["url"],
        "https://example.invalid/a.png"
    );
}

#[test]
fn the_responses_input_array_and_instructions_are_covered() {
    let mut body = json!({
        "instructions": "token ghp_abcdefghijklmnopqrstuvwxyz",
        "input": [{ "role": "user", "content": [{ "type": "input_text", "text": "hi" }] }]
    });
    let redaction = redact_request(&config(), &mut body);
    assert_eq!(redaction.issued(), 1);
    assert!(!body["instructions"].as_str().unwrap().contains("ghp_"));
}

#[test]
fn rehydration_restores_the_original_in_one_piece() {
    let mut body = json!({
        "messages": [{ "role": "user", "content": "key sk-proj-abcdefghijklmnopqrstuvwx" }]
    });
    let redaction = redact_request(&config(), &mut body);
    let mut rehydrator = Rehydrator::new(&redaction).expect("something to restore");
    let mut out = rehydrator.push("the key was LLMPROXY_REDACTED_KEY_1 exactly");
    out.push_str(&rehydrator.finish());
    assert_eq!(out, "the key was sk-proj-abcdefghijklmnopqrstuvwx exactly");
}

#[test]
fn a_placeholder_split_across_chunks_is_still_restored() {
    let mut body = json!({
        "messages": [{ "role": "user", "content": "key sk-proj-abcdefghijklmnopqrstuvwx" }]
    });
    let redaction = redact_request(&config(), &mut body);
    let mut rehydrator = Rehydrator::new(&redaction).expect("something to restore");

    let full = "value: LLMPROXY_REDACTED_KEY_1 end";
    let mut out = String::new();
    for chunk in full.as_bytes().chunks(3) {
        out.push_str(&rehydrator.push(std::str::from_utf8(chunk).expect("ascii")));
    }
    out.push_str(&rehydrator.finish());
    assert_eq!(out, "value: sk-proj-abcdefghijklmnopqrstuvwx end");
}

#[test]
fn multibyte_chunks_are_never_split_mid_character() {
    let mut body = json!({
        "messages": [{ "role": "user", "content": "key sk-proj-abcdefghijklmnopqrstuvwx" }]
    });
    let redaction = redact_request(&config(), &mut body);
    let mut rehydrator = Rehydrator::new(&redaction).expect("something to restore");
    let mut out = rehydrator.push("héllo → LLMPROXY_REDACTED_KEY_1 ✓");
    out.push_str(&rehydrator.finish());
    assert!(out.starts_with("héllo → "));
    assert!(out.ends_with(" ✓"));
    assert!(out.contains("sk-proj-abcdefghijklmnopqrstuvwx"));
}

#[test]
fn rehydration_can_be_turned_off_entirely() {
    let mut body = json!({
        "messages": [{ "role": "user", "content": "key sk-proj-abcdefghijklmnopqrstuvwx" }]
    });
    let redaction = redact_request(
        &RedactConfig {
            rehydrate: false,
            ..config()
        },
        &mut body,
    );
    assert_eq!(redaction.issued(), 1);
    assert_eq!(redaction.restorable(), 0);
    assert!(Rehydrator::new(&redaction).is_none());
}

#[test]
fn extra_literals_cover_what_the_detectors_cannot_know() {
    let mut body = json!({
        "messages": [{ "role": "user", "content": "reach build01.internal.invalid now" }]
    });
    let redaction = redact_request(
        &RedactConfig {
            extra_literals: vec!["build01.internal.invalid".to_string()],
            ..config()
        },
        &mut body,
    );
    assert_eq!(redaction.issued(), 1);
    assert!(
        body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("LLMPROXY_REDACTED_LITERAL_1")
    );
}

#[test]
fn validation_rejects_literals_short_enough_to_match_everywhere() {
    assert!(
        RedactConfig {
            enabled: true,
            extra_literals: vec!["ab".to_string()],
            ..config()
        }
        .validate()
        .is_err()
    );
    assert!(config().validate().is_ok());
    // Nothing is validated while redaction is off.
    assert!(RedactConfig::default().validate().is_ok());
}

#[test]
fn an_ordinary_prompt_is_left_byte_for_byte_alone() {
    let original = json!({
        "messages": [{
            "role": "user",
            "content": "Refactor parse_usage_value in src/stats.rs to return a Result."
        }]
    });
    let mut body = original.clone();
    let redaction = redact_request(&config(), &mut body);
    assert!(redaction.is_empty());
    assert_eq!(body, original);
}
