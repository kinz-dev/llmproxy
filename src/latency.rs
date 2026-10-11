//! Allocation-free, lock-free latency instrumentation for the request path.
//!
//! The proxy's own cost has to be measured in microseconds, not milliseconds:
//! on a local provider the round trip is dominated by the model, and on a
//! remote one the only number this project controls is the slice of time spent
//! inside this process. Two things follow from that.
//!
//! First, recording a sample must not allocate, lock, or contend. A sample is
//! one relaxed `fetch_add` to claim a ring slot and one relaxed store into it,
//! so a request pays a few nanoseconds and never blocks another request. The
//! previous `Mutex<VecDeque<u32>>` window took a global lock per sample, which
//! is exactly the kind of coupling that shows up as a tail under concurrency.
//!
//! Second, a phase has to be measured, not inferred. Subtracting a p50 from a
//! p50 does not yield the p50 of the difference, so [`RequestTiming`] carries
//! the per-request phase boundaries and the proxy's own overhead is computed
//! for each request before it is ever summarized.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures_util::{Stream, StreamExt};
use serde_json::{Value, json};

/// Samples retained per window. Two thousand requests is long enough for a
/// stable p99 and short enough that the window still reflects recent behavior.
pub(crate) const LATENCY_WINDOW_CAP: usize = 2048;

/// Samples retained per provider. Providers are many and individually less
/// trafficked, so they keep a shorter window.
pub(crate) const PROVIDER_LATENCY_CAP: usize = 512;

/// Duration as microseconds, clamped into the ring's storage width.
///
/// `u32` microseconds saturate at roughly 71 minutes, far beyond any request
/// this proxy will not already have timed out, and saturating keeps one absurd
/// outlier from wrapping into a small value and quietly flattering the tail.
fn saturating_micros(elapsed: Duration) -> u32 {
    elapsed.as_micros().min(u128::from(u32::MAX - 1)) as u32
}

/// A lock-free ring of the most recent `CAP` samples, in microseconds.
///
/// Slots store `micros + 1` so that zero unambiguously means "never written".
/// That removes the only real race a reader could hit: a writer that has
/// claimed a slot but not yet stored into it is simply not counted, instead of
/// contributing a phantom zero-microsecond sample to the percentiles.
pub(crate) struct LatencyRing<const CAP: usize> {
    slots: [AtomicU32; CAP],
    /// Lifetime sample count, and the source of the write cursor.
    writes: AtomicU64,
}

impl<const CAP: usize> Default for LatencyRing<CAP> {
    fn default() -> Self {
        Self {
            slots: std::array::from_fn(|_| AtomicU32::new(0)),
            writes: AtomicU64::new(0),
        }
    }
}

impl<const CAP: usize> LatencyRing<CAP> {
    /// Record one sample. Allocation-free, lock-free, and wait-free.
    pub(crate) fn record(&self, elapsed: Duration) {
        self.record_micros(saturating_micros(elapsed));
    }

    pub(crate) fn record_micros(&self, micros: u32) {
        let index = (self.writes.fetch_add(1, Ordering::Relaxed) % CAP as u64) as usize;
        // Stored biased by one; see the type comment.
        self.slots[index].store(micros.saturating_add(1), Ordering::Relaxed);
    }

    /// Lifetime number of samples recorded, which outlives the window itself.
    pub(crate) fn count(&self) -> u64 {
        self.writes.load(Ordering::Relaxed)
    }

    /// Summarize the window. This allocates and sorts, but it runs only when
    /// the admin interface polls, never on the request path.
    pub(crate) fn summary(&self) -> LatencySummary {
        let mut samples: Vec<u32> = self
            .slots
            .iter()
            .filter_map(|slot| slot.load(Ordering::Relaxed).checked_sub(1))
            .collect();
        samples.sort_unstable();
        LatencySummary::from_sorted(&samples, self.count())
    }

    pub(crate) fn clear(&self) {
        for slot in &self.slots {
            slot.store(0, Ordering::Relaxed);
        }
        self.writes.store(0, Ordering::Relaxed);
    }
}

/// Percentiles over one window, in microseconds.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LatencySummary {
    pub(crate) p50_us: Option<u32>,
    pub(crate) p90_us: Option<u32>,
    pub(crate) p99_us: Option<u32>,
    pub(crate) max_us: Option<u32>,
    /// Samples currently in the window.
    pub(crate) samples: usize,
    /// Samples recorded since start, which the window has long since evicted.
    pub(crate) total: u64,
}

impl LatencySummary {
    fn from_sorted(sorted: &[u32], total: u64) -> Self {
        if sorted.is_empty() {
            return Self {
                total,
                ..Self::default()
            };
        }
        // Nearest-rank: the smallest value at or above the quantile position,
        // which is the definition the previous window used and the one that
        // does not interpolate a latency nobody actually observed.
        let rank = |quantile: f64| {
            let index = ((quantile * sorted.len() as f64).ceil() as usize).max(1) - 1;
            sorted[index.min(sorted.len() - 1)]
        };
        Self {
            p50_us: Some(rank(0.50)),
            p90_us: Some(rank(0.90)),
            p99_us: Some(rank(0.99)),
            max_us: sorted.last().copied(),
            samples: sorted.len(),
            total,
        }
    }

    /// Microseconds rounded to whole milliseconds, for the millisecond-scale
    /// fields the admin interface has always published.
    fn millis(value: Option<u32>) -> Option<u32> {
        value.map(|micros| micros.div_ceil(1_000))
    }

    /// The historical shape: millisecond percentiles plus a window size. Kept
    /// so existing dashboard rows keep rendering, with the microsecond figures
    /// added alongside rather than replacing them.
    pub(crate) fn to_json(self) -> Value {
        json!({
            "p50_ms": Self::millis(self.p50_us),
            "p99_ms": Self::millis(self.p99_us),
            "samples": self.samples,
            "p50_us": self.p50_us,
            "p90_us": self.p90_us,
            "p99_us": self.p99_us,
            "max_us": self.max_us,
            "total": self.total,
        })
    }
}

/// The phases of one request's latency.
///
/// `pre_dispatch` and `relay` are the two spans the proxy is answerable for;
/// `upstream` is the provider's. Keeping them separate is what makes a
/// regression attributable: a rise in `overhead` is this codebase's problem,
/// and a rise in `upstream` is not.
#[derive(Default)]
pub(crate) struct LatencyMetrics {
    /// Request entry to the moment the first upstream request is dispatched:
    /// extraction, screening, routing, rewriting, and token estimation.
    pre_dispatch: LatencyRing<LATENCY_WINDOW_CAP>,
    /// Time awaiting upstream, summed across failover attempts. For a stream
    /// this ends at the response headers; for a buffered call it ends when the
    /// body has been read, because until then there is nothing to render.
    upstream: LatencyRing<LATENCY_WINDOW_CAP>,
    /// Request entry to the first byte handed back to the caller.
    client_ttfb: LatencyRing<LATENCY_WINDOW_CAP>,
    /// First byte minus the upstream wait and the pre-dispatch work: the
    /// proxy's own relay and protocol-translation cost, computed per request.
    relay: LatencyRing<LATENCY_WINDOW_CAP>,
    /// `pre_dispatch + relay`. The headline number: latency this process added.
    overhead: LatencyRing<LATENCY_WINDOW_CAP>,
    /// Request entry to the last byte of the response body.
    total: LatencyRing<LATENCY_WINDOW_CAP>,
}

impl LatencyMetrics {
    pub(crate) fn record_pre_dispatch(&self, elapsed: Duration) {
        self.pre_dispatch.record(elapsed);
    }

    pub(crate) fn record_upstream(&self, elapsed: Duration) {
        self.upstream.record(elapsed);
    }

    pub(crate) fn record_total(&self, elapsed: Duration) {
        self.total.record(elapsed);
    }

    /// Record the first-byte phases together, so `client_ttfb`, `relay`, and
    /// `overhead` always describe the same request.
    ///
    /// `upstream_wait` is what this request actually spent waiting on
    /// providers. Saturating subtraction is deliberate: the spans are sampled
    /// from different points in the pipeline, so a few microseconds of overlap
    /// must read as zero overhead rather than wrap into an enormous one.
    pub(crate) fn record_first_byte(
        &self,
        client_ttfb: Duration,
        pre_dispatch: Duration,
        upstream_wait: Duration,
    ) {
        self.client_ttfb.record(client_ttfb);
        let relay = client_ttfb
            .saturating_sub(pre_dispatch)
            .saturating_sub(upstream_wait);
        self.relay.record(relay);
        self.overhead.record(pre_dispatch.saturating_add(relay));
    }

    pub(crate) fn to_json(&self) -> Value {
        json!({
            "pre_dispatch": self.pre_dispatch.summary().to_json(),
            "upstream": self.upstream.summary().to_json(),
            "client_ttfb": self.client_ttfb.summary().to_json(),
            "relay": self.relay.summary().to_json(),
            "overhead": self.overhead.summary().to_json(),
            "total": self.total.summary().to_json(),
        })
    }

    pub(crate) fn clear(&self) {
        self.pre_dispatch.clear();
        self.upstream.clear();
        self.client_ttfb.clear();
        self.relay.clear();
        self.overhead.clear();
        self.total.clear();
    }
}

/// Per-request phase boundaries.
///
/// The upstream wait is discovered deep in the send path and consumed at the
/// response boundary. Threading it through every intermediate signature would
/// add a parameter to a dozen functions that have no interest in it, so it
/// travels in a task-local instead.
///
/// It is shared and atomic rather than task-owned because the response body is
/// streamed after the handler returns, outside the task-local scope: a stream
/// wrapper has to hold a handle to the timing, not look one up.
#[derive(Default)]
pub(crate) struct RequestTiming {
    /// Total microseconds spent awaiting upstream, accumulated across failover
    /// attempts and across both halves of a streamed response, so a retried or
    /// slow-to-first-token request does not bill the provider's time to the
    /// proxy's own overhead.
    upstream_micros: AtomicU32,
    /// Time spent waiting for a sharing execution slot. It is neither the
    /// proxy's work nor the provider's, so it is excluded from both.
    queue_micros: AtomicU32,
}

impl RequestTiming {
    fn add_upstream(&self, elapsed: Duration) {
        let micros = saturating_micros(elapsed);
        let _ =
            self.upstream_micros
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                    Some(current.saturating_add(micros))
                });
    }

    pub(crate) fn upstream_wait(&self) -> Duration {
        Duration::from_micros(u64::from(self.upstream_micros.load(Ordering::Relaxed)))
    }

    pub(crate) fn queue_wait(&self) -> Duration {
        Duration::from_micros(u64::from(self.queue_micros.load(Ordering::Relaxed)))
    }
}

tokio::task_local! {
    static REQUEST_TIMING: Arc<RequestTiming>;
}

/// Run `future` with a fresh timing context. Every served request passes
/// through the tracing middleware, so this is installed exactly once per
/// request and phases recorded anywhere beneath it are attributed correctly.
pub(crate) fn with_request_timing<F: Future>(future: F) -> impl Future<Output = F::Output> {
    REQUEST_TIMING.scope(Arc::new(RequestTiming::default()), future)
}

/// A handle to the current request's timing, for code that outlives the
/// handler's scope. `None` outside a request context.
pub(crate) fn current_timing() -> Option<Arc<RequestTiming>> {
    REQUEST_TIMING.try_with(Arc::clone).ok()
}

/// Add an upstream wait to the current request. Outside a request context
/// (tests calling the pipeline directly, background refreshes) this is a
/// no-op rather than an error: unattributed work simply is not attributed.
pub(crate) fn add_upstream_wait(elapsed: Duration) {
    let _ = REQUEST_TIMING.try_with(|timing| timing.add_upstream(elapsed));
}

/// Record the current request's wait for a sharing execution slot.
pub(crate) fn add_queue_wait(elapsed: Duration) {
    let _ = REQUEST_TIMING.try_with(|timing| {
        timing
            .queue_micros
            .fetch_add(saturating_micros(elapsed), Ordering::Relaxed)
    });
}

/// The upstream wait accumulated so far for the current request. Production
/// code holds a [`RequestTiming`] handle instead, because it reads the total
/// after the task-local scope has ended; this is the ambient-lookup form the
/// attribution tests assert against.
#[cfg(test)]
pub(crate) fn upstream_wait() -> Duration {
    REQUEST_TIMING
        .try_with(|timing| timing.upstream_wait())
        .unwrap_or_default()
}

/// Time an upstream await and attribute it to the current request.
pub(crate) async fn timed_upstream<F: Future>(future: F) -> F::Output {
    let started = Instant::now();
    let output = future.await;
    add_upstream_wait(started.elapsed());
    output
}

/// Attribute the wait for an upstream body's first byte to the provider.
///
/// For a streamed response the headers arrive long before the first token: on
/// a real provider that gap *is* time to first token, and it is the model's,
/// not the proxy's. Without this the gap would fall out of the subtraction as
/// proxy overhead and swamp the figure it is there to report.
pub(crate) fn timed_body_stream<S>(stream: S) -> impl Stream<Item = S::Item>
where
    S: Stream,
{
    // Captured here rather than looked up later: the wrapper is built while
    // the handler still holds the request's timing context, but it is polled
    // after that scope has ended.
    let timing = current_timing();
    let started = Instant::now();
    let mut first = true;
    stream.inspect(move |_| {
        if first {
            first = false;
            if let Some(timing) = &timing {
                timing.add_upstream(started.elapsed());
            }
        }
    })
}

#[cfg(test)]
#[path = "latency_tests.rs"]
mod tests;
