//! Per-`provider/model` latency EWMA, for latency-aware routing.
//!
//! The server records each successful attempt's latency; the registry stamps
//! the current EWMA onto an `ExecutionTarget` at routing time so the router can
//! sort fastest-first. An exponentially-weighted moving average tracks recent
//! behavior without storing history — old samples decay, recent ones dominate.

use std::collections::HashMap;
use std::sync::Mutex;

/// Smoothing factor: weight of the newest sample (0..1). 0.3 ≈ "the last few
/// requests matter most" without over-reacting to a single slow outlier.
const ALPHA: f64 = 0.3;

#[derive(Debug, Default)]
pub struct LatencyTracker {
    ewma_ms: Mutex<HashMap<String, f64>>,
}

impl LatencyTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold a new latency sample for `provider/model` into its EWMA.
    pub fn record(&self, provider_id: &str, model: &str, latency_ms: f64) {
        if let Ok(mut map) = self.ewma_ms.lock() {
            let key = format!("{provider_id}/{model}");
            let entry = map.entry(key).or_insert(latency_ms);
            *entry = ALPHA * latency_ms + (1.0 - ALPHA) * *entry;
        }
    }

    /// Current EWMA (ms) for `provider/model`, or `None` if never measured.
    pub fn get(&self, provider_id: &str, model: &str) -> Option<f64> {
        let key = format!("{provider_id}/{model}");
        self.ewma_ms.lock().ok()?.get(&key).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmeasured_is_none_then_tracks_ewma() {
        let t = LatencyTracker::new();
        assert!(t.get("p", "m").is_none());
        t.record("p", "m", 100.0);
        assert_eq!(t.get("p", "m"), Some(100.0), "first sample seeds the ewma");
        t.record("p", "m", 200.0);
        // 0.3*200 + 0.7*100 = 130
        assert!((t.get("p", "m").unwrap() - 130.0).abs() < 1e-9);
    }

    #[test]
    fn keys_are_per_provider_model() {
        let t = LatencyTracker::new();
        t.record("a", "m", 10.0);
        t.record("b", "m", 50.0);
        assert_eq!(t.get("a", "m"), Some(10.0));
        assert_eq!(t.get("b", "m"), Some(50.0));
    }
}
