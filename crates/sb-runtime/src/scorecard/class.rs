//! outcome-routing-v1 §2 — the outcome taxonomy. This is the correctness
//! heart of the scorecard: a malformed request or a broken key can never
//! demote a healthy target, because neutral classes never enter the
//! scoreable denominator (Success + Truncated + TargetFailure only).

use sb_core::{ErrorClass, FinishReason};

/// How one attempt terminated, as observed at the `finish_attempt` seam
/// (wired in commit 4): it returned ok (carrying the model's own
/// `finish_reason`), it failed (carrying the adapter's `ErrorClass`), or it
/// was cancelled (client abort / hedge loser — always neutral).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptOutcome {
    Ok(FinishReason),
    Failed(ErrorClass),
    Cancelled,
}

/// §2 taxonomy. `Success` / `Truncated` / `TargetFailure` are scoreable (the
/// only classes counted in the shrinkage posterior's denominator);
/// `Refusal` / `ClientOrAccountFault` / `Cancelled` are neutral —
/// observability only, never demote a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeClass {
    Success,
    Truncated,
    Refusal,
    TargetFailure,
    ClientOrAccountFault,
    Cancelled,
}

impl OutcomeClass {
    /// Scoreable classes are the posterior's denominator. Everything else is
    /// neutral: account-scoped faults (already owned by the credential
    /// resolver/breaker) and safety refusals never punish a target's score.
    pub fn is_scoreable(self) -> bool {
        matches!(
            self,
            OutcomeClass::Success | OutcomeClass::Truncated | OutcomeClass::TargetFailure
        )
    }
}

/// §2 `map(ok, error_class, finish_reason) -> OutcomeClass`.
pub fn classify(outcome: AttemptOutcome) -> OutcomeClass {
    match outcome {
        AttemptOutcome::Ok(reason) => match reason {
            FinishReason::Length => OutcomeClass::Truncated,
            FinishReason::ContentFilter => OutcomeClass::Refusal,
            FinishReason::Error => OutcomeClass::TargetFailure,
            FinishReason::Stop | FinishReason::ToolCalls => OutcomeClass::Success,
        },
        AttemptOutcome::Failed(err) => match err {
            ErrorClass::Authentication
            | ErrorClass::Authorization
            | ErrorClass::QuotaExceeded
            | ErrorClass::InvalidRequest
            | ErrorClass::ContextTooLong
            | ErrorClass::UnsupportedCapability => OutcomeClass::ClientOrAccountFault,
            ErrorClass::SafetyBlocked => OutcomeClass::Refusal,
            ErrorClass::RateLimited
            | ErrorClass::ProviderOverloaded
            | ErrorClass::Timeout
            | ErrorClass::Network
            | ErrorClass::ServerError
            | ErrorClass::StreamInterrupted
            | ErrorClass::Unknown => OutcomeClass::TargetFailure,
        },
        AttemptOutcome::Cancelled => OutcomeClass::Cancelled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_finish_reasons_map_correctly() {
        assert_eq!(
            classify(AttemptOutcome::Ok(FinishReason::Stop)),
            OutcomeClass::Success
        );
        assert_eq!(
            classify(AttemptOutcome::Ok(FinishReason::ToolCalls)),
            OutcomeClass::Success
        );
        assert_eq!(
            classify(AttemptOutcome::Ok(FinishReason::Length)),
            OutcomeClass::Truncated
        );
        assert_eq!(
            classify(AttemptOutcome::Ok(FinishReason::ContentFilter)),
            OutcomeClass::Refusal
        );
        assert_eq!(
            classify(AttemptOutcome::Ok(FinishReason::Error)),
            OutcomeClass::TargetFailure
        );
    }

    #[test]
    fn neutral_error_classes_never_score() {
        for err in [
            ErrorClass::Authentication,
            ErrorClass::Authorization,
            ErrorClass::QuotaExceeded,
            ErrorClass::InvalidRequest,
            ErrorClass::ContextTooLong,
            ErrorClass::UnsupportedCapability,
        ] {
            let class = classify(AttemptOutcome::Failed(err));
            assert_eq!(class, OutcomeClass::ClientOrAccountFault);
            assert!(!class.is_scoreable());
        }
        assert_eq!(
            classify(AttemptOutcome::Failed(ErrorClass::SafetyBlocked)),
            OutcomeClass::Refusal
        );
        assert!(!OutcomeClass::Refusal.is_scoreable());
    }

    #[test]
    fn target_side_error_classes_score() {
        for err in [
            ErrorClass::RateLimited,
            ErrorClass::ProviderOverloaded,
            ErrorClass::Timeout,
            ErrorClass::Network,
            ErrorClass::ServerError,
            ErrorClass::StreamInterrupted,
            ErrorClass::Unknown,
        ] {
            let class = classify(AttemptOutcome::Failed(err));
            assert_eq!(class, OutcomeClass::TargetFailure);
            assert!(class.is_scoreable());
        }
    }

    #[test]
    fn cancelled_is_neutral() {
        let class = classify(AttemptOutcome::Cancelled);
        assert_eq!(class, OutcomeClass::Cancelled);
        assert!(!class.is_scoreable());
    }
}
