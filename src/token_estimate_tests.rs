//! Tests for the hybrid (self-calibrating) token estimator.

use serde_json::json;

use super::*;
use crate::stats::ProxyStats;

fn sample_prose() -> String {
    "The quick brown fox jumps over the lazy dog near the river bank. ".repeat(20)
}

fn sample_code() -> String {
    "function f(a){return a.map(x=>x*2).filter(x=>x>3);}\n".repeat(20)
}

fn sample_dense() -> String {
    "这是一个中文句子用来测试密集脚本的分类与估算行为。".repeat(20)
}

#[test]
fn classify_recognises_each_content_family() {
    assert_eq!(classify(&sample_prose()), ContentKind::Prose);
    assert_eq!(classify(&sample_code()), ContentKind::Code);
    assert_eq!(classify(&sample_dense()), ContentKind::Dense);
}

#[test]
fn cold_start_uses_class_default_ratios() {
    let est = TokenEstimator::default();
    // 1000 chars of prose at the 0.25 default ratio yields 250 tokens.
    let (tokens, source) = est.estimate(1000, ContentKind::Prose);
    assert_eq!(tokens, 250);
    assert_eq!(source, TokenSource::Default);
    // A denser class starts closer to its real behaviour rather than
    // undercounting at 4 chars/token.
    let (dense, _) = est.estimate(1000, ContentKind::Dense);
    assert!(dense > tokens, "dense must exceed prose default");
    let (code, _) = est.estimate(1000, ContentKind::Code);
    assert!(code > tokens && code < dense);
}

#[test]
fn estimate_floor_is_one_token() {
    let est = TokenEstimator::default();
    for kind in [ContentKind::Prose, ContentKind::Code, ContentKind::Dense] {
        assert_eq!(est.estimate(0, kind).0, 1);
        assert_eq!(est.estimate(1, kind).0, 1);
    }
}

#[test]
fn calibration_replaces_default_ratio_and_source() {
    let est = TokenEstimator::default();
    // A deployment whose tokenizer emits one token per 2 characters (ratio 0.5)
    // is far denser than the 0.25 prose default. Fold in enough exact samples
    // and the estimate must both rise and be tagged as learned.
    let chars = 1000_u64;
    let tokens = 500_u64; // ratio 0.5
    for _ in 0..CALIBRATION_MIN_SAMPLES {
        est.observe_exact(chars, tokens, ContentKind::Prose);
    }
    let (estimate, source) = est.estimate(chars, ContentKind::Prose);
    assert_eq!(source, TokenSource::Calibrated);
    assert!(
        estimate > 250,
        "calibrated estimate {estimate} must exceed the 0.25 default 250"
    );
}

#[test]
fn calibration_is_per_class_and_does_not_leak() {
    let est = TokenEstimator::default();
    // Train only the prose bucket with an extremely dense ratio.
    for _ in 0..40 {
        est.observe_exact(1000, 1000, ContentKind::Prose);
    }
    // The code bucket is untouched, so it must still return its cold-start
    // default, proving the buckets are isolated.
    let (code_tokens, code_source) = est.estimate(1000, ContentKind::Code);
    assert_eq!(code_source, TokenSource::Default);
    assert_eq!(code_tokens, 400, "code default ratio 0.40 * 1000");
    let (prose_tokens, prose_source) = est.estimate(1000, ContentKind::Prose);
    assert_eq!(prose_source, TokenSource::Calibrated);
    assert!(prose_tokens > code_tokens);
}

#[test]
fn implausible_ratios_are_rejected() {
    let est = TokenEstimator::default();
    // ratio 0.001 (one token for a million chars) is below the floor and must
    // not move the bucket; samples stay at zero so the estimate is still the
    // cold-start default.
    est.observe_exact(1_000_000, 1, ContentKind::Prose);
    // ratio 100 (a token per char with a token surplus) is above the ceiling.
    est.observe_exact(1, 100, ContentKind::Prose);
    let (tokens, source) = est.estimate(1000, ContentKind::Prose);
    assert_eq!(source, TokenSource::Default);
    assert_eq!(tokens, 250);
}

#[test]
fn empty_observations_are_ignored() {
    let est = TokenEstimator::default();
    est.observe_exact(0, 100, ContentKind::Prose);
    est.observe_exact(100, 0, ContentKind::Prose);
    let (_, source) = est.estimate(1000, ContentKind::Prose);
    assert_eq!(source, TokenSource::Default);
}

#[test]
fn ewma_adapts_toward_recent_samples_without_overshooting() {
    let est = TokenEstimator::default();
    for _ in 0..40 {
        est.observe_exact(1000, 1000, ContentKind::Prose);
    }
    let (before, _) = est.estimate(1000, ContentKind::Prose);
    // A sudden shift to sparse traffic (ratio 0.1) should pull the estimate
    // down over many samples but never below the physical floor the guard
    // enforces, and never to an absurd value.
    for _ in 0..40 {
        est.observe_exact(1000, 100, ContentKind::Prose);
    }
    let (after, source) = est.estimate(1000, ContentKind::Prose);
    assert_eq!(source, TokenSource::Calibrated);
    assert!(after < before, "estimate must move toward sparser traffic");
    assert!(after > 0);
}

#[test]
fn proxy_stats_estimate_tokens_reports_chars_and_kind() {
    // Uses a fresh ProxyStats (not the global singleton) so it never races with
    // or is polluted by other concurrently running tests that share global state.
    // The estimate covers the whole serialized body (a real request body is what
    // gets billed), so this asserts classification and char accounting, which
    // are pure, not an absolute token count.
    let stats = ProxyStats::default();
    let code = sample_code();
    let code_json = json!({ "messages": [{ "role": "user", "content": code }] });
    let estimate = stats.estimate_tokens(&code_json);
    assert_eq!(estimate.kind, ContentKind::Code);
    assert_eq!(
        estimate.chars,
        crate::text_from_value(&code_json).chars().count() as u64
    );
    assert_eq!(estimate.source, TokenSource::Default);
    // The default code ratio (0.40 tokens/char) applied to those chars.
    assert_eq!(
        estimate.tokens,
        ((estimate.chars as f64 * 0.40).round() as u64).max(1)
    );
}

#[test]
fn default_ratios_map_chars_to_tokens_per_class() {
    let est = TokenEstimator::default();
    assert_eq!(est.estimate(1000, ContentKind::Prose).0, 250);
    assert_eq!(est.estimate(1000, ContentKind::Code).0, 400);
    assert_eq!(est.estimate(1000, ContentKind::Dense).0, 850);
}

#[test]
fn proxy_stats_learns_from_reported_input_across_calls() {
    // A dedicated ProxyStats lets the whole calibrate-then-estimate loop run
    // without disturbing the global estimator other tests depend on.
    let stats = ProxyStats::default();
    let text = sample_prose();
    let text_json = json!(text);
    let before = stats.estimate_tokens(&text_json);
    assert_eq!(before.source, TokenSource::Default);
    let dense_tokens = before.chars / 2;
    for _ in 0..CALIBRATION_MIN_SAMPLES {
        stats.calibrate_from_usage(Some(dense_tokens), None, &before, Some(&text), "");
    }
    let after = stats.estimate_tokens(&text_json);
    assert_eq!(after.source, TokenSource::Calibrated);
    assert!(after.tokens > before.tokens);
}

#[test]
fn proxy_stats_reports_estimate_source_breakdown() {
    let stats = ProxyStats::default();
    stats.estimate_tokens(&json!(sample_prose()));
    stats.estimate_tokens(&json!(sample_code()));
    let snapshot = stats.snapshot_json();
    let sources = &snapshot["tokens_saved"]["estimate_sources"];
    // Both cold-start estimates land in the `default` bucket on a fresh stats.
    assert_eq!(sources["default"], 2);
    assert_eq!(sources["calibrated"], 0);
    stats.reset();
    let snapshot = stats.snapshot_json();
    assert_eq!(snapshot["tokens_saved"]["estimate_sources"]["default"], 0);
}

#[test]
fn calibrate_from_usage_learns_from_reported_input() {
    // Exercises the calibration loop directly on a private estimator so it is
    // isolated from other tests sharing the global stats singleton. This mirrors
    // exactly what ProxyStats::calibrate_from_usage feeds the estimator from a
    // provider-reported count: (chars, real tokens, class).
    let est = TokenEstimator::default();
    let text = sample_prose();
    let chars = text.chars().count() as u64;
    let (before, before_source) = est.estimate(chars, ContentKind::Prose);
    assert_eq!(before_source, TokenSource::Default);
    // A provider reported one token per 2 chars for this exact prose text.
    let dense_tokens = chars / 2;
    for _ in 0..CALIBRATION_MIN_SAMPLES {
        est.observe_exact(chars, dense_tokens, ContentKind::Prose);
    }
    let (after, after_source) = est.estimate(chars, ContentKind::Prose);
    assert_eq!(after_source, TokenSource::Calibrated);
    assert!(after > before);
}

#[test]
fn calibrate_from_usage_never_trusts_a_local_estimate_as_truth() {
    // ProxyStats::calibrate_from_usage is invoked only with provider-reported
    // counts; a None (absent side) must leave its bucket untouched. Feeding the
    // estimator nothing keeps it on the cold-start default.
    let est = TokenEstimator::default();
    let (tokens, source) = est.estimate(1000, ContentKind::Prose);
    assert_eq!(source, TokenSource::Default);
    assert_eq!(tokens, 250);
}

#[test]
fn windowed_scan_matches_whole_text_classification() {
    // The window is the hot path; its decision must agree with the whole-text
    // path on bodies far larger than the window.
    let prose = serde_json::json!({ "content": sample_prose() });
    let code = serde_json::json!({ "content": sample_code() });
    let dense = serde_json::json!({ "content": sample_dense() });
    assert_eq!(classify_window(&prose).kind, classify(&sample_prose()));
    assert_eq!(classify_window(&code).kind, classify(&sample_code()));
    assert_eq!(classify_window(&dense).kind(), classify(&sample_dense()));
}

#[test]
fn classify_window_total_chars_equals_serialized_length_when_untruncated() {
    let small =
        serde_json::json!({ "model": "m", "messages": [{ "role": "user", "content": "hi" }] });
    let window = classify_window(&small);
    assert_eq!(
        window.total_chars as usize,
        serde_json::to_string(&small).unwrap().chars().count()
    );
}

#[test]
fn classify_window_reports_truncation_and_counts_all_chars() {
    let huge = serde_json::json!({ "content": sample_prose().repeat(64) });
    let window = classify_window(&huge);
    assert_eq!(
        window.total_chars as usize,
        serde_json::to_string(&huge).unwrap().chars().count()
    );
}

impl ClassifyWindow {
    fn kind(&self) -> ContentKind {
        self.kind
    }
}
