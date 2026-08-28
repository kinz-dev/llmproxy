use super::*;

fn record(model: &str, answers: u64, retried: u64, failed: u64, cost: Option<f64>) -> Record {
    Record {
        provider: "openai".to_string(),
        model: model.to_string(),
        answers,
        retried,
        failed,
        total_time_ms: answers * 1000,
        cost,
    }
}

#[test]
fn satisfaction_counts_re_asks_and_failures_against_a_route() {
    let clean = record("a", 100, 0, 0, None);
    assert_eq!(clean.satisfaction(), Some(1.0));

    let mixed = record("b", 100, 10, 5, None);
    assert_eq!(mixed.satisfaction(), Some(0.85));

    // Nonsense inputs cannot produce a negative rate.
    let broken = record("c", 10, 40, 40, None);
    assert_eq!(broken.satisfaction(), Some(0.0));

    assert_eq!(record("d", 0, 0, 0, None).satisfaction(), None);
}

#[test]
fn a_barely_used_model_is_reported_but_never_ranked() {
    let ranking = rank(&[
        record("busy", 100, 5, 0, None),
        record("rare", 3, 0, 0, None),
    ]);
    let models = ranking["models"].as_array().expect("models");
    let rare = models
        .iter()
        .find(|row| row["model"] == "rare")
        .expect("the row is still shown");
    assert_eq!(rare["ranked"], false);
    assert!(rare["rank"].is_null());

    let busy = models
        .iter()
        .find(|row| row["model"] == "busy")
        .expect("row");
    assert_eq!(busy["rank"], 1);
}

#[test]
fn satisfaction_orders_the_table_and_cost_only_breaks_ties() {
    let ranking = rank(&[
        record("cheap-but-re-asked", 100, 30, 0, Some(1.0)),
        record("dearer-but-accepted", 100, 2, 0, Some(50.0)),
    ]);
    let models = ranking["models"].as_array().expect("models");
    assert_eq!(
        models[0]["model"], "dearer-but-accepted",
        "a cheap route that gets re-asked is not cheap"
    );

    // With equal satisfaction, the cheaper one wins.
    let ranking = rank(&[
        record("dear", 100, 5, 0, Some(50.0)),
        record("cheap", 100, 5, 0, Some(1.0)),
    ]);
    assert_eq!(ranking["models"].as_array().unwrap()[0]["model"], "cheap");
}

#[test]
fn ranking_is_stable_for_identical_records() {
    let first = rank(&[
        record("b", 100, 1, 0, Some(1.0)),
        record("a", 100, 1, 0, Some(1.0)),
    ]);
    let second = rank(&[
        record("a", 100, 1, 0, Some(1.0)),
        record("b", 100, 1, 0, Some(1.0)),
    ]);
    assert_eq!(first, second);
    assert_eq!(first["models"].as_array().unwrap()[0]["model"], "a");
}

#[test]
fn hint_candidates_surface_terms_that_are_distinctive_not_merely_common() {
    let prompts = BTreeMap::from([
        (
            "frontend".to_string(),
            vec![
                "restyle the button component".to_string(),
                "the button component spacing is wrong".to_string(),
                "align the component grid".to_string(),
            ],
        ),
        (
            "backend".to_string(),
            vec![
                "the migration deadlocks on write".to_string(),
                "fix the migration ordering".to_string(),
                "why does the write path deadlock".to_string(),
            ],
        ),
    ]);
    let candidates = hint_candidates(&prompts, 5);
    let terms = |target: &str| {
        candidates[target]["terms"]
            .as_array()
            .expect("terms")
            .iter()
            .map(|term| term["term"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    assert!(terms("frontend").contains(&"component".to_string()));
    assert!(terms("backend").contains(&"migration".to_string()));
    // A term that appears on both sides is not distinctive of either.
    assert!(!terms("frontend").contains(&"the".to_string()));
    assert!(!terms("backend").contains(&"the".to_string()));
    assert_eq!(candidates["frontend"]["prompts"], 3);
}

#[test]
fn hint_candidates_are_empty_without_evidence() {
    let candidates = hint_candidates(&BTreeMap::new(), 5);
    assert_eq!(candidates, json!({}));

    let single = BTreeMap::from([("a".to_string(), vec!["one prompt only".to_string()])]);
    // One prompt cannot show that a term repeats, so nothing is proposed.
    assert_eq!(
        hint_candidates(&single, 5)["a"]["terms"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

fn candidate(target: &str, answers: u64, retried: u64, cost: f64) -> RouteCandidate {
    RouteCandidate {
        target: target.to_string(),
        provider: "openai".to_string(),
        model: format!("{target}-model"),
        record: Record {
            provider: "openai".to_string(),
            model: format!("{target}-model"),
            answers,
            retried,
            failed: 0,
            total_time_ms: answers * 1000,
            cost: Some(cost),
        },
    }
}

#[test]
fn cost_insight_proposes_a_move_the_evidence_supports() {
    let insight = cost_insight(
        &[
            candidate("expensive", 100, 2, 100.0),
            candidate("cheap", 100, 2, 5.0),
        ],
        Some("USD"),
    );
    let suggestions = insight["suggestions"].as_array().expect("suggestions");
    assert_eq!(suggestions.len(), 1);
    assert_eq!(suggestions[0]["target"], "expensive");
    assert_eq!(suggestions[0]["to"]["target"], "cheap");
    // 100 answers at (1.00 - 0.05) each.
    assert!((suggestions[0]["observed_saving"].as_f64().unwrap() - 95.0).abs() < 1e-9);
    // The payload is exactly what the routing editor takes.
    assert_eq!(suggestions[0]["apply"]["target"], "expensive");
    assert_eq!(suggestions[0]["apply"]["model"], "cheap-model");
    assert!((insight["total"].as_f64().unwrap() - 105.0).abs() < 1e-9);
}

#[test]
fn a_cheaper_route_that_is_observably_worse_is_not_proposed() {
    let insight = cost_insight(
        &[
            candidate("expensive", 100, 1, 100.0),
            // Re-asked a third of the time: cheap on paper only.
            candidate("cheap", 100, 33, 5.0),
        ],
        Some("USD"),
    );
    assert!(insight["suggestions"].as_array().unwrap().is_empty());
}

#[test]
fn a_route_without_enough_evidence_is_never_the_recommendation() {
    let insight = cost_insight(
        &[
            candidate("expensive", 100, 1, 100.0),
            candidate("barely-used", 4, 0, 0.5),
        ],
        Some("USD"),
    );
    assert!(insight["suggestions"].as_array().unwrap().is_empty());
}

#[test]
fn unpriced_routes_produce_no_insight_at_all() {
    let mut unpriced = candidate("mystery", 100, 1, 0.0);
    unpriced.record.cost = None;
    let insight = cost_insight(&[unpriced], None);
    assert_eq!(insight["priced_routes"], 0);
    assert!(insight["suggestions"].as_array().unwrap().is_empty());
    assert!(insight["currency"].is_null());
}
