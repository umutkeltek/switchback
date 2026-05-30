//! Provider-level circuit breaker — the layer above per-(account,model) locks.
//!
//! Per-account availability already sidelines a single bad credential. The
//! breaker handles the *provider-wide* failure: when a provider's accounts keep
//! failing (it's down, not just rate-limited), trip the breaker OPEN so the
//! router stops attempting any of its targets and falls straight over — then
//! HALF-OPEN after a cooldown to probe recovery with a single request.
//!
//! `now: Instant` is explicit so state transitions are deterministically
//! testable without sleeping.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use sb_core::BreakerConfig;

#[derive(Default)]
struct State {
    /// Consecutive failures while closed.
    failures: u32,
    /// `Some(t)` = OPEN until `t`; `None` = closed (or half-open after a probe).
    opened_until: Option<Instant>,
    /// True while probing recovery (one request let through after the cooldown).
    half_open: bool,
}

pub struct CircuitBreaker {
    enabled: bool,
    threshold: u32,
    open: Duration,
    states: Mutex<HashMap<String, State>>,
}

impl CircuitBreaker {
    pub fn new(cfg: &BreakerConfig) -> Self {
        CircuitBreaker {
            enabled: cfg.enabled,
            threshold: cfg.failure_threshold.max(1),
            open: Duration::from_secs(cfg.open_secs),
            states: Mutex::new(HashMap::new()),
        }
    }

    /// May the router attempt this provider now? `false` = breaker OPEN (skip it).
    /// Crossing the cooldown transitions OPEN → HALF-OPEN and lets one probe through.
    pub fn allows(&self, provider: &str, now: Instant) -> bool {
        if !self.enabled {
            return true;
        }
        let mut guard = self.states.lock().expect("breaker mutex");
        let state = guard.entry(provider.to_string()).or_default();
        match state.opened_until {
            Some(until) if now < until => false,
            Some(_) => {
                // Cooldown elapsed → probe once.
                state.opened_until = None;
                state.half_open = true;
                true
            }
            None => true,
        }
    }

    /// Record a provider attempt outcome. A success closes the breaker; a failure
    /// while half-open re-opens immediately; otherwise failures accumulate and
    /// trip OPEN at the threshold.
    pub fn record(&self, provider: &str, ok: bool, now: Instant) {
        if !self.enabled {
            return;
        }
        let mut guard = self.states.lock().expect("breaker mutex");
        let state = guard.entry(provider.to_string()).or_default();
        if ok {
            state.failures = 0;
            state.opened_until = None;
            state.half_open = false;
        } else if state.half_open {
            state.opened_until = Some(now + self.open);
            state.half_open = false;
            state.failures = 0;
        } else {
            state.failures += 1;
            if state.failures >= self.threshold {
                state.opened_until = Some(now + self.open);
                state.failures = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> BreakerConfig {
        BreakerConfig {
            enabled: true,
            failure_threshold: 3,
            open_secs: 30,
        }
    }

    #[test]
    fn disabled_always_allows() {
        let b = CircuitBreaker::new(&BreakerConfig::default());
        let now = Instant::now();
        for _ in 0..10 {
            b.record("p", false, now);
            assert!(b.allows("p", now));
        }
    }

    #[test]
    fn opens_at_threshold_then_half_opens_after_cooldown() {
        let b = CircuitBreaker::new(&cfg());
        let t0 = Instant::now();
        // Two failures: still closed.
        b.record("p", false, t0);
        b.record("p", false, t0);
        assert!(b.allows("p", t0), "below threshold → closed");
        // Third failure trips it OPEN.
        b.record("p", false, t0);
        assert!(!b.allows("p", t0), "threshold reached → open");
        // Still open within the cooldown.
        assert!(!b.allows("p", t0 + Duration::from_secs(10)));
        // After the cooldown → half-open, one probe allowed.
        let t1 = t0 + Duration::from_secs(31);
        assert!(b.allows("p", t1), "cooldown elapsed → half-open probe");
    }

    #[test]
    fn half_open_success_closes_failure_reopens() {
        let b = CircuitBreaker::new(&cfg());
        let t0 = Instant::now();
        for _ in 0..3 {
            b.record("p", false, t0);
        }
        let t1 = t0 + Duration::from_secs(31);
        assert!(b.allows("p", t1)); // half-open probe

        // Probe fails → re-open immediately.
        b.record("p", false, t1);
        assert!(!b.allows("p", t1), "failed probe → reopened");

        // Cooldown again, probe succeeds → closed.
        let t2 = t1 + Duration::from_secs(31);
        assert!(b.allows("p", t2));
        b.record("p", true, t2);
        assert!(b.allows("p", t2), "successful probe → closed");
    }

    #[test]
    fn per_provider_isolation() {
        let b = CircuitBreaker::new(&cfg());
        let now = Instant::now();
        for _ in 0..3 {
            b.record("bad", false, now);
        }
        assert!(!b.allows("bad", now), "bad provider open");
        assert!(b.allows("good", now), "good provider unaffected");
    }
}
