//! Provider price metadata and the spend arithmetic built on it.
//!
//! Published rates change without notice, so nothing here is hardcoded: the
//! configuration file is the only source of truth. A provider with no
//! `[providers.*.pricing]` table simply has no price, and every consumer must
//! render that as "unpriced" rather than as zero.

use std::collections::{BTreeMap, HashMap};

use serde::Deserialize;

/// Rates are quoted per million tokens, matching how every vendor publishes
/// them, so a config entry can be copied from a price page without arithmetic.
pub(crate) const TOKENS_PER_PRICE_UNIT: f64 = 1_000_000.0;

/// Per-model rates. `cache_read` and `cache_write` fall back to the uncached
/// input rate when unset, which is the correct behaviour for providers that do
/// not discount cache reads at all.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct ModelPrice {
    pub(crate) input: f64,
    pub(crate) output: f64,
    pub(crate) cache_read: Option<f64>,
    pub(crate) cache_write: Option<f64>,
}

impl ModelPrice {
    fn cache_read_rate(&self) -> f64 {
        self.cache_read.unwrap_or(self.input)
    }

    fn cache_write_rate(&self) -> f64 {
        self.cache_write.unwrap_or(self.input)
    }

    fn is_usable(&self) -> bool {
        [
            self.input,
            self.output,
            self.cache_read_rate(),
            self.cache_write_rate(),
        ]
        .iter()
        .all(|rate| rate.is_finite() && *rate >= 0.0)
    }
}

/// A provider's price table.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct PricingConfig {
    /// The provider bills against a plan — a borrowed CLI login, a seat
    /// licence — rather than per token. Showing a dollar figure for those rows
    /// is actively misleading, so they report token volume only.
    pub(crate) subscription: bool,
    /// Informational; the admin UI renders it beside the totals.
    pub(crate) currency: String,
    /// Model id (or id prefix, or `*`) to rate.
    pub(crate) models: BTreeMap<String, ModelPrice>,
}

impl Default for PricingConfig {
    fn default() -> Self {
        Self {
            subscription: false,
            currency: "USD".to_string(),
            models: BTreeMap::new(),
        }
    }
}

impl PricingConfig {
    /// Whether the operator said anything at all about this provider's cost.
    pub(crate) fn is_configured(&self) -> bool {
        self.subscription || !self.models.is_empty()
    }

    /// Exact model id first, then the longest configured prefix, then `*`.
    /// Prefix matching exists so a dated id such as `gpt-5.4-2026-01-30`
    /// inherits its family rate without one config entry per snapshot.
    pub(crate) fn rate_for(&self, model: &str) -> Option<&ModelPrice> {
        if let Some(price) = self.models.get(model) {
            return Some(price);
        }
        let mut best: Option<(&str, &ModelPrice)> = None;
        for (key, price) in &self.models {
            if key == "*" || !model.starts_with(key.as_str()) {
                continue;
            }
            if best.is_none_or(|(current, _)| key.len() > current.len()) {
                best = Some((key.as_str(), price));
            }
        }
        best.map(|(_, price)| price)
            .or_else(|| self.models.get("*"))
    }

    pub(crate) fn validate(&self, provider: &str) -> Result<(), String> {
        if self.currency.trim().is_empty() {
            return Err(format!(
                "provider {provider:?} pricing.currency cannot be empty"
            ));
        }
        for (model, price) in &self.models {
            if model.trim().is_empty() {
                return Err(format!(
                    "provider {provider:?} has a pricing entry with an empty model key"
                ));
            }
            if !price.is_usable() {
                return Err(format!(
                    "provider {provider:?} pricing for {model:?} must use finite, non-negative rates"
                ));
            }
        }
        Ok(())
    }
}

/// Token counts a spend figure is derived from. `input_tokens` is the full
/// prompt count including anything served from or written to cache, matching
/// OpenAI's `prompt_tokens`; the two cache fields are subsets of it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TokenCounters {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cache_read_tokens: u64,
    pub(crate) cache_write_tokens: u64,
}

impl TokenCounters {
    /// Prompt tokens billed at the full input rate.
    pub(crate) fn uncached_input_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_sub(self.cache_read_tokens)
            .saturating_sub(self.cache_write_tokens)
    }
}

/// Spend broken out by the rate that produced it, so the UI can explain a
/// figure rather than only state it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Spend {
    pub(crate) input: f64,
    pub(crate) output: f64,
    pub(crate) cache_read: f64,
    pub(crate) cache_write: f64,
}

impl Spend {
    pub(crate) fn total(&self) -> f64 {
        self.input + self.output + self.cache_read + self.cache_write
    }

    pub(crate) fn add(&mut self, other: Spend) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
    }
}

pub(crate) fn spend_for(price: &ModelPrice, counters: TokenCounters) -> Spend {
    let cost = |tokens: u64, rate: f64| tokens as f64 * rate / TOKENS_PER_PRICE_UNIT;
    Spend {
        input: cost(counters.uncached_input_tokens(), price.input),
        output: cost(counters.output_tokens, price.output),
        cache_read: cost(counters.cache_read_tokens, price.cache_read_rate()),
        cache_write: cost(counters.cache_write_tokens, price.cache_write_rate()),
    }
}

/// What pricing has to say about one (provider, model) pair.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ModelRate<'a> {
    /// A rate exists; spend is a number.
    Priced(&'a ModelPrice),
    /// The provider bills against a plan. Token volume is meaningful, dollars
    /// are not.
    Subscription,
    /// No price was configured for this provider or model.
    Unpriced,
}

impl ModelRate<'_> {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Priced(_) => "priced",
            Self::Subscription => "subscription",
            Self::Unpriced => "unpriced",
        }
    }
}

/// Provider name to price table, snapshotted from the live config so a report
/// prices exactly the configuration it was generated under.
#[derive(Debug, Clone, Default)]
pub(crate) struct PricingIndex {
    providers: HashMap<String, PricingConfig>,
}

impl PricingIndex {
    pub(crate) fn from_providers<'a>(
        providers: impl IntoIterator<Item = (&'a String, &'a PricingConfig)>,
    ) -> Self {
        Self {
            providers: providers
                .into_iter()
                .filter(|(_, pricing)| pricing.is_configured())
                .map(|(name, pricing)| (name.clone(), pricing.clone()))
                .collect(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    pub(crate) fn rate(&self, provider: &str, model: &str) -> ModelRate<'_> {
        let Some(pricing) = self.providers.get(provider) else {
            return ModelRate::Unpriced;
        };
        if pricing.subscription {
            return ModelRate::Subscription;
        }
        pricing
            .rate_for(model)
            .map_or(ModelRate::Unpriced, ModelRate::Priced)
    }

    /// The currency every priced provider agrees on, or `None` when they
    /// disagree — mixing currencies in one total would be a lie.
    pub(crate) fn currency(&self) -> Option<&str> {
        let mut currency: Option<&str> = None;
        for pricing in self.providers.values() {
            if pricing.subscription && pricing.models.is_empty() {
                continue;
            }
            match currency {
                None => currency = Some(pricing.currency.as_str()),
                Some(existing) if existing == pricing.currency => {}
                Some(_) => return None,
            }
        }
        currency
    }

    /// Spend for one (provider, model) pair, or `None` when it has no price.
    pub(crate) fn spend(
        &self,
        provider: &str,
        model: &str,
        counters: TokenCounters,
    ) -> Option<Spend> {
        match self.rate(provider, model) {
            ModelRate::Priced(price) => Some(spend_for(price, counters)),
            ModelRate::Subscription | ModelRate::Unpriced => None,
        }
    }
}

#[cfg(test)]
#[path = "pricing_tests.rs"]
mod tests;
