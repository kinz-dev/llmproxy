use std::time::Duration;

use serde_json::json;

use crate::token_estimate::TokenSource;

use super::*;

fn estimate(tokens: u64, source: TokenSource) -> (u64, TokenSource) {
    (tokens, source)
}

fn estimate_for(text: &str) -> TokenEstimate {
    proxy_stats().estimate_tokens(&json!(text))
}

#[test]
fn latency_percentiles_use_nearest_rank() {
    let window = LatencyRing::<LATENCY_WINDOW_CAP>::default();
    assert_eq!(window.summary(), Default::default());
    for ms in 1..=100u64 {
        window.record(Duration::from_millis(ms));
    }
    let summary = window.summary();
    assert_eq!(summary.p50_us, Some(50_000));
    assert_eq!(summary.p99_us, Some(99_000));
    assert_eq!(summary.samples, 100);
    // The millisecond fields the dashboard has always read still hold.
    let json = summary.to_json();
    assert_eq!(json["p50_ms"], json!(50));
    assert_eq!(json["p99_ms"], json!(99));
}

#[test]
fn latency_window_is_bounded() {
    let window = LatencyRing::<LATENCY_WINDOW_CAP>::default();
    for _ in 0..(LATENCY_WINDOW_CAP + 10) {
        window.record(Duration::from_millis(7));
    }
    let summary = window.summary();
    assert_eq!(summary.samples, LATENCY_WINDOW_CAP);
    // The window is bounded; the lifetime count it reports is not.
    assert_eq!(summary.total, LATENCY_WINDOW_CAP as u64 + 10);
}

#[test]
fn provider_call_window_is_bounded_but_count_is_not() {
    let calls = ProviderCalls::default();
    for _ in 0..(PROVIDER_LATENCY_CAP + 10) {
        calls.record(Duration::from_millis(5));
    }
    assert_eq!(
        calls.count.load(Ordering::Relaxed) as usize,
        PROVIDER_LATENCY_CAP + 10
    );
    assert_eq!(calls.latency.summary().samples, PROVIDER_LATENCY_CAP);
}

#[test]
fn a_route_row_is_interned_once_and_reused() {
    // The request path must not allocate a fresh key per request: the second
    // sighting of a route has to resolve to the very same row.
    let registry = MetricRegistry::<2, AtomicU64>::default();
    let first = registry.row(["chat_completions", "local"]);
    let second = registry.row(["chat_completions", "local"]);
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(registry.snapshot().len(), 1);
    registry.row(["responses", "local"]);
    assert_eq!(registry.snapshot().len(), 2);
}

#[test]
fn money_accumulates_through_a_shared_reference() {
    let total = AtomicF64::default();
    assert_eq!(total.get(), 0.0);
    total.add(0.25);
    total.add(0.5);
    assert!((total.get() - 0.75).abs() < f64::EPSILON);
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

fn sniffer_read(key: &str, body: &[u8], input: u64, output: u64) {
    let input_estimate = estimate_for("hello there");
    let mut sniffer = UsageSniffer::new(false);
    sniffer.feed(body);
    assert_eq!(
        sniffer.finish(&input_estimate, true),
        TokenUsage {
            input_tokens: input,
            output_tokens: output,
            estimated: false,
            input_source: TokenSource::Exact,
            output_source: TokenSource::Exact,
        },
        "{key}"
    );
}

#[test]
fn sniffer_reads_usage_from_buffered_json() {
    sniffer_read(
        "buffered",
        br#"{"id":"x","choices":[{"message":{"content":"hi"}}],"usage":{"prompt_tokens":100,"completion_tokens":25}}"#,
        100,
        25,
    );
}

#[test]
fn sniffer_skips_null_usage_in_stream_chunks() {
    let input_estimate = estimate_for("hello there");
    let mut sniffer = UsageSniffer::new(true);
    sniffer.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}],\"usage\":null}\n\n");
    sniffer.feed(
        b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n\n",
    );
    sniffer.feed(b"data: [DONE]\n\n");
    assert_eq!(
        sniffer.finish(&input_estimate, true),
        TokenUsage {
            input_tokens: 7,
            output_tokens: 3,
            estimated: false,
            input_source: TokenSource::Exact,
            output_source: TokenSource::Exact,
        }
    );
}

#[test]
fn sniffer_estimates_when_usage_is_missing() {
    let input_estimate = estimate_for("hello there");
    let input_tokens = input_estimate.tokens;
    let mut sniffer = UsageSniffer::new(false);
    sniffer.feed(&[b'x'; 400]);
    // The 400 'x' bytes decode to 400 ASCII characters. With no prior
    // calibration the prose default ratio (0.25 tokens/char) yields 100.
    let usage = sniffer.finish(&input_estimate, true);
    assert_eq!(usage.input_tokens, input_tokens);
    assert_eq!(usage.output_tokens, 100);
    assert!(usage.estimated);
    assert_eq!(usage.input_source, input_estimate.source);
    assert_eq!(usage.output_source, TokenSource::Default);
}

#[test]
fn sniffer_does_not_invent_embedding_output_tokens() {
    let input_estimate = estimate_for("hello there");
    let mut sniffer = UsageSniffer::new(false);
    sniffer.feed(br#"{"usage":{"prompt_tokens":12,"total_tokens":12},"data":[[1,2,3]]}"#);
    assert_eq!(
        sniffer.finish(&input_estimate, false),
        TokenUsage {
            input_tokens: 12,
            output_tokens: 0,
            estimated: true,
            input_source: TokenSource::Exact,
            output_source: TokenSource::Exact,
        }
    );
}

#[test]
fn sniffer_survives_usage_split_across_tail_boundary() {
    let input_estimate = estimate_for("hello there");
    let mut sniffer = UsageSniffer::new(true);
    // Fill beyond the tail cap, then append the usage chunk; the tail must
    // still contain the complete usage object.
    sniffer.feed(&vec![b'p'; SNIFFER_TAIL_CAP + 512]);
    sniffer.feed(b"data: {\"usage\":{\"input_tokens\":11,\"output_tokens\":22}}\n\n");
    assert_eq!(
        sniffer.finish(&input_estimate, true),
        TokenUsage {
            input_tokens: 11,
            output_tokens: 22,
            estimated: false,
            input_source: TokenSource::Exact,
            output_source: TokenSource::Exact,
        }
    );
}

#[test]
fn sniffer_combines_anthropic_stream_usage_events() {
    let input_estimate = estimate_for("hello there");
    let mut sniffer = UsageSniffer::new(true);
    sniffer.feed(b"data: {\"usage\":{\"input_tokens\":17}}\n\n");
    sniffer.feed(b"data: {\"usage\":{\"output_tokens\":8}}\n\n");
    assert_eq!(
        sniffer.finish(&input_estimate, true),
        TokenUsage {
            input_tokens: 17,
            output_tokens: 8,
            estimated: false,
            input_source: TokenSource::Exact,
            output_source: TokenSource::Exact,
        }
    );
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
    stats.record_remap(RemapKind::Namespace);
    stats.record_remap(RemapKind::Auto);
    stats.record_intercept_attempt();
    stats.record_intercept_bypass();
    stats.record_intercept_reject();
    let local_usage = TokenUsage::from_response(
        Some(&json!({ "prompt_tokens": 10, "completion_tokens": 5 })),
        estimate(1, TokenSource::Default),
        estimate(1, TokenSource::Default),
    );
    stats.record_model_usage(
        "local",
        "triage-model",
        Duration::from_millis(100),
        local_usage,
        RequestPricing::default(),
    );
    stats.record_intercept_local(
        RouteClass::Local,
        "local",
        "responses",
        Duration::from_millis(120),
        local_usage,
    );
    stats.record_intercept_pass();
    stats.record_intercept_redirect("PROVIDER_TEST");
    stats.record_intercept_fallback();
    stats.record_served(
        RouteClass::External,
        "openai",
        "responses",
        Duration::from_millis(900),
    );
    stats.record_model_usage(
        "openai",
        "gpt-test",
        Duration::from_millis(800),
        TokenUsage {
            input_tokens: 20,
            output_tokens: 30,
            estimated: false,
            input_source: TokenSource::Exact,
            output_source: TokenSource::Exact,
        },
        RequestPricing::default(),
    );
    stats.record_routing_destination("responses", "openai", "gpt-test", false);
    stats.record_routing_destination("responses", "openai", "gpt-test", true);
    stats.record_tokens_saved(0, 50, TokenSource::Exact, TokenSource::Default);

    let snapshot = stats.snapshot_json();
    assert_eq!(snapshot["requests"]["total"], 1);
    assert_eq!(snapshot["requests"]["remapped"], 3);
    assert_eq!(snapshot["requests"]["remap_namespace"], 1);
    assert_eq!(snapshot["intercept"]["attempts"], 1);
    assert_eq!(snapshot["intercept"]["bypass"], 1);
    assert_eq!(snapshot["intercept"]["local"], 1);
    assert_eq!(snapshot["intercept"]["reject"], 1);
    assert_eq!(snapshot["intercept"]["pass"], 1);
    assert_eq!(snapshot["intercept"]["redirect"], 1);
    assert_eq!(snapshot["intercept"]["redirects"]["PROVIDER_TEST"], 1);
    assert_eq!(snapshot["intercept"]["fallback"], 1);
    assert_eq!(snapshot["served"]["local"], 1);
    assert_eq!(snapshot["served"]["external"], 1);
    assert_eq!(snapshot["served"]["providers"]["local"]["count"], 1);
    assert_eq!(snapshot["served"]["providers"]["openai"]["count"], 1);
    assert_eq!(snapshot["served"]["providers"]["openai"]["p50_ms"], 900);
    assert_eq!(snapshot["served"]["providers"]["openai"]["p99_ms"], 900);
    assert_eq!(snapshot["served"]["providers"]["openai"]["samples"], 1);
    assert_eq!(snapshot["served"]["matrix"]["responses"]["openai"], 1);
    assert_eq!(
        snapshot["served"]["destinations"][0]["frontend"],
        "responses"
    );
    assert_eq!(snapshot["served"]["destinations"][0]["provider"], "openai");
    assert_eq!(snapshot["served"]["destinations"][0]["model"], "gpt-test");
    assert_eq!(snapshot["served"]["destinations"][0]["requests"], 2);
    assert_eq!(snapshot["served"]["destinations"][0]["errors"], 1);
    assert_eq!(snapshot["latency"]["local"]["p50_ms"], 120);
    assert_eq!(snapshot["latency"]["external"]["p99_ms"], 900);
    let models = snapshot["model_usage"].as_array().unwrap();
    let local_model = models
        .iter()
        .find(|entry| entry["provider"] == "local")
        .unwrap();
    assert_eq!(local_model["model"], "triage-model");
    assert_eq!(local_model["requests"], 1);
    assert_eq!(local_model["input_tokens"], 10);
    assert_eq!(local_model["output_tokens"], 5);
    assert_eq!(local_model["total_time_ms"], 100);
    let external_model = models
        .iter()
        .find(|entry| entry["provider"] == "openai")
        .unwrap();
    assert_eq!(external_model["model"], "gpt-test");
    assert_eq!(external_model["total_tokens"], 50);
    assert_eq!(external_model["total_time_ms"], 800);
    assert_eq!(snapshot["tokens_saved"]["prompt"], 10);
    assert_eq!(snapshot["tokens_saved"]["completion"], 55);
    assert_eq!(snapshot["tokens_saved"]["total"], 65);
    assert_eq!(snapshot["tokens_saved"]["estimated"], 50);

    stats.reset();
    let snapshot = stats.snapshot_json();
    assert_eq!(snapshot["requests"]["total"], 0);
    assert_eq!(snapshot["intercept"]["bypass"], 0);
    assert_eq!(snapshot["intercept"]["reject"], 0);
    assert_eq!(snapshot["served"]["local"], 0);
    assert_eq!(snapshot["latency"]["local"]["samples"], 0);
    assert!(snapshot["model_usage"].as_array().unwrap().is_empty());
    assert!(
        snapshot["served"]["destinations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn traffic_log_records_only_when_enabled() {
    let log = TrafficLog::default();
    log.record(
        RouteClass::Local,
        "local",
        LogDirection::Request,
        "ignored".into(),
    );
    assert_eq!(
        log.snapshot_after(0)["entries"].as_array().unwrap().len(),
        0
    );

    log.set_enabled(true);
    log.record(
        RouteClass::Local,
        "local",
        LogDirection::Request,
        "req body".into(),
    );
    log.record(
        RouteClass::External,
        "openai",
        LogDirection::Response,
        "rsp body".into(),
    );
    let snapshot = log.snapshot_after(0);
    let entries = snapshot["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["class"], "local");
    assert_eq!(entries[0]["provider"], "local");
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
    log.record(
        RouteClass::Local,
        "local",
        LogDirection::Request,
        "one".into(),
    );
    let first = log.snapshot_after(0);
    let latest = first["latest_id"].as_u64().unwrap();

    log.record(
        RouteClass::Local,
        "local",
        LogDirection::Response,
        "two".into(),
    );
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
        log.record(
            RouteClass::Local,
            "local",
            LogDirection::Request,
            index.to_string(),
        );
    }
    let snapshot = log.snapshot_after(0);
    let entries = snapshot["entries"].as_array().unwrap();
    assert_eq!(entries.len(), TRAFFIC_LOG_CAP);
    assert_eq!(entries[0]["text"], "25");
}

#[test]
fn routing_decision_log_is_incremental_and_sanitized() {
    let log = RoutingDecisionLog::default();
    log.set_enabled(true);
    log.record(RoutingDecisionAudit {
        request_id: "request-test".to_string(),
        mode: "integrated",
        protocol: "responses",
        requested_model: Some("Auto".to_string()),
        prompt: format!("inspect\n{}", "x".repeat(ROUTING_PROMPT_PREVIEW_CHARS + 20)),
        decision: "redirect",
        reason: "matched the research route\nwith current sources".to_string(),
        policy: "built-in",
        triage_provider: Some("local".to_string()),
        triage_model: Some("triage".to_string()),
        triage_elapsed_ms: Some(12),
        target: Some("RESEARCH".to_string()),
        target_hint: Some("online research".to_string()),
        provider: Some("external".to_string()),
        model: Some("research-model".to_string()),
        route_class: Some(RouteClass::External),
        reasoning_effort: Some("max".to_string()),
        error: None,
    });

    let snapshot = log.snapshot_after(0);
    let entry = &snapshot["entries"][0];
    assert_eq!(entry["decision"], "redirect");
    assert_eq!(entry["class"], "external");
    assert_eq!(entry["model"], "research-model");
    assert_eq!(entry["reasoning_effort"], "max");
    assert!(!entry["prompt"].as_str().unwrap().contains('\n'));
    assert!(entry["prompt"].as_str().unwrap().ends_with('…'));
    assert!(!entry["reason"].as_str().unwrap().contains('\n'));

    let latest = snapshot["latest_id"].as_u64().unwrap();
    assert!(
        log.snapshot_after(latest)["entries"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    log.clear();
    assert!(
        log.snapshot_after(0)["entries"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn previews_are_single_line_and_truncated_to_the_budget() {
    let flattened = flatten_preview("line one\nline two\ttabbed".to_string());
    assert_eq!(flattened, "line one line two tabbed");

    let long = flatten_preview("x".repeat(TRAFFIC_PREVIEW_CHARS * 3));
    assert_eq!(long.chars().count(), TRAFFIC_PREVIEW_CHARS + 1);
    assert!(long.ends_with('…'));

    let short = json_preview(&json!({ "model": "gpt-x" }));
    assert_eq!(short, r#"{"model":"gpt-x"}"#);

    let big = json_preview(
        &json!({ "model": "gpt-x", "content": "y".repeat(TRAFFIC_PREVIEW_CHARS * 3) }),
    );
    assert_eq!(big.chars().count(), TRAFFIC_PREVIEW_CHARS + 1);
    assert!(big.starts_with(r#"{"content":"yyy"#), "got: {big}");
    assert!(big.ends_with('…'));
}
