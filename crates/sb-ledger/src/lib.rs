//! Append-only usage/cost ledger — the accounting seam beneath budgets, cost
//! attribution, and (later) marketplace billing (deepresearch "add a minimal
//! append-only usage ledger"; spec §22 Layer 3). v1 is seams-not-machinery: an
//! in-memory append-only ledger with an optional JSONL sink and aggregation,
//! costs computed from the catalog's price ledger. Money is integer micro-USD,
//! never a float. Records are never mutated.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use sb_core::{Catalog, TokenKind, Usage};
use serde::{Deserialize, Serialize};

/// Seconds since the Unix epoch (record timestamp). No calendar formatting, so
/// sb-core/sb-ledger stay free of a time-crate dependency.
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn default_tenant() -> u64 {
    1
}

/// Attributed cost of `usage` for `model_id`, in micro-USD, at the catalog's
/// CURRENT prices. A missing price contributes 0 — the record still carries the
/// raw usage so it can be re-priced later from the ledger history (auditable).
pub fn compute_cost_micros(catalog: &Catalog, model_id: &str, usage: &Usage) -> u64 {
    let per = |kind: TokenKind, tokens: u64| {
        catalog
            .current_price(model_id, kind)
            .map(|p| p.unit_price_micros_per_mtok.saturating_mul(tokens) / 1_000_000)
            .unwrap_or(0)
    };
    per(TokenKind::Input, usage.input_tokens)
        .saturating_add(per(TokenKind::Output, usage.output_tokens))
        .saturating_add(per(TokenKind::CachedInput, usage.cached_input_tokens))
        .saturating_add(per(TokenKind::Reasoning, usage.reasoning_tokens))
}

/// One executed request's usage + attributed cost. Append-only; never mutated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    pub request_id: String,
    #[serde(default = "default_tenant")]
    pub tenant_id: u64,
    #[serde(default)]
    pub owner_id: Option<String>,
    pub provider_id: String,
    pub model: String,
    #[serde(default)]
    pub account_id: Option<String>,
    pub timestamp_unix: u64,
    pub usage: Usage,
    pub cost_micros: u64,
    pub latency_ms: u64,
    #[serde(default)]
    pub streamed: bool,
    /// Gateway tenant this request was attributed to (None = unattributed /
    /// single-tenant). Drives per-tenant spend rollups + budget enforcement.
    #[serde(default)]
    pub tenant: Option<String>,
}

impl UsageRecord {
    /// Build a record, computing cost from the catalog at current prices.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        request_id: impl Into<String>,
        provider_id: impl Into<String>,
        model: impl Into<String>,
        account_id: Option<String>,
        usage: Usage,
        latency_ms: u64,
        streamed: bool,
        catalog: &Catalog,
    ) -> Self {
        let model = model.into();
        let cost_micros = compute_cost_micros(catalog, &model, &usage);
        UsageRecord {
            request_id: request_id.into(),
            tenant_id: 1,
            owner_id: None,
            provider_id: provider_id.into(),
            model,
            account_id,
            timestamp_unix: now_unix(),
            usage,
            cost_micros,
            latency_ms,
            streamed,
            tenant: None,
        }
    }

    /// Build a record with a PRE-COMPUTED cost (micro-USD). Used when cost is
    /// priced from the router's price index rather than the catalog, so route and
    /// ledger never diverge (audit #5).
    #[allow(clippy::too_many_arguments)]
    pub fn priced(
        request_id: impl Into<String>,
        provider_id: impl Into<String>,
        model: impl Into<String>,
        account_id: Option<String>,
        usage: Usage,
        latency_ms: u64,
        streamed: bool,
        cost_micros: u64,
    ) -> Self {
        UsageRecord {
            request_id: request_id.into(),
            tenant_id: 1,
            owner_id: None,
            provider_id: provider_id.into(),
            model: model.into(),
            account_id,
            timestamp_unix: now_unix(),
            usage,
            cost_micros,
            latency_ms,
            streamed,
            tenant: None,
        }
    }

    /// Attribute this record to a tenant (builder).
    pub fn with_tenant(mut self, tenant: Option<String>) -> Self {
        self.tenant = tenant;
        self
    }
}

/// Append-only ledger. Records accumulate in memory and (optionally) stream to a
/// JSONL sink and a durable [`StateStore`](sb_store::StateStore). Aggregation is
/// computed in memory on read (the hot path — budgets read this per request);
/// `base` carries historical totals hydrated from the store at startup so the
/// summary survives restarts without scanning the DB per request.
pub struct UsageLedger {
    records: Mutex<Vec<UsageRecord>>,
    sink: Option<PathBuf>,
    store: Option<Arc<dyn sb_store::StateStore>>,
    /// Historical totals loaded from the store at attach time (immutable after).
    base: LedgerSummary,
}

impl UsageLedger {
    pub fn in_memory() -> Self {
        Self {
            records: Mutex::new(Vec::new()),
            sink: None,
            store: None,
            base: LedgerSummary::default(),
        }
    }

    /// Also append each record as a JSONL line to `path` (an audit trail).
    pub fn with_sink(path: impl Into<PathBuf>) -> Self {
        Self {
            records: Mutex::new(Vec::new()),
            sink: Some(path.into()),
            store: None,
            base: LedgerSummary::default(),
        }
    }

    /// Attach a durable usage store: each record is also persisted there, and the
    /// in-memory summary is seeded with the store's existing rollup so historical
    /// spend (budgets, `/v1/usage`) survives a restart. Consuming builder — call
    /// before the ledger is shared. New records written after this point are NOT
    /// double-counted: `base` is a snapshot of the rollup at attach time, and only
    /// post-attach records live in `records`.
    pub fn with_store(mut self, store: Arc<dyn sb_store::StateStore>) -> Self {
        match store.usage_rollup() {
            Ok(rollup) => self.base = rollup_to_summary(&rollup),
            Err(e) => {
                tracing::warn!(error = %e, "usage store hydrate failed; starting totals from zero")
            }
        }
        self.store = Some(store);
        self
    }

    /// Append a record. Best-effort JSONL + store writes — a failure is logged but
    /// can never break a request; the in-memory append always succeeds.
    pub fn record(&self, record: UsageRecord) {
        if let Some(path) = &self.sink {
            if let Ok(line) = serde_json::to_string(&record) {
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    let _ = writeln!(file, "{line}");
                }
            }
        }
        if let Some(store) = &self.store {
            if let Err(e) = store.record_usage(&record_to_event(&record)) {
                tracing::warn!(error = %e, request_id = %record.request_id, "usage store write failed");
            }
        }
        if let Ok(mut records) = self.records.lock() {
            records.push(record);
        }
    }

    pub fn len(&self) -> usize {
        self.records.lock().map(|r| r.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn snapshot(&self) -> Vec<UsageRecord> {
        self.records.lock().map(|r| r.clone()).unwrap_or_default()
    }

    /// Aggregate counts + attributed cost by model and provider, including any
    /// historical totals hydrated from the store (`base`) so the view survives a
    /// restart. `base` holds pre-attach totals; `records` holds only post-attach
    /// records, so the two never double-count.
    pub fn summary(&self) -> LedgerSummary {
        let records = self.records.lock().map(|r| r.clone()).unwrap_or_default();
        let mut summary = self.base.clone();
        summary.requests += records.len();
        for record in &records {
            summary.total_cost_micros = summary.total_cost_micros.saturating_add(record.cost_micros);
            let model = summary.by_model.entry(record.model.clone()).or_default();
            model.0 += 1;
            model.1 = model.1.saturating_add(record.cost_micros);
            let provider = summary
                .by_provider
                .entry(record.provider_id.clone())
                .or_default();
            provider.0 += 1;
            provider.1 = provider.1.saturating_add(record.cost_micros);
            if let Some(tenant) = &record.tenant {
                let t = summary.by_tenant.entry(tenant.clone()).or_default();
                t.0 += 1;
                t.1 = t.1.saturating_add(record.cost_micros);
            }
        }
        summary
    }

    /// Attributed spend (USD) for one tenant — the budget-enforcement read.
    pub fn tenant_spend_usd(&self, tenant: &str) -> f64 {
        self.summary()
            .by_tenant
            .get(tenant)
            .map(|(_count, micros)| *micros as f64 / 1_000_000.0)
            .unwrap_or(0.0)
    }
}

impl Default for UsageLedger {
    fn default() -> Self {
        Self::in_memory()
    }
}

/// Aggregated view of the ledger. `(count, cost_micros)` per key.
#[derive(Debug, Clone, Default, Serialize)]
pub struct LedgerSummary {
    pub requests: usize,
    pub total_cost_micros: u64,
    pub by_model: BTreeMap<String, (usize, u64)>,
    pub by_provider: BTreeMap<String, (usize, u64)>,
    pub by_tenant: BTreeMap<String, (usize, u64)>,
}

/// Project a `UsageRecord` onto the store's metadata-only `UsageEvent`.
fn record_to_event(r: &UsageRecord) -> sb_store::UsageEvent {
    sb_store::UsageEvent {
        request_id: r.request_id.clone(),
        provider_id: r.provider_id.clone(),
        model: r.model.clone(),
        account_id: r.account_id.clone(),
        tenant: r.tenant.clone(),
        cost_micros: r.cost_micros,
        input_tokens: r.usage.input_tokens,
        output_tokens: r.usage.output_tokens,
        latency_ms: r.latency_ms,
        streamed: r.streamed,
        created_at_ms: (r.timestamp_unix as i64).saturating_mul(1000),
    }
}

/// Seed an in-memory summary from a store rollup (the historical base).
fn rollup_to_summary(rollup: &sb_store::UsageRollup) -> LedgerSummary {
    let to_map = |buckets: &[sb_store::UsageBucket]| {
        buckets
            .iter()
            .map(|(k, count, cost)| (k.clone(), (*count as usize, *cost)))
            .collect()
    };
    LedgerSummary {
        requests: rollup.requests as usize,
        total_cost_micros: rollup.total_cost_micros,
        by_model: to_map(&rollup.by_model),
        by_provider: to_map(&rollup.by_provider),
        by_tenant: to_map(&rollup.by_tenant),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_core::{Price, TokenKind, Usage};

    fn priced_catalog() -> Catalog {
        Catalog {
            prices: vec![
                Price {
                    tenant_id: Default::default(),
                    model_id: "m".into(),
                    token_kind: TokenKind::Input,
                    unit_price_micros_per_mtok: 3_000_000, // $3 / Mtok
                    effective_from: "2025-01-01T00:00:00Z".into(),
                    effective_to: None,
                },
                Price {
                    tenant_id: Default::default(),
                    model_id: "m".into(),
                    token_kind: TokenKind::Output,
                    unit_price_micros_per_mtok: 15_000_000, // $15 / Mtok
                    effective_from: "2025-01-01T00:00:00Z".into(),
                    effective_to: None,
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn cost_is_priced_from_the_catalog() {
        let catalog = priced_catalog();
        let usage = Usage {
            input_tokens: 1000,
            output_tokens: 500,
            ..Usage::default()
        };
        // 1000 * 3_000_000/1e6 + 500 * 15_000_000/1e6 = 3000 + 7500 = 10500 micros
        assert_eq!(compute_cost_micros(&catalog, "m", &usage), 10_500);
        // unknown model -> 0 (still recorded, re-priceable later)
        assert_eq!(compute_cost_micros(&catalog, "ghost", &usage), 0);
    }

    #[test]
    fn ledger_appends_and_aggregates() {
        let catalog = priced_catalog();
        let ledger = UsageLedger::in_memory();
        let usage = Usage {
            input_tokens: 1000,
            output_tokens: 500,
            ..Usage::default()
        };
        ledger.record(UsageRecord::new(
            "req1", "anthropic", "m", Some("acct".into()), usage.clone(), 42, false, &catalog,
        ));
        ledger.record(UsageRecord::new(
            "req2", "anthropic", "m", None, usage, 10, true, &catalog,
        ));

        assert_eq!(ledger.len(), 2);
        let summary = ledger.summary();
        assert_eq!(summary.requests, 2);
        assert_eq!(summary.total_cost_micros, 21_000);
        assert_eq!(summary.by_model.get("m"), Some(&(2, 21_000)));
        assert_eq!(summary.by_provider.get("anthropic"), Some(&(2, 21_000)));
    }

    #[test]
    fn store_sink_persists_and_hydrates_without_double_counting() {
        use sb_store::{SqliteStore, StateStore};
        let catalog = priced_catalog();
        let usage = Usage {
            input_tokens: 1000,
            output_tokens: 500,
            ..Usage::default()
        };
        let store: Arc<dyn StateStore> = Arc::new(SqliteStore::in_memory().unwrap());

        // First "process": two requests, dual-written to memory + store.
        let ledger = UsageLedger::in_memory().with_store(store.clone());
        ledger.record(UsageRecord::new(
            "r1", "anthropic", "m", Some("a".into()), usage.clone(), 5, false, &catalog,
        ));
        ledger.record(UsageRecord::new(
            "r2", "anthropic", "m", None, usage.clone(), 6, true, &catalog,
        ));
        assert_eq!(ledger.summary().requests, 2);
        assert_eq!(ledger.summary().total_cost_micros, 21_000);
        // Durably recorded.
        assert_eq!(store.usage_rollup().unwrap().requests, 2);

        // Second "process": a fresh ledger on the SAME store hydrates the history
        // (base = 2), and a new record adds on top WITHOUT double-counting.
        let restarted = UsageLedger::in_memory().with_store(store.clone());
        assert_eq!(restarted.summary().requests, 2, "hydrated historical total");
        assert_eq!(restarted.summary().total_cost_micros, 21_000);
        restarted.record(UsageRecord::new(
            "r3", "openai", "m", None, usage, 7, false, &catalog,
        ));
        let s = restarted.summary();
        assert_eq!(s.requests, 3, "base(2) + one new record, not 4");
        assert_eq!(s.by_provider.get("anthropic"), Some(&(2, 21_000)));
        assert_eq!(s.by_provider.get("openai"), Some(&(1, 10_500)));
        assert_eq!(store.usage_rollup().unwrap().requests, 3);
    }

    #[test]
    fn jsonl_sink_is_append_only_and_parseable() {
        let mut path = std::env::temp_dir();
        path.push(format!("sb-ledger-test-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let catalog = priced_catalog();
        let ledger = UsageLedger::with_sink(&path);
        ledger.record(UsageRecord::new(
            "req1", "p", "m", None, Usage::default(), 1, false, &catalog,
        ));
        ledger.record(UsageRecord::new(
            "req2", "p", "m", None, Usage::default(), 2, false, &catalog,
        ));

        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "two append-only lines");
        // each line is a parseable record
        let first: UsageRecord = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first.request_id, "req1");

        std::fs::remove_file(&path).ok();
    }
}
