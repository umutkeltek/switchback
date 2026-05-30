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
    /// Total request latency EWMA (ms) — the duration signal.
    ewma_ms: Mutex<HashMap<String, f64>>,
    /// Time-to-first-token EWMA (ms), recorded only on streamed responses — the
    /// interactivity signal. Interactive requests rank on this; long-output ones
    /// care more about total/throughput. (Oracle #3: split TTFT from throughput.)
    ttft_ms: Mutex<HashMap<String, f64>>,
}

fn fold(map: &Mutex<HashMap<String, f64>>, provider_id: &str, model: &str, sample: f64) {
    if let Ok(mut map) = map.lock() {
        let key = format!("{provider_id}/{model}");
        let entry = map.entry(key).or_insert(sample);
        *entry = ALPHA * sample + (1.0 - ALPHA) * *entry;
    }
}

fn read(map: &Mutex<HashMap<String, f64>>, provider_id: &str, model: &str) -> Option<f64> {
    let key = format!("{provider_id}/{model}");
    map.lock().ok()?.get(&key).copied()
}

impl LatencyTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold a new total-latency sample for `provider/model` into its EWMA.
    pub fn record(&self, provider_id: &str, model: &str, latency_ms: f64) {
        fold(&self.ewma_ms, provider_id, model, latency_ms);
    }

    /// Current total-latency EWMA (ms) for `provider/model`, or `None`.
    pub fn get(&self, provider_id: &str, model: &str) -> Option<f64> {
        read(&self.ewma_ms, provider_id, model)
    }

    /// Fold a new time-to-first-token sample (streamed responses only).
    pub fn record_ttft(&self, provider_id: &str, model: &str, ttft_ms: f64) {
        fold(&self.ttft_ms, provider_id, model, ttft_ms);
    }

    /// Current TTFT EWMA (ms) for `provider/model`, or `None` if never streamed.
    pub fn get_ttft(&self, provider_id: &str, model: &str) -> Option<f64> {
        read(&self.ttft_ms, provider_id, model)
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

    #[test]
    fn ttft_is_tracked_independently_of_total_latency() {
        let t = LatencyTracker::new();
        assert!(t.get_ttft("p", "m").is_none());
        t.record("p", "m", 800.0); // total latency
        t.record_ttft("p", "m", 120.0); // time to first token
        assert_eq!(t.get("p", "m"), Some(800.0));
        assert_eq!(t.get_ttft("p", "m"), Some(120.0), "TTFT is a separate EWMA");
    }
}
