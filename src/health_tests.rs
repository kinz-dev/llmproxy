use super::*;

fn tracked() -> ProviderHealth {
    let health = ProviderHealth::default();
    health.configure(HealthConfig {
        enabled: true,
        failure_threshold: 3,
        open_secs: 30,
        max_open_secs: 120,
        max_rate_limit_secs: 900,
    });
    health
}

#[test]
fn an_unknown_provider_is_available() {
    assert_eq!(tracked().availability("openai"), Availability::Available);
}

#[test]
fn the_breaker_opens_only_after_the_configured_run_of_failures() {
    let health = tracked();
    health.record_failure("openai", "boom");
    health.record_failure("openai", "boom");
    assert!(health.availability("openai").is_available());
    health.record_failure("openai", "boom");
    assert!(matches!(
        health.availability("openai"),
        Availability::Open { .. }
    ));
}

#[test]
fn a_success_resets_the_run_and_closes_the_breaker() {
    let health = tracked();
    for _ in 0..3 {
        health.record_failure("openai", "boom");
    }
    health.record_success("openai");
    assert_eq!(health.availability("openai"), Availability::Available);

    // The run counter reset too, so two more failures are not enough.
    health.record_failure("openai", "boom");
    health.record_failure("openai", "boom");
    assert!(health.availability("openai").is_available());
}

#[test]
fn only_one_probe_is_admitted_while_half_open() {
    let health = tracked();
    for _ in 0..3 {
        health.record_failure("openai", "boom");
    }
    // Force the open window to have elapsed.
    {
        let mut entries = health.entries.lock().expect("health poisoned");
        let entry = entries.get_mut("openai").expect("entry");
        entry.open_until = Some(Instant::now() - Duration::from_secs(1));
    }
    assert!(
        health.availability("openai").is_available(),
        "the first request after the window is the probe"
    );
    assert!(
        !health.availability("openai").is_available(),
        "nothing else goes out until the probe lands"
    );

    // A failed probe re-opens immediately rather than waiting for another run.
    health.record_failure("openai", "still down");
    assert!(matches!(
        health.availability("openai"),
        Availability::Open { .. }
    ));
}

#[test]
fn repeat_trips_back_off_and_are_capped() {
    let config = HealthConfig {
        enabled: true,
        failure_threshold: 1,
        open_secs: 30,
        max_open_secs: 120,
        max_rate_limit_secs: 900,
    };
    assert_eq!(backoff_secs(config, 1), 30);
    assert_eq!(backoff_secs(config, 2), 60);
    assert_eq!(backoff_secs(config, 3), 120);
    assert_eq!(backoff_secs(config, 40), 120);
}

#[test]
fn a_rate_limit_uses_the_providers_own_estimate_and_is_capped() {
    let health = tracked();
    health.record_rate_limit("openai", Some(Duration::from_secs(45)));
    match health.availability("openai") {
        Availability::RateLimited { retry_in } => {
            assert!(retry_in <= Duration::from_secs(45));
            assert!(retry_in > Duration::from_secs(40));
        }
        other => panic!("expected a rate limit, got {other:?}"),
    }

    let capped = tracked();
    capped.record_rate_limit("openai", Some(Duration::from_secs(86_400)));
    match capped.availability("openai") {
        Availability::RateLimited { retry_in } => {
            assert!(
                retry_in <= Duration::from_secs(900),
                "a hostile header is capped"
            );
        }
        other => panic!("expected a rate limit, got {other:?}"),
    }
}

#[test]
fn a_rate_limit_without_a_header_falls_back_to_the_breaker_window() {
    let health = tracked();
    health.record_rate_limit("openai", None);
    match health.availability("openai") {
        Availability::RateLimited { retry_in } => {
            assert!(retry_in <= Duration::from_secs(30));
        }
        other => panic!("expected a rate limit, got {other:?}"),
    }
}

#[test]
fn disabling_health_makes_every_provider_read_as_available() {
    let health = ProviderHealth::default();
    health.configure(HealthConfig {
        enabled: false,
        ..HealthConfig::default()
    });
    for _ in 0..50 {
        health.record_failure("openai", "boom");
    }
    health.record_rate_limit("openai", Some(Duration::from_secs(600)));
    assert_eq!(health.availability("openai"), Availability::Available);
}

#[test]
fn only_retryable_statuses_are_worth_another_provider() {
    for status in [408, 429, 500, 502, 503, 504, 529] {
        assert!(status_is_failover_worthy(status), "{status}");
    }
    // A malformed or unauthorized request fails identically everywhere.
    for status in [400, 401, 403, 404, 422] {
        assert!(!status_is_failover_worthy(status), "{status}");
    }
}

#[test]
fn retry_after_accepts_delta_seconds_and_ignores_dates() {
    assert_eq!(
        retry_after_duration(Some("30")),
        Some(Duration::from_secs(30))
    );
    assert_eq!(
        retry_after_duration(Some(" 5 ")),
        Some(Duration::from_secs(5))
    );
    assert_eq!(
        retry_after_duration(Some("Wed, 21 Oct 2026 07:28:00 GMT")),
        None
    );
    assert_eq!(retry_after_duration(None), None);
}

#[test]
fn the_snapshot_reports_status_per_provider() {
    let health = tracked();
    health.record_success("good");
    for _ in 0..3 {
        health.record_failure("bad", "boom");
    }
    health.record_rate_limit("busy", Some(Duration::from_secs(60)));

    let snapshot = health.snapshot_json();
    let row = |name: &str| {
        snapshot["providers"]
            .as_array()
            .expect("providers")
            .iter()
            .find(|row| row["provider"] == name)
            .expect("row")
            .clone()
    };
    assert_eq!(row("good")["status"], "closed");
    assert_eq!(row("bad")["status"], "open");
    assert_eq!(row("bad")["last_error"], "boom");
    assert_eq!(row("busy")["status"], "rate_limited");
}

#[test]
fn validation_rejects_a_nonsensical_breaker() {
    assert!(
        HealthConfig {
            failure_threshold: 0,
            ..HealthConfig::default()
        }
        .validate()
        .is_err()
    );
    assert!(
        HealthConfig {
            open_secs: 0,
            ..HealthConfig::default()
        }
        .validate()
        .is_err()
    );
    assert!(
        HealthConfig {
            open_secs: 100,
            max_open_secs: 10,
            ..HealthConfig::default()
        }
        .validate()
        .is_err()
    );
    assert!(HealthConfig::default().validate().is_ok());
}
