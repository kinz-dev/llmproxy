use super::*;

fn ring() -> LatencyRing<8> {
    LatencyRing::default()
}

#[test]
fn an_empty_window_reports_no_percentiles() {
    let summary = ring().summary();
    assert_eq!(summary.p50_us, None);
    assert_eq!(summary.p99_us, None);
    assert_eq!(summary.samples, 0);
    assert_eq!(summary.total, 0);
}

#[test]
fn percentiles_use_nearest_rank_over_the_recorded_samples() {
    let ring = ring();
    for micros in [10, 20, 30, 40] {
        ring.record_micros(micros);
    }
    let summary = ring.summary();
    assert_eq!(summary.samples, 4);
    assert_eq!(summary.total, 4);
    // Nearest rank never interpolates: every reported value was observed.
    assert_eq!(summary.p50_us, Some(20));
    assert_eq!(summary.p90_us, Some(40));
    assert_eq!(summary.p99_us, Some(40));
    assert_eq!(summary.max_us, Some(40));
}

#[test]
fn the_window_keeps_only_the_most_recent_samples() {
    let ring = ring();
    for micros in 0..8 {
        ring.record_micros(micros);
    }
    for micros in 100..108 {
        ring.record_micros(micros);
    }
    let summary = ring.summary();
    // Eight slots, sixteen writes: the first generation has been overwritten
    // while the lifetime count still remembers every sample.
    assert_eq!(summary.samples, 8);
    assert_eq!(summary.total, 16);
    assert_eq!(summary.max_us, Some(107));
    assert_eq!(summary.p50_us, Some(103));
}

#[test]
fn a_zero_microsecond_sample_is_counted_rather_than_read_as_an_empty_slot() {
    // Slots are stored biased by one precisely so a genuinely sub-microsecond
    // request is not mistaken for a slot nobody has written yet.
    let ring = ring();
    ring.record_micros(0);
    let summary = ring.summary();
    assert_eq!(summary.samples, 1);
    assert_eq!(summary.p50_us, Some(0));
}

#[test]
fn an_absurd_duration_saturates_instead_of_wrapping() {
    let ring = ring();
    ring.record(Duration::from_secs(60 * 60 * 24));
    let summary = ring.summary();
    assert_eq!(summary.max_us, Some(u32::MAX - 1));
}

#[test]
fn clearing_drops_both_the_window_and_the_lifetime_count() {
    let ring = ring();
    ring.record_micros(5);
    ring.clear();
    let summary = ring.summary();
    assert_eq!(summary.samples, 0);
    assert_eq!(summary.total, 0);
}

#[test]
fn millisecond_fields_round_up_so_a_fast_request_never_reads_as_zero() {
    let ring = ring();
    ring.record_micros(1);
    ring.record_micros(1_400);
    let json = ring.summary().to_json();
    // The historical millisecond fields stay populated for existing dashboard
    // rows, and a 1.4ms p99 must not round down to 1ms and hide the tail.
    assert_eq!(json["p50_ms"], serde_json::json!(1));
    assert_eq!(json["p99_ms"], serde_json::json!(2));
    assert_eq!(json["p99_us"], serde_json::json!(1_400));
}

#[test]
fn relay_overhead_is_first_byte_minus_the_upstream_wait_and_pre_dispatch() {
    let metrics = LatencyMetrics::default();
    metrics.record_first_byte(
        Duration::from_micros(5_000),
        Duration::from_micros(300),
        Duration::from_micros(4_000),
    );
    let json = metrics.to_json();
    assert_eq!(json["client_ttfb"]["p50_us"], serde_json::json!(5_000));
    assert_eq!(json["relay"]["p50_us"], serde_json::json!(700));
    // The headline figure is what the proxy itself cost, not the round trip.
    assert_eq!(json["overhead"]["p50_us"], serde_json::json!(1_000));
}

#[test]
fn overlapping_phase_samples_read_as_zero_overhead_rather_than_wrapping() {
    // The spans are sampled at different points in the pipeline, so an
    // upstream wait can legitimately exceed the measured first-byte span by a
    // few microseconds. That must not wrap into an enormous overhead.
    let metrics = LatencyMetrics::default();
    metrics.record_first_byte(
        Duration::from_micros(1_000),
        Duration::from_micros(100),
        Duration::from_micros(2_000),
    );
    let json = metrics.to_json();
    assert_eq!(json["relay"]["p50_us"], serde_json::json!(0));
    assert_eq!(json["overhead"]["p50_us"], serde_json::json!(100));
}

#[tokio::test]
async fn upstream_waits_accumulate_across_failover_attempts() {
    let total = with_request_timing(async {
        add_upstream_wait(Duration::from_micros(400));
        add_upstream_wait(Duration::from_micros(600));
        upstream_wait()
    })
    .await;
    // A retried request must not bill its first attempt's wait to the proxy.
    assert_eq!(total, Duration::from_micros(1_000));
}

#[tokio::test]
async fn timing_contexts_do_not_leak_between_requests() {
    let first = with_request_timing(async {
        add_upstream_wait(Duration::from_micros(900));
        upstream_wait()
    })
    .await;
    let second = with_request_timing(async { upstream_wait() }).await;
    assert_eq!(first, Duration::from_micros(900));
    assert_eq!(second, Duration::ZERO);
}

#[test]
fn recording_outside_a_request_context_is_a_no_op() {
    // Background refreshes and direct pipeline tests have no request context.
    // Unattributed work is simply not attributed, never a panic.
    add_upstream_wait(Duration::from_micros(10));
    assert_eq!(upstream_wait(), Duration::ZERO);
}

#[tokio::test]
async fn a_body_that_stalls_before_its_first_byte_bills_the_wait_to_upstream() {
    // A provider sends response headers as soon as it accepts the request and
    // its first token some time later. On a real model that gap is the time to
    // first token, and it belongs to the provider. Billing it to the proxy
    // would swamp an overhead figure measured in microseconds.
    let stall = Duration::from_millis(120);
    let waited = with_request_timing(async move {
        let stream = futures_util::stream::once(async move {
            tokio::time::sleep(stall).await;
            Ok::<_, std::io::Error>(vec![1_u8, 2, 3])
        });
        let mut stream = Box::pin(timed_body_stream(stream));
        while stream.next().await.is_some() {}
        upstream_wait()
    })
    .await;
    assert!(
        waited >= stall,
        "a {stall:?} stall before the first byte was attributed as {waited:?} of upstream time"
    );
}

#[tokio::test]
async fn body_stream_timing_outside_a_request_context_is_a_no_op() {
    let stream = futures_util::stream::iter([1, 2, 3]);
    let mut stream = Box::pin(timed_body_stream(stream));
    let mut seen = 0;
    while stream.next().await.is_some() {
        seen += 1;
    }
    // The wrapper must stay transparent: every item still arrives.
    assert_eq!(seen, 3);
    assert_eq!(upstream_wait(), Duration::ZERO);
}
