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
/// materializing the whole body: serialization captures a fixed stack prefix
/// while counting the full payload in the same pass.
pub(crate) fn classify_window(value: &serde_json::Value) -> ClassifyWindow {
    let mut buf = [0_u8; CLASSIFY_WINDOW_BYTES];
    let mut writer = WindowWriter {
        buf: &mut buf,
        len: 0,
        total_chars: 0,
    };
    // Continue counting after the prefix fills. Returning an overflow error
    // allocated both an IO error and a serde error on large prompts, then
    // required a second serialization to count their tail.
    let _ = serde_json::to_writer(&mut writer, value);
    let prefix = &writer.buf[..writer.len];
    let text = std::str::from_utf8(prefix).unwrap_or_else(|error| {
        // The bounded prefix may end inside a multi-byte character.
        std::str::from_utf8(&prefix[..error.valid_up_to()]).expect("valid UTF-8 prefix")
    });
    let (structural, non_latin, latin_letters, newlines, indented, _) = scan_structural(text);
    let chars = text.chars().count() as u64;
    ClassifyWindow {
        kind: decide_kind(
            structural,
            non_latin,
            latin_letters,
            newlines,
            indented,
            chars,
        ),
        total_chars: writer.total_chars,
    }
}

struct WindowWriter<'a> {
    buf: &'a mut [u8; CLASSIFY_WINDOW_BYTES],
    len: usize,
    total_chars: u64,
}

impl std::io::Write for WindowWriter<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        // UTF-8 characters have exactly one non-continuation byte. This also
        // counts correctly if a serializer splits a character across writes.
        self.total_chars += data.iter().filter(|byte| **byte & 0xc0 != 0x80).count() as u64;
        let copy = data.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + copy].copy_from_slice(&data[..copy]);
        self.len += copy;
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
