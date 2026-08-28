//! Asking two providers the same question.
//!
//! Two features share one mechanism. A **second opinion** is opt-in per target
//! and shown to the caller: agreement is signal, and divergence is a stronger
//! signal that nothing else surfaces. A **shadow request** is sampled, invisible
//! to the caller, and exists only to turn provider choice into evidence.
//!
//! Both give up streaming for the requests they touch, because a diff needs
//! both complete answers. Second opinion is per-target and deliberate; shadow
//! sampling is one-in-N and should stay low for that reason.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::Deserialize;
use serde_json::Value;

/// How a second opinion is presented.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SecondOpinionMode {
    /// Show both answers, labelled, and let the reader judge.
    #[default]
    Present,
    /// Have a third provider produce one answer, naming the disagreements.
    Reconcile,
}

impl SecondOpinionMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::Reconcile => "reconcile",
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct SecondOpinionConfig {
    /// The other target to ask. Must be a configured integrated target.
    pub(crate) target: String,
    #[serde(default)]
    pub(crate) mode: SecondOpinionMode,
    /// Target that reconciles the two answers. Required for `reconcile`.
    #[serde(default)]
    pub(crate) reconciler: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct ShadowConfig {
    pub(crate) enabled: bool,
    /// Target the sampled copy is sent to.
    pub(crate) target: String,
    /// Sample one request in this many. Sampling is a counter, not a random
    /// draw, so the rate is exact and a replay is reproducible.
    pub(crate) sample_one_in: u64,
    pub(crate) timeout_ms: u64,
}

impl Default for ShadowConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            target: String::new(),
            sample_one_in: 20,
            timeout_ms: 120_000,
        }
    }
}

impl ShadowConfig {
    pub(crate) fn is_active(&self) -> bool {
        self.enabled && !self.target.is_empty() && self.sample_one_in > 0
    }

    pub(crate) fn validate(&self, has_target: impl Fn(&str) -> bool) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if self.target.trim().is_empty() {
            return Err(
                "integrated.shadow.target is required when shadowing is enabled".to_string(),
            );
        }
        if !has_target(&self.target) {
            return Err(format!(
                "integrated.shadow.target {:?} is not a configured integrated target",
                self.target
            ));
        }
        if self.sample_one_in == 0 {
            return Err("integrated.shadow.sample_one_in must be greater than zero".to_string());
        }
        if self.timeout_ms == 0 {
            return Err("integrated.shadow.timeout_ms must be greater than zero".to_string());
        }
        Ok(())
    }
}

/// Exact one-in-N sampling from a monotonic counter. A pseudo-random draw would
/// need a dependency and would make the observed rate only approximately right.
#[derive(Default)]
pub(crate) struct Sampler {
    seen: AtomicU64,
}

impl Sampler {
    pub(crate) fn take(&self, one_in: u64) -> bool {
        if one_in == 0 {
            return false;
        }
        let seen = self.seen.fetch_add(1, Ordering::Relaxed);
        seen.is_multiple_of(one_in)
    }
}

pub(crate) fn shadow_sampler() -> &'static Sampler {
    static SAMPLER: std::sync::OnceLock<Sampler> = std::sync::OnceLock::new();
    SAMPLER.get_or_init(Sampler::default)
}

/// Assistant text of a chat completion, or the empty string.
pub(crate) fn answer_text(completion: &Value) -> String {
    completion
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .map(crate::text_from_value)
        .unwrap_or_default()
}

/// Whether a completion asked for a tool call.
///
/// A composed or reconciled answer is text, so it would silently drop a tool
/// call. Comparison is skipped entirely when either side wants to act.
pub(crate) fn wants_tool_call(completion: &Value) -> bool {
    completion
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("tool_calls"))
        .and_then(Value::as_array)
        .is_some_and(|calls| !calls.is_empty())
}

/// A cheap, dependency-free agreement score: the Jaccard overlap of the two
/// answers' word sets.
///
/// It is deliberately coarse. It is not a semantic judgement and must be
/// presented as what it is — a hint that two answers used the same words — so
/// that a reader treats divergence as a prompt to look, not as a verdict.
pub(crate) fn word_overlap(left: &str, right: &str) -> f32 {
    let words = |text: &str| {
        text.split(|character: char| !character.is_alphanumeric())
            .filter(|word| word.len() > 2)
            .map(str::to_ascii_lowercase)
            .collect::<std::collections::HashSet<_>>()
    };
    let left = words(left);
    let right = words(right);
    if left.is_empty() && right.is_empty() {
        return 1.0;
    }
    let union = left.union(&right).count();
    if union == 0 {
        return 0.0;
    }
    left.intersection(&right).count() as f32 / union as f32
}

/// Compose two answers into one message for the caller.
pub(crate) fn present_both(
    primary_label: &str,
    primary_answer: &str,
    second_label: &str,
    second_answer: &str,
) -> String {
    let overlap = word_overlap(primary_answer, second_answer);
    let verdict = if overlap >= 0.6 {
        "The two answers overlap heavily; treat that as agreement."
    } else if overlap >= 0.3 {
        "The two answers partly overlap; the differences are worth reading."
    } else {
        "The two answers diverge. Divergence is the stronger signal: read both before acting."
    };
    format!(
        "Two providers answered this independently. {verdict}\n\n\
         ## {primary_label}\n\n{primary_answer}\n\n\
         ## {second_label}\n\n{second_answer}"
    )
}

/// Prompt for the reconciling provider. It is told to name the disagreements
/// rather than smooth them over, because a smoothed answer would destroy the
/// only thing a second opinion produces.
pub(crate) fn reconcile_prompt(
    question: &str,
    primary_label: &str,
    primary_answer: &str,
    second_label: &str,
    second_answer: &str,
) -> String {
    format!(
        "Two providers answered the same request independently. Produce one answer for the user.\n\n\
         Rules:\n\
         - Where they agree, answer directly and without hedging.\n\
         - Where they disagree, say so explicitly, state which position you take, and why.\n\
         - Never invent a third position that neither answer supports.\n\
         - Do not mention this reconciliation process beyond noting disagreements.\n\n\
         Original request:\n{question}\n\n\
         Answer from {primary_label}:\n{primary_answer}\n\n\
         Answer from {second_label}:\n{second_answer}"
    )
}

#[cfg(test)]
#[path = "comparison_tests.rs"]
mod tests;
