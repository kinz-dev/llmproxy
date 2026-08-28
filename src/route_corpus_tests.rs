use super::*;

fn case(prompt: &str, decision: &str, target: Option<&str>) -> Case {
    Case {
        prompt: prompt.to_string(),
        decision: Some(decision.to_string()),
        target: target.map(str::to_string),
        note: None,
    }
}

#[test]
fn a_corpus_parses_as_an_object_or_a_bare_array() {
    let object = Corpus::parse(
        r#"{ "mode": "integrated", "cases": [{ "prompt": "hi", "decision": "local" }] }"#,
    )
    .expect("object form parses");
    assert_eq!(object.mode.as_deref(), Some("integrated"));
    assert_eq!(object.cases.len(), 1);

    // A bare array is what exporting the decision log produces.
    let array =
        Corpus::parse(r#"[{ "prompt": "hi", "decision": "local" }]"#).expect("array form parses");
    assert!(array.mode.is_none());
    assert_eq!(array.cases.len(), 1);

    assert!(Corpus::parse("not json").is_err());
}

#[test]
fn capture_keeps_reproducible_decisions_and_drops_the_rest() {
    let rows = vec![
        json!({ "prompt": "format this", "decision": "local" }),
        json!({ "prompt": "design the schema", "decision": "redirect", "target": "backend" }),
        // Not reproducible: these record what a provider did that day.
        json!({ "prompt": "anything", "decision": "fallback" }),
        json!({ "prompt": "anything else", "decision": "error" }),
        json!({ "prompt": "bypassed", "decision": "bypass" }),
        // Nothing to replay.
        json!({ "prompt": "(no user text)", "decision": "local" }),
        json!({ "prompt": "   ", "decision": "local" }),
    ];
    let corpus = from_decisions(&rows, Some("integrated"));
    assert_eq!(corpus.mode.as_deref(), Some("integrated"));
    assert_eq!(corpus.cases.len(), 2);
    assert_eq!(corpus.cases[1].target.as_deref(), Some("backend"));
}

#[test]
fn capture_deduplicates_repeated_prompts() {
    let rows = vec![
        json!({ "prompt": "same question", "decision": "local" }),
        json!({ "prompt": "same question", "decision": "local" }),
    ];
    assert_eq!(from_decisions(&rows, None).cases.len(), 1);
}

#[test]
fn the_mechanism_that_reached_a_target_is_not_what_is_judged() {
    // A sticky reuse and a cached replay both mean "it went to that target".
    let recorded = case("design the schema", "sticky", Some("backend"));
    let (verdict, _) = compare(
        &recorded,
        &json!({ "decision": "redirect", "target": "backend" }),
    );
    assert_eq!(verdict, Verdict::Same);

    let recorded = case("something routine", "pass", None);
    let (verdict, _) = compare(&recorded, &json!({ "decision": "default_route" }));
    assert_eq!(verdict, Verdict::Same);
}

#[test]
fn a_moved_decision_or_a_moved_target_is_a_regression() {
    let recorded = case("design the schema", "redirect", Some("backend"));
    let (verdict, row) = compare(
        &recorded,
        &json!({ "decision": "redirect", "target": "frontend" }),
    );
    assert_eq!(verdict, Verdict::Changed);
    assert_eq!(row["expected"]["target"], "backend");
    assert_eq!(row["actual"]["target"], "frontend");

    let (verdict, _) = compare(&recorded, &json!({ "decision": "local" }));
    assert_eq!(verdict, Verdict::Changed);
}

#[test]
fn a_recorded_target_that_vanishes_is_a_regression() {
    let recorded = case("design the schema", "redirect", Some("backend"));
    let (verdict, _) = compare(&recorded, &json!({ "decision": "redirect" }));
    assert_eq!(verdict, Verdict::Changed);
}

#[test]
fn a_case_with_nothing_recorded_is_new_rather_than_failing() {
    let fresh = Case {
        prompt: "brand new".to_string(),
        decision: None,
        target: None,
        note: Some("added by hand".to_string()),
    };
    let (verdict, row) = compare(&fresh, &json!({ "decision": "local" }));
    assert_eq!(verdict, Verdict::New);
    assert_eq!(row["note"], "added by hand");
}

#[test]
fn a_replay_that_could_not_decide_is_an_error() {
    let recorded = case("anything", "local", None);
    let (verdict, _) = compare(
        &recorded,
        &json!({ "decision": "error", "detail": "timeout" }),
    );
    assert_eq!(verdict, Verdict::Error);

    let (verdict, _) = compare(&recorded, &json!({}));
    assert_eq!(verdict, Verdict::Error);
}

#[test]
fn the_summary_counts_only_real_regressions() {
    let rows = vec![
        json!({ "verdict": "same" }),
        json!({ "verdict": "same" }),
        json!({ "verdict": "changed" }),
        json!({ "verdict": "new" }),
        json!({ "verdict": "error" }),
    ];
    let summary = summarize(rows, "integrated");
    assert_eq!(summary["cases"], 5);
    assert_eq!(summary["same"], 2);
    assert_eq!(summary["changed"], 1);
    assert_eq!(summary["new"], 1);
    assert_eq!(summary["errors"], 1);
    // A new case has nothing to compare against, so it is not a regression.
    assert_eq!(summary["regressions"], 2);
}
