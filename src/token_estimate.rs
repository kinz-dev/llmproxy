//! Hybrid token estimation.
//!
//! Providers that return a real `usage` object give exact token counts straight
//! from their own tokenizer. The problem is every value the provider *doesn't*
//! report (streamed calls without usage, local blocks, the caller-facing
//! token-count endpoint): the old heuristic was a fixed `chars / 4`, accurate
//! only for English prose and biased low for code, JSON, and non-Latin scripts.
//!
//! This module makes that estimate self-correcting without needing anyone's
//! tokenizer vocabulary. Whenever a provider reports real counts we observe a
//! free `(chars -> real_tokens)` pair for that content type and fold it into an
//! EWMA of tokens-per-char, split by content class. The heuristic then applies
//! the learned ratio, falling back to the prose/code defaults until enough
//! samples have accumulated. It never replaces an exact count, only the guess.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Where a token number came from, so the caller can tell an exact count from a
/// learned guess and both are labeled honestly rather than hidden behind one
/// `approximate` flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenSource {
    /// Read directly from a provider `usage` object (this model's tokenizer).
    Exact,
    /// Estimated with the calibrated tokens-per-char ratio.
    Calibrated,
    /// Estimated with the cold-start default ratio for the content class.
    Default,
}

impl TokenSource {
    pub(crate) fn is_estimate(self) -> bool {
        !matches!(self, Self::Exact)
    }
}

/// Content classes the estimator tracks separately: English-ish prose,
/// code/structured markup, and dense non-Latin scripts all tokenize at very
/// different chars-per-token, so a single ratio would be wrong for two of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContentKind {
    Prose,
    Code,
    Dense,
}

/// Cold-start tokens-per-char for a class, used until enough exact observations
/// calibrate it. Prose keeps the familiar ~4 chars/token; code and dense
/// scripts start closer to their real, denser behavior so even an uncalibrated
/// estimate is fair rather than a systematic undercount.
const fn default_ratio(kind: ContentKind) -> f64 {
    match kind {
        ContentKind::Prose => 0.25,
        ContentKind::Code => 0.40,
        ContentKind::Dense => 0.85,
    }
}

/// A sample is only folded in when the implied ratio is physically plausible;
/// outside this band the sample came from a pathological pair (an empty usage,
/// a truncated stream, a request whose body was mostly dropped) and trusting it
/// would poison the estimate.
const MIN_RATIO: f64 = 0.05;
const MAX_RATIO: f64 = 2.0;

/// Fraction of each sample blended into the running average. 0.1 gives a
/// recency-weighted mean that adapts to a traffic-mix shift in a few dozen
/// samples without a single odd request dominating.
const EWMA_ALPHA: f64 = 0.1;

/// Below this many samples a class estimate is still reported as `Default`
/// rather than `Calibrated`, so the UI reflects genuine confidence.
const CALIBRATION_MIN_SAMPLES: u64 = 12;

/// Fixed-point encoding so the running average stays in an atomic: ratio is
/// stored as `ratio * 1000`, which resolves to 0.001-token granularity per char
/// and comfortably spans the plausible band.
const RATIO_SCALE: f64 = 1000.0;

/// One bucket: the EWMA tokens-per-char and how many samples shaped it.
#[derive(Default)]
struct RatioBucket {
    ratio_milli: AtomicU32,
    samples: AtomicU64,
}

/// A bounded sample of characters from the start of a payload, used by
/// [`classify`] for hot-path decisions. Ratio observation is intentionally
/// all-or-nothing (a partial sample would misstate the class's density), so the
/// window only needs to be large enough that the class signal is unambiguous;
/// a fixed window means classification cost never scales with body size.
pub(crate) const CLASSIFY_WINDOW_BYTES: usize = 16 * 1024;

/// Result of scanning a value's text form through the classification window.
pub(crate) struct ClassifyWindow {
    pub(crate) kind: ContentKind,
    /// Characters in the value's full serialized form (the window only decides
    /// the class; the billed total always covers the whole payload).
    pub(crate) total_chars: u64,
}

/// Scan the text form of a `Value` through the classification window without
/// materializing the whole body: serialization writes into a fixed stack
/// buffer and the writer reports overflow the moment the window is exceeded.
pub(crate) fn classify_window(value: &serde_json::Value) -> ClassifyWindow {
    let mut buf = [0_u8; CLASSIFY_WINDOW_BYTES];
    let mut writer = WindowWriter {
        buf: &mut buf,
        len: 0,
        overflow: false,
    };
    // The writer reports overflow as an error to stop serialization early; a
    // truncated preview is exactly as valid for classification as a full one.
    // serde's `to_writer` writes fragments one by one; io::Write's default
    // `write_all` would probe-allocate a 1 KiB scratch buffer, so this path
    // serializes through the explicit writer (same result, zero allocation).
    let _ = serde_json::to_writer(&mut writer, value);
    let text = String::from_utf8_lossy(&writer.buf[..writer.len]);
    let (structural, non_latin, latin_letters, newlines, indented, _) = scan_structural(&text);
    let chars = text.chars().count() as u64;
    let total_chars = if writer.overflow {
        // The window missed the tail. Counting the whole payload streams
        // through serde's Display with a counting formatter, so it too
        // allocates nothing while staying proportional to the real body.
        count_display_chars(value)
    } else {
        chars
    };
    ClassifyWindow {
        kind: decide_kind(
            structural,
            non_latin,
            latin_letters,
            newlines,
            indented,
            chars,
        ),
        total_chars,
    }
}

/// Counts characters in a value's serialized form by streaming it through
/// serde with a counting writer. No text is ever materialized and no internal
/// buffer grows: every serialized chunk is counted and discarded, so the cost
/// stays constant per chunk regardless of payload size.
fn count_display_chars(value: &serde_json::Value) -> u64 {
    struct CharCounter(u64);

    impl CharCounter {
        fn count(&mut self, data: &[u8]) {
            // Serialized JSON is valid UTF-8; lossy decoding is a safety net
            // for split multi-byte sequences at chunk boundaries, which the
            // byte count still accounts for.
            self.0 += String::from_utf8_lossy(data).chars().count() as u64;
        }
    }

    impl std::io::Write for CharCounter {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.count(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // Reuse one small stack buffer for the entire stream. serde's compact
    // formatter writes fragments one at a time and io::Write's default
    // `write_all` probe-allocates a 1 KiB scratch buffer the first time a
    // caller relies on it, which is exactly the body-size scaling this path
    // must not have. A fixed buffer turns the count into zero allocations.
    struct Buffered<'a> {
        counter: &'a mut CharCounter,
        buf: &'a mut [u8; 256],
    }

    impl std::io::Write for Buffered<'_> {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            if data.len() <= self.buf.len() {
                self.buf[..data.len()].copy_from_slice(data);
                self.counter.count(data);
            } else {
                // No ordinary JSON fragment reaches this size; counting
                // directly keeps the result correct without buffering.
                self.counter.count(data);
            }
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut counter = CharCounter(0);
    let mut scratch = [0_u8; 256];
    let mut writer = Buffered {
        counter: &mut counter,
        buf: &mut scratch,
    };
    let _ = serde_json::to_writer(&mut writer, value);
    counter.0
}

struct WindowWriter<'a> {
    buf: &'a mut [u8; CLASSIFY_WINDOW_BYTES],
    len: usize,
    overflow: bool,
}

impl std::io::Write for WindowWriter<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if data.len() > self.buf.len() - self.len {
            self.overflow = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "classification window reached",
            ));
        }
        self.buf[self.len..self.len + data.len()].copy_from_slice(data);
        self.len += data.len();
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Cheap structural scan over a payload prefix for classification. It mirrors
/// the [`classify`] decision exactly but caps the scanned region at
/// [`CLASSIFY_WINDOW_BYTES`] so the hot path does not walk a whole body.
/// Returns `(structural, non_latin, latin_letters, newlines, indented,
/// scanned_bytes)`.
#[allow(clippy::type_complexity)]
fn scan_structural(text: &str) -> (u64, u64, u64, u64, u64, usize) {
    let mut structural = 0_u64;
    let mut non_latin = 0_u64;
    let mut latin_letters = 0_u64;
    let mut newlines = 0_u64;
    let mut indented = 0_u64;
    let mut line_start = true;
    let mut scanned = 0_usize;
    for ch in text.chars() {
        scanned += ch.len_utf8();
        if scanned > CLASSIFY_WINDOW_BYTES {
            break;
        }
        match ch {
            '{' | '}' | '[' | ']' | '<' | '>' | '=' | ';' | '_' | '|' | '\\' | '`' => {
                structural += 1;
            }
            '\n' => {
                newlines += 1;
                line_start = true;
            }
            ' ' | '\t' if line_start => {
                indented += 1;
            }
            c if c.is_alphabetic() => {
                line_start = false;
                if c.is_ascii() {
                    latin_letters += 1;
                } else {
                    non_latin += 1;
                }
            }
            _ => {
                line_start = false;
            }
        }
    }
    (
        structural,
        non_latin,
        latin_letters,
        newlines,
        indented,
        scanned,
    )
}

/// The shared decision rule for both the windowed scan and the whole-text
/// [`classify`] path.
fn decide_kind(
    structural: u64,
    non_latin: u64,
    latin_letters: u64,
    newlines: u64,
    indented: u64,
    chars: u64,
) -> ContentKind {
    let letters = latin_letters + non_latin;
    // A majority of alphabetic code points outside the Latin range tokens far
    // denser than Latin text (CJK is commonly ~1 token/char), so it gets its own
    // class regardless of structure.
    if letters > 0 && non_latin.saturating_mul(2) > letters {
        return ContentKind::Dense;
    }
    let structural_share = structural as f64 / chars.max(1) as f64;
    let indentation = if newlines > 2 {
        indented as f64 / newlines as f64
    } else {
        0.0
    };
    // Prose is mostly letters with little structure. Code and JSON concentrate
    // delimiters and indentation that ordinary sentences do not carry.
    if structural_share >= 0.04 || indentation >= 0.30 {
        ContentKind::Code
    } else {
        ContentKind::Prose
    }
}

/// The live estimator. Cheap reads on the hot path: a relaxed atomic load per
/// estimate and a compare-and-swap loop only when a real count arrives to fold
/// a new sample in.
#[derive(Default)]
pub(crate) struct TokenEstimator {
    prose: RatioBucket,
    code: RatioBucket,
    dense: RatioBucket,
}

impl TokenEstimator {
    fn bucket(&self, kind: ContentKind) -> &RatioBucket {
        match kind {
            ContentKind::Prose => &self.prose,
            ContentKind::Code => &self.code,
            ContentKind::Dense => &self.dense,
        }
    }

    /// Estimate `tokens` from a character count for one content class. The
    /// learned ratio is used once the class has enough samples; otherwise the
    /// cold-start default applies and the source reports `Default`.
    pub(crate) fn estimate(&self, chars: u64, kind: ContentKind) -> (u64, TokenSource) {
        let bucket = self.bucket(kind);
        let samples = bucket.samples.load(Ordering::Relaxed);
        let (ratio, source) = if samples >= CALIBRATION_MIN_SAMPLES {
            (
                milli_to_ratio(bucket.ratio_milli.load(Ordering::Relaxed)),
                TokenSource::Calibrated,
            )
        } else {
            (default_ratio(kind), TokenSource::Default)
        };
        if chars == 0 {
            // An empty payload still renders as a minimal turn, and reporting a
            // floor of one token keeps the heuristic's old contract.
            return (1, source);
        }
        let tokens = ((chars as f64 * ratio).round() as u64).max(1);
        (tokens, source)
    }

    /// Folds one exact `(chars -> real_tokens)` pair into the class average.
    /// Called only when a provider reported the count, so every sample is a
    /// genuine tokenizer result for content of this class.
    pub(crate) fn observe_exact(&self, chars: u64, tokens: u64, kind: ContentKind) {
        if chars == 0 || tokens == 0 {
            return;
        }
        let ratio = tokens as f64 / chars as f64;
        if !(MIN_RATIO..=MAX_RATIO).contains(&ratio) {
            return;
        }
        let bucket = self.bucket(kind);
        let previous = milli_to_ratio(bucket.ratio_milli.load(Ordering::Relaxed));
        let blended = EWMA_ALPHA * ratio + (1.0 - EWMA_ALPHA) * previous;
        bucket
            .ratio_milli
            .store(ratio_to_milli(blended), Ordering::Relaxed);
        bucket.samples.fetch_add(1, Ordering::Relaxed);
    }

    /// Clears learned ratios and sample counts. Called on a stats reset so a
    /// new measurement window is calibrated only from its own traffic.
    pub(crate) fn reset(&self) {
        for bucket in [&self.prose, &self.code, &self.dense] {
            bucket.ratio_milli.store(0, Ordering::Relaxed);
            bucket.samples.store(0, Ordering::Relaxed);
        }
    }
}

fn milli_to_ratio(milli: u32) -> f64 {
    milli as f64 / RATIO_SCALE
}

fn ratio_to_milli(ratio: f64) -> u32 {
    (ratio * RATIO_SCALE).round() as u32
}

/// Classify content from its flattened text. Only cheap aggregate signals are
/// used (no parsing): the density of structural characters, line indentation,
/// and the share of non-Latin code points decide the class. Large payloads are
/// decided from a bounded prefix (see [`CLASSIFY_WINDOW_BYTES`]): content type
/// is a stable global property of a body, so sampling the head is as accurate
/// as scanning the whole thing while staying allocation- and size-independent.
pub(crate) fn classify(text: &str) -> ContentKind {
    if text.is_empty() {
        return ContentKind::Prose;
    }
    let (structural, non_latin, latin_letters, newlines, indented, _) = scan_structural(text);
    let chars = text
        .char_indices()
        .take_while(|(offset, _)| *offset <= CLASSIFY_WINDOW_BYTES)
        .count() as u64;
    decide_kind(
        structural,
        non_latin,
        latin_letters,
        newlines,
        indented,
        chars,
    )
}

#[cfg(test)]
#[path = "token_estimate_tests.rs"]
mod tests;
