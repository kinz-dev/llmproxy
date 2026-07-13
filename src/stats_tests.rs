use std::time::Duration;

use serde_json::json;

use super::*;

#[test]
fn latency_percentiles_use_nearest_rank() {
    let window = LatencyWindow::default();
    for ms in 1..=100u64 {
        window.record(Duration::from_millis(ms));
    }
    let (p50, p99, samples) = window.percentiles();
    assert_eq!(p50, Some(50));
    assert_eq!(p99, Some(99));
    assert_eq!(samples, 100);
}

#[test]
fn latency_window_is_bounded() {
    let window = LatencyWindow::default();
    for _ in 0..(LATENCY_WINDOW_CAP + 10) {
        window.record(Duration::from_millis(7));
    }
    let (_, _, samples) = window.percentiles();
    assert_eq!(samples, LATENCY_WINDOW_CAP);
}

#[test]
fn empty_latency_window_has_no_percentiles() {
    let window = LatencyWindow::default();
    assert_eq!(window.percentiles(), (None, None, 0));
}

#[test]
fn local_base_urls_are_classified_local() {
    // Synthetic boundary fixtures only; these are not deployment addresses.
    let urls = [
        "http://127.0.0.1:8000/v1".to_string(),
        "http://localhost:11434/v1".to_string(),
        "https://[::1]:8443/v1".to_string(),
        format!("http://{}:8000/v1", std::net::Ipv4Addr::new(192, 168, 0, 1)),
        format!("http://{}/v1", std::net::Ipv4Addr::new(10, 0, 0, 1)),
        format!("http://{}:9000/v1", std::net::Ipv4Addr::new(172, 16, 0, 1)),
        "http://service.local/v1".to_string(),
        "http://service.lan/v1".to_string(),
        "http://local-service/v1".to_string(),
    ];
    for url in urls {
        assert_eq!(
            route_class_for_base_url(&url),
            RouteClass::Local,
            "expected local: {url}"
        );
    }
}

#[test]
fn external_base_urls_are_classified_external() {
    let urls = [
        "https://api.openai.com/v1".to_string(),
        "https://api.anthropic.com/v1".to_string(),
        "https://chatgpt.com/backend-api/codex".to_string(),
        "http://192.0.2.1/v1".to_string(), // RFC 5737 documentation address
        format!("http://{}/v1", std::net::Ipv4Addr::new(172, 32, 0, 1)),
    ];
    for url in urls {
        assert_eq!(
            route_class_for_base_url(&url),
            RouteClass::External,
            "expected external: {url}"
        );
    }
}

#[test]
fn usage_extraction_reads_openai_and_anthropic_shapes() {
    let openai = json!({ "prompt_tokens": 12, "completion_tokens": 34, "total_tokens": 46 });
    assert_eq!(usage_tokens(&openai), Some((12, 34)));
    let anthropic = json!({ "input_tokens": 5, "output_tokens": 9 });
    assert_eq!(usage_tokens(&anthropic), Some((5, 9)));
    assert_eq!(usage_tokens(&json!({ "other": 1 })), None);
}

#[test]
fn sniffer_reads_usage_from_buffered_json() {
    let mut sniffer = UsageSniffer::new(false);
    sniffer.feed(
        br#"{"id":"x","choices":[{"message":{"content":"hi"}}],"usage":{"prompt_tokens":100,"completion_tokens":25}}"#,
    );
    assert_eq!(sniffer.finish(), (100, 25, false));
}

#[test]
fn sniffer_skips_null_usage_in_stream_chunks() {
    let mut sniffer = UsageSniffer::new(true);
    sniffer.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}],\"usage\":null}\n\n");
    sniffer.feed(
        b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n\n",
    );
    sniffer.feed(b"data: [DONE]\n\n");
    assert_eq!(sniffer.finish(), (7, 3, false));
}

#[test]
fn sniffer_estimates_when_usage_is_missing() {
    let mut sniffer = UsageSniffer::new(false);
    sniffer.feed(&[b'x'; 400]);
    let (prompt, completion, estimated) = sniffer.finish();
    assert_eq!(prompt, 0);
    assert_eq!(completion, 100);
    assert!(estimated);
}

#[test]
fn sniffer_survives_usage_split_across_tail_boundary() {
    let mut sniffer = UsageSniffer::new(true);
    // Fill beyond the tail cap, then append the usage chunk; the tail must
    // still contain the complete usage object.
    sniffer.feed(&vec![b'p'; SNIFFER_TAIL_CAP + 512]);
    sniffer.feed(b"data: {\"usage\":{\"input_tokens\":11,\"output_tokens\":22}}\n\n");
    assert_eq!(sniffer.finish(), (11, 22, false));
}

#[test]
fn usage_object_with_nested_braces_and_strings_parses() {
    let bytes = br#": {"prompt_tokens": 1, "detail": {"note": "brace } in string"}, "completion_tokens": 2}"#;
    let usage = parse_usage_value(bytes).expect("usage parses");
    assert_eq!(usage_tokens(&usage), Some((1, 2)));
}

#[test]
fn stats_counters_and_snapshot_shape() {
    let stats = ProxyStats::default();
    stats.touch_start();
    stats.record_request();
    stats.record_remap(RemapKind::Alias);
    stats.record_remap(RemapKind::Auto);
    stats.record_intercept_attempt();
    stats.record_intercept_bypass();
    stats.record_intercept_local(
        Duration::from_millis(120),
        Some(&json!({ "prompt_tokens": 10, "completion_tokens": 5 })),
    );
    stats.record_intercept_pass();
    stats.record_intercept_fallback();
    stats.record_served(RouteClass::External, Duration::from_millis(900));
    stats.record_tokens_saved(0, 50, true);

    let snapshot = stats.snapshot_json();
    assert_eq!(snapshot["requests"]["total"], 1);
    assert_eq!(snapshot["requests"]["remapped"], 2);
    assert_eq!(snapshot["intercept"]["attempts"], 1);
    assert_eq!(snapshot["intercept"]["bypass"], 1);
    assert_eq!(snapshot["intercept"]["local"], 1);
    assert_eq!(snapshot["intercept"]["pass"], 1);
    assert_eq!(snapshot["intercept"]["fallback"], 1);
    assert_eq!(snapshot["served"]["local"], 1);
    assert_eq!(snapshot["served"]["external"], 1);
    assert_eq!(snapshot["latency"]["local"]["p50_ms"], 120);
    assert_eq!(snapshot["latency"]["external"]["p99_ms"], 900);
    assert_eq!(snapshot["tokens_saved"]["prompt"], 10);
    assert_eq!(snapshot["tokens_saved"]["completion"], 55);
    assert_eq!(snapshot["tokens_saved"]["total"], 65);
    assert_eq!(snapshot["tokens_saved"]["estimated"], 50);

    stats.reset();
    let snapshot = stats.snapshot_json();
    assert_eq!(snapshot["requests"]["total"], 0);
    assert_eq!(snapshot["intercept"]["bypass"], 0);
    assert_eq!(snapshot["served"]["local"], 0);
    assert_eq!(snapshot["latency"]["local"]["samples"], 0);
}

#[test]
fn traffic_log_records_only_when_enabled() {
    let log = TrafficLog::default();
    log.record(RouteClass::Local, LogDirection::Request, "ignored".into());
    assert_eq!(
        log.snapshot_after(0)["entries"].as_array().unwrap().len(),
        0
    );

    log.set_enabled(true);
    log.record(RouteClass::Local, LogDirection::Request, "req body".into());
    log.record(
        RouteClass::External,
        LogDirection::Response,
        "rsp body".into(),
    );
    let snapshot = log.snapshot_after(0);
    let entries = snapshot["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["class"], "local");
    assert_eq!(entries[0]["dir"], "req");
    assert_eq!(entries[0]["text"], "req body");
    assert_eq!(entries[1]["class"], "external");
    assert_eq!(entries[1]["dir"], "rsp");
    assert!(entries[1]["ts_ms"].as_u64().unwrap() > 0);
}

#[test]
fn traffic_log_snapshot_is_incremental_and_clearable() {
    let log = TrafficLog::default();
    log.set_enabled(true);
    log.record(RouteClass::Local, LogDirection::Request, "one".into());
    let first = log.snapshot_after(0);
    let latest = first["latest_id"].as_u64().unwrap();

    log.record(RouteClass::Local, LogDirection::Response, "two".into());
    let update = log.snapshot_after(latest);
    let entries = update["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["text"], "two");

    log.clear();
    assert_eq!(
        log.snapshot_after(0)["entries"].as_array().unwrap().len(),
        0
    );
}

#[test]
fn traffic_log_is_bounded() {
    let log = TrafficLog::default();
    log.set_enabled(true);
    for index in 0..(TRAFFIC_LOG_CAP + 25) {
        log.record(RouteClass::Local, LogDirection::Request, index.to_string());
    }
    let snapshot = log.snapshot_after(0);
    let entries = snapshot["entries"].as_array().unwrap();
    assert_eq!(entries.len(), TRAFFIC_LOG_CAP);
    assert_eq!(entries[0]["text"], "25"); // oldest 25 dropped
}

#[test]
fn traffic_log_prunes_expired_entries() {
    let mut entries = VecDeque::new();
    entries.push_back(TrafficLogEntry {
        id: 1,
        epoch_ms: 0,
        recorded_at: Instant::now(),
        class: RouteClass::Local,
        direction: LogDirection::Request,
        preview: "old".into(),
    });
    std::thread::sleep(Duration::from_millis(3));
    TrafficLog::prune(&mut entries, Duration::from_millis(1));
    assert!(entries.is_empty());

    entries.push_back(TrafficLogEntry {
        id: 2,
        epoch_ms: 0,
        recorded_at: Instant::now(),
        class: RouteClass::Local,
        direction: LogDirection::Request,
        preview: "fresh".into(),
    });
    TrafficLog::prune(&mut entries, Duration::from_secs(60));
    assert_eq!(entries.len(), 1);
}

#[test]
fn previews_are_single_line_and_truncated_to_the_budget() {
    let flattened = flatten_preview("line one\nline two\ttabbed".to_string());
    assert_eq!(flattened, "line one line two tabbed");

    let long = flatten_preview("x".repeat(TRAFFIC_PREVIEW_CHARS * 3));
    assert_eq!(long.chars().count(), TRAFFIC_PREVIEW_CHARS + 1); // budget + ellipsis
    assert!(long.ends_with('…'));

    let short = json_preview(&json!({ "model": "gpt-x" }));
    assert_eq!(short, r#"{"model":"gpt-x"}"#);

    let big = json_preview(
        &json!({ "model": "gpt-x", "content": "y".repeat(TRAFFIC_PREVIEW_CHARS * 3) }),
    );
    assert_eq!(big.chars().count(), TRAFFIC_PREVIEW_CHARS + 1);
    // serde_json serializes keys sorted, so "content" leads.
    assert!(big.starts_with(r#"{"content":"yyy"#), "got: {big}");
    assert!(big.ends_with('…'));
}
