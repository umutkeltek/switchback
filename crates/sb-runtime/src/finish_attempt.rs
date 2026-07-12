//! outcome-routing-v1 — the single attempt-terminal seam (build plan commit
//! 4). Every attempt terminal block in `execute.rs` (stream precommit
//! failure, stream Clean/UpstreamError/Aborted, non-stream success/failure,
//! adapter dispatch failure) converges on [`Engine::finish_attempt`] instead
//! of calling `circuit_record`/`post_attempt` directly. It preserves that
//! pre-refactor behavior byte-for-byte and additionally classifies + records
//! the outcome to the scorecard (spec §2).
//!
//! This is a free-standing associated function, not a `&self` method: the
//! streaming path's finish callback must be `'static` (it can run long after
//! `execute_inner` returns), so it cannot borrow `Engine`. Callers pass the
//! `Arc`-cloned pieces they already carry (`resolver`, `plugins`,
//! `scorecard`) and the `ScorecardConfig` captured at dispatch time — i.e.
//! from the snapshot pinned for this request, not a fresh `self.snapshot()`
//! read at completion time — so a config hot-reload mid-stream can never
//! reclassify an attempt that was already dispatched under the old config.

use sb_core::{ErrorClass, FinishReason};

use crate::scorecard::{self, AttemptOutcome};
use crate::Engine;

/// Exactly-once guard for [`Engine::finish_attempt`]: created once per
/// dispatched attempt (right before the adapter call), then moved into
/// whichever terminal branch actually fires. Deliberately not
/// `Clone`/`Copy` — the borrow checker rejects any code path that would try
/// to consume it twice, so "exactly once per attempt" is a compile-time
/// property of the call sites, not a runtime check.
pub(crate) struct AttemptToken(());

impl AttemptToken {
    pub(crate) fn new() -> Self {
        AttemptToken(())
    }
}

/// The attempt's terminal result, as already known at each call site — bridges
/// the adapter/HTTP-facing shape (ok/error_class/finish_reason, used for the
/// circuit breaker + `AttemptInfo`) to the scorecard's `AttemptOutcome`
/// classification (spec §2). `cost_micros` is only ever known on a returned
/// (possibly truncated) response, never on a failure.
pub(crate) enum FinishOutcome {
    /// A returned response, streamed or collected. Always records
    /// `circuit_record(provider_id, true)`; `finish_reason` decides
    /// Success vs Truncated vs Refusal vs TargetFailure (spec §2).
    Ok {
        finish_reason: FinishReason,
        cost_micros: Option<u64>,
    },
    /// An adapter/upstream error. Always records `circuit_record(provider_id,
    /// false)`.
    Failed { error_class: ErrorClass },
    /// Client abort / cancellation (the stream-`Aborted` case). Neutral —
    /// mirrors the pre-refactor `Aborted` branch, which never called
    /// `circuit_record` at all.
    Cancelled,
}

/// Attempt identity, borrowed for the duration of the call — the same values
/// that were already threaded individually to `circuit_record`/`post_attempt`
/// before this refactor.
pub(crate) struct AttemptFinishCtx<'a> {
    pub request_id: &'a str,
    pub target_id: &'a str,
    pub provider_id: &'a str,
    pub account_id: &'a str,
    pub egress: &'a str,
    pub latency_ms: u64,
}

impl Engine {
    /// The exactly-once attempt-terminal seam (outcome-routing-v1 §1). See
    /// the module doc for why this takes no `&self`.
    pub(crate) fn finish_attempt(
        _token: AttemptToken,
        resolver: &sb_credentials::CredentialResolver,
        plugins: &sb_plugin::PluginHost,
        scorecard: &scorecard::Scorecard,
        cfg: &scorecard::ScorecardConfig,
        ctx: AttemptFinishCtx<'_>,
        outcome: FinishOutcome,
    ) {
        let AttemptFinishCtx {
            request_id,
            target_id,
            provider_id,
            account_id,
            egress,
            latency_ms,
        } = ctx;

        // Circuit breaker: byte-for-byte the same as every pre-refactor call
        // site — Ok -> true, Failed -> false, Cancelled -> no call at all
        // (the stream-Aborted branch never touched the breaker).
        match &outcome {
            FinishOutcome::Ok { .. } => resolver.circuit_record(provider_id, true),
            FinishOutcome::Failed { .. } => resolver.circuit_record(provider_id, false),
            FinishOutcome::Cancelled => {}
        }

        let (ok, error_class_str) = match &outcome {
            FinishOutcome::Ok { .. } => (true, None),
            FinishOutcome::Failed { error_class } => (false, Some(error_class.as_str())),
            FinishOutcome::Cancelled => (false, Some("client_aborted")),
        };
        plugins.post_attempt(&sb_plugin::AttemptInfo {
            request_id,
            target_id,
            provider_id,
            account_id,
            egress,
            ok,
            error_class: error_class_str,
            latency_ms,
        });

        // Scorecard (spec §2/§3). `Scorecard::record` itself no-ops when
        // `cfg.enabled` is false, so no separate gate is needed here.
        let cost_micros = match &outcome {
            FinishOutcome::Ok { cost_micros, .. } => *cost_micros,
            FinishOutcome::Failed { .. } | FinishOutcome::Cancelled => None,
        };
        let latency_ms_u32 = u32::try_from(latency_ms).unwrap_or(u32::MAX);
        let attempt_outcome = match outcome {
            FinishOutcome::Ok { finish_reason, .. } => AttemptOutcome::Ok(finish_reason),
            FinishOutcome::Failed { error_class } => AttemptOutcome::Failed(error_class),
            FinishOutcome::Cancelled => AttemptOutcome::Cancelled,
        };
        let class = scorecard::classify(attempt_outcome);
        let err = match attempt_outcome {
            AttemptOutcome::Failed(e) => Some(e),
            AttemptOutcome::Ok(_) | AttemptOutcome::Cancelled => None,
        };
        let sample = scorecard::Sample::new(
            std::time::Instant::now(),
            class,
            latency_ms_u32,
            cost_micros,
            err,
        );
        scorecard.record(
            target_id,
            "any",
            scorecard::Prior::from_config(cfg),
            cfg,
            sample,
        );
    }
}
