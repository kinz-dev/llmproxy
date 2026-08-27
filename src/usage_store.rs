//! Durable per-project usage counters.
//!
//! The in-memory [`crate::stats::ProxyStats`] counters answer "what is
//! happening now" and die with the daemon. Answering "what did I spend this
//! month" needs the same counters to survive a restart, so every completed
//! request is also folded into a small month-bucketed ledger on disk.
//!
//! Only token counts are stored, never prices. Published rates change, and the
//! configuration file is the source of truth for them, so a report prices the
//! stored counters at read time and says which prices it used.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{error, info, warn};

use crate::pricing::{ModelRate, PricingIndex, Spend, TokenCounters, spend_for};
use crate::stats::TokenUsage;

/// Calendar months retained. Older buckets are dropped on load and on flush so
/// the file cannot grow without bound on a long-lived install.
const RETAINED_MONTHS: usize = 24;
/// Distinct (month, project, provider, model) rows retained. A pathological
/// caller inventing model ids must not be able to grow the file indefinitely.
const MAX_ROWS: usize = 20_000;
/// Minimum gap between disk writes. Recording stays a lock-and-increment; the
/// write happens on the flush task or at shutdown.
const FLUSH_INTERVAL: Duration = Duration::from_secs(30);
const FILE_NAME: &str = "usage.json";
const FILE_VERSION: u32 = 1;
/// Attribution for traffic that arrived without a project scope.
pub(crate) const UNSCOPED_PROJECT: &str = "(unscoped)";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct UsageKey {
    month: String,
    project: String,
    provider: String,
    model: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct UsageRecord {
    requests: u64,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    estimated_requests: u64,
    total_time_ms: u64,
    first_seen_ms: u64,
    last_seen_ms: u64,
}

impl UsageRecord {
    fn counters(&self) -> TokenCounters {
        TokenCounters {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_read_tokens: self.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens,
        }
    }

    fn add(&mut self, elapsed: Duration, usage: TokenUsage, now_ms: u64) {
        self.requests = self.requests.saturating_add(1);
        self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(usage.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(usage.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(usage.cache_write_tokens);
        self.total_time_ms = self
            .total_time_ms
            .saturating_add(elapsed.as_millis().min(u128::from(u64::MAX)) as u64);
        if usage.estimated {
            self.estimated_requests = self.estimated_requests.saturating_add(1);
        }
        if self.first_seen_ms == 0 {
            self.first_seen_ms = now_ms;
        }
        self.last_seen_ms = now_ms;
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct LedgerFile {
    version: u32,
    #[serde(default)]
    rows: Vec<LedgerRow>,
}

#[derive(Debug, Serialize, Deserialize)]
struct LedgerRow {
    #[serde(flatten)]
    key: UsageKey,
    #[serde(flatten)]
    record: UsageRecord,
}

#[derive(Default)]
struct StoreState {
    entries: BTreeMap<UsageKey, UsageRecord>,
    dirty: bool,
}

#[derive(Default)]
pub(crate) struct UsageStore {
    state: Mutex<StoreState>,
    path: OnceLock<PathBuf>,
    enabled: AtomicBool,
}

pub(crate) fn usage_store() -> &'static UsageStore {
    static STORE: OnceLock<UsageStore> = OnceLock::new();
    STORE.get_or_init(UsageStore::default)
}

impl UsageStore {
    /// Point the ledger at a state directory and load whatever is already
    /// there. A load failure is reported and then ignored: losing history is
    /// not a reason to refuse to proxy.
    pub(crate) fn open(&self, directory: &Path) {
        let path = directory.join(FILE_NAME);
        if self.path.set(path.clone()).is_err() {
            return;
        }
        match load_file(&path) {
            Ok(Some(rows)) => {
                let mut state = self.state.lock().expect("usage store poisoned");
                for row in rows {
                    let entry = state.entries.entry(row.key).or_default();
                    *entry = merge(*entry, row.record);
                }
                prune(&mut state.entries);
                info!(
                    path = %path.display(),
                    rows = state.entries.len(),
                    "loaded durable usage ledger"
                );
            }
            Ok(None) => {}
            Err(error) => warn!(
                path = %path.display(),
                error = %error,
                "could not read the usage ledger; starting a fresh one"
            ),
        }
        self.enabled.store(true, Ordering::Relaxed);
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub(crate) fn record(
        &self,
        project: Option<&str>,
        provider: &str,
        model: &str,
        elapsed: Duration,
        usage: TokenUsage,
    ) {
        if !self.enabled() {
            return;
        }
        let now_ms = epoch_millis();
        let key = UsageKey {
            month: month_of(now_ms),
            project: project.unwrap_or(UNSCOPED_PROJECT).to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
        };
        let mut state = self.state.lock().expect("usage store poisoned");
        if state.entries.len() >= MAX_ROWS && !state.entries.contains_key(&key) {
            prune(&mut state.entries);
            if state.entries.len() >= MAX_ROWS {
                return;
            }
        }
        state
            .entries
            .entry(key)
            .or_default()
            .add(elapsed, usage, now_ms);
        state.dirty = true;
    }

    /// Write the ledger when it has changed. Called on a timer and once more
    /// during graceful shutdown.
    pub(crate) fn flush(&self) {
        let Some(path) = self.path.get() else {
            return;
        };
        let rows = {
            let mut state = self.state.lock().expect("usage store poisoned");
            if !state.dirty {
                return;
            }
            prune(&mut state.entries);
            state.dirty = false;
            state
                .entries
                .iter()
                .map(|(key, record)| LedgerRow {
                    key: key.clone(),
                    record: *record,
                })
                .collect::<Vec<_>>()
        };
        if let Err(error) = write_file(path, rows) {
            error!(path = %path.display(), error = %error, "failed to persist the usage ledger");
            self.state.lock().expect("usage store poisoned").dirty = true;
        }
    }

    pub(crate) fn reset(&self) {
        {
            let mut state = self.state.lock().expect("usage store poisoned");
            state.entries.clear();
            state.dirty = true;
        }
        self.flush();
    }

    /// Spend and volume, priced at the rates currently configured.
    ///
    /// `month` selects one bucket (`YYYY-MM`); `None` totals everything
    /// retained. `project` narrows to one project.
    pub(crate) fn report(
        &self,
        pricing: &PricingIndex,
        month: Option<&str>,
        project: Option<&str>,
    ) -> Value {
        let state = self.state.lock().expect("usage store poisoned");
        let mut months = Vec::new();
        let mut by_project: BTreeMap<String, Bucket> = BTreeMap::new();
        let mut by_model: BTreeMap<(String, String), Bucket> = BTreeMap::new();
        let mut total = Bucket::default();

        for (key, record) in &state.entries {
            if !months.contains(&key.month) {
                months.push(key.month.clone());
            }
            if month.is_some_and(|selected| selected != key.month) {
                continue;
            }
            if project.is_some_and(|selected| selected != key.project) {
                continue;
            }
            let rate = pricing.rate(&key.provider, &key.model);
            let spend = match rate {
                ModelRate::Priced(price) => Some(spend_for(price, record.counters())),
                ModelRate::Subscription | ModelRate::Unpriced => None,
            };
            total.add(record, spend, rate);
            by_project
                .entry(key.project.clone())
                .or_default()
                .add(record, spend, rate);
            by_model
                .entry((key.provider.clone(), key.model.clone()))
                .or_default()
                .add(record, spend, rate);
        }
        months.sort();
        months.dedup();
        months.reverse();

        json!({
            "available": true,
            "months": months,
            "month": month,
            "project": project,
            "currency": pricing.currency(),
            "priced": !pricing.is_empty(),
            "total": total.to_json(),
            "projects": by_project
                .into_iter()
                .map(|(name, bucket)| {
                    let mut row = bucket.to_json();
                    row["project"] = json!(name);
                    row
                })
                .collect::<Vec<_>>(),
            "models": by_model
                .into_iter()
                .map(|((provider, model), bucket)| {
                    let mut row = bucket.to_json();
                    row["provider"] = json!(provider);
                    row["model"] = json!(model);
                    row
                })
                .collect::<Vec<_>>(),
        })
    }
}

#[derive(Default)]
struct Bucket {
    requests: u64,
    estimated_requests: u64,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    total_time_ms: u64,
    spend: Spend,
    priced_requests: u64,
    subscription_requests: u64,
    unpriced_requests: u64,
}

impl Bucket {
    fn add(&mut self, record: &UsageRecord, spend: Option<Spend>, rate: ModelRate<'_>) {
        self.requests = self.requests.saturating_add(record.requests);
        self.estimated_requests = self
            .estimated_requests
            .saturating_add(record.estimated_requests);
        self.input_tokens = self.input_tokens.saturating_add(record.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(record.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(record.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(record.cache_write_tokens);
        self.total_time_ms = self.total_time_ms.saturating_add(record.total_time_ms);
        if let Some(spend) = spend {
            self.spend.add(spend);
        }
        match rate {
            ModelRate::Priced(_) => {
                self.priced_requests = self.priced_requests.saturating_add(record.requests)
            }
            ModelRate::Subscription => {
                self.subscription_requests =
                    self.subscription_requests.saturating_add(record.requests)
            }
            ModelRate::Unpriced => {
                self.unpriced_requests = self.unpriced_requests.saturating_add(record.requests)
            }
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "requests": self.requests,
            "estimated_requests": self.estimated_requests,
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "cache_read_tokens": self.cache_read_tokens,
            "cache_write_tokens": self.cache_write_tokens,
            "total_time_ms": self.total_time_ms,
            "cost": {
                "input": self.spend.input,
                "output": self.spend.output,
                "cache_read": self.spend.cache_read,
                "cache_write": self.spend.cache_write,
                "total": self.spend.total(),
            },
            "priced_requests": self.priced_requests,
            "subscription_requests": self.subscription_requests,
            "unpriced_requests": self.unpriced_requests,
        })
    }
}

fn merge(mut into: UsageRecord, from: UsageRecord) -> UsageRecord {
    into.requests = into.requests.saturating_add(from.requests);
    into.input_tokens = into.input_tokens.saturating_add(from.input_tokens);
    into.output_tokens = into.output_tokens.saturating_add(from.output_tokens);
    into.cache_read_tokens = into
        .cache_read_tokens
        .saturating_add(from.cache_read_tokens);
    into.cache_write_tokens = into
        .cache_write_tokens
        .saturating_add(from.cache_write_tokens);
    into.estimated_requests = into
        .estimated_requests
        .saturating_add(from.estimated_requests);
    into.total_time_ms = into.total_time_ms.saturating_add(from.total_time_ms);
    into.first_seen_ms = match (into.first_seen_ms, from.first_seen_ms) {
        (0, other) | (other, 0) => other,
        (left, right) => left.min(right),
    };
    into.last_seen_ms = into.last_seen_ms.max(from.last_seen_ms);
    into
}

/// Drop the oldest months first, then the least-used rows, until the ledger is
/// inside both bounds.
fn prune(entries: &mut BTreeMap<UsageKey, UsageRecord>) {
    let mut months = entries
        .keys()
        .map(|key| key.month.clone())
        .collect::<Vec<_>>();
    months.sort();
    months.dedup();
    if months.len() > RETAINED_MONTHS {
        let cutoff = months[months.len() - RETAINED_MONTHS].clone();
        entries.retain(|key, _| key.month >= cutoff);
    }
    if entries.len() <= MAX_ROWS {
        return;
    }
    let mut ranked = entries
        .iter()
        .map(|(key, record)| (record.requests, key.month.clone(), key.clone()))
        .collect::<Vec<_>>();
    // Oldest month first, then fewest requests: the rows least likely to be
    // asked about are the ones that go.
    ranked.sort_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)));
    for (_, _, key) in ranked.into_iter().take(entries.len() - MAX_ROWS) {
        entries.remove(&key);
    }
}

fn load_file(path: &Path) -> std::io::Result<Option<Vec<LedgerRow>>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let file: LedgerFile = serde_json::from_str(&raw)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if file.version != FILE_VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unsupported usage ledger version {}", file.version),
        ));
    }
    Ok(Some(file.rows))
}

/// Write through a temporary file so an interrupted flush cannot leave a
/// truncated ledger behind.
fn write_file(path: &Path, rows: Vec<LedgerRow>) -> std::io::Result<()> {
    let file = LedgerFile {
        version: FILE_VERSION,
        rows,
    };
    let serialized = serde_json::to_vec(&file)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let temporary = path.with_extension("json.tmp");
    write_owner_only(&temporary, &serialized)?;
    std::fs::rename(&temporary, path)
}

#[cfg(unix)]
fn write_owner_only(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_owner_only(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

pub(crate) fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}

/// `YYYY-MM` in UTC for an epoch-milliseconds instant.
pub(crate) fn month_of(epoch_ms: u64) -> String {
    let (year, month, _) = civil_from_days((epoch_ms / 1000 / 86_400) as i64);
    format!("{year:04}-{month:02}")
}

/// Days since the Unix epoch to a civil (year, month, day), by Howard
/// Hinnant's `civil_from_days`. Avoids a date dependency for one calculation.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// How often the flush task should write. Exposed so the daemon owns the task
/// rather than the store spawning one.
pub(crate) fn flush_interval() -> Duration {
    FLUSH_INTERVAL
}

#[cfg(test)]
#[path = "usage_store_tests.rs"]
mod tests;
