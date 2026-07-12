use std::collections::HashSet;
use std::time::Instant;

use futures::StreamExt;
use sb_adapter::PreparedRequest;
use sb_core::{AiRequest, AiResponse};
use sb_credentials::ResolveOutcome;

use super::collect::collect_response;
use super::finish_attempt::{AttemptFinishCtx, AttemptToken, FinishOutcome};
use super::helpers::{resolve_egress, session_affinity_key};
use super::scorecard::{Scorecard, ScorecardConfig};
use super::{Engine, Snapshot};

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

/// Attempt identity captured at dispatch time, owned so it can outlive the
/// borrow of `target`/`account_id` across the `.await` points below.
struct HedgeAttemptCtx {
    request_id: String,
    target_id: String,
    provider_id: String,
    account_id: String,
    egress: String,
}

/// Finalizes ONE hedge racer's scorecard/breaker/plugin attempt exactly
/// once (outcome-routing-v1 F6): explicitly via `finish()` on a known
/// outcome (the winner's Success, or a real adapter/collect failure), or —
/// if this racer's future is dropped before reaching either (the hedge
/// LOSER, canceled the moment `run_hedge` already has a winner and drops the
/// rest of the `FuturesUnordered`) — implicitly as neutral `Cancelled` when
/// the guard drops. Mirrors `stream::FinishGuard`'s "armed at drop ⇒
/// Aborted" pattern for the identical problem in the hedge race: a started
/// attempt whose outcome nobody explicitly observed must still be recorded,
/// neutrally, not silently dropped.
struct HedgeFinishGuard<'a> {
    token: Option<AttemptToken>,
    started: Instant,
    resolver: &'a sb_credentials::CredentialResolver,
    plugins: &'a sb_plugin::PluginHost,
    scorecard: &'a Scorecard,
    scorecard_cfg: &'a ScorecardConfig,
    ctx: HedgeAttemptCtx,
}

impl HedgeFinishGuard<'_> {
    fn finish(mut self, outcome: FinishOutcome) {
        self.finish_inner(outcome);
    }

    fn finish_inner(&mut self, outcome: FinishOutcome) {
        let Some(token) = self.token.take() else {
            return;
        };
        Engine::finish_attempt(
            token,
            self.resolver,
            self.plugins,
            self.scorecard,
            self.scorecard_cfg,
            AttemptFinishCtx {
                request_id: &self.ctx.request_id,
                target_id: &self.ctx.target_id,
                provider_id: &self.ctx.provider_id,
                account_id: &self.ctx.account_id,
                egress: &self.ctx.egress,
                latency_ms: self.started.elapsed().as_millis() as u64,
            },
            outcome,
        );
    }
}

impl Drop for HedgeFinishGuard<'_> {
    fn drop(&mut self) {
        // Still armed at drop -> this racer never reached its own terminal
        // outcome (it lost the race while still in flight) -> neutral.
        self.finish_inner(FinishOutcome::Cancelled);
    }
}

/// One self-contained non-streaming attempt for the hedge race: resolve an
/// account, refresh the lease, execute, and collect. `None` on any failure.
async fn hedge_attempt(
    snap: &Snapshot,
    req: &AiRequest,
    target: &sb_core::ExecutionTarget,
    scorecard: &Scorecard,
    scorecard_cfg: &ScorecardConfig,
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

    // outcome-routing-v1 F6: every STARTED racer (i.e. one that reaches
    // actual dispatch) gets its own dispatch-time token/guard. Account
    // resolution/lease-refresh failures above are not attempts (mirrors the
    // sequential fallback loop in execute.rs, which also only creates a
    // token at dispatch time).
    let guard = HedgeFinishGuard {
        token: Some(AttemptToken::new()),
        started,
        resolver: &snap.resolver,
        plugins: &snap.plugins,
        scorecard,
        scorecard_cfg,
        ctx: HedgeAttemptCtx {
            request_id: req.id.clone(),
            target_id: target.id.clone(),
            provider_id: target.provider_id.clone(),
            account_id: account_id.clone(),
            egress: egress_eff.clone(),
        },
    };

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
            guard.finish(FinishOutcome::Failed {
                error_class: error.class,
            });
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
            guard.finish(FinishOutcome::Failed {
                error_class: error.class,
            });
            return None;
        }
    };
    // This racer reached its own success path -- by construction (see
    // `run_hedge`'s "first Some(win) wins" loop), whichever racer gets here
    // first IS unconditionally the race winner, so it's correct to finalize
    // as Success here rather than deferring to the caller.
    snap.resolver
        .report_success(&target.provider_id, &account_id);
    let cost = snap
        .registry
        .cost_micros(&target.provider_id, &target.model, &response.usage);
    let latency_ms = started.elapsed().as_millis() as u64;
    guard.finish(FinishOutcome::Ok {
        finish_reason: response.finish_reason,
        cost_micros: Some(cost),
    });
    Some(HedgeWin {
        response,
        target_id: target.id.clone(),
        provider_id: target.provider_id.clone(),
        model: target.model.clone(),
        account_id,
        egress: egress_eff,
        latency_ms,
        canceled: Vec::new(),
    })
}

/// Race the top `max_parallel` candidates (the n-th delayed by `n*delay_ms`),
/// returning the first success. Losers are cancelled when this returns; each
/// started loser finalizes itself as neutral `Cancelled` via its own
/// `HedgeFinishGuard` (outcome-routing-v1 F6) when its future is dropped.
pub(crate) async fn run_hedge(
    snap: &Snapshot,
    req: &AiRequest,
    candidates: &[sb_core::ExecutionTarget],
    scorecard: &Scorecard,
    scorecard_cfg: &ScorecardConfig,
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
            hedge_attempt(snap, req, target, scorecard, scorecard_cfg).await
        });
    }
    while let Some(result) = futs.next().await {
        if let Some(mut win) = result {
            win.canceled = launched
                .iter()
                .filter(|launched| launched.target_id != win.target_id)
                .cloned()
                .collect();
            return Some(win); // first success wins; remaining futures are dropped,
                              // each finalizing itself as neutral Cancelled via
                              // its own HedgeFinishGuard (outcome-routing-v1 F6).
        }
    }
    None
}
