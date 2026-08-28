//! Per-project model ranking and cost insight.
//!
//! Public benchmarks rank models on generic tasks. The proxy sees the request,
//! the response, and the aftermath — whether the same question was immediately
//! re-asked, whether the route failed — on real work in a real repository.
//! That is free implicit labelling, and it yields a ranking for *this* project
//! rather than for a leaderboard's idea of an average one.
//!
//! Every number here is stated with its sample size, because a rank derived
//! from four requests is noise wearing a number's clothes.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Value, json};

/// Below this many answers a row is reported but never ranked: the score would
/// be dominated by whichever way the first couple of requests happened to go.
pub(crate) const MIN_RANKED_ANSWERS: u64 = 20;

/// One (provider, model) pair's observed record.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Record {
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) answers: u64,
    /// The user immediately re-asked the same question a different way, with
    /// no intervening tool use. The clearest dissatisfaction signal available.
    pub(crate) retried: u64,
    /// The route answered with an error, or its stream broke.
    pub(crate) failed: u64,
    pub(crate) total_time_ms: u64,
    /// Spend at currently configured rates, or `None` when unpriced.
    pub(crate) cost: Option<f64>,
}

impl Record {
    /// Share of answers that were neither re-asked nor failed.
    pub(crate) fn satisfaction(&self) -> Option<f64> {
        if self.answers == 0 {
            return None;
        }
        let bad = self.retried.saturating_add(self.failed).min(self.answers);
        Some(1.0 - bad as f64 / self.answers as f64)
    }

    pub(crate) fn cost_per_answer(&self) -> Option<f64> {
        let cost = self.cost?;
        (self.answers > 0).then(|| cost / self.answers as f64)
    }

    pub(crate) fn mean_ms(&self) -> Option<u64> {
        (self.answers > 0).then(|| self.total_time_ms / self.answers)
    }

    fn to_json(&self, rank: Option<usize>) -> Value {
        json!({
            "rank": rank,
            "provider": self.provider,
            "model": self.model,
            "answers": self.answers,
            "retried": self.retried,
            "failed": self.failed,
            "satisfaction": self.satisfaction(),
            "cost": self.cost,
            "cost_per_answer": self.cost_per_answer(),
            "mean_ms": self.mean_ms(),
            // Everything below the threshold is shown but not ranked, so a
            // barely-used model cannot top the table on three lucky requests.
            "ranked": rank.is_some(),
        })
    }
}

/// Rank by observed satisfaction, then by cost, then by latency.
///
/// Cost breaks ties rather than driving the order: a cheap route that gets
/// re-asked is not cheap.
pub(crate) fn rank(records: &[Record]) -> Value {
    let mut ranked = records
        .iter()
        .filter(|record| record.answers >= MIN_RANKED_ANSWERS)
        .cloned()
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        let satisfaction = right
            .satisfaction()
            .unwrap_or(0.0)
            .total_cmp(&left.satisfaction().unwrap_or(0.0));
        satisfaction
            .then_with(|| {
                left.cost_per_answer()
                    .unwrap_or(f64::MAX)
                    .total_cmp(&right.cost_per_answer().unwrap_or(f64::MAX))
            })
            .then_with(|| {
                left.mean_ms()
                    .unwrap_or(u64::MAX)
                    .cmp(&right.mean_ms().unwrap_or(u64::MAX))
            })
            .then_with(|| left.model.cmp(&right.model))
    });
    let ranks = ranked
        .iter()
        .enumerate()
        .map(|(index, record)| ((record.provider.clone(), record.model.clone()), index + 1))
        .collect::<HashMap<_, _>>();

    let mut rows = records.to_vec();
    rows.sort_by(|left, right| {
        let left_rank = ranks.get(&(left.provider.clone(), left.model.clone()));
        let right_rank = ranks.get(&(right.provider.clone(), right.model.clone()));
        match (left_rank, right_rank) {
            (Some(left), Some(right)) => left.cmp(right),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => right.answers.cmp(&left.answers),
        }
    });
    json!({
        "min_ranked_answers": MIN_RANKED_ANSWERS,
        "models": rows
            .iter()
            .map(|record| {
                record.to_json(ranks.get(&(record.provider.clone(), record.model.clone())).copied())
            })
            .collect::<Vec<_>>(),
    })
}

/// Words that carry no routing signal. Kept short deliberately: a longer list
/// would start deciding what matters instead of observing it.
const STOP_WORDS: &[&str] = &[
    "the", "and", "for", "that", "this", "with", "from", "into", "have", "has", "was", "are",
    "you", "your", "our", "not", "but", "can", "will", "would", "should", "could", "please",
    "just", "get", "set", "use", "using", "make", "want", "need", "help", "what", "when", "where",
    "why", "how", "all", "any", "its", "it's", "then", "than", "there", "here", "them", "they",
    "these", "those", "some", "more", "most", "also", "about", "which", "while", "does", "did",
    "let", "like", "one", "two", "new", "now",
];

/// Terms that appear far more often in one target's prompts than across the
/// whole corpus — a proposed hint derived from observed clusters rather than
/// guessed at by hand.
///
/// This is a frequency contrast, not a topic model. It is offered as a
/// suggestion for a human to edit, never applied.
pub(crate) fn hint_candidates(
    prompts_by_target: &BTreeMap<String, Vec<String>>,
    per_target: usize,
) -> Value {
    let stop = STOP_WORDS.iter().copied().collect::<HashSet<_>>();
    let terms_of = |text: &str| {
        text.split(|character: char| !character.is_alphanumeric() && character != '_')
            .map(str::to_ascii_lowercase)
            .filter(|word| word.len() >= 3 && !stop.contains(word.as_str()))
            .collect::<Vec<_>>()
    };

    // Document frequency across every target, so a term common everywhere
    // cannot be distinctive anywhere.
    let mut corpus: HashMap<String, u64> = HashMap::new();
    let mut per_target_counts: BTreeMap<&String, HashMap<String, u64>> = BTreeMap::new();
    let mut totals: BTreeMap<&String, u64> = BTreeMap::new();
    for (target, prompts) in prompts_by_target {
        let counts = per_target_counts.entry(target).or_default();
        for prompt in prompts {
            for term in terms_of(prompt).into_iter().collect::<HashSet<_>>() {
                *counts.entry(term.clone()).or_default() += 1;
                *corpus.entry(term).or_default() += 1;
            }
        }
        totals.insert(target, prompts.len() as u64);
    }
    let corpus_total: u64 = totals.values().sum();

    let mut out = serde_json::Map::new();
    for (target, counts) in per_target_counts {
        let target_total = *totals.get(target).unwrap_or(&0);
        if target_total == 0 {
            continue;
        }
        let mut scored = counts
            .iter()
            .filter(|(_, count)| **count >= 2)
            .map(|(term, count)| {
                let local = *count as f64 / target_total as f64;
                let global =
                    corpus.get(term).copied().unwrap_or(1) as f64 / corpus_total.max(1) as f64;
                (term.clone(), local / global.max(f64::MIN_POSITIVE), *count)
            })
            .filter(|(_, lift, _)| *lift > 1.2)
            .collect::<Vec<_>>();
        scored.sort_by(|left, right| {
            right
                .1
                .total_cmp(&left.1)
                .then_with(|| right.2.cmp(&left.2))
                .then_with(|| left.0.cmp(&right.0))
        });
        out.insert(
            target.clone(),
            json!({
                "prompts": target_total,
                "terms": scored
                    .into_iter()
                    .take(per_target)
                    .map(|(term, lift, count)| json!({
                        "term": term,
                        "lift": lift,
                        "prompts": count,
                    }))
                    .collect::<Vec<_>>(),
            }),
        );
    }
    Value::Object(out)
}

/// A configured route and what it costs per answer, used to find a cheaper
/// alternative that is not observably worse.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RouteCandidate {
    pub(crate) target: String,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) record: Record,
}

/// Where the money went, and whether observation supports moving any of it.
///
/// A suggestion is only made when the cheaper route has enough evidence behind
/// it and is not observably worse. Cost alone is never a reason to move: a
/// cheap route that gets re-asked costs more than the expensive one.
pub(crate) fn cost_insight(candidates: &[RouteCandidate], currency: Option<&str>) -> Value {
    let priced = candidates
        .iter()
        .filter(|candidate| candidate.record.cost_per_answer().is_some())
        .collect::<Vec<_>>();
    // Rust's f64 sum identity is -0.0, which would render as "-0.00" in a
    // report that has simply seen no spend yet.
    let total: f64 = priced
        .iter()
        .filter_map(|candidate| candidate.record.cost)
        .sum::<f64>()
        + 0.0;

    let cheapest = priced
        .iter()
        .filter(|candidate| candidate.record.answers >= MIN_RANKED_ANSWERS)
        .min_by(|left, right| {
            left.record
                .cost_per_answer()
                .unwrap_or(f64::MAX)
                .total_cmp(&right.record.cost_per_answer().unwrap_or(f64::MAX))
        });

    let mut suggestions = Vec::new();
    if let Some(cheapest) = cheapest {
        let cheap_rate = cheapest.record.cost_per_answer().unwrap_or(f64::MAX);
        let cheap_satisfaction = cheapest.record.satisfaction().unwrap_or(0.0);
        for candidate in &priced {
            if candidate.target == cheapest.target {
                continue;
            }
            if candidate.record.answers < MIN_RANKED_ANSWERS {
                continue;
            }
            let rate = candidate.record.cost_per_answer().unwrap_or(0.0);
            if rate <= cheap_rate {
                continue;
            }
            // Only propose a move the evidence supports: the cheaper route
            // must not be doing observably worse work.
            if cheap_satisfaction + 0.02 < candidate.record.satisfaction().unwrap_or(0.0) {
                continue;
            }
            let saving = (rate - cheap_rate) * candidate.record.answers as f64;
            suggestions.push(json!({
                "target": candidate.target,
                "from": {
                    "provider": candidate.provider,
                    "model": candidate.model,
                    "cost_per_answer": rate,
                    "satisfaction": candidate.record.satisfaction(),
                    "answers": candidate.record.answers,
                },
                "to": {
                    "target": cheapest.target,
                    "provider": cheapest.provider,
                    "model": cheapest.model,
                    "cost_per_answer": cheap_rate,
                    "satisfaction": cheapest.record.satisfaction(),
                    "answers": cheapest.record.answers,
                },
                "observed_saving": saving,
                "share_of_spend": if total > 0.0 {
                    json!(candidate.record.cost.unwrap_or(0.0) / total)
                } else {
                    Value::Null
                },
                // Exactly the body PUT /api/integrated expects, so the admin
                // interface can offer the change as one action.
                "apply": {
                    "target": candidate.target,
                    "provider": cheapest.provider,
                    "model": cheapest.model,
                },
            }));
        }
    }
    suggestions.sort_by(|left, right| {
        right["observed_saving"]
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&left["observed_saving"].as_f64().unwrap_or(0.0))
    });

    json!({
        "currency": currency,
        "total": total,
        "priced_routes": priced.len(),
        "suggestions": suggestions,
    })
}

#[cfg(test)]
#[path = "leaderboard_tests.rs"]
mod tests;
