//! Embedding-cached routing, route drift, and retry detection.
//!
//! `RoutingDecisionAudit` already records the prompt, the decision, the target,
//! and the reason for every request and then throws it away. Embedding the
//! request and matching it against past decisions turns that record into a
//! routing cache: triage cost drops, the hit path costs one local embedding,
//! and identical requests route identically by construction.
//!
//! The same vectors answer two other questions that have no other source:
//! whether an actively worked session has drifted far enough from the request
//! that pinned its route to deserve re-triage, and whether the newest turn is
//! the user re-asking the same question a different way, which is the clearest
//! dissatisfaction signal a proxy can see.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

/// A unit-length embedding. Normalising once makes every later similarity a
/// dot product, which matters because lookups are on the request path.
pub(crate) type Vector = Arc<Vec<f32>>;

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct EmbeddingConfig {
    pub(crate) enabled: bool,
    /// Provider serving `/v1/embeddings`. A local one keeps the hit path free.
    pub(crate) provider: String,
    pub(crate) model: String,
    /// Cosine similarity at or above which a past decision is reused.
    pub(crate) threshold: f32,
    pub(crate) capacity: usize,
    pub(crate) ttl_secs: u64,
    /// Re-run routing when the newest request has drifted below this
    /// similarity to the one that pinned the sticky route. Zero disables it,
    /// which leaves the time and turn bounds as the only triggers.
    pub(crate) retriage_below: f32,
    /// Treat a near-identical repeat of the previous user turn, with no
    /// intervening tool use, as dissatisfaction and escalate the route.
    pub(crate) retry_escalation: bool,
    pub(crate) retry_threshold: f32,
    /// Embedding is on the request path, so it gets its own short deadline
    /// rather than the provider's generation timeout.
    pub(crate) timeout_ms: u64,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: String::new(),
            model: String::new(),
            threshold: 0.94,
            capacity: 2048,
            ttl_secs: 24 * 60 * 60,
            retriage_below: 0.0,
            retry_escalation: false,
            retry_threshold: 0.93,
            timeout_ms: 2_000,
        }
    }
}

impl EmbeddingConfig {
    /// Whether anything at all needs an embedding computed for a request.
    pub(crate) fn is_active(&self) -> bool {
        self.enabled && !self.provider.is_empty() && !self.model.is_empty()
    }

    pub(crate) fn validate(&self, has_provider: impl Fn(&str) -> bool) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if self.provider.trim().is_empty() || self.model.trim().is_empty() {
            return Err(
                "integrated.embedding requires both a provider and a model when enabled"
                    .to_string(),
            );
        }
        if !has_provider(&self.provider) {
            return Err(format!(
                "integrated.embedding references unknown provider {:?}",
                self.provider
            ));
        }
        for (name, value) in [
            ("threshold", self.threshold),
            ("retry_threshold", self.retry_threshold),
        ] {
            if !(0.0..=1.0).contains(&value) || !value.is_finite() {
                return Err(format!(
                    "integrated.embedding.{name} must be between 0.0 and 1.0"
                ));
            }
        }
        if !(0.0..=1.0).contains(&self.retriage_below) || !self.retriage_below.is_finite() {
            return Err(
                "integrated.embedding.retriage_below must be between 0.0 and 1.0".to_string(),
            );
        }
        if self.capacity == 0 {
            return Err("integrated.embedding.capacity must be greater than zero".to_string());
        }
        if self.timeout_ms == 0 {
            return Err("integrated.embedding.timeout_ms must be greater than zero".to_string());
        }
        Ok(())
    }
}

/// What a cached decision replays. Only outcomes that are safe to repeat
/// without the triage model are stored: an invalid or rejected classification
/// must always be re-derived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CachedDecision {
    Local,
    DefaultRoute,
    Redirect(String),
}

impl CachedDecision {
    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::Local => "local",
            Self::DefaultRoute => "default_route",
            Self::Redirect(target) => target,
        }
    }
}

struct CacheEntry {
    vector: Vector,
    decision: CachedDecision,
    reason: String,
    inserted_at: Instant,
    hits: u64,
}

#[derive(Default)]
pub(crate) struct RouteCache {
    entries: Mutex<VecDeque<CacheEntry>>,
    lookups: Mutex<CacheCounters>,
}

#[derive(Default, Clone, Copy)]
struct CacheCounters {
    lookups: u64,
    hits: u64,
    inserts: u64,
    evictions: u64,
}

pub(crate) fn route_cache() -> &'static RouteCache {
    static CACHE: OnceLock<RouteCache> = OnceLock::new();
    CACHE.get_or_init(RouteCache::default)
}

/// A cache hit: the decision to replay and how close the match was.
pub(crate) struct CacheHit {
    pub(crate) decision: CachedDecision,
    pub(crate) similarity: f32,
    pub(crate) reason: String,
}

impl RouteCache {
    /// Nearest cached decision at or above `threshold`, if any.
    pub(crate) fn lookup(
        &self,
        vector: &Vector,
        config: &EmbeddingConfig,
        is_valid: impl Fn(&CachedDecision) -> bool,
    ) -> Option<CacheHit> {
        let ttl = Duration::from_secs(config.ttl_secs);
        let mut entries = self.entries.lock().expect("route cache poisoned");
        let mut counters = self.lookups.lock().expect("route cache counters poisoned");
        counters.lookups = counters.lookups.saturating_add(1);
        entries.retain(|entry| entry.inserted_at.elapsed() < ttl);

        let mut best: Option<(usize, f32)> = None;
        for (index, entry) in entries.iter().enumerate() {
            if !is_valid(&entry.decision) {
                continue;
            }
            let similarity = dot(vector, &entry.vector);
            if similarity >= config.threshold && best.is_none_or(|(_, top)| similarity > top) {
                best = Some((index, similarity));
            }
        }
        let (index, similarity) = best?;
        let entry = &mut entries[index];
        entry.hits = entry.hits.saturating_add(1);
        counters.hits = counters.hits.saturating_add(1);
        Some(CacheHit {
            decision: entry.decision.clone(),
            similarity,
            reason: entry.reason.clone(),
        })
    }

    pub(crate) fn insert(
        &self,
        vector: Vector,
        decision: CachedDecision,
        reason: String,
        config: &EmbeddingConfig,
    ) {
        let mut entries = self.entries.lock().expect("route cache poisoned");
        let mut counters = self.lookups.lock().expect("route cache counters poisoned");
        counters.inserts = counters.inserts.saturating_add(1);
        while entries.len() >= config.capacity {
            entries.pop_front();
            counters.evictions = counters.evictions.saturating_add(1);
        }
        entries.push_back(CacheEntry {
            vector,
            decision,
            reason,
            inserted_at: Instant::now(),
            hits: 0,
        });
    }

    /// Drop cached decisions naming a target that no longer exists, so a
    /// config edit cannot be replayed into a dead route.
    pub(crate) fn retain_valid(&self, is_valid: impl Fn(&CachedDecision) -> bool) {
        self.entries
            .lock()
            .expect("route cache poisoned")
            .retain(|entry| is_valid(&entry.decision));
    }

    pub(crate) fn clear(&self) {
        self.entries.lock().expect("route cache poisoned").clear();
        *self.lookups.lock().expect("route cache counters poisoned") = CacheCounters::default();
    }

    pub(crate) fn snapshot_json(&self) -> Value {
        let entries = self.entries.lock().expect("route cache poisoned");
        let counters = *self.lookups.lock().expect("route cache counters poisoned");
        let mut by_decision: std::collections::BTreeMap<String, u64> = Default::default();
        for entry in entries.iter() {
            *by_decision
                .entry(entry.decision.as_str().to_string())
                .or_default() += 1;
        }
        json!({
            "entries": entries.len(),
            "lookups": counters.lookups,
            "hits": counters.hits,
            "inserts": counters.inserts,
            "evictions": counters.evictions,
            "hit_rate": if counters.lookups == 0 {
                Value::Null
            } else {
                json!(counters.hits as f64 / counters.lookups as f64)
            },
            "by_decision": by_decision,
        })
    }
}

/// Unit-normalise a raw embedding. A zero vector is rejected rather than
/// normalised, because every similarity against it would be meaningless.
pub(crate) fn normalize(values: Vec<f32>) -> Option<Vector> {
    let norm = values.iter().map(|value| value * value).sum::<f32>().sqrt();
    if !norm.is_finite() || norm <= f32::EPSILON {
        return None;
    }
    Some(Arc::new(
        values.into_iter().map(|value| value / norm).collect(),
    ))
}

/// Cosine similarity of two already-normalised vectors. Different dimensions
/// mean different models, which is not comparable, so it scores zero.
pub(crate) fn dot(left: &[f32], right: &[f32]) -> f32 {
    if left.len() != right.len() {
        return 0.0;
    }
    left.iter()
        .zip(right.iter())
        .map(|(a, b)| a * b)
        .sum::<f32>()
        .clamp(-1.0, 1.0)
}

/// Pull the embedding out of an OpenAI-shaped `/v1/embeddings` response.
pub(crate) fn vector_from_response(response: &Value) -> Option<Vector> {
    let values = response
        .get("data")
        .and_then(Value::as_array)?
        .first()?
        .get("embedding")?
        .as_array()?
        .iter()
        .map(|value| value.as_f64().map(|value| value as f32))
        .collect::<Option<Vec<f32>>>()?;
    normalize(values)
}

/// The user turn before the newest one, when nothing happened between them
/// except conversation.
///
/// Intervening tool use means the agent made progress, so a similar follow-up
/// is continuation rather than dissatisfaction and there is nothing to compare.
pub(crate) fn previous_user_turn(chat_request: &Value) -> Option<String> {
    let messages = chat_request.get("messages")?.as_array()?;
    let mut user_indexes = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.get("role").and_then(Value::as_str) == Some("user"))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let newest = user_indexes.pop()?;
    let previous = user_indexes.pop()?;
    for message in &messages[previous + 1..newest] {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        let called_tools = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|calls| !calls.is_empty());
        if role == "tool" || role == "function" || called_tools {
            return None;
        }
    }
    let text = |index: usize| {
        messages[index]
            .get("content")
            .map(crate::text_from_value)
            .filter(|text| !text.trim().is_empty())
    };
    let _ = newest;
    text(previous)
}

#[cfg(test)]
#[path = "route_cache_tests.rs"]
mod tests;
