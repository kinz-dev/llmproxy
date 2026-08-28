use super::*;

use crate::pricing::{ModelPrice, PricingConfig};

fn usage(input: u64, output: u64, estimated: bool) -> TokenUsage {
    TokenUsage {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        estimated,
    }
}

fn priced_index() -> PricingIndex {
    let mut providers = std::collections::HashMap::new();
    providers.insert(
        "openai".to_string(),
        PricingConfig {
            models: [(
                "gpt-test".to_string(),
                ModelPrice {
                    input: 2.0,
                    output: 8.0,
                    cache_read: None,
                    cache_write: None,
                },
            )]
            .into_iter()
            .collect(),
            ..PricingConfig::default()
        },
    );
    PricingIndex::from_providers(providers.iter())
}

fn temp_dir(tag: u32) -> PathBuf {
    let path = std::env::temp_dir().join(format!("llmproxy-usage-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&path).expect("temp dir");
    path
}

#[test]
fn months_are_derived_from_the_epoch_without_a_date_dependency() {
    assert_eq!(month_of(0), "1970-01");
    // 2024-02-29T00:00:00Z, a leap day, is the interesting boundary case.
    assert_eq!(month_of(1_709_164_800_000), "2024-02");
    // 2024-03-01T00:00:00Z is the day after it.
    assert_eq!(month_of(1_709_251_200_000), "2024-03");
    assert_eq!(month_of(1_735_689_600_000), "2025-01");
}

#[test]
fn recording_is_inert_until_the_ledger_is_opened() {
    let store = UsageStore::default();
    store.record(
        Some("demo"),
        "openai",
        "gpt-test",
        Duration::ZERO,
        usage(1, 1, false),
    );
    let report = store.report(&priced_index(), None, None);
    assert_eq!(report["total"]["requests"], 0);
}

#[test]
fn spend_is_grouped_by_project_and_priced_at_read_time() {
    let directory = temp_dir(line!());
    let store = UsageStore::default();
    store.open(&directory);

    store.record(
        Some("alpha"),
        "openai",
        "gpt-test",
        Duration::from_millis(10),
        usage(1_000_000, 1_000_000, false),
    );
    store.record(
        Some("beta"),
        "openai",
        "gpt-test",
        Duration::from_millis(10),
        usage(500_000, 0, true),
    );
    store.record(
        None,
        "mystery",
        "unknown",
        Duration::from_millis(10),
        usage(10, 10, false),
    );

    let report = store.report(&priced_index(), None, None);
    assert_eq!(report["total"]["requests"], 3);
    assert_eq!(report["total"]["estimated_requests"], 1);
    assert_eq!(report["total"]["unpriced_requests"], 1);
    // 1M in + 1M out at 2/8, plus 0.5M in at 2.
    assert!((report["total"]["cost"]["total"].as_f64().unwrap() - 11.0).abs() < 1e-9);

    let projects = report["projects"].as_array().expect("project rows");
    let alpha = projects
        .iter()
        .find(|row| row["project"] == "alpha")
        .expect("alpha row");
    assert!((alpha["cost"]["total"].as_f64().unwrap() - 10.0).abs() < 1e-9);
    assert!(
        projects
            .iter()
            .any(|row| row["project"] == UNSCOPED_PROJECT),
        "traffic without a project scope is still attributed"
    );

    let scoped = store.report(&priced_index(), None, Some("beta"));
    assert_eq!(scoped["total"]["requests"], 1);
    assert!((scoped["total"]["cost"]["total"].as_f64().unwrap() - 1.0).abs() < 1e-9);

    std::fs::remove_dir_all(&directory).ok();
}

#[test]
fn counters_survive_a_restart() {
    let directory = temp_dir(line!());
    {
        let store = UsageStore::default();
        store.open(&directory);
        store.record(
            Some("alpha"),
            "openai",
            "gpt-test",
            Duration::from_millis(5),
            usage(100, 50, false),
        );
        store.flush();
    }

    let reopened = UsageStore::default();
    reopened.open(&directory);
    let report = reopened.report(&priced_index(), None, None);
    assert_eq!(report["total"]["requests"], 1);
    assert_eq!(report["total"]["input_tokens"], 100);
    assert_eq!(report["total"]["output_tokens"], 50);
    assert_eq!(report["months"].as_array().expect("months").len(), 1);

    std::fs::remove_dir_all(&directory).ok();
}

#[test]
fn reset_clears_the_persisted_ledger_too() {
    let directory = temp_dir(line!());
    let store = UsageStore::default();
    store.open(&directory);
    store.record(
        Some("a"),
        "openai",
        "gpt-test",
        Duration::ZERO,
        usage(1, 1, false),
    );
    store.reset();

    let reopened = UsageStore::default();
    reopened.open(&directory);
    assert_eq!(
        reopened.report(&priced_index(), None, None)["total"]["requests"],
        0
    );

    std::fs::remove_dir_all(&directory).ok();
}

#[test]
fn old_months_are_pruned_before_the_ledger_grows_without_bound() {
    let mut entries = BTreeMap::new();
    for month in 0..(RETAINED_MONTHS + 5) {
        entries.insert(
            UsageKey {
                month: format!("20{:02}-01", month),
                project: "a".to_string(),
                provider: "p".to_string(),
                model: "m".to_string(),
            },
            UsageRecord {
                requests: 1,
                ..UsageRecord::default()
            },
        );
    }
    prune(&mut entries);
    assert_eq!(entries.len(), RETAINED_MONTHS);
    assert!(entries.keys().all(|key| key.month.as_str() >= "2005-01"));
}

#[test]
fn a_corrupt_ledger_is_reported_and_then_ignored() {
    let directory = temp_dir(line!());
    std::fs::write(directory.join(FILE_NAME), "{not json").expect("write");
    let store = UsageStore::default();
    store.open(&directory);
    assert!(store.enabled(), "a bad ledger must not disable proxying");
    store.record(
        Some("a"),
        "openai",
        "gpt-test",
        Duration::ZERO,
        usage(1, 1, false),
    );
    assert_eq!(
        store.report(&priced_index(), None, None)["total"]["requests"],
        1
    );
    std::fs::remove_dir_all(&directory).ok();
}
