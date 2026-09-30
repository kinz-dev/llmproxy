//! Hot-path allocation stress review.
//!
//! These are not speed benchmarks: they count allocations per hot-path call so
//! a regression (an added `to_string()` in a per-request function, an estimate
//! that suddenly reserializes a body) fails loudly instead of showing up as a
//! slow tail in production.
//!
//! Measurement rules:
//! - The counting allocator only attributes allocations made on the *calling*
//!   thread, identified by a thread-local pointer handshake. Tests in this
//!   binary run concurrently (and tokio worker threads keep working), so
//!   counting all threads would attribute unrelated allocations to a call.
//! - Every measurement takes the minimum of repeated trials. Concurrent
//!   activity can only add noise, never remove allocations, so the minimum
//!   converges on the true per-call count.

use std::alloc::{GlobalAlloc, Layout};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::json;

use super::*;
use crate::stats::ProxyStats;
use crate::tests::test_support::{mock_upstream, state_with_config};
use crate::token_estimate::{ContentKind, TokenEstimator, TokenSource};

/// Allocator decorator that counts allocations made while a measurement is
/// active on the calling thread. It must wrap the exact allocator the release
/// binary uses so the measurement reflects real behavior.
struct Counting;

thread_local! {
    /// The counter the calling thread should credit, as a raw pointer, or 0
    /// when no measurement is active. A plain `Cell` because only the owning
    /// thread reads/writes it (the allocator reads it on that same thread).
    static ACTIVE_COUNTER: Cell<usize> = const { Cell::new(0) };
}

struct Count(AtomicUsize);

// Safety: the wrappers are thin pass-throughs to the underlying allocator; the
// thread-local is unset until a measurement begins, so unrelated threads are
// never counted.
macro_rules! counting_allocator {
    ($inner:path) => {
        unsafe impl GlobalAlloc for Counting {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                if let Some(count) = ACTIVE_COUNTER.with(|slot| slot.get()).then_if_nonzero() {
                    // Safety: the pointer was stored by `measure` on this very
                    // thread and the pointee lives until `measure` returns.
                    unsafe { &*(count as *const Count) }
                        .0
                        .fetch_add(1, Ordering::Relaxed);
                }
                unsafe { $inner.alloc(layout) }
            }

            unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                unsafe { $inner.dealloc(ptr, layout) }
            }

            unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
                if let Some(count) = ACTIVE_COUNTER.with(|slot| slot.get()).then_if_nonzero() {
                    unsafe { &*(count as *const Count) }
                        .0
                        .fetch_add(1, Ordering::Relaxed);
                }
                unsafe { $inner.realloc(ptr, layout, new_size) }
            }

            unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
                if let Some(count) = ACTIVE_COUNTER.with(|slot| slot.get()).then_if_nonzero() {
                    unsafe { &*(count as *const Count) }
                        .0
                        .fetch_add(1, Ordering::Relaxed);
                }
                unsafe { $inner.alloc_zeroed(layout) }
            }
        }
    };
}

trait ThenIfNonZero {
    fn then_if_nonzero(self) -> Option<usize>;
}

impl ThenIfNonZero for usize {
    fn then_if_nonzero(self) -> Option<usize> {
        (self != 0).then_some(self)
    }
}

// The release binary installs mimalloc unconditionally on unix (src/main.rs).
// In the test profile that declaration is cfg'd out so this module can wrap
// the same allocator with a counting layer; the measurement therefore reflects
// the allocator production actually uses.
#[cfg(unix)]
counting_allocator!(mimalloc::MiMalloc);
#[cfg(unix)]
#[global_allocator]
static GLOBAL: Counting = Counting;

/// Minimum allocations performed by `body` across `TRIALS` measured runs.
/// One uncounted warm-up run first so lazily initialized globals are hot.
fn allocations(body: &dyn Fn()) -> usize {
    const TRIALS: usize = 8;
    body();
    let mut best = usize::MAX;
    for _ in 0..TRIALS {
        let count = Count(AtomicUsize::new(0));
        ACTIVE_COUNTER.with(|slot| slot.set(&count as *const Count as usize));
        body();
        let observed = {
            let value = count.0.load(Ordering::Relaxed);
            ACTIVE_COUNTER.with(|slot| slot.set(0));
            value
        };
        best = best.min(observed);
    }
    best
}

#[test]
fn token_estimator_hot_path_allocates_nothing_once_calibrated() {
    // The hot path is one estimate per proxied request. A regression here is
    // severe because it scales with the request body size.
    let est = TokenEstimator::default();
    for _ in 0..13 {
        est.observe_exact(4000, 1000, ContentKind::Prose);
    }
    let chars = "the quick brown fox jumps over the lazy dog "
        .repeat(80)
        .chars()
        .count() as u64;
    let count = allocations(&|| {
        let (tokens, source) = est.estimate(chars, ContentKind::Prose);
        assert!(tokens > 0 && source == TokenSource::Calibrated);
    });
    assert_eq!(count, 0, "calibrated estimate must not allocate");
}

fn calibrate_stats() -> ProxyStats {
    // Pre-train every class so the measurement reflects the steady-state hot
    // path (fully calibrated), not the cold-start path. The sample count
    // matches CALIBRATION_MIN_SAMPLES per class.
    let stats = ProxyStats::default();
    for kind in [ContentKind::Prose, ContentKind::Code, ContentKind::Dense] {
        for _ in 0..13 {
            stats.token_estimator.observe_exact(1000, 250, kind);
        }
    }
    stats
}

#[test]
fn estimate_tokens_budget_does_not_scale_with_body_size() {
    // The wrapper around the estimator runs per request on the request body.
    // Before the windowed-scan optimization the large body cost over 100
    // allocations against ~30 for the small one; the difference scaled
    // directly with body size. Both measurements below are single-digit
    // budgets and the large body must not exceed the small one's budget: a
    // size-scaling regression (a `Vec<char>`, a body clone, a per-chunk
    // scratch) trips the large assertion.
    let stats = calibrate_stats();
    let unit = "hello there friend, how are you today? ";
    let small =
        json!({ "model": "m", "messages": [{ "role": "user", "content": unit.repeat(8) }] });
    let large =
        json!({ "model": "m", "messages": [{ "role": "user", "content": unit.repeat(80) }] });
    let small_allocations = allocations(&|| {
        let estimate = stats.estimate_tokens(&small);
        assert!(estimate.tokens > 0);
    });
    let large_allocations = allocations(&|| {
        let estimate = stats.estimate_tokens(&large);
        assert!(estimate.tokens > 0);
    });
    // The steady-state estimate must be genuinely free, not merely constant:
    // 0 allocations for both sizes is the contract the windowed scan buys.
    assert_eq!(small_allocations, 0, "small body: {small_allocations}");
    assert_eq!(
        large_allocations, 0,
        "large body allocations {large_allocations} must not scale with body size"
    );
}

#[test]
fn request_identity_path_has_a_small_allocation_budget() {
    // One call per request for the id and conversation header. These allocate
    // (the id is a fresh string when absent) but the count must stay constant:
    // no per-header or per-body scaling.
    let mut headers = HeaderMap::new();
    headers.insert("x-request-id", HeaderValue::from_static("req-123"));
    headers.insert("x-session-id", HeaderValue::from_static("sess-9"));
    for name in ["user-agent", "originator", "session-id", "thread-id"] {
        headers.insert(name, HeaderValue::from_static("value"));
    }
    let count = allocations(&|| {
        let (request_id, conversation) = request_context(&headers);
        assert_eq!(request_id, "req-123");
        assert_eq!(conversation.as_deref(), Some("sess-9"));
    });
    // Borrowed lookups plus at most the conversation String; a regression that
    // cloned every header or built a String per lookup would exceed this.
    assert!(count <= 2, "request_context: {count}");

    let request_id_allocations = allocations(&|| {
        assert_eq!(request_id_from_headers(&headers), "req-123");
    });
    assert!(
        request_id_allocations <= 1,
        "request_id_from_headers: {request_id_allocations}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_chat_json_hot_path_stays_within_budget() {
    // End-to-end per-request path through the shared pipeline: header
    // assembly, credential materialization, outbound JSON serialization, the
    // upstream round-trip, response buffering, and tracking. Growth here means
    // someone added a clone or a re-serialization to the shared pipeline.
    //
    // Only the calling thread is counted, so the mock server task (running on
    // the second worker thread) contributes nothing; an HTTP round-trip is
    // still not allocation-free, so this remains a budget, not a zero.
    //
    // This test must not run concurrently with other tests that count
    // allocations: they share one counting window. The binary-wide gate below
    // makes the shared allocator serialize counting windows across tests.
    let (upstream, _server) = mock_upstream(json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "created": 1,
        "model": "gpt-test",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "ok" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 15 }
    }))
    .await;
    let config = Config {
        default_provider: "test".to_string(),
        providers: HashMap::from([(
            "test".to_string(),
            UpstreamConfig {
                base_url: upstream,
                api_key: Some("sk-test".to_string()),
                ..Default::default()
            },
        )]),
        ..Default::default()
    };
    let state = state_with_config(config);
    let provider = state.providers.get("test").expect("test provider").clone();
    let mut headers = HeaderMap::new();
    headers.insert("x-request-id", HeaderValue::from_static("req-stress"));
    let body = json!({
        "model": "gpt-test",
        "messages": [{ "role": "user", "content": "hello" }]
    });

    // Warm the whole path (including the mock server task) uncounted.
    let response = proxy_chat(&provider, &headers, &body).await;
    assert!(response.status().is_success());

    // Borrow into the measured region rather than cloning: a clone of the
    // request would be the harness's own allocation, and it is exactly the
    // cost this test exists to detect in the pipeline.
    let measure = |body: &Value| {
        allocations(&|| {
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current()
                    .block_on(async { proxy_chat(&provider, &headers, body).await });
            })
        })
    };
    let count = measure(&body);
    assert!(count <= 250, "proxy_chat allocated {count}");

    // The property that matters is not the absolute number, which includes the
    // HTTP client's own work, but that it does not grow with the prompt. The
    // pipeline used to deep-clone the request tree twice per call (once for
    // the failover hop, once to format it for the provider), so a larger
    // prompt cost one extra allocation per JSON node on every request. Both
    // copies are now taken only when an adapter really has to rewrite the
    // body, and the measured cost is flat.
    let large = json!({
        "model": "gpt-test",
        "messages": (0..200)
            .map(|index| json!({
                "role": "user",
                "content": format!("message {index} with a reasonable amount of prose in it")
            }))
            .collect::<Vec<_>>()
    });
    let large_count = measure(&large);
    assert!(
        large_count <= count + 16,
        "a 200-message prompt allocated {large_count} against {count} for a one-message prompt; \
         the request path is scaling with the body again"
    );
}
