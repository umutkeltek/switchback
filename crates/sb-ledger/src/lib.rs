//! Append-only usage/cost ledger — the accounting seam beneath budgets, cost
//! attribution, and (later) marketplace billing (deepresearch "add a minimal
//! append-only usage ledger"; spec §22 Layer 3). v1 is seams-not-machinery: an
//! in-memory append-only ledger with an optional JSONL sink and aggregation,
//! costs computed from the catalog's price ledger. Money is integer micro-USD,
//! never a float. Records are never mutated.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
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
        }
    }
}

/// Append-only ledger. Records accumulate in memory and (optionally) stream to a
/// JSONL sink; aggregation is computed on read.
pub struct UsageLedger {
    records: Mutex<Vec<UsageRecord>>,
    sink: Option<PathBuf>,
}

impl UsageLedger {
    pub fn in_memory() -> Self {
        Self {
            records: Mutex::new(Vec::new()),
            sink: None,
        }
    }

    /// Also append each record as a JSONL line to `path` (an audit trail).
    pub fn with_sink(path: impl Into<PathBuf>) -> Self {
        Self {
            records: Mutex::new(Vec::new()),
            sink: Some(path.into()),
        }
    }

    /// Append a record. Best-effort JSONL write — an IO error is swallowed so it
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

    /// Aggregate counts + attributed cost by model and provider.
    pub fn summary(&self) -> LedgerSummary {
        let records = self.records.lock().map(|r| r.clone()).unwrap_or_default();
        let mut summary = LedgerSummary {
            requests: records.len(),
            ..Default::default()
        };
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
        }
        summary
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
