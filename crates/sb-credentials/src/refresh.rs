//! Refresh-coordination seam.
//!
//! v1 auth methods (api_key, static oauth bearer) need no live refresh, so
//! this is intentionally a thin, correct seam rather than a half-built flow.
//! Its REASON for existing now is the bug it prevents later: when live OAuth
//! refresh lands, concurrent requests on one expiring token must trigger
//! exactly ONE refresh — otherwise providers using rotating one-time refresh
//! tokens (Auth0-style) revoke the whole token family and brick the account
//! (the failure 9router's `dedupRefresh` exists to stop).
//!
//! The coordinator de-duplicates in-flight refreshes per `(provider, account)`.
//! The actual token exchange will be supplied by a provider `RefreshHook`
//! (future); today `should_refresh` is always false for the supported methods.

use std::collections::HashSet;
use std::sync::Mutex;

#[derive(Default)]
pub struct RefreshCoordinator {
    /// `(provider, account)` pairs with a refresh currently in flight.
    in_flight: Mutex<HashSet<(String, String)>>,
}

impl RefreshCoordinator {
    pub fn new() -> Self {
        RefreshCoordinator::default()
    }

    /// Try to claim the right to refresh `(provider, account)`. Returns `true`
    /// for exactly one caller; concurrent callers get `false` and must wait for
    /// the winner's result instead of issuing their own refresh.
    pub fn try_claim(&self, provider: &str, account: &str) -> bool {
        let mut guard = self.in_flight.lock().expect("refresh mutex");
        guard.insert((provider.to_string(), account.to_string()))
    }

    /// Release the claim once the refresh (success or failure) completes.
    pub fn release(&self, provider: &str, account: &str) {
        let mut guard = self.in_flight.lock().expect("refresh mutex");
        guard.remove(&(provider.to_string(), account.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_caller_claims_a_refresh() {
        let rc = RefreshCoordinator::new();
        assert!(rc.try_claim("p", "a"), "first claim wins");
        assert!(!rc.try_claim("p", "a"), "second concurrent claim must wait");
        rc.release("p", "a");
        assert!(rc.try_claim("p", "a"), "after release, claimable again");
    }
}
