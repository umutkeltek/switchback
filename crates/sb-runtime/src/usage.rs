use std::time::Instant;

use sb_core::Usage;

use super::Engine;

impl Engine {
    /// Append a usage/cost record for a completed (non-streamed) request,
    /// attributed to `tenant` (for per-tenant rollups + budget enforcement). Cost
    /// is priced from the registry's price index — the SAME one the router routes
    /// on — so a request's route decision and its ledger cost never diverge (#5).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_usage(
        &self,
        registry: &sb_adapters::AdapterRegistry,
        request_id: &str,
        provider_id: &str,
        model: &str,
        account_id: &str,
        tenant: Option<&str>,
        project: Option<&str>,
        usage: Usage,
        started: Instant,
        streamed: bool,
    ) -> Result<(), String> {
        let cost = registry.cost_micros(provider_id, model, &usage);
        let cache_savings = registry.cache_savings_micros(provider_id, model, &usage);
        let record = sb_ledger::UsageRecord::priced(
            request_id,
            provider_id,
            model,
            Some(account_id.to_string()),
            usage,
            started.elapsed().as_millis() as u64,
            streamed,
            cost,
        )
        .with_tenant(tenant.map(str::to_string))
        .with_project(project.map(str::to_string))
        .with_cache_savings(Some(cache_savings));
        if self.store_required {
            self.ledger.record_checked(record)
        } else {
            self.ledger.record(record);
            Ok(())
        }
    }

    pub(crate) fn global_spend_usd(&self) -> Result<f64, String> {
        if let Some(store) = self.store() {
            match store.usage_rollup() {
                Ok(rollup) => return Ok(rollup.total_cost_micros as f64 / 1_000_000.0),
                Err(e) if self.store_required => {
                    return Err(format!("usage store rollup failed: {e}"));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "usage store rollup failed; falling back to memory")
                }
            }
        }
        Ok(self.ledger.summary().total_cost_micros as f64 / 1_000_000.0)
    }

    pub(crate) fn tenant_spend_usd(&self, tenant: &str) -> Result<f64, String> {
        if let Some(store) = self.store() {
            match store.usage_rollup() {
                Ok(rollup) => {
                    let micros = rollup
                        .by_tenant
                        .iter()
                        .find(|(id, ..)| id == tenant)
                        .map(|(_, _, micros)| *micros)
                        .unwrap_or(0);
                    return Ok(micros as f64 / 1_000_000.0);
                }
                Err(e) if self.store_required => {
                    return Err(format!("usage store rollup failed: {e}"));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "usage store rollup failed; falling back to memory")
                }
            }
        }
        Ok(self.ledger.tenant_spend_usd(tenant))
    }

    /// Attributed spend (USD) for one provider, from the usage ledger summary.
    pub(crate) fn provider_spend_usd(&self, provider_id: &str) -> Result<f64, String> {
        if let Some(store) = self.store() {
            match store.usage_rollup() {
                Ok(rollup) => {
                    let micros = rollup
                        .by_provider
                        .iter()
                        .find(|(id, ..)| id == provider_id)
                        .map(|(_, _, micros)| *micros)
                        .unwrap_or(0);
                    return Ok(micros as f64 / 1_000_000.0);
                }
                Err(e) if self.store_required => {
                    return Err(format!("usage store rollup failed: {e}"));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "usage store rollup failed; falling back to memory")
                }
            }
        }
        Ok(self
            .ledger
            .summary()
            .by_provider
            .get(provider_id)
            .map(|(_count, micros)| *micros as f64 / 1_000_000.0)
            .unwrap_or(0.0))
    }
}
