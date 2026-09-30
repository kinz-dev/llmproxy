//! In-memory proxy statistics for the admin interface.
//!
//! One global [`ProxyStats`] instance collects counters, latency sample
//! windows, and approximate token savings. Everything resets on restart.

use std::collections::VecDeque;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::response::Response;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tracing::{error, info, trace};

use crate::is_event_stream_content_type;
use crate::latency::{LATENCY_WINDOW_CAP, LatencyMetrics, LatencyRing, PROVIDER_LATENCY_CAP};
use crate::token_estimate::{self, TokenEstimator, TokenSource};

/// Derived provider network scope used for privacy checks and aggregate
/// telemetry. This is not a provider type or a routing preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RouteClass {
    Local,
    External,
}

impl RouteClass {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::External => "external",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemapKind {
    Alias,
    Namespace,
    Auto,
}

/// A read-mostly registry of per-route metric rows.
///
/// Route cardinality is bounded by configuration (frontends x providers x
/// models), so a short linear scan under a read lock beats a hash map here: it
/// compares borrowed `&str` segments directly instead of building an owned key
/// to hash, and that is what keeps the request path allocation-free once a row
/// exists. Only the first sighting of a route takes the write lock and interns
/// its segments; every request after that is a shared read plus atomic
/// updates, so recording statistics never serializes requests against each
/// other the way the previous `Mutex<HashMap<(String, String), _>>` did.
struct MetricRegistry<const N: usize, V> {
    rows: RwLock<Vec<MetricRow<N, V>>>,
}

struct MetricRow<const N: usize, V> {
    key: [Arc<str>; N],
    value: Arc<V>,
}

impl<const N: usize, V> Default for MetricRegistry<N, V> {
    fn default() -> Self {
        Self {
            rows: RwLock::new(Vec::new()),
        }
    }
}

fn key_matches<const N: usize>(key: &[Arc<str>; N], parts: [&str; N]) -> bool {
    key.iter().zip(parts).all(|(owned, part)| &**owned == part)
}

impl<const N: usize, V: Default> MetricRegistry<N, V> {
    /// The row for `parts`, interning it on first sight. The returned handle is
    /// an `Arc` clone (a refcount bump), never a fresh allocation.
    fn row(&self, parts: [&str; N]) -> Arc<V> {
        if let Some(row) = self
            .rows
            .read()
            .expect("metric registry poisoned")
            .iter()
            .find(|row| key_matches(&row.key, parts))
        {
            return Arc::clone(&row.value);
        }
        let mut rows = self.rows.write().expect("metric registry poisoned");
        // Another writer may have interned the same route while this one
        // waited for the write lock.
        if let Some(row) = rows.iter().find(|row| key_matches(&row.key, parts)) {
            return Arc::clone(&row.value);
        }
        let value = Arc::new(V::default());
        rows.push(MetricRow {
            key: parts.map(Arc::from),
            value: Arc::clone(&value),
        });
        value
    }

    /// Every row, for the admin snapshot.
    fn snapshot(&self) -> Vec<([Arc<str>; N], Arc<V>)> {
        self.rows
            .read()
            .expect("metric registry poisoned")
            .iter()
            .map(|row| (row.key.clone(), Arc::clone(&row.value)))
            .collect()
    }

    fn clear(&self) {
        self.rows.write().expect("metric registry poisoned").clear();
    }
}

/// A `u64` accumulator that saturates instead of wrapping, addressable through
/// a shared reference. The registry hands out `&V`, so the counters inside a
/// row have to be updatable without a lock while keeping the saturating
/// semantics the ledger has always had.
fn saturating_add(counter: &AtomicU64, value: u64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(value))
    });
}

/// An `f64` accumulator addressable through a shared reference. Money is the
/// only quantity here that is not an integer, and it still has to be summed
/// without taking a lock, so the bits live in an atomic and accumulate under a
/// compare-and-swap.
#[derive(Default)]
struct AtomicF64(AtomicU64);

impl AtomicF64 {
    fn add(&self, value: f64) {
        if value == 0.0 {
            return;
        }
        let _ = self
            .0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
                Some((f64::from_bits(bits) + value).to_bits())
            });
    }

    fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }
}

#[derive(Default)]
struct ProviderCalls {
    count: AtomicU64,
    latency: LatencyRing<PROVIDER_LATENCY_CAP>,
}

impl ProviderCalls {
    fn record(&self, elapsed: Duration) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.latency.record(elapsed);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TokenUsage {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) estimated: bool,
    pub(crate) input_source: TokenSource,
    pub(crate) output_source: TokenSource,
}

/// The result of a calibrated token estimate for one payload: the token count,
/// the class that produced it, the decoded character count, and the provenance
/// (`Calibrated` once enough real counts have trained the ratio, `Default` on
/// cold start). Threading this to [`ProxyStats::calibrate_from_usage`] closes
/// the loop: a later real count for the same class sharpens the next estimate.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TokenEstimate {
    pub(crate) tokens: u64,
    pub(crate) source: TokenSource,
    pub(crate) chars: u64,
    pub(crate) kind: token_estimate::ContentKind,
}

impl TokenUsage {
    /// Builds a usage record from a provider's `usage` object where present and
    /// the caller-supplied estimates where not. The estimates are produced by
    /// [`ProxyStats::estimate_tokens`], so each carries the source that made it
    /// (learned ratio or cold-start default) and is only upgraded to `Exact`
    /// when the provider actually reported that side.
    pub(crate) fn from_response(
        usage: Option<&Value>,
        estimated_input: (u64, TokenSource),
        estimated_output: (u64, TokenSource),
    ) -> Self {
        let (input, output) = usage.map(usage_token_fields).unwrap_or_default();
        Self::from_fields(input, output, estimated_input, estimated_output)
    }

    fn from_fields(
        input: Option<u64>,
        output: Option<u64>,
        estimated_input: (u64, TokenSource),
        estimated_output: (u64, TokenSource),
    ) -> Self {
        Self {
            input_tokens: input.unwrap_or(estimated_input.0),
            output_tokens: output.unwrap_or(estimated_output.0),
            input_source: input
                .map(|_| TokenSource::Exact)
                .unwrap_or(estimated_input.1),
            output_source: output
                .map(|_| TokenSource::Exact)
                .unwrap_or(estimated_output.1),
            estimated: input.is_none() || output.is_none(),
        }
    }
}

#[derive(Default)]
struct ModelUsage {
    requests: AtomicU64,
    input_tokens: AtomicU64,
    output_tokens: AtomicU64,
    total_time_ms: AtomicU64,
    estimated_requests: AtomicU64,
    cost_usd: AtomicF64,
    /// What this traffic would have cost at the configured baseline price.
    /// Local providers are the reason the proxy exists, so the counterfactual
    /// is worth as much as the bill itself.
    baseline_usd: AtomicF64,
    priced_requests: AtomicU64,
}

/// USD per one million tokens, as published by providers. Rates are per model
/// because a provider's cheap and expensive models differ by orders of
/// magnitude; a per-provider average would be meaningless.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct ModelPrice {
    pub(crate) input_per_mtok: f64,
    pub(crate) output_per_mtok: f64,
}

impl ModelPrice {
    pub(crate) fn is_zero(self) -> bool {
        self.input_per_mtok == 0.0 && self.output_per_mtok == 0.0
    }

    pub(crate) fn cost_usd(self, usage: TokenUsage) -> f64 {
        const PER_MTOK: f64 = 1_000_000.0;
        (usage.input_tokens as f64 / PER_MTOK) * self.input_per_mtok
            + (usage.output_tokens as f64 / PER_MTOK) * self.output_per_mtok
    }
}

/// The prices a single completed request is measured against: what it actually
/// cost, and what the same tokens would have cost on the baseline model.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RequestPricing {
    pub(crate) actual: ModelPrice,
    pub(crate) baseline: ModelPrice,
}

#[derive(Default)]
struct RoutingDestination {
    requests: AtomicU64,
    errors: AtomicU64,
}

impl ModelUsage {
    fn record(&self, elapsed: Duration, usage: TokenUsage, pricing: RequestPricing) {
        saturating_add(&self.requests, 1);
        // A model with no configured price contributes nothing rather than
        // silently counting as free, so a partial price table stays honest.
        if !pricing.actual.is_zero() || !pricing.baseline.is_zero() {
            self.cost_usd.add(pricing.actual.cost_usd(usage));
            self.baseline_usd.add(pricing.baseline.cost_usd(usage));
            saturating_add(&self.priced_requests, 1);
        }
        saturating_add(&self.input_tokens, usage.input_tokens);
        saturating_add(&self.output_tokens, usage.output_tokens);
        saturating_add(
            &self.total_time_ms,
            elapsed.as_millis().min(u128::from(u64::MAX)) as u64,
        );
        if usage.estimated {
            saturating_add(&self.estimated_requests, 1);
        }
    }
}

/// Where the last N token estimates came from, so the UI can state whether the
/// number reflects a learned ratio or the cold-start fallback rather than a
/// single opaque `approximate` flag. Only the two estimate sources are counted
/// (an exact provider count is never routed through here).
const TOKEN_SOURCE_CALIBRATED: usize = 0;
const TOKEN_SOURCE_DEFAULT: usize = 1;

#[derive(Default)]
pub(crate) struct ProxyStats {
    started_at: OnceLock<Instant>,
    requests_total: AtomicU64,
    remap_alias: AtomicU64,
    remap_namespace: AtomicU64,
    remap_auto: AtomicU64,
    intercept_attempts: AtomicU64,
    intercept_bypass: AtomicU64,
    intercept_local: AtomicU64,
    intercept_reject: AtomicU64,
    intercept_pass: AtomicU64,
    privacy_screens: AtomicU64,
    privacy_screen_errors: AtomicU64,
    privacy_blocks: AtomicU64,
    failovers: AtomicU64,
    breaker_skips: AtomicU64,
    rule_matches: AtomicU64,
    intercept_redirect: AtomicU64,
    intercept_fallback: AtomicU64,
    intercept_errors: AtomicU64,
    served_local: AtomicU64,
    served_external: AtomicU64,
    tokens_saved_prompt: AtomicU64,
    tokens_saved_completion: AtomicU64,
    tokens_saved_estimated: AtomicU64,
    estimate_calls: [AtomicU64; 3],
    pub(crate) token_estimator: TokenEstimator,
    latency_local: LatencyRing<LATENCY_WINDOW_CAP>,
    latency_external: LatencyRing<LATENCY_WINDOW_CAP>,
    /// Phase-attributed latency: what the proxy spent versus what it waited on.
    pub(crate) latency: LatencyMetrics,
    redirects_by_target: MetricRegistry<1, AtomicU64>,
    upstream_ttfb_by_provider: MetricRegistry<1, LatencyRing<PROVIDER_LATENCY_CAP>>,
    served_by_provider: MetricRegistry<1, ProviderCalls>,
    model_usage: MetricRegistry<2, ModelUsage>,
    routing_matrix: MetricRegistry<2, AtomicU64>,
    routing_destinations: MetricRegistry<3, RoutingDestination>,
}

pub(crate) fn proxy_stats() -> &'static ProxyStats {
    static STATS: OnceLock<ProxyStats> = OnceLock::new();
    STATS.get_or_init(ProxyStats::default)
}

impl ProxyStats {
    /// Anchor the uptime clock; the first caller wins.
    pub(crate) fn touch_start(&self) {
        let _ = self.started_at.get_or_init(Instant::now);
    }

    pub(crate) fn record_request(&self) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_remap(&self, kind: RemapKind) {
        match kind {
            RemapKind::Alias => self.remap_alias.fetch_add(1, Ordering::Relaxed),
            RemapKind::Namespace => self.remap_namespace.fetch_add(1, Ordering::Relaxed),
            RemapKind::Auto => self.remap_auto.fetch_add(1, Ordering::Relaxed),
        };
    }

    pub(crate) fn record_intercept_attempt(&self) {
        self.intercept_attempts.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_intercept_bypass(&self) {
        self.intercept_bypass.fetch_add(1, Ordering::Relaxed);
    }

    /// The triage provider answered directly. Network scope is derived from
    /// that provider rather than assuming every interceptor is local.
    pub(crate) fn record_intercept_local(
        &self,
        class: RouteClass,
        provider: &str,
        frontend: &str,
        elapsed: Duration,
        usage: TokenUsage,
    ) {
        self.intercept_local.fetch_add(1, Ordering::Relaxed);
        self.record_served(class, provider, frontend, elapsed);
        self.record_tokens_saved(
            usage.input_tokens,
            usage.output_tokens,
            usage.input_source,
            usage.output_source,
        );
    }

    pub(crate) fn record_intercept_pass(&self) {
        self.intercept_pass.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_intercept_reject(&self) {
        self.intercept_reject.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_intercept_redirect(&self, target: &str) {
        self.intercept_redirect.fetch_add(1, Ordering::Relaxed);
        self.redirects_by_target
            .row([target])
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_intercept_fallback(&self) {
        self.intercept_fallback.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_intercept_error(&self) {
        self.intercept_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Time one request spent waiting on providers, summed across every
    /// upstream call it made and attributed to the provider that served it.
    ///
    /// This is the provider's latency, kept apart from the proxy's own so a
    /// slow upstream is never mistaken for a regression in this codebase. It
    /// is deliberately per request rather than per call, so that it subtracts
    /// cleanly from the same request's time to first byte: a request that also
    /// paid for model discovery really did wait for both.
    pub(crate) fn record_upstream_wait(&self, provider: &str, elapsed: Duration) {
        self.latency.record_upstream(elapsed);
        self.upstream_ttfb_by_provider
            .row([provider])
            .record(elapsed);
    }

    fn record_served(&self, class: RouteClass, provider: &str, frontend: &str, elapsed: Duration) {
        match class {
            RouteClass::Local => {
                self.served_local.fetch_add(1, Ordering::Relaxed);
                self.latency_local.record(elapsed);
            }
            RouteClass::External => {
                self.served_external.fetch_add(1, Ordering::Relaxed);
                self.latency_external.record(elapsed);
            }
        }
        self.served_by_provider.row([provider]).record(elapsed);
        self.routing_matrix
            .row([frontend, provider])
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_model_usage(
        &self,
        provider: &str,
        model: &str,
        elapsed: Duration,
        usage: TokenUsage,
        pricing: RequestPricing,
    ) {
        self.model_usage
            .row([provider, model])
            .record(elapsed, usage, pricing);
    }

    /// Estimates `value`'s token count using the calibrated ratio for its
    /// content class.
    ///
    /// The hot-path shape is deliberate: classification scans only a bounded
    /// window of the value's serialized form (a fixed stack buffer plus an
    /// overflow probe), so neither the scan nor the overflow char count
    /// allocates or scales per-chunk beyond the body itself. The steady-state
    /// calibrated estimate therefore allocates nothing on this path.
    pub(crate) fn estimate_tokens(&self, value: &Value) -> TokenEstimate {
        let window = token_estimate::classify_window(value);
        let (tokens, source) = self
            .token_estimator
            .estimate(window.total_chars.max(1), window.kind);
        self.record_estimate_source(source);
        TokenEstimate {
            tokens,
            source,
            chars: window.total_chars,
            kind: window.kind,
        }
    }

    /// Streaming-side output estimate: characters from the decoded body, with a
    /// byte-based fallback for SSE envelopes whose structural overhead is
    /// meaningless to classify.
    pub(crate) fn estimate_output(
        &self,
        decoded_chars: u64,
        total_bytes: u64,
        is_sse: bool,
    ) -> (u64, TokenSource) {
        if is_sse {
            // SSE framing (event/data/blank-line overhead, JSON envelope
            // around each delta) is not content the client sees; a byte
            // divisor is the fair proxy for the tokens actually generated.
            let tokens = total_bytes.max(1) / 6;
            self.record_estimate_source(TokenSource::Default);
            return (tokens.max(1), TokenSource::Default);
        }
        let (tokens, source) = self
            .token_estimator
            .estimate(decoded_chars, token_estimate::ContentKind::Prose);
        self.record_estimate_source(source);
        (tokens, source)
    }

    fn record_estimate_source(&self, source: TokenSource) {
        let index = match source {
            TokenSource::Calibrated => TOKEN_SOURCE_CALIBRATED,
            // A cold-start default and an unclassifiable estimate both count as
            // the fallback bucket: neither reflects a learned ratio.
            TokenSource::Default | TokenSource::Exact => TOKEN_SOURCE_DEFAULT,
        };
        self.estimate_calls[index].fetch_add(1, Ordering::Relaxed);
    }

    /// Folds a real provider-reported count into the running estimator so
    /// future estimates for the same content class reflect how this deployment
    /// actually tokenizes. Only exact provider counts become samples; local
    /// estimates are never treated as ground truth. `None` means the provider
    /// omitted that side, so nothing is observed for it. An explicit input text
    /// overrides the classification used at estimate time (for callers whose
    /// screened payload differs from the raw request).
    pub(crate) fn calibrate_from_usage(
        &self,
        exact_input: Option<u64>,
        exact_output: Option<u64>,
        input_estimate: &TokenEstimate,
        input_text: Option<&str>,
        response_text: &str,
    ) {
        if let Some(tokens) = exact_input
            && input_estimate.chars > 0
        {
            let kind = input_text
                .map(token_estimate::classify)
                .unwrap_or(input_estimate.kind);
            self.token_estimator
                .observe_exact(input_estimate.chars, tokens, kind);
        }
        if let Some(tokens) = exact_output
            && !response_text.is_empty()
        {
            let chars = response_text.chars().count() as u64;
            let kind = token_estimate::classify(response_text);
            self.token_estimator.observe_exact(chars, tokens, kind);
        }
    }

    fn record_routing_destination(
        &self,
        frontend: &str,
        provider: &str,
        model: &str,
        failed: bool,
    ) {
        let destination = self.routing_destinations.row([frontend, provider, model]);
        saturating_add(&destination.requests, 1);
        if failed {
            saturating_add(&destination.errors, 1);
        }
    }

    fn record_routing_error(&self, frontend: &str, provider: &str, model: &str) {
        saturating_add(
            &self
                .routing_destinations
                .row([frontend, provider, model])
                .errors,
            1,
        );
    }

    /// A request routed by a deterministic rule, skipping the triage call.
    /// One confidentiality classification call against the private screener.
    pub(crate) fn record_privacy_screen(&self) {
        self.privacy_screens.fetch_add(1, Ordering::Relaxed);
    }

    /// Screening could not complete, so the request stayed private-only. This
    /// is counted separately from a classifier that genuinely answered unknown
    /// because an infrastructure failure must not look like a policy result.
    pub(crate) fn record_privacy_screen_error(&self) {
        self.privacy_screen_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// A destination was refused because the content was not eligible for it.
    pub(crate) fn record_privacy_block(&self) {
        self.privacy_blocks.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_rule_match(&self) {
        self.rule_matches.fetch_add(1, Ordering::Relaxed);
    }

    /// A request that a fallback provider rescued.
    pub(crate) fn record_failover(&self) {
        self.failovers.fetch_add(1, Ordering::Relaxed);
    }

    /// A provider skipped because its breaker was open.
    pub(crate) fn record_breaker_skip(&self) {
        self.breaker_skips.fetch_add(1, Ordering::Relaxed);
    }

    fn record_tokens_saved(
        &self,
        prompt: u64,
        completion: u64,
        prompt_source: TokenSource,
        completion_source: TokenSource,
    ) {
        self.tokens_saved_prompt
            .fetch_add(prompt, Ordering::Relaxed);
        self.tokens_saved_completion
            .fetch_add(completion, Ordering::Relaxed);
        // Only estimated sides count toward "avoided tokens we guessed at"; a
        // side backed by a real provider count is not an estimate, so it is not
        // double-reported in the estimated bucket.
        let mut estimated = 0_u64;
        if prompt_source.is_estimate() {
            estimated = estimated.saturating_add(prompt);
        }
        if completion_source.is_estimate() {
            estimated = estimated.saturating_add(completion);
        }
        if estimated > 0 {
            self.tokens_saved_estimated
                .fetch_add(estimated, Ordering::Relaxed);
        }
    }

    pub(crate) fn reset(&self) {
        for counter in [
            &self.requests_total,
            &self.remap_alias,
            &self.remap_namespace,
            &self.remap_auto,
            &self.intercept_attempts,
            &self.intercept_bypass,
            &self.intercept_local,
            &self.intercept_reject,
            &self.intercept_pass,
            &self.intercept_redirect,
            &self.intercept_fallback,
            &self.intercept_errors,
            &self.privacy_screens,
            &self.privacy_screen_errors,
            &self.privacy_blocks,
            &self.served_local,
            &self.served_external,
            &self.tokens_saved_prompt,
            &self.tokens_saved_completion,
            &self.tokens_saved_estimated,
        ] {
            counter.store(0, Ordering::Relaxed);
        }
        for counter in &self.estimate_calls {
            counter.store(0, Ordering::Relaxed);
        }
        self.token_estimator.reset();
        for window in [&self.latency_local, &self.latency_external] {
            window.clear();
        }
        self.latency.clear();
        self.redirects_by_target.clear();
        self.upstream_ttfb_by_provider.clear();
        self.served_by_provider.clear();
        self.model_usage.clear();
        self.routing_matrix.clear();
        self.routing_destinations.clear();
    }

    /// Stable snake_case keys: the admin UI treats this shape as a contract.
    pub(crate) fn snapshot_json(&self) -> Value {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        let latency = |window: &LatencyRing<LATENCY_WINDOW_CAP>| window.summary().to_json();
        let saved_prompt = load(&self.tokens_saved_prompt);
        let saved_completion = load(&self.tokens_saved_completion);
        let redirects = self
            .redirects_by_target
            .snapshot()
            .into_iter()
            .map(|([target], count)| (target.to_string(), json!(load(&count))))
            .collect::<serde_json::Map<_, _>>();
        let providers = self
            .served_by_provider
            .snapshot()
            .into_iter()
            .map(|([provider], calls)| {
                let mut summary = calls.latency.summary().to_json();
                summary["count"] = json!(load(&calls.count));
                (provider.to_string(), summary)
            })
            .collect::<serde_json::Map<_, _>>();
        let upstream_by_provider = self
            .upstream_ttfb_by_provider
            .snapshot()
            .into_iter()
            .map(|([provider], ring)| (provider.to_string(), ring.summary().to_json()))
            .collect::<serde_json::Map<_, _>>();
        let mut model_usage = self
            .model_usage
            .snapshot()
            .into_iter()
            .map(|([provider, model], usage)| {
                let input_tokens = load(&usage.input_tokens);
                let output_tokens = load(&usage.output_tokens);
                json!({
                    "provider": provider.as_ref(),
                    "model": model.as_ref(),
                    "requests": load(&usage.requests),
                    "input_tokens": input_tokens,
                    "output_tokens": output_tokens,
                    "total_tokens": input_tokens.saturating_add(output_tokens),
                    "total_time_ms": load(&usage.total_time_ms),
                    "estimated_requests": load(&usage.estimated_requests),
                    "cost_usd": usage.cost_usd.get(),
                    "baseline_usd": usage.baseline_usd.get(),
                    "priced_requests": load(&usage.priced_requests),
                })
            })
            .collect::<Vec<_>>();
        // Roll the per-model ledger up once: the dashboard wants a single
        // spend figure, and recomputing it per poll from the JSON would be
        // both slower and easy to drift from the per-model rows.
        // `Sum` for f64 uses -0.0 as its identity, so an empty ledger would
        // otherwise be published as a negative zero. Adding 0.0 normalizes it.
        let spend_usd = model_usage
            .iter()
            .filter_map(|row| row["cost_usd"].as_f64())
            .sum::<f64>()
            + 0.0;
        let baseline_usd = model_usage
            .iter()
            .filter_map(|row| row["baseline_usd"].as_f64())
            .sum::<f64>()
            + 0.0;
        let priced_requests = model_usage
            .iter()
            .filter_map(|row| row["priced_requests"].as_u64())
            .sum::<u64>();
        let total_requests = model_usage
            .iter()
            .filter_map(|row| row["requests"].as_u64())
            .sum::<u64>();
        model_usage.sort_by(|left, right| {
            left["provider"]
                .as_str()
                .cmp(&right["provider"].as_str())
                .then_with(|| left["model"].as_str().cmp(&right["model"].as_str()))
        });
        let mut matrix = serde_json::Map::new();
        for ([frontend, provider], count) in self.routing_matrix.snapshot() {
            matrix
                .entry(frontend.to_string())
                .or_insert_with(|| Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .expect("routing matrix rows are objects")
                .insert(provider.to_string(), json!(load(&count)));
        }
        let mut destinations = self
            .routing_destinations
            .snapshot()
            .into_iter()
            .map(|([frontend, provider, model], destination)| {
                json!({
                    "frontend": frontend.as_ref(),
                    "provider": provider.as_ref(),
                    "model": model.as_ref(),
                    "requests": load(&destination.requests),
                    "errors": load(&destination.errors),
                })
            })
            .collect::<Vec<_>>();
        destinations.sort_by(|left, right| {
            left["frontend"]
                .as_str()
                .cmp(&right["frontend"].as_str())
                .then_with(|| left["provider"].as_str().cmp(&right["provider"].as_str()))
                .then_with(|| left["model"].as_str().cmp(&right["model"].as_str()))
        });
        json!({
            "uptime_secs": self.started_at.get().map(Instant::elapsed).unwrap_or_default().as_secs(),
            "requests": {
                "total": load(&self.requests_total),
                "remap_alias": load(&self.remap_alias),
                "remap_namespace": load(&self.remap_namespace),
                "remap_auto": load(&self.remap_auto),
                "remapped": load(&self.remap_alias)
                    + load(&self.remap_namespace)
                    + load(&self.remap_auto),
            },
            "intercept": {
                "attempts": load(&self.intercept_attempts),
                "bypass": load(&self.intercept_bypass),
                "local": load(&self.intercept_local),
                "reject": load(&self.intercept_reject),
                "pass": load(&self.intercept_pass),
                "redirect": load(&self.intercept_redirect),
                "fallback": load(&self.intercept_fallback),
                "errors": load(&self.intercept_errors),
                "redirects": redirects,
            },
            "served": {
                "local": load(&self.served_local),
                "external": load(&self.served_external),
                "providers": providers,
                "matrix": matrix,
                "destinations": destinations,
            },
            "latency": {
                "local": latency(&self.latency_local),
                "external": latency(&self.latency_external),
                // Phase attribution. `overhead` is the number this project is
                // answerable for; `upstream` is the provider's, and the two
                // are measured per request rather than inferred by subtracting
                // one percentile from another.
                "phases": self.latency.to_json(),
                "upstream_by_provider": upstream_by_provider,
            },
            "model_usage": model_usage,
            "privacy": {
                "screens": load(&self.privacy_screens),
                "screen_errors": load(&self.privacy_screen_errors),
                "blocks": load(&self.privacy_blocks),
            },
            "resilience": {
                "failovers": load(&self.failovers),
                "breaker_skips": load(&self.breaker_skips),
                "rule_matches": load(&self.rule_matches),
            },
            "cost": {
                "spend_usd": spend_usd,
                "baseline_usd": baseline_usd,
                "saved_usd": (baseline_usd - spend_usd).max(0.0),
                "priced_requests": priced_requests,
                "unpriced_requests": total_requests.saturating_sub(priced_requests),
            },
            "tokens_saved": {
                "prompt": saved_prompt,
                "completion": saved_completion,
                "total": saved_prompt + saved_completion,
                "estimated": load(&self.tokens_saved_estimated),
                "approximate": true,
                "estimate_sources": {
                    "calibrated": load(&self.estimate_calls[TOKEN_SOURCE_CALIBRATED]),
                    "default": load(&self.estimate_calls[TOKEN_SOURCE_DEFAULT]),
                },
            },
        })
    }
}

/// Classify a provider by its base URL host. Loopback, RFC1918/link-local,
/// unique-local IPv6, and obviously-LAN hostnames count as local.
pub(crate) fn route_class_for_base_url(base_url: &str) -> RouteClass {
    if base_url_is_local(base_url) {
        RouteClass::Local
    } else {
        RouteClass::External
    }
}

fn base_url_is_local(base_url: &str) -> bool {
    let rest = base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or("")
    } else {
        authority
            .rsplit_once(':')
            .map_or(authority, |(host, port)| {
                // Only treat the suffix as a port if it is numeric; otherwise the
                // colon belonged to an unbracketed IPv6 literal.
                if port.chars().all(|c| c.is_ascii_digit()) {
                    host
                } else {
                    authority
                }
            })
    };
    if host.is_empty() {
        return false;
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(v4) => {
                v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
            }
            IpAddr::V6(v6) => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    || (v6.segments()[0] & 0xfe00) == 0xfc00 // unique local fc00::/7
                    || (v6.segments()[0] & 0xffc0) == 0xfe80 // link local fe80::/10
            }
        };
    }

    let host = host.to_ascii_lowercase();
    host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".lan")
        || host.ends_with(".internal")
        || host.ends_with(".home.arpa")
        || !host.contains('.') // bare intranet hostname
}

pub(crate) struct ResponseTracking<'a> {
    class: RouteClass,
    provider: &'a str,
    model: &'a str,
    /// The client-facing endpoint name. Always a literal at the call sites, so
    /// it is borrowed for the life of the process and never copied.
    frontend: &'static str,
    request_started: Instant,
    model_started: Instant,
    input_estimate: TokenEstimate,
    estimate_output_tokens: bool,
    pricing: RequestPricing,
}

impl<'a> ResponseTracking<'a> {
    pub(crate) fn new(
        class: RouteClass,
        provider: &'a str,
        model: &'a str,
        frontend: &'static str,
        request_started: Instant,
        model_started: Instant,
        input_estimate: TokenEstimate,
    ) -> Self {
        Self {
            class,
            provider,
            model,
            frontend,
            request_started,
            model_started,
            input_estimate,
            estimate_output_tokens: true,
            pricing: RequestPricing::default(),
        }
    }

    pub(crate) fn without_output_estimation(mut self) -> Self {
        self.estimate_output_tokens = false;
        self
    }

    pub(crate) fn with_pricing(mut self, pricing: RequestPricing) -> Self {
        self.pricing = pricing;
        self
    }
}

/// Wrap a proxied response so every response chunk is traceable and completed
/// successful calls update usage statistics after the full body has streamed.
pub(crate) fn track_response(tracking: ResponseTracking<'_>, response: Response) -> Response {
    let ResponseTracking {
        class,
        provider,
        model,
        frontend,
        request_started,
        model_started,
        input_estimate,
        estimate_output_tokens,
        pricing,
    } = tracking;
    // The tracked stream outlives this call, so the route identity has to be
    // owned. `Arc<str>` rather than `String`: the identity is interned once
    // per route and every later request clones a refcount instead of copying
    // the bytes again.
    let provider: Arc<str> = Arc::from(provider);
    let model: Arc<str> = Arc::from(model);
    let status = response.status();
    let success = status.is_success();
    proxy_stats().record_routing_destination(frontend, &provider, &model, !success);
    // Everything the proxy did before any network work: extraction, screening,
    // routing, rewriting, and token estimation. Recorded now rather than at
    // completion so the figure lands even for a response that never finishes.
    let pre_dispatch = model_started.saturating_duration_since(request_started);
    proxy_stats().latency.record_pre_dispatch(pre_dispatch);
    // Captured, not read, here: the body below is polled by the server after
    // the handler's timing scope has ended, and for a streamed response the
    // wait for the upstream's first byte has not even happened yet.
    let timing = crate::latency::current_timing();
    if !success {
        error!(
            route_class = class.as_str(),
            provider = %provider,
            model = %model,
            frontend,
            status = status.as_u16(),
            "proxied response returned an error status"
        );
        log_response_text(class, &provider, format!("HTTP {status}"));
        error_log().record(&provider, "upstream response", format!("HTTP {status}"));
    }
    let (parts, body) = response.into_parts();
    let is_sse = parts
        .headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_event_stream_content_type);
    let mut sniffer = UsageSniffer::new(is_sse);

    let mut upstream = body.into_data_stream();
    let tracked = async_stream::stream! {
        let mut logged_first_chunk = false;
        let mut completed = true;
        let mut chunk_index = 0_u64;
        while let Some(chunk) = upstream.next().await {
            match &chunk {
                Ok(bytes) => {
                    chunk_index = chunk_index.saturating_add(1);
                    trace!(
                        route_class = class.as_str(),
                        provider = %provider,
                        model = %model,
                        frontend,
                        status = status.as_u16(),
                        chunk_index,
                        body_bytes = bytes.len(),
                        body = %String::from_utf8_lossy(bytes),
                        "proxied response body"
                    );
                    if !success {
                        error!(
                            route_class = class.as_str(),
                            provider = %provider,
                            model = %model,
                            frontend,
                            status = status.as_u16(),
                            chunk_index,
                            body = %String::from_utf8_lossy(bytes),
                            "error response body"
                        );
                    } else {
                        sniffer.feed(bytes);
                    }
                    if !logged_first_chunk && !bytes.is_empty() {
                        logged_first_chunk = true;
                        // By now every upstream wait this request paid for has
                        // been attributed, including the gap between the
                        // provider's response headers and its first token.
                        let upstream_wait = timing
                            .as_ref()
                            .map(|timing| timing.upstream_wait())
                            .unwrap_or_default();
                        proxy_stats().record_upstream_wait(&provider, upstream_wait);
                        proxy_stats().latency.record_first_byte(
                            request_started.elapsed(),
                            pre_dispatch,
                            upstream_wait,
                        );
                        if traffic_log().enabled() {
                            let head = &bytes[..bytes.len().min(TRAFFIC_PREVIEW_CHARS * 2)];
                            traffic_log().record(
                                class,
                                &provider,
                                LogDirection::Response,
                                flatten_preview(String::from_utf8_lossy(head).into_owned()),
                            );
                        }
                    }
                }
                Err(stream_error) => {
                    completed = false;
                    if success {
                        proxy_stats().record_routing_error(frontend, &provider, &model);
                    }
                    error!(
                        route_class = class.as_str(),
                        provider = %provider,
                        model = %model,
                        frontend,
                        status = status.as_u16(),
                        error = %stream_error,
                        "proxied response body stream failed"
                    );
                    error_log().record(
                        &provider,
                        "response stream",
                        stream_error.to_string(),
                    );
                }
            }
            yield chunk;
        }
        if success && completed {
            let usage = sniffer.finish(&input_estimate, estimate_output_tokens);
            proxy_stats().latency.record_total(request_started.elapsed());
            proxy_stats().record_served(class, &provider, frontend, request_started.elapsed());
            proxy_stats().record_model_usage(
                &provider,
                &model,
                model_started.elapsed(),
                usage,
                pricing,
            );
            if class == RouteClass::Local {
                proxy_stats().record_tokens_saved(
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.input_source,
                    usage.output_source,
                );
            }
        }
    };
    Response::from_parts(parts, Body::from_stream(tracked))
}

const SNIFFER_TAIL_CAP: usize = 16 * 1024;

/// Keeps the tail of a response body and extracts the last complete `usage`
/// object once the body ends. Falls back to a size-derived estimate.
struct UsageSniffer {
    tail: Vec<u8>,
    total_bytes: u64,
    decoded_chars: u64,
    is_sse: bool,
}

impl UsageSniffer {
    fn new(is_sse: bool) -> Self {
        Self {
            tail: Vec::new(),
            total_bytes: 0,
            decoded_chars: 0,
            is_sse,
        }
    }

    fn feed(&mut self, chunk: &[u8]) {
        self.total_bytes += chunk.len() as u64;
        // Decoded characters drive the calibrated estimate; bytes drive the
        // crude fallback. Counting them separately is what lets a learned
        // ratio apply to the content the client actually sees rather than its
        // UTF-8 on-the-wire size.
        self.decoded_chars += String::from_utf8_lossy(chunk).chars().count() as u64;
        if chunk.len() >= SNIFFER_TAIL_CAP {
            self.tail.clear();
            self.tail
                .extend_from_slice(&chunk[chunk.len() - SNIFFER_TAIL_CAP..]);
            return;
        }
        let overflow = (self.tail.len() + chunk.len()).saturating_sub(SNIFFER_TAIL_CAP);
        if overflow > 0 {
            self.tail.drain(..overflow);
        }
        self.tail.extend_from_slice(chunk);
    }

    fn finish(self, input_estimate: &TokenEstimate, estimate_output_tokens: bool) -> TokenUsage {
        // Calibrate first: the response tail is the most representative text of
        // what the provider actually generated, and it is what we classify when
        // folding a real output count into the estimator.
        let (exact_input, exact_output) = extract_last_usage(&self.tail).unwrap_or_default();
        let tail_text = String::from_utf8_lossy(&self.tail).into_owned();
        proxy_stats().calibrate_from_usage(
            exact_input,
            exact_output,
            input_estimate,
            None,
            &tail_text,
        );
        let estimated_output = if estimate_output_tokens {
            proxy_stats().estimate_output(self.decoded_chars, self.total_bytes, self.is_sse)
        } else {
            (0, TokenSource::Exact)
        };
        TokenUsage {
            input_tokens: exact_input.unwrap_or(input_estimate.tokens),
            output_tokens: exact_output.unwrap_or(estimated_output.0),
            input_source: exact_input
                .map(|_| TokenSource::Exact)
                .unwrap_or(input_estimate.source),
            output_source: exact_output
                .map(|_| TokenSource::Exact)
                .unwrap_or(estimated_output.1),
            estimated: exact_input.is_none() || exact_output.is_none(),
        }
    }
}

/// Scan backwards for the last `"usage"` key whose value is an object with
/// token counts. Streams emit `"usage":null` chunks, so earlier matches are
/// tried until one parses.
fn extract_last_usage(haystack: &[u8]) -> Option<(Option<u64>, Option<u64>)> {
    const KEY: &[u8] = b"\"usage\"";
    let mut end = haystack.len();
    let mut input = None;
    let mut output = None;
    for _ in 0..16 {
        let Some(start) = rfind(&haystack[..end], KEY) else {
            break;
        };
        if let Some(usage) = parse_usage_value(&haystack[start + KEY.len()..]) {
            let (candidate_input, candidate_output) = usage_token_fields(&usage);
            input = input.or(candidate_input);
            output = output.or(candidate_output);
            if input.is_some() && output.is_some() {
                break;
            }
        }
        end = start;
    }
    (input.is_some() || output.is_some()).then_some((input, output))
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
}

/// Parse the JSON object following a `"usage"` key (after `:` and whitespace).
fn parse_usage_value(bytes: &[u8]) -> Option<Value> {
    let mut index = 0;
    while index < bytes.len() && matches!(bytes[index], b':' | b' ' | b'\t' | b'\r' | b'\n') {
        index += 1;
    }
    if bytes.get(index) != Some(&b'{') {
        return None;
    }
    let start = index;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, &byte) in bytes[start..].iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_slice(&bytes[start..start + offset + 1]).ok();
                }
            }
            _ => {}
        }
    }
    None
}

/// (prompt, completion) from OpenAI- or Anthropic-style usage objects.
#[cfg(test)]
fn usage_tokens(usage: &Value) -> Option<(u64, u64)> {
    let (input, output) = usage_token_fields(usage);
    if input.is_none() && output.is_none() {
        return None;
    }
    Some((input.unwrap_or(0), output.unwrap_or(0)))
}

pub(crate) fn usage_token_fields(usage: &Value) -> (Option<u64>, Option<u64>) {
    let field = |names: [&str; 2]| {
        names
            .iter()
            .find_map(|name| usage.get(name).and_then(Value::as_u64))
    };
    (
        field(["prompt_tokens", "input_tokens"]),
        field(["completion_tokens", "output_tokens"]),
    )
}

// Traffic log: a bounded, expiring feed of request/response previews for the
// admin Monitor tab. Recording is a no-op unless the admin interface enabled
// it at startup, so the proxy hot path pays nothing when admin is off.

const TRAFFIC_LOG_CAP: usize = 512;
const TRAFFIC_LOG_TTL: Duration = Duration::from_secs(10 * 60);
const TRAFFIC_PREVIEW_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LogDirection {
    Request,
    Response,
}

impl LogDirection {
    fn as_str(self) -> &'static str {
        match self {
            Self::Request => "req",
            Self::Response => "rsp",
        }
    }
}

struct LogMetadata {
    id: u64,
    epoch_ms: u64,
    recorded_at: Instant,
}

impl LogMetadata {
    fn new(id: u64) -> Self {
        Self {
            id,
            epoch_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_millis() as u64)
                .unwrap_or_default(),
            recorded_at: Instant::now(),
        }
    }
}

trait LogEntry {
    fn metadata(&self) -> &LogMetadata;
    fn to_json(&self) -> Value;
}

struct LogBuffer<T> {
    enabled: AtomicBool,
    state: Mutex<LogState<T>>,
}

struct LogState<T> {
    next_id: u64,
    entries: VecDeque<T>,
}

impl<T> Default for LogBuffer<T> {
    fn default() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            state: Mutex::new(LogState {
                next_id: 0,
                entries: VecDeque::new(),
            }),
        }
    }
}

impl<T: LogEntry> LogBuffer<T> {
    fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    fn record(&self, capacity: usize, ttl: Duration, build: impl FnOnce(LogMetadata) -> T) {
        if !self.enabled() || capacity == 0 {
            return;
        }
        let mut state = self.state.lock().expect("log buffer poisoned");
        Self::prune(&mut state.entries, ttl);
        while state.entries.len() >= capacity {
            state.entries.pop_front();
        }
        state.next_id += 1;
        let metadata = LogMetadata::new(state.next_id);
        state.entries.push_back(build(metadata));
    }

    fn clear(&self) {
        self.state
            .lock()
            .expect("log buffer poisoned")
            .entries
            .clear();
    }

    fn snapshot_after(&self, after: u64, ttl: Duration) -> Value {
        let mut state = self.state.lock().expect("log buffer poisoned");
        Self::prune(&mut state.entries, ttl);
        let latest_id = state
            .entries
            .back()
            .map(|entry| entry.metadata().id)
            .unwrap_or(after);
        let items = state
            .entries
            .iter()
            .filter(|entry| entry.metadata().id > after)
            .map(LogEntry::to_json)
            .collect::<Vec<_>>();
        json!({
            "entries": items,
            "latest_id": latest_id,
            "ttl_secs": ttl.as_secs(),
        })
    }

    fn prune(entries: &mut VecDeque<T>, ttl: Duration) {
        while entries
            .front()
            .is_some_and(|entry| entry.metadata().recorded_at.elapsed() >= ttl)
        {
            entries.pop_front();
        }
    }
}

struct TrafficLogEntry {
    metadata: LogMetadata,
    class: RouteClass,
    provider: String,
    direction: LogDirection,
    preview: String,
}

impl LogEntry for TrafficLogEntry {
    fn metadata(&self) -> &LogMetadata {
        &self.metadata
    }

    fn to_json(&self) -> Value {
        json!({
            "id": self.metadata.id,
            "ts_ms": self.metadata.epoch_ms,
            "class": self.class.as_str(),
            "provider": self.provider,
            "dir": self.direction.as_str(),
            "text": self.preview,
        })
    }
}

#[derive(Default)]
pub(crate) struct TrafficLog {
    buffer: LogBuffer<TrafficLogEntry>,
}

pub(crate) fn traffic_log() -> &'static TrafficLog {
    static LOG: OnceLock<TrafficLog> = OnceLock::new();
    LOG.get_or_init(TrafficLog::default)
}

impl TrafficLog {
    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.buffer.set_enabled(enabled);
    }

    pub(crate) fn enabled(&self) -> bool {
        self.buffer.enabled()
    }

    pub(crate) fn record(
        &self,
        class: RouteClass,
        provider: &str,
        direction: LogDirection,
        preview: String,
    ) {
        if !self.buffer.enabled() {
            return;
        }
        let provider = provider.to_string();
        self.buffer
            .record(TRAFFIC_LOG_CAP, TRAFFIC_LOG_TTL, move |metadata| {
                TrafficLogEntry {
                    metadata,
                    class,
                    provider,
                    direction,
                    preview,
                }
            });
    }

    pub(crate) fn clear(&self) {
        self.buffer.clear();
    }

    pub(crate) fn snapshot_after(&self, after: u64) -> Value {
        self.buffer.snapshot_after(after, TRAFFIC_LOG_TTL)
    }
}

// Error log: a bounded, expiring feed of upstream and interceptor failures
// for the admin Monitor tab. Errors are rarer than traffic, so entries live
// longer and the preview budget is bigger.

const ERROR_LOG_CAP: usize = 256;
const ERROR_LOG_TTL: Duration = Duration::from_secs(60 * 60);
const ERROR_PREVIEW_CHARS: usize = 300;

struct ErrorLogEntry {
    metadata: LogMetadata,
    provider: String,
    context: String,
    message: String,
}

impl LogEntry for ErrorLogEntry {
    fn metadata(&self) -> &LogMetadata {
        &self.metadata
    }

    fn to_json(&self) -> Value {
        json!({
            "id": self.metadata.id,
            "ts_ms": self.metadata.epoch_ms,
            "provider": self.provider,
            "context": self.context,
            "text": self.message,
        })
    }
}

#[derive(Default)]
pub(crate) struct ErrorLog {
    buffer: LogBuffer<ErrorLogEntry>,
}

pub(crate) fn error_log() -> &'static ErrorLog {
    static LOG: OnceLock<ErrorLog> = OnceLock::new();
    LOG.get_or_init(ErrorLog::default)
}

impl ErrorLog {
    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.buffer.set_enabled(enabled);
    }

    pub(crate) fn record(&self, provider: &str, context: &str, message: impl Into<String>) {
        if !self.buffer.enabled() {
            return;
        }
        let mut message = message
            .into()
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(ERROR_PREVIEW_CHARS + 1)
            .collect::<String>();
        if message.chars().count() > ERROR_PREVIEW_CHARS {
            message.pop();
            message.push('…');
        }
        let provider = provider.to_string();
        let context = context.to_string();
        self.buffer
            .record(ERROR_LOG_CAP, ERROR_LOG_TTL, move |metadata| {
                ErrorLogEntry {
                    metadata,
                    provider,
                    context,
                    message,
                }
            });
    }

    pub(crate) fn clear(&self) {
        self.buffer.clear();
    }

    pub(crate) fn snapshot_after(&self, after: u64) -> Value {
        self.buffer.snapshot_after(after, ERROR_LOG_TTL)
    }
}

const ROUTING_DECISION_LOG_CAP: usize = 512;
const ROUTING_DECISION_LOG_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const ROUTING_PROMPT_PREVIEW_CHARS: usize = 600;
const ROUTING_DETAIL_CHARS: usize = 400;

#[derive(Debug, Clone)]
pub(crate) struct RoutingDecisionAudit {
    pub(crate) request_id: String,
    pub(crate) mode: &'static str,
    pub(crate) protocol: &'static str,
    pub(crate) requested_model: Option<String>,
    pub(crate) prompt: String,
    pub(crate) decision: &'static str,
    pub(crate) reason: String,
    pub(crate) policy: &'static str,
    pub(crate) triage_provider: Option<String>,
    pub(crate) triage_model: Option<String>,
    pub(crate) triage_elapsed_ms: Option<u64>,
    pub(crate) target: Option<String>,
    pub(crate) target_hint: Option<String>,
    pub(crate) provider: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) route_class: Option<RouteClass>,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) error: Option<String>,
}

struct RoutingDecisionEntry {
    metadata: LogMetadata,
    audit: RoutingDecisionAudit,
}

impl LogEntry for RoutingDecisionEntry {
    fn metadata(&self) -> &LogMetadata {
        &self.metadata
    }

    fn to_json(&self) -> Value {
        let audit = &self.audit;
        json!({
            "id": self.metadata.id,
            "ts_ms": self.metadata.epoch_ms,
            "request_id": audit.request_id,
            "mode": audit.mode,
            "protocol": audit.protocol,
            "requested_model": audit.requested_model,
            "prompt": audit.prompt,
            "decision": audit.decision,
            "reason": audit.reason,
            "policy": audit.policy,
            "triage_provider": audit.triage_provider,
            "triage_model": audit.triage_model,
            "triage_elapsed_ms": audit.triage_elapsed_ms,
            "target": audit.target,
            "target_hint": audit.target_hint,
            "provider": audit.provider,
            "model": audit.model,
            "class": audit.route_class.map(RouteClass::as_str),
            "reasoning_effort": audit.reasoning_effort,
            "error": audit.error,
        })
    }
}

#[derive(Default)]
pub(crate) struct RoutingDecisionLog {
    buffer: LogBuffer<RoutingDecisionEntry>,
}

pub(crate) fn routing_decision_log() -> &'static RoutingDecisionLog {
    static LOG: OnceLock<RoutingDecisionLog> = OnceLock::new();
    LOG.get_or_init(RoutingDecisionLog::default)
}

impl RoutingDecisionLog {
    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.buffer.set_enabled(enabled);
    }

    pub(crate) fn record(&self, mut audit: RoutingDecisionAudit) {
        audit.prompt = bounded_single_line(audit.prompt, ROUTING_PROMPT_PREVIEW_CHARS);
        audit.reason = bounded_single_line(audit.reason, ROUTING_DETAIL_CHARS);
        audit.target_hint = audit
            .target_hint
            .map(|hint| bounded_single_line(hint, ROUTING_DETAIL_CHARS));
        audit.error = audit
            .error
            .map(|error| bounded_single_line(error, ROUTING_DETAIL_CHARS));

        info!(
            request_id = audit.request_id,
            mode = audit.mode,
            protocol = audit.protocol,
            requested_model = audit.requested_model.as_deref().unwrap_or(""),
            decision = audit.decision,
            reason = audit.reason,
            policy = audit.policy,
            triage_provider = audit.triage_provider.as_deref().unwrap_or(""),
            triage_model = audit.triage_model.as_deref().unwrap_or(""),
            triage_elapsed_ms = audit.triage_elapsed_ms,
            target = audit.target.as_deref().unwrap_or(""),
            target_hint = audit.target_hint.as_deref().unwrap_or(""),
            provider = audit.provider.as_deref().unwrap_or(""),
            model = audit.model.as_deref().unwrap_or(""),
            route_class = audit.route_class.map(RouteClass::as_str).unwrap_or(""),
            reasoning_effort = audit.reasoning_effort.as_deref().unwrap_or(""),
            error = audit.error.as_deref().unwrap_or(""),
            "routing decision audit"
        );

        self.buffer.record(
            ROUTING_DECISION_LOG_CAP,
            ROUTING_DECISION_LOG_TTL,
            move |metadata| RoutingDecisionEntry { metadata, audit },
        );
    }

    pub(crate) fn clear(&self) {
        self.buffer.clear();
    }

    pub(crate) fn snapshot_after(&self, after: u64) -> Value {
        self.buffer.snapshot_after(after, ROUTING_DECISION_LOG_TTL)
    }
}

fn bounded_single_line(value: String, limit: usize) -> String {
    let mut value = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(limit + 1)
        .collect::<String>();
    if value.chars().count() > limit {
        value.pop();
        value.push('…');
    }
    value
}

/// Trace the full JSON body and retain a bounded preview for the admin UI.
pub(crate) fn log_request_value(class: RouteClass, provider: &str, body: &Value) {
    trace!(
        route_class = class.as_str(),
        provider,
        direction = "request",
        body = %body,
        "LLM request body"
    );
    if !traffic_log().enabled() {
        return;
    }
    traffic_log().record(class, provider, LogDirection::Request, json_preview(body));
}

/// Trace a full buffered JSON response and retain a bounded admin preview.
pub(crate) fn log_response_value(class: RouteClass, provider: &str, body: &Value) {
    trace!(
        route_class = class.as_str(),
        provider,
        direction = "response",
        body = %body,
        "LLM response body"
    );
    if !traffic_log().enabled() {
        return;
    }
    traffic_log().record(class, provider, LogDirection::Response, json_preview(body));
}

/// Log a response that is only known as text (errors, status lines).
pub(crate) fn log_response_text(class: RouteClass, provider: &str, text: impl Into<String>) {
    let text = text.into();
    trace!(
        route_class = class.as_str(),
        provider,
        direction = "response",
        body = %text,
        "LLM response body"
    );
    if !traffic_log().enabled() {
        return;
    }
    traffic_log().record(
        class,
        provider,
        LogDirection::Response,
        flatten_preview(text),
    );
}

/// Serialize a JSON value into a bounded single-line preview without
/// allocating the full serialization of large bodies.
fn json_preview(value: &Value) -> String {
    // A little headroom over the char budget: multi-byte chars and the
    // truncation cut both happen at finish time.
    let byte_cap = TRAFFIC_PREVIEW_CHARS * 2;
    let mut writer = BoundedWriter {
        buf: Vec::with_capacity(byte_cap),
        cap: byte_cap,
    };
    // The writer aborts serialization once the cap is reached.
    let _ = serde_json::to_writer(&mut writer, value);
    flatten_preview(String::from_utf8_lossy(&writer.buf).into_owned())
}

/// Collapse to one line and truncate to the preview budget.
fn flatten_preview(text: String) -> String {
    let mut flattened = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>();
    let mut count = 0;
    for (offset, _) in flattened.char_indices() {
        count += 1;
        if count > TRAFFIC_PREVIEW_CHARS {
            flattened.truncate(offset);
            flattened.push('…');
            break;
        }
    }
    flattened
}

struct BoundedWriter {
    buf: Vec<u8>,
    cap: usize,
}

impl std::io::Write for BoundedWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let remaining = self.cap.saturating_sub(self.buf.len());
        if remaining == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "preview budget reached",
            ));
        }
        self.buf
            .extend_from_slice(&data[..remaining.min(data.len())]);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[path = "stats_tests.rs"]
mod tests;
