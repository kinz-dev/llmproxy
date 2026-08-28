//! Provider health tracking and circuit breaking.
//!
//! `fallback_on_error` is one-dimensional: local to external. This is the
//! missing axis — per-provider health, so a provider that is rate-limited or
//! down is routed around instead of being retried into the ground, and so the
//! admin interface can say which provider is unhappy rather than only that a
//! request failed.
//!
//! Two failure shapes are tracked separately because they need different
//! recoveries. A quota exhaustion has a known duration the provider tells us
//! about; an outage has none, so it gets exponential backoff with a
//! single-probe half-open state.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct HealthConfig {
    /// Track provider health at all. Off means every provider always reads as
    /// available and nothing is ever routed around.
    pub(crate) enabled: bool,
    /// Consecutive failures that trip the breaker.
    pub(crate) failure_threshold: u32,
    /// How long the breaker stays open on its first trip. Repeat trips back
    /// off exponentially from here.
    pub(crate) open_secs: u64,
    /// Ceiling on the backed-off open duration.
    pub(crate) max_open_secs: u64,
    /// Cap on a provider-supplied `Retry-After`, so a hostile or mistaken
    /// header cannot park a provider for hours.
    pub(crate) max_rate_limit_secs: u64,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            failure_threshold: 4,
            open_secs: 30,
            max_open_secs: 600,
            max_rate_limit_secs: 900,
        }
    }
}

impl HealthConfig {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if self.failure_threshold == 0 {
            return Err("health.failure_threshold must be greater than zero".to_string());
        }
        if self.open_secs == 0 {
            return Err("health.open_secs must be greater than zero".to_string());
        }
        if self.max_open_secs < self.open_secs {
            return Err("health.max_open_secs must be at least health.open_secs".to_string());
        }
        Ok(())
    }
}

/// Why a provider is not usable right now, and for how long.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Availability {
    Available,
    /// The provider told us its quota is exhausted. The wait is known.
    RateLimited {
        retry_in: Duration,
    },
    /// Repeated failures tripped the breaker. The wait is a backoff guess.
    Open {
        retry_in: Duration,
    },
}

impl Availability {
    pub(crate) fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }

    pub(crate) fn reason(&self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::RateLimited { .. } => "rate limited",
            Self::Open { .. } => "circuit open",
        }
    }

    pub(crate) fn retry_in(&self) -> Option<Duration> {
        match self {
            Self::Available => None,
            Self::RateLimited { retry_in } | Self::Open { retry_in } => Some(*retry_in),
        }
    }
}

#[derive(Debug, Default)]
struct Entry {
    consecutive_failures: u32,
    /// Set while the breaker is open. Once it passes, the next request is a
    /// half-open probe rather than an immediate return to normal.
    open_until: Option<Instant>,
    /// A probe is out; nothing else should be sent until it lands.
    probe_in_flight: bool,
    rate_limited_until: Option<Instant>,
    trips: u32,
    successes: u64,
    failures: u64,
    rate_limits: u64,
    last_error: Option<String>,
}

#[derive(Default)]
pub(crate) struct ProviderHealth {
    entries: Mutex<HashMap<String, Entry>>,
    config: Mutex<HealthConfig>,
}

pub(crate) fn provider_health() -> &'static ProviderHealth {
    static HEALTH: OnceLock<ProviderHealth> = OnceLock::new();
    HEALTH.get_or_init(ProviderHealth::default)
}

impl ProviderHealth {
    /// Track the live configuration. Health is recorded from the response
    /// pipeline, which has no access to app state, so the policy lives here.
    pub(crate) fn configure(&self, config: HealthConfig) {
        *self.config.lock().expect("provider health poisoned") = config;
    }

    fn config(&self) -> HealthConfig {
        *self.config.lock().expect("provider health poisoned")
    }

    /// Availability without side effects. Used where the answer only informs
    /// a decision, so a half-open probe is not spent on a request that will
    /// never be sent.
    pub(crate) fn peek(&self, provider: &str) -> Availability {
        self.availability_inner(provider, false)
    }

    /// Availability for a request that is about to be sent. In the half-open
    /// state this claims the single probe slot.
    pub(crate) fn availability(&self, provider: &str) -> Availability {
        self.availability_inner(provider, true)
    }

    fn availability_inner(&self, provider: &str, claim_probe: bool) -> Availability {
        let config = self.config();
        if !config.enabled {
            return Availability::Available;
        }
        let now = Instant::now();
        let mut entries = self.entries.lock().expect("provider health poisoned");
        let Some(entry) = entries.get_mut(provider) else {
            return Availability::Available;
        };
        if let Some(until) = entry.rate_limited_until {
            if until > now {
                return Availability::RateLimited {
                    retry_in: until - now,
                };
            }
            entry.rate_limited_until = None;
        }
        if let Some(until) = entry.open_until {
            if until > now {
                return Availability::Open {
                    retry_in: until - now,
                };
            }
            // Half-open: exactly one request is allowed through to find out
            // whether the provider recovered.
            if entry.probe_in_flight {
                return Availability::Open {
                    retry_in: Duration::from_secs(1),
                };
            }
            if claim_probe {
                entry.probe_in_flight = true;
            }
        }
        Availability::Available
    }

    pub(crate) fn record_success(&self, provider: &str) {
        let mut entries = self.entries.lock().expect("provider health poisoned");
        let entry = entries.entry(provider.to_string()).or_default();
        entry.successes = entry.successes.saturating_add(1);
        let was_open = entry.open_until.is_some();
        entry.consecutive_failures = 0;
        entry.open_until = None;
        entry.probe_in_flight = false;
        entry.trips = 0;
        entry.last_error = None;
        if was_open {
            info!(provider, "provider recovered; circuit closed");
        }
    }

    pub(crate) fn record_failure(&self, provider: &str, detail: impl Into<String>) {
        let config = self.config();
        if !config.enabled {
            return;
        }
        let detail = detail.into();
        let mut entries = self.entries.lock().expect("provider health poisoned");
        let entry = entries.entry(provider.to_string()).or_default();
        entry.failures = entry.failures.saturating_add(1);
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        entry.last_error = Some(detail.clone());
        let probing = entry.probe_in_flight;
        entry.probe_in_flight = false;
        if probing || entry.consecutive_failures >= config.failure_threshold {
            entry.trips = entry.trips.saturating_add(1);
            let backoff = backoff_secs(config, entry.trips);
            entry.open_until = Some(Instant::now() + Duration::from_secs(backoff));
            warn!(
                provider,
                failures = entry.consecutive_failures,
                open_secs = backoff,
                error = %detail,
                "provider circuit opened"
            );
        }
    }

    /// A provider reported quota exhaustion. `retry_after` is its own estimate;
    /// without one, the breaker backoff is used instead of a guess.
    pub(crate) fn record_rate_limit(&self, provider: &str, retry_after: Option<Duration>) {
        let config = self.config();
        if !config.enabled {
            return;
        }
        let mut entries = self.entries.lock().expect("provider health poisoned");
        let entry = entries.entry(provider.to_string()).or_default();
        entry.rate_limits = entry.rate_limits.saturating_add(1);
        entry.failures = entry.failures.saturating_add(1);
        entry.probe_in_flight = false;
        entry.last_error = Some("rate limited by the provider".to_string());
        let wait = retry_after
            .unwrap_or_else(|| Duration::from_secs(config.open_secs))
            .min(Duration::from_secs(config.max_rate_limit_secs));
        entry.rate_limited_until = Some(Instant::now() + wait);
        warn!(
            provider,
            retry_in_secs = wait.as_secs(),
            "provider reported quota exhaustion"
        );
    }

    pub(crate) fn reset(&self) {
        self.entries
            .lock()
            .expect("provider health poisoned")
            .clear();
    }

    pub(crate) fn snapshot_json(&self) -> Value {
        let now = Instant::now();
        let entries = self.entries.lock().expect("provider health poisoned");
        let mut providers = entries
            .iter()
            .map(|(name, entry)| {
                let availability = if entry.rate_limited_until.is_some_and(|until| until > now) {
                    Availability::RateLimited {
                        retry_in: entry.rate_limited_until.expect("checked") - now,
                    }
                } else if entry.open_until.is_some_and(|until| until > now) {
                    Availability::Open {
                        retry_in: entry.open_until.expect("checked") - now,
                    }
                } else {
                    Availability::Available
                };
                let retry_in = availability.retry_in().map(|retry| retry.as_secs());
                let status = if entry.rate_limited_until.is_some_and(|until| until > now) {
                    "rate_limited"
                } else if entry.open_until.is_some_and(|until| until > now) {
                    "open"
                } else if entry.open_until.is_some() {
                    "half_open"
                } else {
                    "closed"
                };
                json!({
                    "provider": name,
                    "status": status,
                    "consecutive_failures": entry.consecutive_failures,
                    "successes": entry.successes,
                    "failures": entry.failures,
                    "rate_limits": entry.rate_limits,
                    "trips": entry.trips,
                    "retry_in_secs": retry_in,
                    "last_error": entry.last_error,
                })
            })
            .collect::<Vec<_>>();
        providers.sort_by(|left, right| left["provider"].as_str().cmp(&right["provider"].as_str()));
        json!({
            "enabled": self.config().enabled,
            "providers": providers,
        })
    }
}

fn backoff_secs(config: HealthConfig, trips: u32) -> u64 {
    let shift = trips.saturating_sub(1).min(16);
    config
        .open_secs
        .saturating_mul(1_u64 << shift)
        .min(config.max_open_secs)
}

/// HTTP statuses worth trying somewhere else. Deliberately narrow: a 4xx that
/// is not a quota or timing problem means the request itself is wrong, and
/// re-sending it elsewhere only spends money to fail again.
pub(crate) fn status_is_failover_worthy(status: u16) -> bool {
    matches!(status, 408 | 409 | 425 | 429 | 500 | 502 | 503 | 504 | 529)
}

pub(crate) fn status_is_rate_limit(status: u16) -> bool {
    matches!(status, 429 | 529)
}

/// `Retry-After` as either delta-seconds or an HTTP date. Only the delta form
/// is honoured; a date is treated as "unspecified" rather than parsed, because
/// a wrong clock would produce a worse answer than the backoff default.
pub(crate) fn retry_after_duration(value: Option<&str>) -> Option<Duration> {
    let seconds = value?.trim().parse::<u64>().ok()?;
    Some(Duration::from_secs(seconds))
}

#[cfg(test)]
#[path = "health_tests.rs"]
mod tests;
