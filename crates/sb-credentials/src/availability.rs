//! Per-(account, model) availability with cooldowns. The 9router insight,
//! done cleanly: a model-rate-limited account keeps serving its OTHER models;
//! only a credential/auth failure sidelines the whole account.
//!
//! Methods take an explicit `now: Instant` so lock expiry is deterministically
//! testable without sleeping.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use sb_core::ErrorClass;

/// Sentinel model key meaning "the whole account is locked".
const MODEL_ALL: &str = "__all__";

#[derive(Default)]
struct AccountState {
    /// model_key -> unlocked_at
    locks: HashMap<String, Instant>,
    /// exponential-backoff level, grows on repeated rate-limit, reset on success
    backoff_level: u32,
}

#[derive(Default)]
pub struct Availability {
    inner: Mutex<HashMap<(String, String), AccountState>>,
}

impl Availability {
    pub fn new() -> Self {
        Availability::default()
    }

    /// Is this account usable for this model right now?
    pub fn is_available(&self, provider: &str, account: &str, model: &str, now: Instant) -> bool {
        let guard = self.inner.lock().expect("availability mutex");
        match guard.get(&key(provider, account)) {
            None => true,
            Some(state) => !locked(state, MODEL_ALL, now) && !locked(state, model, now),
        }
    }

    /// Record a failure; sets the appropriate lock and returns the cooldown
    /// applied. Auth/authorization failures lock the whole account; everything
    /// else locks just the model that failed.
    pub fn report_failure(
        &self,
        provider: &str,
        account: &str,
        model: &str,
        class: ErrorClass,
        now: Instant,
    ) -> Duration {
        let mut guard = self.inner.lock().expect("availability mutex");
        let state = guard.entry(key(provider, account)).or_default();
        let cooldown = cooldown_for(class, state.backoff_level);
        let model_key = if account_wide(class) { MODEL_ALL } else { model };
        state.locks.insert(model_key.to_string(), now + cooldown);
        if is_backoff(class) {
            state.backoff_level = state.backoff_level.saturating_add(1);
        }
        cooldown
    }

    /// Record a success: clear this account's locks and reset its backoff.
    pub fn report_success(&self, provider: &str, account: &str) {
        let mut guard = self.inner.lock().expect("availability mutex");
        if let Some(state) = guard.get_mut(&key(provider, account)) {
            state.locks.clear();
            state.backoff_level = 0;
        }
    }

    /// Earliest moment any of `accounts` becomes available again for `model`
    /// (for a `Retry-After` hint when all are locked).
    pub fn earliest_unlock(
        &self,
        provider: &str,
        accounts: &[String],
        model: &str,
        now: Instant,
    ) -> Option<Instant> {
        let guard = self.inner.lock().expect("availability mutex");
        accounts
            .iter()
            .filter_map(|account| guard.get(&key(provider, account)))
            .flat_map(|state| {
                [state.locks.get(MODEL_ALL), state.locks.get(model)]
                    .into_iter()
                    .flatten()
                    .copied()
            })
            .filter(|unlock| *unlock > now)
            .min()
}
}

fn key(provider: &str, account: &str) -> (String, String) {
    (provider.to_string(), account.to_string())
}

fn locked(state: &AccountState, model_key: &str, now: Instant) -> bool {
    state.locks.get(model_key).is_some_and(|unlock| now < *unlock)
}

fn account_wide(class: ErrorClass) -> bool {
    matches!(class, ErrorClass::Authentication | ErrorClass::Authorization)
}

fn is_backoff(class: ErrorClass) -> bool {
    matches!(
        class,
        ErrorClass::RateLimited | ErrorClass::QuotaExceeded | ErrorClass::ProviderOverloaded
    )
}

/// Cooldown policy by error class. Rate-limit family uses exponential backoff;
/// auth failures get a flat 2 minutes; transient errors 30s.
fn cooldown_for(class: ErrorClass, backoff_level: u32) -> Duration {
    match class {
        ErrorClass::RateLimited | ErrorClass::QuotaExceeded | ErrorClass::ProviderOverloaded => {
            Duration::from_millis(backoff_ms(backoff_level))
        }
        ErrorClass::Authentication | ErrorClass::Authorization => Duration::from_secs(120),
        _ => Duration::from_secs(30),
    }
}

/// 2s, 4s, 8s, ... capped at 5 minutes.
fn backoff_ms(level: u32) -> u64 {
    2000u64.saturating_mul(1u64 << level.min(8)).min(300_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_lock_is_per_model_not_per_account() {
        let av = Availability::new();
        let t0 = Instant::now();
        av.report_failure("anthropic", "acct1", "opus", ErrorClass::RateLimited, t0);
        // opus is locked, but sonnet on the SAME account is still available.
        assert!(!av.is_available("anthropic", "acct1", "opus", t0));
        assert!(av.is_available("anthropic", "acct1", "sonnet", t0));
    }

    #[test]
    fn auth_failure_locks_whole_account() {
        let av = Availability::new();
        let t0 = Instant::now();
        av.report_failure("p", "a", "m1", ErrorClass::Authentication, t0);
        assert!(!av.is_available("p", "a", "m1", t0));
        assert!(!av.is_available("p", "a", "m2", t0)); // whole account down
    }

    #[test]
    fn lock_expires_after_cooldown() {
        let av = Availability::new();
        let t0 = Instant::now();
        let cd = av.report_failure("p", "a", "m", ErrorClass::RateLimited, t0);
        assert!(!av.is_available("p", "a", "m", t0));
        assert!(av.is_available("p", "a", "m", t0 + cd + Duration::from_millis(1)));
    }

    #[test]
    fn success_clears_locks_and_backoff() {
        let av = Availability::new();
        let t0 = Instant::now();
        let cd1 = av.report_failure("p", "a", "m", ErrorClass::RateLimited, t0);
        av.report_success("p", "a");
        assert!(av.is_available("p", "a", "m", t0));
        // backoff reset: next failure cooldown == first (not doubled)
        let cd2 = av.report_failure("p", "a", "m", ErrorClass::RateLimited, t0);
        assert_eq!(cd1, cd2);
    }

    #[test]
    fn backoff_grows_on_repeated_failure() {
        let av = Availability::new();
        let t0 = Instant::now();
        let cd1 = av.report_failure("p", "a", "m", ErrorClass::RateLimited, t0);
        let cd2 = av.report_failure("p", "a", "m", ErrorClass::RateLimited, t0);
        assert!(cd2 > cd1, "backoff should grow: {cd1:?} -> {cd2:?}");
    }

    #[test]
    fn earliest_unlock_reports_soonest() {
        let av = Availability::new();
        let t0 = Instant::now();
        av.report_failure("p", "a", "m", ErrorClass::RateLimited, t0); // ~2s
        av.report_failure("p", "b", "m", ErrorClass::Authentication, t0); // 120s
        let accounts = vec!["a".to_string(), "b".to_string()];
        let unlock = av.earliest_unlock("p", &accounts, "m", t0).unwrap();
        // soonest is account a (~2s), well under account b (120s)
        assert!(unlock < t0 + Duration::from_secs(60));
    }
}
