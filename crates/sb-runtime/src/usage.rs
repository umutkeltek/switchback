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
        usage: Usage,
        started: Instant,
        streamed: bool,
    ) {
        let cost = registry.cost_micros(provider_id, model, &usage);
        self.ledger.record(
            sb_ledger::UsageRecord::priced(
                request_id,
                provider_id,
                model,
                Some(account_id.to_string()),
                usage,
                started.elapsed().as_millis() as u64,
                streamed,
                cost,
            )
            .with_tenant(tenant.map(str::to_string)),
        );
    }

    /// Attributed spend (USD) for one provider, from the usage ledger summary.
    pub(crate) fn provider_spend_usd(&self, provider_id: &str) -> f64 {
        self.ledger
            .summary()
            .by_provider
            .get(provider_id)
            .map(|(_count, micros)| *micros as f64 / 1_000_000.0)
            .unwrap_or(0.0)
    }
}
