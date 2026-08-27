//! In-memory proxy statistics for the admin interface.
//!
//! One global [`ProxyStats`] instance collects counters, latency sample
//! windows, and approximate token savings. Everything resets on restart.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::response::Response;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tracing::{error, info, trace};

use crate::is_event_stream_content_type;
use crate::pricing::{ModelRate, PricingIndex, Spend, TokenCounters};

fn spend_json(spend: Spend) -> Value {
    json!({
        "input": spend.input,
        "output": spend.output,
        "cache_read": spend.cache_read,
        "cache_write": spend.cache_write,
        "total": spend.total(),
    })
}

/// Whether a response was produced by a local model (loopback/LAN provider or
/// the interceptor) or forwarded to an external provider.
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

const LATENCY_WINDOW_CAP: usize = 2048;
const PROVIDER_LATENCY_CAP: usize = 512;

fn nearest_rank_percentiles(mut sorted: Vec<u32>) -> (Option<u32>, Option<u32>, usize) {
    if sorted.is_empty() {
        return (None, None, 0);
    }
    sorted.sort_unstable();
    let rank = |q: f64| {
        let index = ((q * sorted.len() as f64).ceil() as usize).max(1) - 1;
        sorted[index.min(sorted.len() - 1)]
    };
    (Some(rank(0.50)), Some(rank(0.99)), sorted.len())
}

#[derive(Default)]
struct LatencySamples<const CAP: usize> {
    values: VecDeque<u32>,
}

impl<const CAP: usize> LatencySamples<CAP> {
    fn record(&mut self, elapsed: Duration) {
        let ms = elapsed.as_millis().min(u128::from(u32::MAX)) as u32;
        if self.values.len() >= CAP {
            self.values.pop_front();
        }
        self.values.push_back(ms);
    }

    fn percentiles(&self) -> (Option<u32>, Option<u32>, usize) {
        nearest_rank_percentiles(self.values.iter().copied().collect())
    }

    fn clear(&mut self) {
        self.values.clear();
    }
}

#[derive(Default)]
struct LatencyWindow {
    samples: Mutex<LatencySamples<LATENCY_WINDOW_CAP>>,
}

impl LatencyWindow {
    fn record(&self, elapsed: Duration) {
        self.samples
            .lock()
            .expect("latency window poisoned")
            .record(elapsed);
    }

    fn percentiles(&self) -> (Option<u32>, Option<u32>, usize) {
        self.samples
            .lock()
            .expect("latency window poisoned")
            .percentiles()
    }

    fn clear(&self) {
        self.samples
            .lock()
            .expect("latency window poisoned")
            .clear();
    }
}

#[derive(Default)]
struct ProviderCalls {
    count: u64,
    latency: LatencySamples<PROVIDER_LATENCY_CAP>,
}

impl ProviderCalls {
    fn record(&mut self, elapsed: Duration) {
        self.count += 1;
        self.latency.record(elapsed);
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TokenUsage {
    /// Full prompt count including anything served from or written to cache,
    /// matching OpenAI's `prompt_tokens`. The cache fields are subsets of it.
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cache_read_tokens: u64,
    pub(crate) cache_write_tokens: u64,
    pub(crate) estimated: bool,
}

/// The token fields a provider reported, each absent when it said nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ReportedUsage {
    input: Option<u64>,
    output: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
}

impl ReportedUsage {
    fn merge(self, other: Self) -> Self {
        Self {
            input: self.input.or(other.input),
            output: self.output.or(other.output),
            cache_read: self.cache_read.or(other.cache_read),
            cache_write: self.cache_write.or(other.cache_write),
        }
    }

    fn is_empty(self) -> bool {
        self.input.is_none()
            && self.output.is_none()
            && self.cache_read.is_none()
            && self.cache_write.is_none()
    }

    fn has_totals(self) -> bool {
        self.input.is_some() && self.output.is_some()
    }
}

impl TokenUsage {
    pub(crate) fn from_response(
        usage: Option<&Value>,
        estimated_input_tokens: u64,
        estimated_output_tokens: u64,
    ) -> Self {
        let reported = usage.map(usage_token_fields).unwrap_or_default();
        Self::from_fields(reported, estimated_input_tokens, estimated_output_tokens)
    }

    fn from_fields(
        reported: ReportedUsage,
        estimated_input_tokens: u64,
        estimated_output_tokens: u64,
    ) -> Self {
        let input_tokens = reported.input.unwrap_or(estimated_input_tokens);
        // Cache counts are subsets of the prompt count; a provider that
        // reports them inconsistently must not inflate the billable split.
        let cache_read_tokens = reported.cache_read.unwrap_or(0).min(input_tokens);
        let cache_write_tokens = reported
            .cache_write
            .unwrap_or(0)
            .min(input_tokens - cache_read_tokens);
        Self {
            input_tokens,
            output_tokens: reported.output.unwrap_or(estimated_output_tokens),
            cache_read_tokens,
            cache_write_tokens,
            estimated: !reported.has_totals(),
        }
    }

    pub(crate) fn counters(self) -> crate::pricing::TokenCounters {
        crate::pricing::TokenCounters {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_read_tokens: self.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens,
        }
    }
}

#[derive(Default)]
struct ModelUsage {
    requests: u64,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    total_time_ms: u64,
    estimated_requests: u64,
}

#[derive(Default)]
struct RoutingDestination {
    requests: u64,
    errors: u64,
}

impl ModelUsage {
    fn record(&mut self, elapsed: Duration, usage: TokenUsage) {
        self.requests = self.requests.saturating_add(1);
        self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(usage.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(usage.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(usage.cache_write_tokens);
        self.total_time_ms = self
            .total_time_ms
            .saturating_add(elapsed.as_millis().min(u128::from(u64::MAX)) as u64);
        if usage.estimated {
            self.estimated_requests = self.estimated_requests.saturating_add(1);
        }
    }
}

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
    intercept_redirect: AtomicU64,
    intercept_fallback: AtomicU64,
    intercept_errors: AtomicU64,
    served_local: AtomicU64,
    served_external: AtomicU64,
    tokens_saved_prompt: AtomicU64,
    tokens_saved_completion: AtomicU64,
    tokens_saved_estimated: AtomicU64,
    latency_local: LatencyWindow,
    latency_external: LatencyWindow,
    redirects_by_target: Mutex<HashMap<String, u64>>,
    served_by_provider: Mutex<HashMap<String, ProviderCalls>>,
    model_usage: Mutex<HashMap<(String, String), ModelUsage>>,
    routing_matrix: Mutex<HashMap<(String, String), u64>>,
    routing_destinations: Mutex<HashMap<(String, String, String), RoutingDestination>>,
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

    /// The interceptor answered locally: counts as a local serve and records
    /// the local completion latency plus exact usage when present.
    pub(crate) fn record_intercept_local(
        &self,
        provider: &str,
        frontend: &str,
        elapsed: Duration,
        usage: TokenUsage,
    ) {
        self.intercept_local.fetch_add(1, Ordering::Relaxed);
        self.record_served(RouteClass::Local, provider, frontend, elapsed);
        self.record_tokens_saved(usage.input_tokens, usage.output_tokens, usage.estimated);
    }

    pub(crate) fn record_intercept_pass(&self) {
        self.intercept_pass.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_intercept_reject(&self) {
        self.intercept_reject.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_intercept_redirect(&self, target: &str) {
        self.intercept_redirect.fetch_add(1, Ordering::Relaxed);
        let mut redirects = self
            .redirects_by_target
            .lock()
            .expect("redirect stats poisoned");
        *redirects.entry(target.to_string()).or_default() += 1;
    }

    pub(crate) fn record_intercept_fallback(&self) {
        self.intercept_fallback.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_intercept_error(&self) {
        self.intercept_errors.fetch_add(1, Ordering::Relaxed);
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
        self.served_by_provider
            .lock()
            .expect("provider stats poisoned")
            .entry(provider.to_string())
            .or_default()
            .record(elapsed);
        *self
            .routing_matrix
            .lock()
            .expect("routing matrix stats poisoned")
            .entry((frontend.to_string(), provider.to_string()))
            .or_default() += 1;
    }

    /// Fold one completed call into the in-memory counters and the durable
    /// ledger together, so "right now" and "this month" can never disagree
    /// about what was recorded.
    pub(crate) fn record_model_usage(
        &self,
        project: Option<&str>,
        provider: &str,
        model: &str,
        elapsed: Duration,
        usage: TokenUsage,
    ) {
        self.model_usage
            .lock()
            .expect("model usage stats poisoned")
            .entry((provider.to_string(), model.to_string()))
            .or_default()
            .record(elapsed, usage);
        crate::usage_store::usage_store().record(project, provider, model, elapsed, usage);
    }

    fn record_routing_destination(
        &self,
        frontend: &str,
        provider: &str,
        model: &str,
        failed: bool,
    ) {
        let mut destinations = self
            .routing_destinations
            .lock()
            .expect("routing destination stats poisoned");
        let destination = destinations
            .entry((
                frontend.to_string(),
                provider.to_string(),
                model.to_string(),
            ))
            .or_default();
        destination.requests = destination.requests.saturating_add(1);
        if failed {
            destination.errors = destination.errors.saturating_add(1);
        }
    }

    fn record_routing_error(&self, frontend: &str, provider: &str, model: &str) {
        let mut destinations = self
            .routing_destinations
            .lock()
            .expect("routing destination stats poisoned");
        let destination = destinations
            .entry((
                frontend.to_string(),
                provider.to_string(),
                model.to_string(),
            ))
            .or_default();
        destination.errors = destination.errors.saturating_add(1);
    }

    fn record_tokens_saved(&self, prompt: u64, completion: u64, estimated: bool) {
        self.tokens_saved_prompt
            .fetch_add(prompt, Ordering::Relaxed);
        self.tokens_saved_completion
            .fetch_add(completion, Ordering::Relaxed);
        if estimated {
            self.tokens_saved_estimated
                .fetch_add(prompt + completion, Ordering::Relaxed);
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
            &self.served_local,
            &self.served_external,
            &self.tokens_saved_prompt,
            &self.tokens_saved_completion,
            &self.tokens_saved_estimated,
        ] {
            counter.store(0, Ordering::Relaxed);
        }
        for window in [&self.latency_local, &self.latency_external] {
            window.clear();
        }
        self.redirects_by_target
            .lock()
            .expect("redirect stats poisoned")
            .clear();
        self.served_by_provider
            .lock()
            .expect("provider stats poisoned")
            .clear();
        self.model_usage
            .lock()
            .expect("model usage stats poisoned")
            .clear();
        self.routing_matrix
            .lock()
            .expect("routing matrix stats poisoned")
            .clear();
        self.routing_destinations
            .lock()
            .expect("routing destination stats poisoned")
            .clear();
    }

    /// Stable snake_case keys: the admin UI treats this shape as a contract.
    ///
    /// Pricing is passed in rather than read from a global so a report prices
    /// exactly the configuration snapshot it was generated under.
    pub(crate) fn snapshot_json(&self, pricing: &PricingIndex) -> Value {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        let latency = |window: &LatencyWindow| {
            let (p50, p99, samples) = window.percentiles();
            json!({ "p50_ms": p50, "p99_ms": p99, "samples": samples })
        };
        let saved_prompt = load(&self.tokens_saved_prompt);
        let saved_completion = load(&self.tokens_saved_completion);
        let redirects = self
            .redirects_by_target
            .lock()
            .expect("redirect stats poisoned")
            .iter()
            .map(|(target, count)| (target.clone(), json!(count)))
            .collect::<serde_json::Map<_, _>>();
        let providers = self
            .served_by_provider
            .lock()
            .expect("provider stats poisoned")
            .iter()
            .map(|(provider, calls)| {
                let (p50, p99, samples) = calls.latency.percentiles();
                (
                    provider.clone(),
                    json!({
                        "count": calls.count,
                        "p50_ms": p50,
                        "p99_ms": p99,
                        "samples": samples,
                    }),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        let mut spend_total = Spend::default();
        let mut priced_requests = 0_u64;
        let mut priced_estimated_requests = 0_u64;
        let mut subscription_requests = 0_u64;
        let mut unpriced_requests = 0_u64;
        let mut model_usage = self
            .model_usage
            .lock()
            .expect("model usage stats poisoned")
            .iter()
            .map(|((provider, model), usage)| {
                let counters = TokenCounters {
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    cache_read_tokens: usage.cache_read_tokens,
                    cache_write_tokens: usage.cache_write_tokens,
                };
                let rate = pricing.rate(provider, model);
                let spend = match rate {
                    ModelRate::Priced(price) => {
                        let spend = crate::pricing::spend_for(price, counters);
                        spend_total.add(spend);
                        priced_requests = priced_requests.saturating_add(usage.requests);
                        priced_estimated_requests =
                            priced_estimated_requests.saturating_add(usage.estimated_requests);
                        Some(spend)
                    }
                    ModelRate::Subscription => {
                        subscription_requests =
                            subscription_requests.saturating_add(usage.requests);
                        None
                    }
                    ModelRate::Unpriced => {
                        unpriced_requests = unpriced_requests.saturating_add(usage.requests);
                        None
                    }
                };
                json!({
                    "provider": provider,
                    "model": model,
                    "requests": usage.requests,
                    "input_tokens": usage.input_tokens,
                    "output_tokens": usage.output_tokens,
                    "cache_read_tokens": usage.cache_read_tokens,
                    "cache_write_tokens": usage.cache_write_tokens,
                    "uncached_input_tokens": counters.uncached_input_tokens(),
                    "total_tokens": usage.input_tokens.saturating_add(usage.output_tokens),
                    "total_time_ms": usage.total_time_ms,
                    "estimated_requests": usage.estimated_requests,
                    // "priced" | "subscription" | "unpriced". A subscription
                    // row bills against a plan, so its token volume is real
                    // but a dollar figure for it would be a fiction.
                    "pricing": rate.as_str(),
                    "cost": spend.map(spend_json),
                })
            })
            .collect::<Vec<_>>();
        model_usage.sort_by(|left, right| {
            left["provider"]
                .as_str()
                .cmp(&right["provider"].as_str())
                .then_with(|| left["model"].as_str().cmp(&right["model"].as_str()))
        });
        let mut matrix = serde_json::Map::new();
        for ((frontend, provider), count) in self
            .routing_matrix
            .lock()
            .expect("routing matrix stats poisoned")
            .iter()
        {
            matrix
                .entry(frontend.clone())
                .or_insert_with(|| Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .expect("routing matrix rows are objects")
                .insert(provider.clone(), json!(count));
        }
        let mut destinations = self
            .routing_destinations
            .lock()
            .expect("routing destination stats poisoned")
            .iter()
            .map(|((frontend, provider, model), destination)| {
                json!({
                    "frontend": frontend,
                    "provider": provider,
                    "model": model,
                    "requests": destination.requests,
                    "errors": destination.errors,
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
            },
            "model_usage": model_usage,
            "spend": {
                "configured": !pricing.is_empty(),
                "currency": pricing.currency(),
                "total": spend_total.total(),
                "breakdown": spend_json(spend_total),
                "priced_requests": priced_requests,
                // Spend derived from estimated token counts is an
                // approximation; the UI must say so rather than present a
                // precise-looking figure.
                "estimated_requests": priced_estimated_requests,
                "subscription_requests": subscription_requests,
                "unpriced_requests": unpriced_requests,
            },
            "tokens_saved": {
                "prompt": saved_prompt,
                "completion": saved_completion,
                "total": saved_prompt + saved_completion,
                "estimated": load(&self.tokens_saved_estimated),
                "approximate": true,
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
    frontend: &'a str,
    project: Option<&'a str>,
    request_started: Instant,
    model_started: Instant,
    estimated_input_tokens: u64,
    estimate_output_tokens: bool,
}

impl<'a> ResponseTracking<'a> {
    pub(crate) fn new(
        class: RouteClass,
        provider: &'a str,
        model: &'a str,
        frontend: &'a str,
        request_started: Instant,
        model_started: Instant,
        estimated_input_tokens: u64,
    ) -> Self {
        Self {
            class,
            provider,
            model,
            frontend,
            project: None,
            request_started,
            model_started,
            estimated_input_tokens,
            estimate_output_tokens: true,
        }
    }

    /// Attribute this call to a project so durable spend can be broken down
    /// by repository rather than only in aggregate.
    pub(crate) fn for_project(mut self, project: Option<&'a str>) -> Self {
        self.project = project;
        self
    }

    pub(crate) fn without_output_estimation(mut self) -> Self {
        self.estimate_output_tokens = false;
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
        project,
        request_started,
        model_started,
        estimated_input_tokens,
        estimate_output_tokens,
    } = tracking;
    let provider = provider.to_string();
    let model = model.to_string();
    let frontend = frontend.to_string();
    let project = project.map(str::to_string);
    let status = response.status();
    let success = status.is_success();
    proxy_stats().record_routing_destination(&frontend, &provider, &model, !success);
    if !success {
        error!(
            route_class = class.as_str(),
            provider,
            model,
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
                        provider,
                        model,
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
                            provider,
                            model,
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
                        proxy_stats().record_routing_error(&frontend, &provider, &model);
                    }
                    error!(
                        route_class = class.as_str(),
                        provider,
                        model,
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
            let usage = sniffer.finish(estimated_input_tokens, estimate_output_tokens);
            proxy_stats().record_served(class, &provider, &frontend, request_started.elapsed());
            proxy_stats().record_model_usage(
                project.as_deref(),
                &provider,
                &model,
                model_started.elapsed(),
                usage,
            );
            if class == RouteClass::Local {
                proxy_stats().record_tokens_saved(
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.estimated,
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
    is_sse: bool,
}

impl UsageSniffer {
    fn new(is_sse: bool) -> Self {
        Self {
            tail: Vec::new(),
            total_bytes: 0,
            is_sse,
        }
    }

    fn feed(&mut self, chunk: &[u8]) {
        self.total_bytes += chunk.len() as u64;
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

    fn finish(self, estimated_input_tokens: u64, estimate_output_tokens: bool) -> TokenUsage {
        let divisor = if self.is_sse { 6 } else { 4 };
        let estimated_output_tokens = if estimate_output_tokens {
            self.total_bytes / divisor
        } else {
            0
        };
        let reported = extract_last_usage(&self.tail).unwrap_or_default();
        TokenUsage::from_fields(reported, estimated_input_tokens, estimated_output_tokens)
    }
}

/// Scan backwards for the last `"usage"` key whose value is an object with
/// token counts. Streams emit `"usage":null` chunks, so earlier matches are
/// tried until one parses.
fn extract_last_usage(haystack: &[u8]) -> Option<ReportedUsage> {
    const KEY: &[u8] = b"\"usage\"";
    let mut end = haystack.len();
    let mut reported = ReportedUsage::default();
    for _ in 0..16 {
        let Some(start) = rfind(&haystack[..end], KEY) else {
            break;
        };
        if let Some(usage) = parse_usage_value(&haystack[start + KEY.len()..]) {
            reported = reported.merge(usage_token_fields(&usage));
            if reported.has_totals() {
                break;
            }
        }
        end = start;
    }
    (!reported.is_empty()).then_some(reported)
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
    let reported = usage_token_fields(usage);
    if reported.is_empty() {
        return None;
    }
    Some((reported.input.unwrap_or(0), reported.output.unwrap_or(0)))
}

/// Normalize the OpenAI and Anthropic usage shapes onto one set of fields.
///
/// The two disagree about what the prompt count contains: OpenAI's
/// `prompt_tokens` already includes cache reads, Anthropic's `input_tokens`
/// excludes both cache reads and cache creation. Cache-aware spend needs one
/// meaning, so Anthropic's parts are summed back into the prompt total.
fn usage_token_fields(usage: &Value) -> ReportedUsage {
    let field = |names: &[&str]| {
        names
            .iter()
            .find_map(|name| usage.get(*name).and_then(Value::as_u64))
    };
    let details = |parent: &str, names: &[&str]| {
        usage.get(parent).map(|details| {
            names
                .iter()
                .find_map(|name| details.get(*name).and_then(Value::as_u64))
        })?
    };
    let cache_read = details(
        "prompt_tokens_details",
        &["cached_tokens", "cache_read_tokens"],
    )
    .or_else(|| {
        details(
            "input_tokens_details",
            &["cached_tokens", "cache_read_tokens"],
        )
    })
    .or_else(|| field(&["cache_read_input_tokens"]));
    let cache_write = details(
        "prompt_tokens_details",
        &["cache_creation_tokens", "cache_write_tokens"],
    )
    .or_else(|| field(&["cache_creation_input_tokens"]));

    let openai_prompt = field(&["prompt_tokens"]);
    let anthropic_input = field(&["input_tokens"]);
    let input = match (openai_prompt, anthropic_input) {
        (Some(prompt), _) => Some(prompt),
        (None, Some(input)) => Some(
            input
                .saturating_add(cache_read.unwrap_or(0))
                .saturating_add(cache_write.unwrap_or(0)),
        ),
        (None, None) => None,
    };
    ReportedUsage {
        input,
        output: field(&["completion_tokens", "output_tokens"]),
        cache_read,
        cache_write,
    }
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
