use super::*;

use serde_json::json;

fn completion(content: &str) -> Value {
    json!({ "choices": [{ "message": { "role": "assistant", "content": content } }] })
}

#[test]
fn sampling_is_exact_rather_than_approximate() {
    let sampler = Sampler::default();
    let taken = (0..100).filter(|_| sampler.take(20)).count();
    assert_eq!(taken, 5);

    let every = Sampler::default();
    assert!((0..10).all(|_| every.take(1)));

    let never = Sampler::default();
    assert!(!never.take(0));
}

#[test]
fn answer_text_reads_both_string_and_part_array_content() {
    assert_eq!(answer_text(&completion("hello")), "hello");
    let parts = json!({
        "choices": [{ "message": { "content": [{ "type": "text", "text": "hello" }] } }]
    });
    assert_eq!(answer_text(&parts), "hello");
    assert_eq!(answer_text(&json!({})), "");
}

#[test]
fn a_tool_call_on_either_side_disqualifies_comparison() {
    let acting = json!({
        "choices": [{ "message": { "tool_calls": [{ "id": "1", "function": { "name": "write" } }] } }]
    });
    assert!(wants_tool_call(&acting));
    assert!(!wants_tool_call(&completion("just text")));
    assert!(!wants_tool_call(&json!({
        "choices": [{ "message": { "content": "x", "tool_calls": [] } }]
    })));
}

#[test]
fn word_overlap_separates_agreement_from_divergence() {
    let agree = word_overlap(
        "use a bounded queue and drop the oldest entry",
        "use a bounded queue and drop the oldest entry first",
    );
    assert!(agree > 0.8, "near-identical answers score high: {agree}");

    let diverge = word_overlap(
        "use a bounded queue and drop the oldest entry",
        "rewrite the scheduler in terms of futures instead",
    );
    assert!(diverge < 0.2, "unrelated answers score low: {diverge}");

    assert_eq!(word_overlap("", ""), 1.0);
    assert_eq!(word_overlap("", "something"), 0.0);
}

#[test]
fn presenting_both_labels_each_answer_and_states_the_verdict() {
    let agreed = present_both(
        "primary (openai/gpt)",
        "use a queue",
        "second (anthropic/claude)",
        "use a queue",
    );
    assert!(agreed.contains("## primary (openai/gpt)"));
    assert!(agreed.contains("## second (anthropic/claude)"));
    assert!(agreed.contains("agreement"));

    let diverged = present_both("a", "use a queue", "b", "rewrite everything in futures");
    assert!(
        diverged.contains("Divergence is the stronger signal"),
        "divergence must be called out, not smoothed over"
    );
}

#[test]
fn the_reconcile_prompt_forbids_smoothing_over_disagreement() {
    let prompt = reconcile_prompt("why is it slow", "a", "lock contention", "b", "gc pressure");
    assert!(prompt.contains("why is it slow"));
    assert!(prompt.contains("lock contention"));
    assert!(prompt.contains("gc pressure"));
    assert!(prompt.contains("say so explicitly"));
    assert!(prompt.contains("Never invent a third position"));
}

#[test]
fn shadow_validation_requires_a_real_target() {
    let config = ShadowConfig {
        enabled: true,
        target: "ghost".to_string(),
        ..ShadowConfig::default()
    };
    assert!(config.validate(|name| name == "backend").is_err());

    let config = ShadowConfig {
        enabled: true,
        target: "backend".to_string(),
        ..ShadowConfig::default()
    };
    assert!(config.validate(|name| name == "backend").is_ok());
    assert!(config.is_active());

    // Nothing is validated while shadowing is off.
    assert!(ShadowConfig::default().validate(|_| false).is_ok());
    assert!(!ShadowConfig::default().is_active());

    let config = ShadowConfig {
        enabled: true,
        target: "backend".to_string(),
        sample_one_in: 0,
        ..ShadowConfig::default()
    };
    assert!(config.validate(|_| true).is_err());
}
