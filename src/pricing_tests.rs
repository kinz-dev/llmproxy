use super::*;

fn price(input: f64, output: f64) -> ModelPrice {
    ModelPrice {
        input,
        output,
        cache_read: None,
        cache_write: None,
    }
}

fn pricing(entries: &[(&str, ModelPrice)]) -> PricingConfig {
    PricingConfig {
        models: entries
            .iter()
            .map(|(name, price)| ((*name).to_string(), *price))
            .collect(),
        ..PricingConfig::default()
    }
}

#[test]
fn exact_model_id_wins_over_prefix() {
    let config = pricing(&[
        ("gpt-5.4", price(1.0, 2.0)),
        ("gpt-5.4-mini", price(0.1, 0.2)),
    ]);
    assert_eq!(config.rate_for("gpt-5.4"), Some(&price(1.0, 2.0)));
    assert_eq!(config.rate_for("gpt-5.4-mini"), Some(&price(0.1, 0.2)));
}

#[test]
fn longest_prefix_matches_dated_model_ids() {
    let config = pricing(&[("gpt-5", price(5.0, 10.0)), ("gpt-5.4", price(1.0, 2.0))]);
    assert_eq!(
        config.rate_for("gpt-5.4-2026-01-30"),
        Some(&price(1.0, 2.0))
    );
    assert_eq!(config.rate_for("gpt-5-turbo"), Some(&price(5.0, 10.0)));
}

#[test]
fn star_is_the_last_resort() {
    let config = pricing(&[("*", price(3.0, 6.0)), ("gpt-5.4", price(1.0, 2.0))]);
    assert_eq!(config.rate_for("claude-opus-5"), Some(&price(3.0, 6.0)));
    assert_eq!(config.rate_for("gpt-5.4"), Some(&price(1.0, 2.0)));
}

#[test]
fn unknown_model_without_star_has_no_rate() {
    let config = pricing(&[("gpt-5.4", price(1.0, 2.0))]);
    assert_eq!(config.rate_for("claude-opus-5"), None);
}

#[test]
fn cache_tokens_are_billed_at_their_own_rates() {
    let price = ModelPrice {
        input: 10.0,
        output: 100.0,
        cache_read: Some(1.0),
        cache_write: Some(12.5),
    };
    let counters = TokenCounters {
        input_tokens: 1_000_000,
        output_tokens: 1_000_000,
        cache_read_tokens: 600_000,
        cache_write_tokens: 200_000,
    };
    let spend = spend_for(&price, counters);
    // 200k uncached at 10, 600k reads at 1, 200k writes at 12.5, 1M output.
    assert!((spend.input - 2.0).abs() < 1e-9);
    assert!((spend.cache_read - 0.6).abs() < 1e-9);
    assert!((spend.cache_write - 2.5).abs() < 1e-9);
    assert!((spend.output - 100.0).abs() < 1e-9);
    assert!((spend.total() - 105.1).abs() < 1e-9);
}

#[test]
fn cache_rates_default_to_the_input_rate() {
    let counters = TokenCounters {
        input_tokens: 1_000_000,
        output_tokens: 0,
        cache_read_tokens: 500_000,
        cache_write_tokens: 0,
    };
    let spend = spend_for(&price(10.0, 0.0), counters);
    assert!((spend.total() - 10.0).abs() < 1e-9);
}

#[test]
fn cache_subsets_never_underflow_the_uncached_count() {
    let counters = TokenCounters {
        input_tokens: 100,
        output_tokens: 0,
        cache_read_tokens: 400,
        cache_write_tokens: 0,
    };
    assert_eq!(counters.uncached_input_tokens(), 0);
}

#[test]
fn subscription_providers_report_no_dollar_figure() {
    let mut index_source = std::collections::HashMap::new();
    index_source.insert(
        "claude_sub".to_string(),
        PricingConfig {
            subscription: true,
            ..PricingConfig::default()
        },
    );
    index_source.insert(
        "openai".to_string(),
        pricing(&[("gpt-5.4", price(1.0, 2.0))]),
    );
    let index = PricingIndex::from_providers(index_source.iter());

    assert_eq!(
        index.rate("claude_sub", "claude-opus-5"),
        ModelRate::Subscription
    );
    assert!(
        index
            .spend(
                "claude_sub",
                "claude-opus-5",
                TokenCounters {
                    input_tokens: 1_000_000,
                    output_tokens: 1_000_000,
                    ..TokenCounters::default()
                },
            )
            .is_none()
    );
    assert!(
        index
            .spend("openai", "gpt-5.4", TokenCounters::default())
            .is_some()
    );
    assert_eq!(index.rate("unknown", "gpt-5.4"), ModelRate::Unpriced);
}

#[test]
fn unconfigured_providers_are_dropped_from_the_index() {
    let mut source = std::collections::HashMap::new();
    source.insert("silent".to_string(), PricingConfig::default());
    let index = PricingIndex::from_providers(source.iter());
    assert!(index.is_empty());
    assert_eq!(index.currency(), None);
}

#[test]
fn mixed_currencies_refuse_to_report_one() {
    let mut source = std::collections::HashMap::new();
    source.insert("usd".to_string(), pricing(&[("a", price(1.0, 1.0))]));
    source.insert(
        "eur".to_string(),
        PricingConfig {
            currency: "EUR".to_string(),
            models: [("b".to_string(), price(1.0, 1.0))].into_iter().collect(),
            ..PricingConfig::default()
        },
    );
    let index = PricingIndex::from_providers(source.iter());
    assert_eq!(index.currency(), None);
}

#[test]
fn validation_rejects_negative_and_non_finite_rates() {
    let config = pricing(&[("gpt-5.4", price(-1.0, 2.0))]);
    assert!(config.validate("openai").is_err());

    let config = pricing(&[("gpt-5.4", price(f64::NAN, 2.0))]);
    assert!(config.validate("openai").is_err());

    let config = pricing(&[("gpt-5.4", price(1.0, 2.0))]);
    assert!(config.validate("openai").is_ok());
}
