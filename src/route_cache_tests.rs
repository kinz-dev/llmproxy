use super::*;

use serde_json::json;

fn sample() -> EmbeddingConfig {
    EmbeddingConfig {
        enabled: true,
        provider: "embeddings".to_string(),
        model: "embed-test".to_string(),
        threshold: 0.9,
        capacity: 4,
        ttl_secs: 3600,
        retriage_below: 0.5,
        retry_escalation: true,
        retry_threshold: 0.9,
        timeout_ms: 2000,
    }
}

fn vector(values: &[f32]) -> Vector {
    normalize(values.to_vec()).expect("non-zero vector")
}

#[test]
fn normalization_makes_similarity_a_dot_product() {
    let a = vector(&[3.0, 4.0]);
    assert!((a.iter().map(|v| v * v).sum::<f32>() - 1.0).abs() < 1e-6);
    assert!((dot(&a, &a) - 1.0).abs() < 1e-6);

    let b = vector(&[-3.0, -4.0]);
    assert!((dot(&a, &b) + 1.0).abs() < 1e-6);

    let orthogonal = vector(&[4.0, -3.0]);
    assert!(dot(&a, &orthogonal).abs() < 1e-6);
}

#[test]
fn a_zero_vector_is_rejected_rather_than_normalized() {
    assert!(normalize(vec![0.0, 0.0]).is_none());
    assert!(normalize(vec![f32::NAN, 1.0]).is_none());
    assert!(normalize(Vec::new()).is_none());
}

#[test]
fn vectors_of_different_dimensions_are_not_comparable() {
    let short = vector(&[1.0, 0.0]);
    let long = vector(&[1.0, 0.0, 0.0]);
    assert_eq!(dot(&short, &long), 0.0);
}

#[test]
fn an_embeddings_response_is_parsed_and_normalized() {
    let response = json!({ "data": [{ "embedding": [3.0, 4.0] }] });
    let parsed = vector_from_response(&response).expect("vector parses");
    assert!((parsed[0] - 0.6).abs() < 1e-6);
    assert!((parsed[1] - 0.8).abs() < 1e-6);

    assert!(vector_from_response(&json!({ "data": [] })).is_none());
    assert!(vector_from_response(&json!({})).is_none());
    assert!(vector_from_response(&json!({ "data": [{ "embedding": [0.0, 0.0] }] })).is_none());
}

#[test]
fn a_near_identical_request_replays_the_same_decision() {
    let cache = RouteCache::default();
    let config = sample();
    cache.insert(
        vector(&[1.0, 0.0]),
        CachedDecision::Redirect("backend".to_string()),
        "hint matched".to_string(),
        &config,
    );

    let hit = cache
        .lookup(&vector(&[0.99, 0.1]), &config, |_| true)
        .expect("a close request hits");
    assert_eq!(
        hit.decision,
        CachedDecision::Redirect("backend".to_string())
    );
    assert_eq!(hit.reason, "hint matched");
    assert!(hit.similarity >= config.threshold);

    // A different request must not be answered from the cache.
    assert!(
        cache
            .lookup(&vector(&[0.0, 1.0]), &config, |_| true)
            .is_none()
    );
}

#[test]
fn the_closest_match_wins_when_several_are_above_the_threshold() {
    let cache = RouteCache::default();
    let config = EmbeddingConfig {
        threshold: 0.5,
        ..sample()
    };
    cache.insert(
        vector(&[1.0, 1.0]),
        CachedDecision::Redirect("far".to_string()),
        String::new(),
        &config,
    );
    cache.insert(
        vector(&[1.0, 0.0]),
        CachedDecision::Redirect("near".to_string()),
        String::new(),
        &config,
    );
    let hit = cache
        .lookup(&vector(&[1.0, 0.05]), &config, |_| true)
        .expect("hit");
    assert_eq!(hit.decision, CachedDecision::Redirect("near".to_string()));
}

#[test]
fn a_decision_naming_a_deleted_target_is_never_replayed() {
    let cache = RouteCache::default();
    let config = sample();
    cache.insert(
        vector(&[1.0, 0.0]),
        CachedDecision::Redirect("removed".to_string()),
        String::new(),
        &config,
    );
    let live = |decision: &CachedDecision| !matches!(decision, CachedDecision::Redirect(name) if name == "removed");
    assert!(cache.lookup(&vector(&[1.0, 0.0]), &config, live).is_none());

    cache.retain_valid(live);
    assert_eq!(cache.snapshot_json()["entries"], 0);
}

#[test]
fn the_cache_is_bounded_and_evicts_oldest_first() {
    let cache = RouteCache::default();
    let config = EmbeddingConfig {
        capacity: 2,
        ..sample()
    };
    for index in 0..4 {
        cache.insert(
            vector(&[1.0, index as f32]),
            CachedDecision::Redirect(format!("t{index}")),
            String::new(),
            &config,
        );
    }
    let snapshot = cache.snapshot_json();
    assert_eq!(snapshot["entries"], 2);
    assert_eq!(snapshot["evictions"], 2);
}

#[test]
fn expired_entries_are_dropped_on_lookup() {
    let cache = RouteCache::default();
    let expired = EmbeddingConfig {
        ttl_secs: 0,
        ..sample()
    };
    cache.insert(
        vector(&[1.0, 0.0]),
        CachedDecision::Local,
        String::new(),
        &expired,
    );
    assert!(
        cache
            .lookup(&vector(&[1.0, 0.0]), &expired, |_| true)
            .is_none()
    );
    assert_eq!(cache.snapshot_json()["entries"], 0);
}

#[test]
fn the_snapshot_reports_a_hit_rate() {
    let cache = RouteCache::default();
    let config = sample();
    cache.insert(
        vector(&[1.0, 0.0]),
        CachedDecision::Local,
        String::new(),
        &config,
    );
    cache.lookup(&vector(&[1.0, 0.0]), &config, |_| true);
    cache.lookup(&vector(&[0.0, 1.0]), &config, |_| true);
    let snapshot = cache.snapshot_json();
    assert_eq!(snapshot["lookups"], 2);
    assert_eq!(snapshot["hits"], 1);
    assert!((snapshot["hit_rate"].as_f64().unwrap() - 0.5).abs() < 1e-9);
}

#[test]
fn a_retry_window_needs_two_user_turns_and_no_tool_use_between_them() {
    let window = retry_window(&json!({
        "messages": [
            { "role": "user", "content": "why is the build slow" },
            { "role": "assistant", "content": "not sure" },
            { "role": "user", "content": "why is the build slow really" }
        ]
    }))
    .expect("a plain re-ask is a retry window");
    assert_eq!(window.newest, "why is the build slow really");
    assert_eq!(window.previous, "why is the build slow");

    // Intervening tool use means the agent made progress.
    assert!(
        retry_window(&json!({
            "messages": [
                { "role": "user", "content": "why is the build slow" },
                { "role": "assistant", "content": "", "tool_calls": [{ "id": "1" }] },
                { "role": "tool", "content": "timings" },
                { "role": "user", "content": "why is the build slow really" }
            ]
        }))
        .is_none()
    );

    // A first turn has nothing to repeat.
    assert!(
        retry_window(&json!({
            "messages": [{ "role": "user", "content": "hello" }]
        }))
        .is_none()
    );
}

#[test]
fn validation_requires_a_real_provider_and_sane_thresholds() {
    let known = |name: &str| name == "embeddings";

    let mut config = sample();
    config.provider = "ghost".to_string();
    assert!(config.validate(known).is_err());

    let mut config = sample();
    config.model = String::new();
    assert!(config.validate(known).is_err());

    let mut config = sample();
    config.threshold = 1.5;
    assert!(config.validate(known).is_err());

    let mut config = sample();
    config.capacity = 0;
    assert!(config.validate(known).is_err());

    let mut config = sample();
    config.timeout_ms = 0;
    assert!(config.validate(known).is_err());

    assert!(sample().validate(known).is_ok());

    // A disabled cache is never validated, so a half-filled section is fine.
    let disabled = EmbeddingConfig::default();
    assert!(disabled.validate(|_| false).is_ok());
    assert!(!disabled.is_active());
}
