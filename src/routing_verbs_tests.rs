use super::*;

use serde_json::json;

fn targets() -> BTreeMap<String, IntegratedTarget> {
    BTreeMap::from([(
        "frontend".to_string(),
        IntegratedTarget {
            provider: "anthropic".to_string(),
            model: "claude-test".to_string(),
            hint: "GUI work".to_string(),
            reasoning_effort: None,
            ..IntegratedTarget::default()
        },
    )])
}

fn chat(messages: Value) -> Value {
    json!({ "model": "Auto", "messages": messages })
}

fn text_of(body: &Value, index: usize) -> String {
    body["messages"][index]["content"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn fixed_verbs_are_recognized_and_stripped() {
    for (raw, expected) in [
        ("triage: rewrite the parser", RoutingVerb::Triage),
        ("Unstick: rewrite the parser", RoutingVerb::Unstick),
        ("LOCAL: rewrite the parser", RoutingVerb::Local),
        ("default: rewrite the parser", RoutingVerb::Default),
    ] {
        let mut body = chat(json!([{ "role": "user", "content": raw }]));
        assert_eq!(detect_and_strip(&mut body, &targets()), Some(expected));
        assert_eq!(text_of(&body, 0), "rewrite the parser");
    }
}

#[test]
fn a_target_name_pins_that_route() {
    let mut body = chat(json!([{ "role": "user", "content": "frontend: restyle the header" }]));
    assert_eq!(
        detect_and_strip(&mut body, &targets()),
        Some(RoutingVerb::Target("frontend".to_string()))
    );
    assert_eq!(text_of(&body, 0), "restyle the header");
}

#[test]
fn unknown_tokens_are_left_completely_alone() {
    for raw in [
        "note: this is not a verb",
        "TODO: fix the parser",
        "https://example.invalid/a",
        "ratio: 3:1",
        "triage the queue please",
        "  :leading colon",
    ] {
        let mut body = chat(json!([{ "role": "user", "content": raw }]));
        assert_eq!(detect_and_strip(&mut body, &targets()), None, "{raw:?}");
        assert_eq!(text_of(&body, 0), raw, "{raw:?}");
    }
}

#[test]
fn a_backslash_escapes_a_message_that_genuinely_starts_that_way() {
    let mut body = chat(json!([{ "role": "user", "content": "\\triage: explain this word" }]));
    assert_eq!(detect_and_strip(&mut body, &targets()), None);
    assert_eq!(text_of(&body, 0), "triage: explain this word");
}

#[test]
fn a_backslash_before_a_non_verb_is_preserved() {
    let mut body = chat(json!([{ "role": "user", "content": "\\newline is a thing" }]));
    assert_eq!(detect_and_strip(&mut body, &targets()), None);
    assert_eq!(text_of(&body, 0), "\\newline is a thing");
}

#[test]
fn only_the_newest_user_message_acts_but_stale_prefixes_are_stripped() {
    let mut body = chat(json!([
        { "role": "user", "content": "frontend: restyle the header" },
        { "role": "assistant", "content": "done" },
        { "role": "user", "content": "triage: now rewrite the parser" }
    ]));
    assert_eq!(
        detect_and_strip(&mut body, &targets()),
        Some(RoutingVerb::Triage)
    );
    // The old directive must not reach the triage model as if it were current.
    assert_eq!(text_of(&body, 0), "restyle the header");
    assert_eq!(text_of(&body, 2), "now rewrite the parser");
}

#[test]
fn assistant_and_system_turns_are_never_rewritten() {
    let mut body = chat(json!([
        { "role": "system", "content": "local: never touch me" },
        { "role": "assistant", "content": "triage: nor me" },
        { "role": "user", "content": "plain question" }
    ]));
    assert_eq!(detect_and_strip(&mut body, &targets()), None);
    assert_eq!(text_of(&body, 0), "local: never touch me");
    assert_eq!(text_of(&body, 1), "triage: nor me");
}

#[test]
fn multimodal_content_strips_only_the_leading_text_part() {
    let mut body = chat(json!([{
        "role": "user",
        "content": [
            { "type": "text", "text": "local: describe this" },
            { "type": "image_url", "image_url": { "url": "https://example.invalid/a.png" } },
            { "type": "text", "text": "triage: trailing text is untouched" }
        ]
    }]));
    assert_eq!(
        detect_and_strip(&mut body, &targets()),
        Some(RoutingVerb::Local)
    );
    assert_eq!(body["messages"][0]["content"][0]["text"], "describe this");
    assert_eq!(
        body["messages"][0]["content"][2]["text"],
        "triage: trailing text is untouched"
    );
}

#[test]
fn the_responses_input_array_is_handled_too() {
    let mut body = json!({
        "model": "Auto",
        "input": [{
            "role": "user",
            "content": [{ "type": "input_text", "text": "frontend: restyle the header" }]
        }]
    });
    assert_eq!(
        detect_and_strip(&mut body, &targets()),
        Some(RoutingVerb::Target("frontend".to_string()))
    );
    assert_eq!(body["input"][0]["content"][0]["text"], "restyle the header");
}

#[test]
fn indentation_survives_the_strip() {
    let mut body = chat(json!([{ "role": "user", "content": "   triage:    rewrite" }]));
    assert_eq!(
        detect_and_strip(&mut body, &targets()),
        Some(RoutingVerb::Triage)
    );
    assert_eq!(text_of(&body, 0), "   rewrite");
}

#[test]
fn a_body_without_a_conversation_is_ignored() {
    let mut body = json!({ "model": "Auto" });
    assert_eq!(detect_and_strip(&mut body, &targets()), None);
}

#[test]
fn memory_verbs_are_recognized_and_stripped() {
    for (raw, expected, remainder) in [
        (
            "remember: the parser is the bottleneck",
            RoutingVerb::Remember,
            "the parser is the bottleneck",
        ),
        ("forget: everything", RoutingVerb::Forget, "everything"),
        ("recall:", RoutingVerb::Recall, ""),
    ] {
        let mut body = chat(json!([{ "role": "user", "content": raw }]));
        assert_eq!(
            detect_and_strip(&mut body, &targets()),
            Some(expected),
            "{raw:?}"
        );
        assert_eq!(text_of(&body, 0), remainder, "{raw:?}");
    }
}

#[test]
fn every_reserved_verb_parses_back_to_itself() {
    for verb in RESERVED_VERBS {
        let mut body = chat(json!([{ "role": "user", "content": format!("{verb}: body") }]));
        let parsed = detect_and_strip(&mut body, &targets()).expect("reserved verbs parse");
        assert_eq!(parsed.as_str(), verb);
    }
}
