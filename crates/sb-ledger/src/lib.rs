//! Append-only usage/cost ledger — the accounting seam beneath budgets, cost
//! attribution, and (later) marketplace billing (deepresearch "add a minimal
//! append-only usage ledger"; spec §22 Layer 3). v1 is seams-not-machinery: an
//! in-memory append-only ledger with an optional JSONL sink and aggregation.
//! Records can be priced from the catalog with `UsageRecord::new`, or receive a
//! runtime-selected cost from the adapter registry with `UsageRecord::priced`.
//! Money is integer micro-USD, never a float. Records are never mutated.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use sb_core::{Catalog, PricingUnit, TokenKind, UnitPrice, Usage, WorkloadKind};
use sb_store::UsageWriteOutcome;
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

/// Realized cache savings for `usage` on `model_id`, in micro-USD: the money
/// NOT spent because the provider served a cached prefix instead of billing
/// those tokens at the full input rate.
///
/// `cached_input_tokens × (unit_price(Input) − unit_price(CachedInput)) / 1e6`,
/// counted ONLY when the catalog has BOTH an `Input` and a `CachedInput` price
/// for the model AND `Input > CachedInput` (a real discount). Otherwise 0.
/// Integer, saturating math, in the same style as [`compute_cost_micros`].
///
/// `0` means "unknown or no savings", never "definitely nothing was saved":
/// with no `CachedInput` price the discount is simply unpriced. The raw
/// `cached_input_tokens` are recorded regardless, so a record stays
/// re-priceable once the catalog gains the missing price.
pub fn compute_cache_savings_micros(catalog: &Catalog, model_id: &str, usage: &Usage) -> u64 {
    let cached = usage.cached_input_tokens;
    if cached == 0 {
        return 0;
    }
    let price = |kind: TokenKind| {
        catalog
            .current_price(model_id, kind)
            .map(|p| p.unit_price_micros_per_mtok)
    };
    match (price(TokenKind::Input), price(TokenKind::CachedInput)) {
        (Some(input), Some(cached_rate)) if input > cached_rate => {
            input.saturating_sub(cached_rate).saturating_mul(cached) / 1_000_000
        }
        _ => 0,
    }
}

/// Compute a workload charge from registry-v3 unit pricing. Positive
/// fractional micro-USD results round up so a paid unit can never collapse to
/// known-free zero; missing price/units remain unknown (`None`).
pub fn compute_unit_cost_micros(price: Option<UnitPrice>, units: Option<f64>) -> Option<u64> {
    let price = price?;
    if price.pricing_unit == PricingUnit::TokenMetered {
        return None;
    }
    let units = units?;
    if !units.is_finite() || units < 0.0 {
        return None;
    }
    let micros = units * price.unit_price_micros as f64;
    if !micros.is_finite() || micros > u64::MAX as f64 {
        return None;
    }
    Some(micros.ceil() as u64)
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
    pub cost_micros: Option<u64>,
    /// Realized cache savings (micro-USD) for this request — money not spent
    /// because a cached prefix was served below the input rate. `None` = not
    /// attributed (no catalog cache price); the raw `usage.cached_input_tokens`
    /// still make it re-priceable. Defaults for backward-compatible JSONL replay.
    #[serde(default)]
    pub cache_savings_micros: Option<u64>,
    pub latency_ms: u64,
    #[serde(default)]
    pub streamed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload_kind: Option<WorkloadKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_unit: Option<PricingUnit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub units_consumed: Option<f64>,
    /// Gateway tenant this request was attributed to (None = unattributed /
    /// single-tenant). Drives per-tenant spend rollups + budget enforcement.
    #[serde(default)]
    pub tenant: Option<String>,
    /// Optional project label resolved from the inbound API key. This stays
    /// gateway-local metadata and is never sent upstream.
    #[serde(default)]
    pub project: Option<String>,
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
        let cache_savings_micros = compute_cache_savings_micros(catalog, &model, &usage);
        UsageRecord {
            request_id: request_id.into(),
            tenant_id: 1,
            owner_id: None,
            provider_id: provider_id.into(),
            model,
            account_id,
            timestamp_unix: now_unix(),
            usage,
            cost_micros: Some(cost_micros),
            cache_savings_micros: Some(cache_savings_micros),
            latency_ms,
            streamed,
            workload_kind: None,
            pricing_unit: None,
            units_consumed: None,
            tenant: None,
            project: None,
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
            cost_micros: Some(cost_micros),
            // Threaded in by the runtime via `with_cache_savings` (it prices from
            // the same registry the router routes on); unset here.
            cache_savings_micros: None,
            latency_ms,
            streamed,
            workload_kind: None,
            pricing_unit: None,
            units_consumed: None,
            tenant: None,
            project: None,
        }
    }

    /// Build a metadata-only media/workload row. `None` cost is deliberately
    /// distinct from known-free `Some(0)`.
    #[allow(clippy::too_many_arguments)]
    pub fn workload(
        request_id: impl Into<String>,
        provider_id: impl Into<String>,
        model: impl Into<String>,
        account_id: Option<String>,
        workload_kind: WorkloadKind,
        pricing_unit: Option<PricingUnit>,
        units_consumed: Option<f64>,
        cost_micros: Option<u64>,
        latency_ms: u64,
    ) -> Self {
        UsageRecord {
            request_id: request_id.into(),
            tenant_id: 1,
            owner_id: None,
            provider_id: provider_id.into(),
            model: model.into(),
            account_id,
            timestamp_unix: now_unix(),
            usage: Usage::default(),
            cost_micros,
            cache_savings_micros: None,
            latency_ms,
            streamed: false,
            workload_kind: Some(workload_kind),
            pricing_unit,
            units_consumed,
            tenant: None,
            project: None,
        }
    }

    /// Attribute this record to a tenant (builder).
    pub fn with_tenant(mut self, tenant: Option<String>) -> Self {
        self.tenant = tenant;
        self
    }

    /// Attribute this record to a project label (builder).
    pub fn with_project(mut self, project: Option<String>) -> Self {
        self.project = project;
        self
    }

    /// Attach realized cache savings (micro-USD) computed by the runtime from
    /// the registry's price source (builder). Used on the `priced` hot path,
    /// where the record is built without a catalog.
    pub fn with_cache_savings(mut self, cache_savings_micros: Option<u64>) -> Self {
        self.cache_savings_micros = cache_savings_micros;
        self
    }
}

/// Operator-facing health of usage durability. This is intentionally compact:
/// it reports whether usage accounting is memory-only, durably healthy,
/// degraded to memory fallback, or has seen a required post-commit write fail.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UsageDurabilityHealth {
    pub status: String,
    pub store_configured: bool,
    pub persisted_writes: u64,
    pub duplicate_ignored_writes: u64,
    pub memory_writes: u64,
    pub failed_writes: u64,
    pub post_commit_failed_writes: u64,
    pub rollup_failures: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_request_id: Option<String>,
}

impl UsageDurabilityHealth {
    fn new() -> Self {
        let mut health = Self {
            status: String::new(),
            store_configured: false,
            persisted_writes: 0,
            duplicate_ignored_writes: 0,
            memory_writes: 0,
            failed_writes: 0,
            post_commit_failed_writes: 0,
            rollup_failures: 0,
            last_outcome: None,
            last_error: None,
            last_request_id: None,
        };
        health.refresh_status();
        health
    }

    fn refresh_status(&mut self) {
        self.status = if self.post_commit_failed_writes > 0 {
            "post_commit_failed"
        } else if self.failed_writes > 0 || self.rollup_failures > 0 {
            "degraded"
        } else if self.store_configured {
            "durable"
        } else {
            "memory_only"
        }
        .to_string();
    }
}

impl Default for UsageDurabilityHealth {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct UsageReconciliationTotals {
    pub requests: u64,
    pub cost_micros: u64,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct UsageReconciliationDelta {
    pub ledger_minus_durable_requests: i64,
    pub ledger_minus_durable_cost_micros: i64,
    pub unexplained_requests: i64,
    pub unexplained_cost_micros: i64,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct UsageReconciliationScope {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
}

/// Operator-facing check that compares the served usage summary against durable
/// events and known in-memory fallback records.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UsageReconciliationReport {
    pub status: String,
    pub billing_grade: bool,
    pub store_configured: bool,
    pub scope: UsageReconciliationScope,
    pub durable: UsageReconciliationTotals,
    pub ledger: UsageReconciliationTotals,
    pub memory_fallback: UsageReconciliationTotals,
    pub delta: UsageReconciliationDelta,
    pub duplicate_ignored_writes: u64,
    pub memory_writes: u64,
    pub failed_writes: u64,
    pub post_commit_failed_writes: u64,
    pub rollup_failures: u64,
    pub issues: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
enum RequiredUsagePhase {
    PreResponse,
    PostCommit,
}

/// Append-only ledger. Records accumulate in memory and (optionally) stream to a
/// JSONL sink and a durable [`StateStore`](sb_store::StateStore). When a store is
/// attached, successfully persisted records are read back from its live rollup;
/// the in-memory buffer only carries no-store records or best-effort writes that
/// could not be durably accepted.
pub struct UsageLedger {
    records: Mutex<Vec<UsageRecord>>,
    sink: Option<PathBuf>,
    store: Option<Arc<dyn sb_store::StateStore>>,
    /// Historical totals loaded from the store at attach time (immutable after).
    base: LedgerSummary,
    health: Mutex<UsageDurabilityHealth>,
}

impl UsageLedger {
    pub fn in_memory() -> Self {
        Self {
            records: Mutex::new(Vec::new()),
            sink: None,
            store: None,
            base: LedgerSummary::default(),
            health: Mutex::new(UsageDurabilityHealth::default()),
        }
    }

    /// Also append each record as a JSONL line to `path` (an audit trail).
    pub fn with_sink(path: impl Into<PathBuf>) -> Self {
        Self {
            records: Mutex::new(Vec::new()),
            sink: Some(path.into()),
            store: None,
            base: LedgerSummary::default(),
            health: Mutex::new(UsageDurabilityHealth::default()),
        }
    }

    /// Attach a durable usage store: each record is also persisted there, and the
    /// in-memory summary is seeded with the store's existing rollup so historical
    /// spend (budgets, `/v1/usage`) survives a restart. Consuming builder — call
    /// before the ledger is shared.
    pub fn with_store(mut self, store: Arc<dyn sb_store::StateStore>) -> Self {
        self.update_health(|health| {
            health.store_configured = true;
        });
        match store.usage_rollup() {
            Ok(rollup) => self.base = rollup_to_summary(&rollup),
            Err(e) => {
                self.mark_rollup_failure(e.to_string());
                tracing::warn!(error = %e, "usage store hydrate failed; starting totals from zero")
            }
        }
        self.store = Some(store);
        self
    }

    /// Append a record. Best-effort JSONL + store writes — a failure is logged but
    /// can never break a request; the in-memory append always succeeds.
    pub fn record(&self, record: UsageRecord) {
        if let Err(e) = self.record_inner(record, None) {
            tracing::warn!(error = %e, "usage ledger record failed");
        }
    }

    /// Append a record and require the durable store write to succeed. This is
    /// the billing-grade/fail-closed path used when `state_store.required` is
    /// enabled; the in-memory summary is updated only after the store accepts
    /// the event.
    pub fn record_checked(&self, record: UsageRecord) -> Result<(), String> {
        self.record_inner(record, Some(RequiredUsagePhase::PreResponse))
    }

    /// Append a record after the downstream response has already been committed
    /// and require the durable store write to succeed. The error still returns to
    /// the caller for logging, but the health surface records that the client
    /// could not be failed closed anymore.
    pub fn record_checked_post_commit(&self, record: UsageRecord) -> Result<(), String> {
        self.record_inner(record, Some(RequiredUsagePhase::PostCommit))
    }

    fn record_inner(
        &self,
        record: UsageRecord,
        required_phase: Option<RequiredUsagePhase>,
    ) -> Result<(), String> {
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
        let mut persistently_recorded = false;
        let mut memory_outcome: Option<&'static str> = None;
        if let Some(store) = &self.store {
            match store.record_usage(&record_to_event(&record)) {
                Ok(UsageWriteOutcome::Inserted) => {
                    self.mark_persisted(&record.request_id);
                    persistently_recorded = true;
                }
                Ok(UsageWriteOutcome::DuplicateIgnored) => {
                    self.mark_duplicate_ignored(&record.request_id);
                    persistently_recorded = true;
                }
                Err(e) => {
                    if let Some(phase) = required_phase {
                        self.mark_required_failure(&record.request_id, e.to_string(), phase);
                        return Err(format!("usage store write failed: {e}"));
                    }
                    self.mark_best_effort_failure(&record.request_id, e.to_string());
                    tracing::warn!(error = %e, request_id = %record.request_id, "usage store write failed");
                    memory_outcome = Some("degraded_memory_fallback");
                }
            }
        } else if let Some(phase) = required_phase {
            self.mark_required_failure(
                &record.request_id,
                "usage store is required but not configured".to_string(),
                phase,
            );
            return Err("usage store is required but not configured".to_string());
        } else {
            memory_outcome = Some("memory_only");
        }
        if !persistently_recorded {
            let request_id = record.request_id.clone();
            if let Ok(mut records) = self.records.lock() {
                records.push(record);
            }
            if let Some(outcome) = memory_outcome {
                self.mark_memory_write(outcome, &request_id);
            }
        }
        Ok(())
    }

    /// Current durable usage accounting health.
    pub fn durability_health(&self) -> UsageDurabilityHealth {
        self.health.lock().map(|h| h.clone()).unwrap_or_else(|_| {
            let mut health = UsageDurabilityHealth {
                failed_writes: 1,
                last_error: Some("usage durability health mutex poisoned".to_string()),
                ..UsageDurabilityHealth::default()
            };
            health.refresh_status();
            health
        })
    }

    /// Most recent usage rows, newest first, across durable and in-memory
    /// fallback storage.
    pub fn recent(&self, limit: usize) -> Vec<UsageRecord> {
        let mut rows = self
            .records
            .lock()
            .map(|rows| rows.clone())
            .unwrap_or_default();
        if let Some(store) = &self.store {
            if let Ok(events) = store.recent_usage(limit) {
                rows.extend(events.into_iter().map(event_to_record));
            }
        }
        rows.sort_by(|a, b| {
            b.timestamp_unix
                .cmp(&a.timestamp_unix)
                .then_with(|| b.request_id.cmp(&a.request_id))
        });
        let mut seen = std::collections::HashSet::new();
        rows.retain(|row| seen.insert(row.request_id.clone()));
        rows.truncate(limit);
        rows
    }

    /// Reconcile the served usage summary against durable store events and
    /// known memory fallback records. A duplicate ignored write is healthy; an
    /// in-memory fallback is degraded; a post-commit required-store failure is
    /// inconsistent because a client may have observed an unbilled response.
    pub fn reconcile(&self, tenant: Option<&str>) -> UsageReconciliationReport {
        let memory_records = self.records.lock().map(|r| r.clone()).unwrap_or_default();
        let memory = totals_from_records(&memory_records, tenant);
        let store_configured = self.store.is_some();
        let scope = UsageReconciliationScope {
            tenant: tenant.map(str::to_string),
        };
        let mut issues = Vec::new();
        let mut durable_rollup_failed = false;

        let durable = match &self.store {
            Some(store) => match store.usage_rollup() {
                Ok(rollup) => totals_from_summary(&rollup_to_summary(&rollup), tenant),
                Err(e) => {
                    durable_rollup_failed = true;
                    self.mark_rollup_failure(e.to_string());
                    issues.push("durable_rollup_failed".to_string());
                    UsageReconciliationTotals::default()
                }
            },
            None => {
                issues.push("state_store_disabled".to_string());
                UsageReconciliationTotals::default()
            }
        };

        let ledger = if store_configured && !durable_rollup_failed {
            UsageReconciliationTotals {
                requests: durable.requests.saturating_add(memory.requests),
                cost_micros: durable.cost_micros.saturating_add(memory.cost_micros),
            }
        } else {
            let mut summary = self.base.clone();
            apply_records(&mut summary, &memory_records);
            totals_from_summary(&summary, tenant)
        };
        let health = self.durability_health();

        let ledger_minus_durable_requests = ledger.requests as i64 - durable.requests as i64;
        let ledger_minus_durable_cost_micros =
            ledger.cost_micros as i64 - durable.cost_micros as i64;
        let unexplained_requests = ledger_minus_durable_requests - memory.requests as i64;
        let unexplained_cost_micros = ledger_minus_durable_cost_micros - memory.cost_micros as i64;
        let delta = UsageReconciliationDelta {
            ledger_minus_durable_requests,
            ledger_minus_durable_cost_micros,
            unexplained_requests,
            unexplained_cost_micros,
        };

        if memory.requests > 0 {
            issues.push("memory_fallback".to_string());
        }
        if health.failed_writes > 0 {
            issues.push("usage_write_failures".to_string());
        }
        if health.post_commit_failed_writes > 0 {
            issues.push("post_commit_usage_failure".to_string());
        }
        if health.rollup_failures > 0 {
            issues.push("rollup_failures".to_string());
        }
        if delta.unexplained_requests != 0 || delta.unexplained_cost_micros != 0 {
            issues.push("unexplained_usage_delta".to_string());
        }
        issues.sort();
        issues.dedup();

        let status = if health.post_commit_failed_writes > 0
            || delta.unexplained_requests != 0
            || delta.unexplained_cost_micros != 0
        {
            "inconsistent"
        } else if !store_configured
            || memory.requests > 0
            || health.failed_writes > 0
            || health.rollup_failures > 0
        {
            "degraded"
        } else {
            "ok"
        }
        .to_string();

        UsageReconciliationReport {
            billing_grade: status == "ok",
            status,
            store_configured,
            scope,
            durable,
            ledger,
            memory_fallback: memory,
            delta,
            duplicate_ignored_writes: health.duplicate_ignored_writes,
            memory_writes: health.memory_writes,
            failed_writes: health.failed_writes,
            post_commit_failed_writes: health.post_commit_failed_writes,
            rollup_failures: health.rollup_failures,
            issues,
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

    /// Aggregate counts + attributed cost by model and provider. With a live
    /// store, the durable rollup is the source of truth and the in-memory records
    /// are only a best-effort fallback for events that could not be persisted.
    pub fn summary(&self) -> LedgerSummary {
        let records = self.records.lock().map(|r| r.clone()).unwrap_or_default();
        if let Some(store) = &self.store {
            match store.usage_rollup() {
                Ok(rollup) => {
                    let mut summary = rollup_to_summary(&rollup);
                    apply_records(&mut summary, &records);
                    return summary;
                }
                Err(e) => {
                    self.mark_rollup_failure(e.to_string());
                    tracing::warn!(error = %e, "usage store live rollup failed; falling back to hydrated in-memory summary");
                }
            }
        }
        let mut summary = self.base.clone();
        apply_records(&mut summary, &records);
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

    fn update_health(&self, update: impl FnOnce(&mut UsageDurabilityHealth)) {
        if let Ok(mut health) = self.health.lock() {
            update(&mut health);
            health.refresh_status();
        }
    }

    fn mark_persisted(&self, request_id: &str) {
        self.update_health(|health| {
            health.persisted_writes = health.persisted_writes.saturating_add(1);
            health.last_outcome = Some("inserted".to_string());
            health.last_error = None;
            health.last_request_id = Some(request_id.to_string());
        });
    }

    fn mark_duplicate_ignored(&self, request_id: &str) {
        self.update_health(|health| {
            health.duplicate_ignored_writes = health.duplicate_ignored_writes.saturating_add(1);
            health.last_outcome = Some("duplicate_ignored".to_string());
            health.last_error = None;
            health.last_request_id = Some(request_id.to_string());
        });
    }

    fn mark_best_effort_failure(&self, request_id: &str, error: String) {
        self.update_health(|health| {
            health.failed_writes = health.failed_writes.saturating_add(1);
            health.last_outcome = Some("degraded_memory_fallback".to_string());
            health.last_error = Some(error);
            health.last_request_id = Some(request_id.to_string());
        });
    }

    fn mark_required_failure(&self, request_id: &str, error: String, phase: RequiredUsagePhase) {
        self.update_health(|health| {
            health.failed_writes = health.failed_writes.saturating_add(1);
            if matches!(phase, RequiredUsagePhase::PostCommit) {
                health.post_commit_failed_writes =
                    health.post_commit_failed_writes.saturating_add(1);
                health.last_outcome = Some("post_commit_failed".to_string());
            } else {
                health.last_outcome = Some("failed_closed".to_string());
            }
            health.last_error = Some(error);
            health.last_request_id = Some(request_id.to_string());
        });
    }

    fn mark_memory_write(&self, outcome: &'static str, request_id: &str) {
        self.update_health(|health| {
            health.memory_writes = health.memory_writes.saturating_add(1);
            health.last_outcome = Some(outcome.to_string());
            health.last_request_id = Some(request_id.to_string());
            if outcome == "memory_only" {
                health.last_error = None;
            }
        });
    }

    fn mark_rollup_failure(&self, error: String) {
        self.update_health(|health| {
            health.rollup_failures = health.rollup_failures.saturating_add(1);
            health.last_outcome = Some("rollup_failed".to_string());
            health.last_error = Some(error);
        });
    }
}

impl Default for UsageLedger {
    fn default() -> Self {
        Self::in_memory()
    }
}

/// Aggregated view of the ledger. `(count, cost_micros)` per key.
#[derive(Debug, Clone, Default, Serialize)]
pub struct EnergySummary {
    pub requests_with_energy: u64,
    pub energy_joules: f64,
    pub energy_kwh: f64,
    pub duration_seconds: f64,
    pub energy_kwh_consumed: f64,
    pub energy_kwh_charged: f64,
}

impl EnergySummary {
    fn add_usage(&mut self, usage: &Usage) {
        let Some(energy) = usage
            .energy
            .as_ref()
            .filter(|energy| energy.has_measured_energy())
        else {
            return;
        };
        self.requests_with_energy = self.requests_with_energy.saturating_add(1);
        self.energy_joules += energy.energy_joules.unwrap_or_default();
        self.energy_kwh += energy.energy_kwh.unwrap_or_default();
        self.duration_seconds += energy.duration_seconds.unwrap_or_default();
        self.energy_kwh_consumed += energy.energy_kwh_consumed.unwrap_or_default();
        self.energy_kwh_charged += energy.energy_kwh_charged.unwrap_or_default();
    }
}

/// Aggregated view ledger. `(count, cost_micros)` per key.
#[derive(Debug, Clone, Default, Serialize)]
pub struct LedgerSummary {
    pub requests: usize,
    pub total_cost_micros: u64,
    pub unknown_cost_requests: usize,
    pub by_model: BTreeMap<String, (usize, u64)>,
    pub by_provider: BTreeMap<String, (usize, u64)>,
    pub by_tenant: BTreeMap<String, (usize, u64)>,
    /// Realized cache savings (micro-USD), aggregated exactly like
    /// `total_cost_micros` and its breakdowns — money not spent because a
    /// cached prefix was served below the input rate. Parallel maps (mirroring
    /// `energy_*`) keep the `(count, cost)` breakdown tuples untouched.
    pub total_cache_savings_micros: u64,
    pub cache_savings_by_model: BTreeMap<String, u64>,
    pub cache_savings_by_provider: BTreeMap<String, u64>,
    pub cache_savings_by_tenant: BTreeMap<String, u64>,
    pub energy: EnergySummary,
    pub energy_by_model: BTreeMap<String, EnergySummary>,
    pub energy_by_provider: BTreeMap<String, EnergySummary>,
    pub energy_by_tenant: BTreeMap<String, EnergySummary>,
}

/// Project a `UsageRecord` onto the store's metadata-only `UsageEvent`.
fn record_to_event(r: &UsageRecord) -> sb_store::UsageEvent {
    let energy = r.usage.energy.as_ref();
    sb_store::UsageEvent {
        request_id: r.request_id.clone(),
        provider_id: r.provider_id.clone(),
        model: r.model.clone(),
        account_id: r.account_id.clone(),
        tenant: r.tenant.clone(),
        project: r.project.clone(),
        cost_micros: r.cost_micros,
        cache_savings_micros: r.cache_savings_micros,
        workload_kind: r.workload_kind.map(workload_kind_name).map(str::to_string),
        pricing_unit: r.pricing_unit.map(pricing_unit_name).map(str::to_string),
        units_consumed: r.units_consumed,
        input_tokens: r.usage.input_tokens,
        output_tokens: r.usage.output_tokens,
        cached_input_tokens: r.usage.cached_input_tokens,
        latency_ms: r.latency_ms,
        streamed: r.streamed,
        energy_joules: energy.and_then(|energy| energy.energy_joules),
        energy_kwh: energy.and_then(|energy| energy.energy_kwh),
        energy_duration_seconds: energy.and_then(|energy| energy.duration_seconds),
        energy_measurement_available: energy.and_then(|energy| energy.measurement_available),
        energy_attribution_method: energy.and_then(|energy| energy.attribution_method.clone()),
        energy_kwh_consumed: energy.and_then(|energy| energy.energy_kwh_consumed),
        energy_kwh_charged: energy.and_then(|energy| energy.energy_kwh_charged),
        energy_accounting_method: energy.and_then(|energy| energy.accounting_method.clone()),
        energy_total_cost_usd: energy.and_then(|energy| energy.total_cost_usd),
        created_at_ms: (r.timestamp_unix as i64).saturating_mul(1000),
    }
}

fn workload_kind_name(kind: WorkloadKind) -> &'static str {
    match kind {
        WorkloadKind::TextGeneration => "text_generation",
        WorkloadKind::Embedding => "embedding",
        WorkloadKind::ImageGeneration => "image_generation",
        WorkloadKind::VideoGeneration => "video_generation",
        WorkloadKind::WorkflowExecution => "workflow_execution",
    }
}

fn pricing_unit_name(unit: PricingUnit) -> &'static str {
    match unit {
        PricingUnit::PerImage => "per_image",
        PricingUnit::PerMegapixel => "per_megapixel",
        PricingUnit::PerSecond => "per_second",
        PricingUnit::PerVideo => "per_video",
        PricingUnit::TokenMetered => "token_metered",
        PricingUnit::Credits => "credits",
        PricingUnit::Quota => "quota",
    }
}

fn event_to_record(event: sb_store::UsageEvent) -> UsageRecord {
    UsageRecord {
        request_id: event.request_id,
        tenant_id: 1,
        owner_id: None,
        provider_id: event.provider_id,
        model: event.model,
        account_id: event.account_id,
        timestamp_unix: event.created_at_ms.max(0) as u64 / 1000,
        usage: Usage {
            input_tokens: event.input_tokens,
            output_tokens: event.output_tokens,
            cached_input_tokens: event.cached_input_tokens,
            ..Usage::default()
        },
        cost_micros: event.cost_micros,
        cache_savings_micros: event.cache_savings_micros,
        latency_ms: event.latency_ms,
        streamed: event.streamed,
        workload_kind: event.workload_kind.as_deref().and_then(parse_workload_kind),
        pricing_unit: event.pricing_unit.as_deref().and_then(parse_pricing_unit),
        units_consumed: event.units_consumed,
        tenant: event.tenant,
        project: event.project,
    }
}

fn parse_workload_kind(value: &str) -> Option<WorkloadKind> {
    match value {
        "text_generation" => Some(WorkloadKind::TextGeneration),
        "embedding" => Some(WorkloadKind::Embedding),
        "image_generation" => Some(WorkloadKind::ImageGeneration),
        "video_generation" => Some(WorkloadKind::VideoGeneration),
        "workflow_execution" => Some(WorkloadKind::WorkflowExecution),
        _ => None,
    }
}

fn parse_pricing_unit(value: &str) -> Option<PricingUnit> {
    match value {
        "per_image" => Some(PricingUnit::PerImage),
        "per_megapixel" => Some(PricingUnit::PerMegapixel),
        "per_second" => Some(PricingUnit::PerSecond),
        "per_video" => Some(PricingUnit::PerVideo),
        "token_metered" => Some(PricingUnit::TokenMetered),
        "credits" => Some(PricingUnit::Credits),
        "quota" => Some(PricingUnit::Quota),
        _ => None,
    }
}

fn apply_records(summary: &mut LedgerSummary, records: &[UsageRecord]) {
    summary.requests += records.len();
    for record in records {
        let cost_micros = record.cost_micros.unwrap_or(0);
        let savings_micros = record.cache_savings_micros.unwrap_or(0);
        if record.cost_micros.is_none() {
            summary.unknown_cost_requests = summary.unknown_cost_requests.saturating_add(1);
        }
        summary.total_cost_micros = summary.total_cost_micros.saturating_add(cost_micros);
        summary.total_cache_savings_micros = summary
            .total_cache_savings_micros
            .saturating_add(savings_micros);
        let model = summary.by_model.entry(record.model.clone()).or_default();
        model.0 += 1;
        model.1 = model.1.saturating_add(cost_micros);
        if savings_micros > 0 {
            let m = summary
                .cache_savings_by_model
                .entry(record.model.clone())
                .or_default();
            *m = m.saturating_add(savings_micros);
        }
        let provider = summary
            .by_provider
            .entry(record.provider_id.clone())
            .or_default();
        provider.0 += 1;
        provider.1 = provider.1.saturating_add(cost_micros);
        if savings_micros > 0 {
            let p = summary
                .cache_savings_by_provider
                .entry(record.provider_id.clone())
                .or_default();
            *p = p.saturating_add(savings_micros);
        }
        let has_energy = record
            .usage
            .energy
            .as_ref()
            .is_some_and(|energy| energy.has_measured_energy());
        if has_energy {
            summary
                .energy_by_model
                .entry(record.model.clone())
                .or_default()
                .add_usage(&record.usage);
            summary
                .energy_by_provider
                .entry(record.provider_id.clone())
                .or_default()
                .add_usage(&record.usage);
            summary.energy.add_usage(&record.usage);
        }
        if let Some(tenant) = &record.tenant {
            let t = summary.by_tenant.entry(tenant.clone()).or_default();
            t.0 += 1;
            t.1 = t.1.saturating_add(cost_micros);
            if savings_micros > 0 {
                let ts = summary
                    .cache_savings_by_tenant
                    .entry(tenant.clone())
                    .or_default();
                *ts = ts.saturating_add(savings_micros);
            }
            if has_energy {
                summary
                    .energy_by_tenant
                    .entry(tenant.clone())
                    .or_default()
                    .add_usage(&record.usage);
            }
        }
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
    let to_energy = |energy: &sb_store::UsageEnergyRollup| EnergySummary {
        requests_with_energy: energy.requests_with_energy,
        energy_joules: energy.energy_joules,
        energy_kwh: energy.energy_kwh,
        duration_seconds: energy.duration_seconds,
        energy_kwh_consumed: energy.energy_kwh_consumed,
        energy_kwh_charged: energy.energy_kwh_charged,
    };
    let to_energy_map = |buckets: &[sb_store::UsageEnergyBucket]| {
        buckets
            .iter()
            .map(|bucket| (bucket.key.clone(), to_energy(&bucket.energy)))
            .collect()
    };
    let to_savings_map = |buckets: &[sb_store::UsageSavingsBucket]| {
        buckets
            .iter()
            .map(|(k, savings)| (k.clone(), *savings))
            .collect()
    };
    LedgerSummary {
        requests: rollup.requests as usize,
        total_cost_micros: rollup.total_cost_micros,
        unknown_cost_requests: rollup.unknown_cost_requests as usize,
        by_model: to_map(&rollup.by_model),
        by_provider: to_map(&rollup.by_provider),
        by_tenant: to_map(&rollup.by_tenant),
        total_cache_savings_micros: rollup.total_cache_savings_micros,
        cache_savings_by_model: to_savings_map(&rollup.cache_savings_by_model),
        cache_savings_by_provider: to_savings_map(&rollup.cache_savings_by_provider),
        cache_savings_by_tenant: to_savings_map(&rollup.cache_savings_by_tenant),
        energy: to_energy(&rollup.energy),
        energy_by_model: to_energy_map(&rollup.energy_by_model),
        energy_by_provider: to_energy_map(&rollup.energy_by_provider),
        energy_by_tenant: to_energy_map(&rollup.energy_by_tenant),
    }
}

fn totals_from_summary(summary: &LedgerSummary, tenant: Option<&str>) -> UsageReconciliationTotals {
    if let Some(tenant) = tenant {
        let (requests, cost_micros) = summary.by_tenant.get(tenant).copied().unwrap_or_default();
        UsageReconciliationTotals {
            requests: requests as u64,
            cost_micros,
        }
    } else {
        UsageReconciliationTotals {
            requests: summary.requests as u64,
            cost_micros: summary.total_cost_micros,
        }
    }
}

fn totals_from_records(records: &[UsageRecord], tenant: Option<&str>) -> UsageReconciliationTotals {
    let mut totals = UsageReconciliationTotals::default();
    for record in records {
        if tenant
            .map(|tenant| record.tenant.as_deref() == Some(tenant))
            .unwrap_or(true)
        {
            totals.requests = totals.requests.saturating_add(1);
            totals.cost_micros = totals
                .cost_micros
                .saturating_add(record.cost_micros.unwrap_or(0));
        }
    }
    totals
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_core::{Price, PricingUnit, TokenKind, UnitPrice, Usage};

    #[test]
    fn usage_record_accepts_unknown_cost_as_json_null() {
        let record = UsageRecord::priced(
            "job_unknown",
            "fal",
            "fal-ai/unknown",
            None,
            Usage::default(),
            1,
            false,
            0,
        );
        let mut value = serde_json::to_value(record).expect("serialize record");
        value["cost_micros"] = serde_json::Value::Null;
        assert!(
            serde_json::from_value::<UsageRecord>(value).is_ok(),
            "unknown workload prices must round-trip as null rather than known-free zero"
        );
    }

    #[test]
    fn unit_cost_computation_covers_every_pricing_unit() {
        let cost = |pricing_unit, unit_price_micros, units| {
            compute_unit_cost_micros(
                Some(UnitPrice {
                    pricing_unit,
                    unit_price_micros,
                }),
                Some(units),
            )
        };
        assert_eq!(cost(PricingUnit::PerImage, 30_000, 2.0), Some(60_000));
        assert_eq!(cost(PricingUnit::PerMegapixel, 20_000, 1.5), Some(30_000));
        assert_eq!(cost(PricingUnit::PerSecond, 50_000, 3.5), Some(175_000));
        assert_eq!(cost(PricingUnit::PerVideo, 12_000, 2.0), Some(24_000));
        assert_eq!(cost(PricingUnit::Credits, 100, 2.5), Some(250));
        assert_eq!(cost(PricingUnit::Quota, 1, 2.0), Some(2));
        assert_eq!(cost(PricingUnit::TokenMetered, 1, 2.0), None);
        assert_eq!(compute_unit_cost_micros(None, Some(1.0)), None);
        assert_eq!(
            compute_unit_cost_micros(
                Some(UnitPrice {
                    pricing_unit: PricingUnit::PerImage,
                    unit_price_micros: 30_000,
                }),
                None,
            ),
            None
        );
    }

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

    /// Catalog with an Input AND a CachedInput price for model `m` — Anthropic's
    /// documented cache-read ratio (cached = 0.1x input).
    fn cache_priced_catalog() -> Catalog {
        let mut catalog = priced_catalog();
        catalog.prices.push(Price {
            tenant_id: Default::default(),
            model_id: "m".into(),
            token_kind: TokenKind::CachedInput,
            unit_price_micros_per_mtok: 300_000, // 0.1x the $3/Mtok input rate
            effective_from: "2025-01-01T00:00:00Z".into(),
            effective_to: None,
        });
        catalog
    }

    #[test]
    fn cache_savings_priced_from_catalog_delta() {
        let catalog = cache_priced_catalog();
        let usage = Usage {
            input_tokens: 200,
            cached_input_tokens: 1000,
            ..Usage::default()
        };
        // 1000 * (3_000_000 - 300_000)/1e6 = 1000 * 2_700_000/1e6 = 2700 micros
        assert_eq!(compute_cache_savings_micros(&catalog, "m", &usage), 2_700);
        // Unknown model -> 0.
        assert_eq!(compute_cache_savings_micros(&catalog, "ghost", &usage), 0);
    }

    #[test]
    fn cache_savings_is_zero_without_a_cached_price() {
        // priced_catalog has Input+Output but NO CachedInput price.
        let catalog = priced_catalog();
        let usage = Usage {
            cached_input_tokens: 1000,
            ..Usage::default()
        };
        assert_eq!(
            compute_cache_savings_micros(&catalog, "m", &usage),
            0,
            "no CachedInput price => unknown savings, not a fabricated discount"
        );
    }

    #[test]
    fn cache_savings_is_zero_when_cached_not_cheaper_than_input() {
        let mut catalog = priced_catalog();
        // CachedInput priced >= Input: no real discount, so no savings.
        catalog.prices.push(Price {
            tenant_id: Default::default(),
            model_id: "m".into(),
            token_kind: TokenKind::CachedInput,
            unit_price_micros_per_mtok: 3_000_000, // == input rate
            effective_from: "2025-01-01T00:00:00Z".into(),
            effective_to: None,
        });
        let usage = Usage {
            cached_input_tokens: 1000,
            ..Usage::default()
        };
        assert_eq!(compute_cache_savings_micros(&catalog, "m", &usage), 0);
        // No cached tokens => nothing to save regardless of prices.
        assert_eq!(
            compute_cache_savings_micros(&cache_priced_catalog(), "m", &Usage::default()),
            0
        );
    }

    #[test]
    fn cache_savings_math_saturates_instead_of_overflowing() {
        let mut catalog = Catalog::default();
        for (kind, price) in [(TokenKind::Input, u64::MAX), (TokenKind::CachedInput, 1)] {
            catalog.prices.push(Price {
                tenant_id: Default::default(),
                model_id: "m".into(),
                token_kind: kind,
                unit_price_micros_per_mtok: price,
                effective_from: "2025-01-01T00:00:00Z".into(),
                effective_to: None,
            });
        }
        let usage = Usage {
            cached_input_tokens: u64::MAX,
            ..Usage::default()
        };
        // (u64::MAX - 1).saturating_mul(u64::MAX) saturates to u64::MAX, then /1e6.
        assert_eq!(
            compute_cache_savings_micros(&catalog, "m", &usage),
            u64::MAX / 1_000_000
        );
    }

    #[test]
    fn summary_aggregates_cache_savings_across_breakdowns() {
        let catalog = cache_priced_catalog();
        let ledger = UsageLedger::in_memory();
        let usage = Usage {
            input_tokens: 200,
            cached_input_tokens: 1000,
            ..Usage::default()
        };
        for rid in ["req1", "req2"] {
            ledger.record(
                UsageRecord::new(
                    rid,
                    "anthropic",
                    "m",
                    Some("acct".into()),
                    usage.clone(),
                    10,
                    false,
                    &catalog,
                )
                .with_tenant(Some("acme".into())),
            );
        }

        let summary = ledger.summary();
        // 2 requests * 2700 micros each.
        assert_eq!(summary.total_cache_savings_micros, 5_400);
        assert_eq!(summary.cache_savings_by_model.get("m"), Some(&5_400));
        assert_eq!(
            summary.cache_savings_by_provider.get("anthropic"),
            Some(&5_400)
        );
        assert_eq!(summary.cache_savings_by_tenant.get("acme"), Some(&5_400));
        // Cost accounting is untouched by savings (input priced, cached at cached rate):
        // 200*3 + 1000*0.3 = 600 + 300 = 900 micros per request.
        assert_eq!(summary.total_cost_micros, 1_800);
    }

    #[test]
    fn store_hydration_preserves_cache_savings_after_restart() {
        use sb_store::{SqliteStore, StateStore};
        let catalog = cache_priced_catalog();
        let usage = Usage {
            input_tokens: 200,
            cached_input_tokens: 1000,
            ..Usage::default()
        };
        let store: Arc<dyn StateStore> = Arc::new(SqliteStore::in_memory().unwrap());

        let ledger = UsageLedger::in_memory().with_store(store.clone());
        ledger.record(
            UsageRecord::new("r1", "anthropic", "m", None, usage, 5, false, &catalog)
                .with_tenant(Some("acme".into())),
        );
        assert_eq!(ledger.summary().total_cache_savings_micros, 2_700);
        assert_eq!(
            store.usage_rollup().unwrap().total_cache_savings_micros,
            2_700
        );

        // Fresh ledger on the SAME store hydrates realized savings from durable
        // events (cached_input_tokens round-tripped; savings summed in SQL).
        let restarted = UsageLedger::in_memory().with_store(store.clone());
        let summary = restarted.summary();
        assert_eq!(summary.total_cache_savings_micros, 2_700);
        assert_eq!(
            summary.cache_savings_by_provider.get("anthropic"),
            Some(&2_700)
        );
        assert_eq!(summary.cache_savings_by_tenant.get("acme"), Some(&2_700));
        // Raw cached-input tokens survived, keeping the row re-priceable.
        let recent = restarted.recent(1);
        assert_eq!(recent[0].usage.cached_input_tokens, 1000);
        assert_eq!(recent[0].cache_savings_micros, Some(2_700));
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
            "req1",
            "anthropic",
            "m",
            Some("acct".into()),
            usage.clone(),
            42,
            false,
            &catalog,
        ));
        ledger.record(UsageRecord::new(
            "req2",
            "anthropic",
            "m",
            None,
            usage,
            10,
            true,
            &catalog,
        ));

        assert_eq!(ledger.len(), 2);
        let summary = ledger.summary();
        assert_eq!(summary.requests, 2);
        assert_eq!(summary.total_cost_micros, 21_000);
        assert_eq!(summary.by_model.get("m"), Some(&(2, 21_000)));
        assert_eq!(summary.by_provider.get("anthropic"), Some(&(2, 21_000)));
    }

    #[test]
    fn ledger_aggregates_energy_separately_from_cost() {
        let catalog = priced_catalog();
        let ledger = UsageLedger::in_memory();
        let usage = Usage {
            input_tokens: 1000,
            output_tokens: 500,
            energy: Some(sb_core::EnergyUsage {
                energy_joules: Some(5.23),
                energy_kwh: Some(0.00000145),
                duration_seconds: Some(0.0183),
                measurement_available: Some(true),
                attribution_method: Some("time_weighted".into()),
                ..Default::default()
            }),
            ..Usage::default()
        };

        ledger.record(
            UsageRecord::new(
                "req-energy",
                "neuralwatt",
                "m",
                None,
                usage,
                42,
                false,
                &catalog,
            )
            .with_tenant(Some("acme".into())),
        );

        let summary = ledger.summary();
        assert_eq!(summary.total_cost_micros, 10_500);
        assert_eq!(summary.energy.requests_with_energy, 1);
        assert_eq!(summary.energy.energy_joules, 5.23);
        assert_eq!(
            summary.energy_by_provider["neuralwatt"].energy_kwh,
            0.00000145
        );
        assert_eq!(summary.energy_by_tenant["acme"].duration_seconds, 0.0183);
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
            "r1",
            "anthropic",
            "m",
            Some("a".into()),
            usage.clone(),
            5,
            false,
            &catalog,
        ));
        ledger.record(UsageRecord::new(
            "r2",
            "anthropic",
            "m",
            None,
            usage.clone(),
            6,
            true,
            &catalog,
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
    fn durability_health_counts_inserted_and_duplicate_writes() {
        use sb_store::{SqliteStore, StateStore};
        let catalog = priced_catalog();
        let store: Arc<dyn StateStore> = Arc::new(SqliteStore::in_memory().unwrap());
        let ledger = UsageLedger::in_memory().with_store(store);
        let record = UsageRecord::new(
            "r1",
            "anthropic",
            "m",
            Some("a".into()),
            Usage::default(),
            5,
            false,
            &catalog,
        );

        ledger.record(record.clone());
        ledger.record(record);

        let health = ledger.durability_health();
        assert_eq!(health.status, "durable");
        assert!(health.store_configured);
        assert_eq!(health.persisted_writes, 1);
        assert_eq!(health.duplicate_ignored_writes, 1);
        assert_eq!(health.memory_writes, 0);
        assert_eq!(health.failed_writes, 0);
        assert_eq!(health.last_outcome.as_deref(), Some("duplicate_ignored"));
        assert_eq!(ledger.summary().requests, 1);
    }

    #[test]
    fn durability_health_reports_post_commit_required_failures() {
        let catalog = priced_catalog();
        let store: Arc<dyn sb_store::StateStore> = Arc::new(FailingUsageStore);
        let ledger = UsageLedger::in_memory().with_store(store);
        let err = ledger
            .record_checked_post_commit(UsageRecord::new(
                "r1",
                "anthropic",
                "m",
                Some("a".into()),
                Usage::default(),
                5,
                true,
                &catalog,
            ))
            .unwrap_err();

        assert!(err.contains("usage store write failed"));
        let health = ledger.durability_health();
        assert_eq!(health.status, "post_commit_failed");
        assert_eq!(health.failed_writes, 1);
        assert_eq!(health.post_commit_failed_writes, 1);
        assert_eq!(health.last_outcome.as_deref(), Some("post_commit_failed"));
        assert_eq!(health.last_request_id.as_deref(), Some("r1"));
    }

    #[test]
    fn reconciliation_is_ok_for_clean_durable_usage_and_duplicates() {
        use sb_store::{SqliteStore, StateStore};
        let catalog = priced_catalog();
        let store: Arc<dyn StateStore> = Arc::new(SqliteStore::in_memory().unwrap());
        let ledger = UsageLedger::in_memory().with_store(store);
        let record = UsageRecord::new(
            "r1",
            "anthropic",
            "m",
            Some("a".into()),
            Usage::default(),
            5,
            false,
            &catalog,
        );

        ledger.record(record.clone());
        ledger.record(record);

        let report = ledger.reconcile(None);
        assert_eq!(report.status, "ok");
        assert!(report.billing_grade);
        assert_eq!(report.durable.requests, 1);
        assert_eq!(report.ledger.requests, 1);
        assert_eq!(report.memory_fallback.requests, 0);
        assert_eq!(report.delta.unexplained_requests, 0);
        assert_eq!(report.duplicate_ignored_writes, 1);
        assert!(report.issues.is_empty(), "{report:?}");
    }

    #[test]
    fn reconciliation_marks_memory_fallback_degraded() {
        let catalog = priced_catalog();
        let store: Arc<dyn sb_store::StateStore> = Arc::new(FailingUsageStore);
        let ledger = UsageLedger::in_memory().with_store(store);

        ledger.record(UsageRecord::new(
            "r1",
            "anthropic",
            "m",
            Some("a".into()),
            Usage::default(),
            5,
            false,
            &catalog,
        ));

        let report = ledger.reconcile(None);
        assert_eq!(report.status, "degraded");
        assert!(!report.billing_grade);
        assert_eq!(report.durable.requests, 0);
        assert_eq!(report.ledger.requests, 1);
        assert_eq!(report.memory_fallback.requests, 1);
        assert_eq!(report.delta.ledger_minus_durable_requests, 1);
        assert!(report.issues.contains(&"memory_fallback".to_string()));
    }

    #[test]
    fn reconciliation_marks_post_commit_failure_inconsistent() {
        let catalog = priced_catalog();
        let store: Arc<dyn sb_store::StateStore> = Arc::new(FailingUsageStore);
        let ledger = UsageLedger::in_memory().with_store(store);

        let _ = ledger.record_checked_post_commit(UsageRecord::new(
            "r1",
            "anthropic",
            "m",
            Some("a".into()),
            Usage::default(),
            5,
            true,
            &catalog,
        ));

        let report = ledger.reconcile(None);
        assert_eq!(report.status, "inconsistent");
        assert!(!report.billing_grade);
        assert_eq!(report.post_commit_failed_writes, 1);
        assert!(report
            .issues
            .contains(&"post_commit_usage_failure".to_string()));
    }

    #[test]
    fn reconciliation_marks_fresh_rollup_failure_degraded() {
        let store: Arc<dyn sb_store::StateStore> =
            Arc::new(RollupFailsAfterHydrateStore::default());
        let ledger = UsageLedger::in_memory().with_store(store);

        let report = ledger.reconcile(None);
        assert_eq!(report.status, "degraded");
        assert!(!report.billing_grade);
        assert_eq!(report.rollup_failures, 1);
        assert!(report.issues.contains(&"durable_rollup_failed".to_string()));
        assert!(report.issues.contains(&"rollup_failures".to_string()));
    }

    #[test]
    fn jsonl_sink_is_append_only_and_parseable() {
        let mut path = std::env::temp_dir();
        path.push(format!("sb-ledger-test-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let catalog = priced_catalog();
        let ledger = UsageLedger::with_sink(&path);
        ledger.record(UsageRecord::new(
            "req1",
            "p",
            "m",
            None,
            Usage::default(),
            1,
            false,
            &catalog,
        ));
        ledger.record(UsageRecord::new(
            "req2",
            "p",
            "m",
            None,
            Usage::default(),
            2,
            false,
            &catalog,
        ));

        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "two append-only lines");
        // each line is a parseable record
        let first: UsageRecord = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first.request_id, "req1");

        std::fs::remove_file(&path).ok();
    }

    #[derive(Default)]
    struct RollupFailsAfterHydrateStore {
        rollup_calls: std::sync::atomic::AtomicUsize,
    }

    impl sb_store::StateStore for RollupFailsAfterHydrateStore {
        fn record_revision(&self, _rec: &sb_store::RevisionRecord) -> sb_store::Result<()> {
            Ok(())
        }

        fn list_revisions(&self, _limit: usize) -> sb_store::Result<Vec<sb_store::RevisionRecord>> {
            Ok(Vec::new())
        }

        fn get_revision(
            &self,
            _revision: u64,
        ) -> sb_store::Result<Option<sb_store::RevisionRecord>> {
            Ok(None)
        }

        fn record_audit(&self, _entry: &sb_store::AuditEntry) -> sb_store::Result<()> {
            Ok(())
        }

        fn list_audit(&self, _limit: usize) -> sb_store::Result<Vec<sb_store::AuditEntry>> {
            Ok(Vec::new())
        }

        fn record_usage(
            &self,
            _event: &sb_store::UsageEvent,
        ) -> sb_store::Result<sb_store::UsageWriteOutcome> {
            Ok(sb_store::UsageWriteOutcome::Inserted)
        }

        fn usage_rollup(&self) -> sb_store::Result<sb_store::UsageRollup> {
            if self
                .rollup_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                == 0
            {
                Ok(sb_store::UsageRollup::default())
            } else {
                Err(sb_store::StoreError("forced rollup failure".into()))
            }
        }

        fn recent_usage(&self, _limit: usize) -> sb_store::Result<Vec<sb_store::UsageEvent>> {
            Ok(Vec::new())
        }

        fn idempotency_get(
            &self,
            _key: &str,
        ) -> sb_store::Result<Option<sb_store::IdempotencyRecord>> {
            Ok(None)
        }

        fn idempotency_put(&self, _rec: &sb_store::IdempotencyRecord) -> sb_store::Result<bool> {
            Ok(true)
        }

        fn put_draft(&self, _rec: &sb_store::DraftRecord) -> sb_store::Result<()> {
            Ok(())
        }

        fn get_draft(&self, _id: &str) -> sb_store::Result<Option<sb_store::DraftRecord>> {
            Ok(None)
        }

        fn list_drafts(&self) -> sb_store::Result<Vec<sb_store::DraftRecord>> {
            Ok(Vec::new())
        }

        fn delete_draft(&self, _id: &str) -> sb_store::Result<()> {
            Ok(())
        }
    }

    struct FailingUsageStore;

    impl sb_store::StateStore for FailingUsageStore {
        fn record_revision(&self, _rec: &sb_store::RevisionRecord) -> sb_store::Result<()> {
            Ok(())
        }

        fn list_revisions(&self, _limit: usize) -> sb_store::Result<Vec<sb_store::RevisionRecord>> {
            Ok(Vec::new())
        }

        fn get_revision(
            &self,
            _revision: u64,
        ) -> sb_store::Result<Option<sb_store::RevisionRecord>> {
            Ok(None)
        }

        fn record_audit(&self, _entry: &sb_store::AuditEntry) -> sb_store::Result<()> {
            Ok(())
        }

        fn list_audit(&self, _limit: usize) -> sb_store::Result<Vec<sb_store::AuditEntry>> {
            Ok(Vec::new())
        }

        fn record_usage(
            &self,
            _event: &sb_store::UsageEvent,
        ) -> sb_store::Result<sb_store::UsageWriteOutcome> {
            Err(sb_store::StoreError("forced usage failure".into()))
        }

        fn usage_rollup(&self) -> sb_store::Result<sb_store::UsageRollup> {
            Ok(sb_store::UsageRollup::default())
        }

        fn recent_usage(&self, _limit: usize) -> sb_store::Result<Vec<sb_store::UsageEvent>> {
            Ok(Vec::new())
        }

        fn idempotency_get(
            &self,
            _key: &str,
        ) -> sb_store::Result<Option<sb_store::IdempotencyRecord>> {
            Ok(None)
        }

        fn idempotency_put(&self, _rec: &sb_store::IdempotencyRecord) -> sb_store::Result<bool> {
            Ok(true)
        }

        fn put_draft(&self, _rec: &sb_store::DraftRecord) -> sb_store::Result<()> {
            Ok(())
        }

        fn get_draft(&self, _id: &str) -> sb_store::Result<Option<sb_store::DraftRecord>> {
            Ok(None)
        }

        fn list_drafts(&self) -> sb_store::Result<Vec<sb_store::DraftRecord>> {
            Ok(Vec::new())
        }

        fn delete_draft(&self, _id: &str) -> sb_store::Result<()> {
            Ok(())
        }
    }
}
