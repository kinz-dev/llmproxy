//! Routing regression suite.
//!
//! Single-shot route testing answers "where would this one prompt go". A
//! corpus version answers the question that actually matters when a policy is
//! edited: "what did this change move". Capturing real requests, replaying
//! them against an edited prompt or target set, and diffing the decisions makes
//! a prompt edit falsifiable instead of a matter of taste.
//!
//! The corpus is plain JSON so it can live in a repository and be diffed and
//! reviewed like any other fixture.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// One recorded routing decision to replay.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Case {
    pub(crate) prompt: String,
    /// What the recorded run decided: `local`, `pass`, `default_route`,
    /// `redirect`, and so on. Absent means the case is new and only has to
    /// produce *some* decision.
    #[serde(default)]
    pub(crate) decision: Option<String>,
    /// The target the recorded run chose, when it chose one.
    #[serde(default)]
    pub(crate) target: Option<String>,
    /// Free-text note for whoever reads the fixture later.
    #[serde(default)]
    pub(crate) note: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Corpus {
    #[serde(default)]
    pub(crate) mode: Option<String>,
    #[serde(default)]
    pub(crate) cases: Vec<Case>,
}

impl Corpus {
    pub(crate) fn parse(raw: &str) -> Result<Self, String> {
        // A bare array of cases is accepted too, because that is what falls
        // out of exporting the decision log.
        if let Ok(cases) = serde_json::from_str::<Vec<Case>>(raw) {
            return Ok(Self { mode: None, cases });
        }
        serde_json::from_str(raw).map_err(|error| format!("invalid route corpus: {error}"))
    }
}

/// Build a corpus from exported decision-log rows, keeping only the rows that
/// record a decision worth asserting on.
pub(crate) fn from_decisions(rows: &[Value], mode: Option<&str>) -> Corpus {
    let cases = rows
        .iter()
        .filter_map(|row| {
            let prompt = row.get("prompt")?.as_str()?.trim().to_string();
            if prompt.is_empty() || prompt == "(no user text)" {
                return None;
            }
            let decision = row.get("decision").and_then(Value::as_str)?;
            // Only outcomes a replay can reproduce. A fallback or an error
            // records what the provider did that day, not what the policy says.
            if !matches!(
                decision,
                "local" | "pass" | "default_route" | "redirect" | "reject" | "sticky" | "cached"
            ) {
                return None;
            }
            Some(Case {
                prompt,
                decision: Some(normalize_decision(decision).to_string()),
                target: row
                    .get("target")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                note: None,
            })
        })
        .collect::<Vec<_>>();
    Corpus {
        mode: mode.map(str::to_string),
        cases: dedupe(cases),
    }
}

/// Decisions that mean the same destination are compared as one.
///
/// A sticky reuse and a cached replay are both "this went to that target"; the
/// mechanism that got it there is not what a policy edit is being judged on.
fn normalize_decision(decision: &str) -> &str {
    match decision {
        "sticky" | "cached" | "redirect" | "verb_route" => "redirect",
        "pass" | "default_route" => "default_route",
        other => other,
    }
}

fn dedupe(cases: Vec<Case>) -> Vec<Case> {
    let mut seen = std::collections::HashSet::new();
    cases
        .into_iter()
        .filter(|case| seen.insert(case.prompt.clone()))
        .collect()
}

/// The verdict for one replayed case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Matched what the corpus recorded.
    Same,
    /// Differs from what the corpus recorded: a regression to look at.
    Changed,
    /// The corpus recorded nothing to compare against.
    New,
    /// The replay could not produce a decision at all.
    Error,
}

impl Verdict {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Same => "same",
            Self::Changed => "changed",
            Self::New => "new",
            Self::Error => "error",
        }
    }
}

/// Compare one replayed verdict against its recorded case.
pub(crate) fn compare(case: &Case, replayed: &Value) -> (Verdict, Value) {
    let decision = replayed.get("decision").and_then(Value::as_str);
    let target = replayed.get("target").and_then(Value::as_str);
    let row = json!({
        "prompt": case.prompt,
        "expected": {
            "decision": case.decision,
            "target": case.target,
        },
        "actual": {
            "decision": decision,
            "target": target,
            "detail": replayed.get("detail").cloned().unwrap_or(Value::Null),
            "elapsed_ms": replayed.get("elapsed_ms").cloned().unwrap_or(Value::Null),
        },
        "note": case.note,
    });
    let Some(decision) = decision else {
        return (Verdict::Error, tagged(row, Verdict::Error));
    };
    if decision == "error" {
        return (Verdict::Error, tagged(row, Verdict::Error));
    }
    let Some(expected) = case.decision.as_deref() else {
        return (Verdict::New, tagged(row, Verdict::New));
    };
    let same_decision = normalize_decision(decision) == normalize_decision(expected);
    // A target only has to match when the recorded run named one.
    let same_target = match (&case.target, target) {
        (Some(expected), Some(actual)) => expected == actual,
        (Some(_), None) => false,
        (None, _) => true,
    };
    let verdict = if same_decision && same_target {
        Verdict::Same
    } else {
        Verdict::Changed
    };
    (verdict, tagged(row, verdict))
}

fn tagged(mut row: Value, verdict: Verdict) -> Value {
    row["verdict"] = json!(verdict.as_str());
    row
}

/// Roll up per-case rows into the summary a CI run reports on.
pub(crate) fn summarize(rows: Vec<Value>, mode: &str) -> Value {
    let count = |name: &str| rows.iter().filter(|row| row["verdict"] == name).count();
    let changed = count("changed");
    let errors = count("error");
    json!({
        "mode": mode,
        "cases": rows.len(),
        "same": count("same"),
        "changed": changed,
        "new": count("new"),
        "errors": errors,
        // What a CI job keys off: a replay is a regression when a decision
        // moved, not when a new case simply has nothing to compare against.
        "regressions": changed + errors,
        "results": rows,
    })
}

#[cfg(test)]
#[path = "route_corpus_tests.rs"]
mod tests;
