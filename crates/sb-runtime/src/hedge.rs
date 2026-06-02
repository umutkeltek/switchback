use std::collections::HashSet;
use std::time::Instant;

use futures::StreamExt;
use sb_adapter::PreparedRequest;
use sb_core::{AiRequest, AiResponse};
use sb_credentials::ResolveOutcome;

use super::collect::collect_response;
use super::helpers::{resolve_egress, session_affinity_key};
use super::Snapshot;

/// A hedged attempt's winning result + the metadata to record it.
pub(crate) struct HedgeWin {
    pub(crate) response: AiResponse,
    pub(crate) target_id: String,
    pub(crate) provider_id: String,
    pub(crate) model: String,
    pub(crate) account_id: String,
    pub(crate) egress: String,
    pub(crate) latency_ms: u64,
    pub(crate) canceled: Vec<HedgeCancel>,
}

#[derive(Clone)]
pub(crate) struct HedgeCancel {
    pub(crate) target_id: String,
    pub(crate) provider_id: String,
    pub(crate) model: String,
}

/// One self-contained non-streaming attempt for the hedge race: resolve an
/// account, refresh the lease, execute, and collect. `None` on any failure.
async fn hedge_attempt(
    snap: &Snapshot,
    req: &AiRequest,
    target: &sb_core::ExecutionTarget,
) -> Option<HedgeWin> {
    let started = Instant::now();
    let adapter = snap.registry.adapter(&target.provider_id)?;
    let ResolveOutcome::Selected { account_id, lease } = snap.resolver.resolve_with_session(
        &target.provider_id,
        &target.model,
        &HashSet::new(),
        session_affinity_key(req),
    ) else {
        return None;
    };
    let egress_id = snap
        .plugins
        .select_egress(req, &target.id)
        .or_else(|| resolve_egress(&snap.config, &target.provider_id, &account_id));
    let egress_eff = snap.registry.effective_egress(egress_id.as_deref());
    let lease = snap
        .resolver
        .fresh_lease(&target.provider_id, &account_id, lease)
        .await
        .ok()?;
    let prepared =
        PreparedRequest::new(req.clone(), target.clone(), Some(lease)).with_egress(egress_id);
    // On a failed attempt, lock the account per the error class and record the
    // breaker — so the sequential fallback (entered when every hedge fails)
    // doesn't re-pick a known-bad account and the circuit reflects the failure.
    let stream = match adapter.execute(prepared).await {
        Ok(stream) => stream,
        Err(error) => {
            snap.resolver.report_failure(
                &target.provider_id,
                &account_id,
                &target.model,
                error.class,
            );
            snap.resolver.circuit_record(&target.provider_id, false);
            return None;
        }
    };
    let response = match collect_response(
        stream,
        req.id.clone(),
        req.model.clone(),
        snap.config.server.max_response_bytes,
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            snap.resolver.report_failure(
                &target.provider_id,
                &account_id,
                &target.model,
                error.class,
            );
            snap.resolver.circuit_record(&target.provider_id, false);
            return None;
        }
    };
    // Success is recorded for the winner only (in `run_hedge`); a racer that
    // completed but lost the race must not skew the account's health signal.
    Some(HedgeWin {
        response,
        target_id: target.id.clone(),
        provider_id: target.provider_id.clone(),
        model: target.model.clone(),
        account_id,
        egress: egress_eff,
        latency_ms: started.elapsed().as_millis() as u64,
        canceled: Vec::new(),
    })
}

/// Race the top `max_parallel` candidates (the n-th delayed by `n*delay_ms`),
/// returning the first success. Losers are cancelled when this returns.
pub(crate) async fn run_hedge(
    snap: &Snapshot,
    req: &AiRequest,
    candidates: &[sb_core::ExecutionTarget],
) -> Option<HedgeWin> {
    let hedge = &snap.config.server.hedge;
    let n = (hedge.max_parallel.max(1) as usize).min(candidates.len());
    let mut futs = futures::stream::FuturesUnordered::new();
    let launched = candidates
        .iter()
        .take(n)
        .map(|target| HedgeCancel {
            target_id: target.id.clone(),
            provider_id: target.provider_id.clone(),
            model: target.model.clone(),
        })
        .collect::<Vec<_>>();
    for (i, target) in candidates.iter().take(n).enumerate() {
        let delay = std::time::Duration::from_millis(hedge.delay_ms.saturating_mul(i as u64));
        futs.push(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            hedge_attempt(snap, req, target).await
        });
    }
    while let Some(result) = futs.next().await {
        if let Some(mut win) = result {
            // Record success for the winner only: losers either failed (already
            // reported in `hedge_attempt`) or completed-but-lost, which must not
            // skew the account's health/breaker signal.
            snap.resolver
                .report_success(&win.provider_id, &win.account_id);
            snap.resolver.circuit_record(&win.provider_id, true);
            win.canceled = launched
                .iter()
                .filter(|launched| launched.target_id != win.target_id)
                .cloned()
                .collect();
            return Some(win); // first success wins; remaining futures are dropped
        }
    }
    None
}
