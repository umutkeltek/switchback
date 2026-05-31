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
    locks: HashMap<String, LockEntry>,
    /// exponential-backoff level, grows on repeated rate-limit, reset on success
    backoff_level: u32,
}

#[derive(Debug, Clone, Copy)]
struct LockEntry {
    unlock_at: Instant,
    class: ErrorClass,
}

#[derive(Debug, Clone)]
pub struct LockSnapshot {
    pub model: Option<String>,
    pub retry_after: Duration,
    pub error_class: ErrorClass,
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
        let model_key = if account_wide(class) {
            MODEL_ALL
        } else {
            model
        };
        state.locks.insert(
            model_key.to_string(),
            LockEntry {
                unlock_at: now + cooldown,
                class,
            },
        );
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
                    .map(|lock| lock.unlock_at)
            })
            .filter(|unlock| *unlock > now)
            .min()
    }

    /// Active non-secret locks for an account. When `model` is empty, return all
    /// active locks; otherwise return account-wide plus the requested model lock.
    pub fn locks_for(
        &self,
        provider: &str,
        account: &str,
        model: &str,
        now: Instant,
    ) -> Vec<LockSnapshot> {
        let guard = self.inner.lock().expect("availability mutex");
        let Some(state) = guard.get(&key(provider, account)) else {
            return Vec::new();
        };

        let mut locks = state
            .locks
            .iter()
            .filter(|(model_key, lock)| {
                lock.unlock_at > now
                    && (model.is_empty()
                        || model_key.as_str() == MODEL_ALL
                        || model_key.as_str() == model)
            })
            .map(|(model_key, lock)| LockSnapshot {
                model: (model_key.as_str() != MODEL_ALL).then(|| model_key.clone()),
                retry_after: lock.unlock_at.saturating_duration_since(now),
                error_class: lock.class,
            })
            .collect::<Vec<_>>();
        locks.sort_by(|a, b| a.retry_after.cmp(&b.retry_after));
        locks
    }

    /// Operator override: clear an active lockout for an account. `model = None`
    /// clears every lock on the account; `Some(model)` clears only that
    /// account/model lock. Returns whether an active lock was actually removed.
    pub fn reset_lockout(&self, provider: &str, account: &str, model: Option<&str>) -> bool {
        let account_key = key(provider, account);
        let mut guard = self.inner.lock().expect("availability mutex");
        let Some(state) = guard.get_mut(&account_key) else {
            return false;
        };

        let cleared = match model {
            Some(model) => state.locks.remove(model).is_some(),
            None => {
                let had_locks = !state.locks.is_empty();
                state.locks.clear();
                had_locks
            }
        };
        if state.locks.is_empty() {
            state.backoff_level = 0;
            guard.remove(&account_key);
        }
        cleared
    }
}

fn key(provider: &str, account: &str) -> (String, String) {
    (provider.to_string(), account.to_string())
}

fn locked(state: &AccountState, model_key: &str, now: Instant) -> bool {
    state
        .locks
        .get(model_key)
        .is_some_and(|lock| now < lock.unlock_at)
}

fn account_wide(class: ErrorClass) -> bool {
    matches!(
        class,
        ErrorClass::Authentication | ErrorClass::Authorization
    )
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
    fn reset_lockout_clears_only_the_requested_model() {
        let av = Availability::new();
        let t0 = Instant::now();
        av.report_failure("p", "a", "m1", ErrorClass::RateLimited, t0);
        av.report_failure("p", "a", "m2", ErrorClass::RateLimited, t0);

        assert!(av.reset_lockout("p", "a", Some("m1")));
        assert!(av.is_available("p", "a", "m1", t0));
        assert!(!av.is_available("p", "a", "m2", t0));
        assert!(!av.reset_lockout("p", "a", Some("missing")));

        assert!(av.reset_lockout("p", "a", None));
        assert!(av.is_available("p", "a", "m2", t0));
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

    #[test]
    fn locks_for_reports_scope_class_and_retry_after() {
        let av = Availability::new();
        let t0 = Instant::now();
        av.report_failure("p", "a", "m", ErrorClass::Authentication, t0);
        let locks = av.locks_for("p", "a", "", t0);
        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].model, None);
        assert_eq!(locks[0].error_class, ErrorClass::Authentication);
        assert!(locks[0].retry_after > Duration::from_secs(0));
    }
}
