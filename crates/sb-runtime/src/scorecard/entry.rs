//! outcome-routing-v1 §3 — the state model: `Sample`/`Entry`/ring/shrinkage
//! posterior/hysteresis. This is the math core; `Scorecard` (in `mod.rs`)
//! just adds the concurrent map + the public record/project/hydrate API
//! around it.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use sb_core::{ErrorClass, OutcomeTier};
use sb_store::ScorecardRow;

use super::class::OutcomeClass;
use super::config::{DemotionConfig, ScorecardConfig};

/// One recorded attempt's terminal outcome (§3). `ts` is the injectable
/// clock: callers (and tests) control it directly rather than the module
/// calling `Instant::now()` itself.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub ts: Instant,
    pub class: OutcomeClass,
    pub latency_ms: u32,
    pub cost_micros: Option<u64>,
    pub err: Option<ErrorClass>,
}

impl Sample {
    pub fn new(
        ts: Instant,
        class: OutcomeClass,
        latency_ms: u32,
        cost_micros: Option<u64>,
        err: Option<ErrorClass>,
    ) -> Self {
        Sample {
            ts,
            class,
            latency_ms,
            cost_micros,
            err,
        }
    }
}

/// Registry-seeded (or hydrated) shrinkage prior — `(p_prior, w)` in
/// `p̂ = (w·p_prior + successes) / (w + n_scoreable)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Prior {
    pub success_rate: f64,
    pub weight: f64,
}

impl Prior {
    pub fn new(success_rate: f64, weight: f64) -> Self {
        Prior {
            success_rate,
            weight,
        }
    }

    /// Fallback seed when no registry fact / hydrated row applies.
    pub fn from_config(cfg: &ScorecardConfig) -> Self {
        Prior {
            success_rate: cfg.prior.default_success_rate,
            weight: cfg.prior.weight,
        }
    }
}

/// Per-`(target_id, class)` mutable scorecard state. Lives behind its own
/// `Mutex` (see `Scorecard`) — never locked alongside the map-level lock, so
/// one busy target can't block reads/writes for any other target.
pub(crate) struct Entry {
    pub ring: VecDeque<Sample>,
    /// Scoreable failures only: incremented on `TargetFailure`, reset on
    /// `Success`. Truncated/neutral samples leave it untouched — they're
    /// neither the fast-demote trigger nor evidence of recovery.
    pub consecutive_failures: u32,
    /// The mirror of `consecutive_failures` for the fast-recover streak
    /// (spec's amended §3 hysteresis): incremented on `Success`, reset on
    /// `TargetFailure`. Truncated/neutral samples leave it untouched, same
    /// as `consecutive_failures` — a neutral event perturbs neither streak.
    /// Not persisted (no `ScorecardRow` column): a restarted process re-earns
    /// its recovery streak from live traffic, same as the tier itself.
    pub consecutive_successes: u32,
    pub tier: OutcomeTier,
    pub demoted_since: Option<Instant>,
    pub prior: Prior,
    pub dirty: bool,
}

impl Entry {
    pub fn new(prior: Prior) -> Self {
        Entry {
            ring: VecDeque::new(),
            consecutive_failures: 0,
            consecutive_successes: 0,
            tier: OutcomeTier::Healthy,
            demoted_since: None,
            prior,
            dirty: false,
        }
    }

    pub fn push(&mut self, sample: Sample, max_samples: usize) {
        while self.ring.len() >= max_samples.max(1) {
            self.ring.pop_front();
        }
        self.ring.push_back(sample);
    }
}

/// TTL-filtered window statistics — the shared math behind `record()`'s
/// hysteresis decision, `project()`'s `OutcomeSignal`, and
/// `dirty_snapshot()`'s `ScorecardRow`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WindowStats {
    pub n_scoreable: u32,
    pub success_count: u32,
    pub truncated_count: u32,
    pub target_fail_count: u32,
    /// Shrinkage posterior `p̂`.
    pub p_hat: f64,
    pub trunc_rate: f64,
    /// `clamp(p̂ · (1 − 0.5·trunc_rate), 0, 1)`.
    pub health_factor: f32,
    pub p50_latency_ms: u32,
    pub p95_latency_ms: u32,
    pub cost_per_success_micros: u64,
    pub dominant_error: Option<ErrorClass>,
}

fn is_live(ts: Instant, now: Instant, ttl: Duration) -> bool {
    match now.checked_duration_since(ts) {
        Some(age) => age <= ttl,
        // `ts` is "in the future" relative to `now` (clock skew edge case) —
        // treat it as live rather than panicking or silently dropping it.
        None => true,
    }
}

fn bump_error(counts: &mut Vec<(ErrorClass, u32)>, err: ErrorClass) {
    if let Some(slot) = counts.iter_mut().find(|(e, _)| *e == err) {
        slot.1 += 1;
    } else {
        counts.push((err, 1));
    }
}

/// Exact nearest-rank percentile over an already-sorted slice. Empty input
/// returns 0 (callers gate on `len() >= 2` before calling this per §3).
fn percentile(sorted: &[u32], p: f64) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

/// Compute the TTL-filtered window statistics used by every read/write path.
/// `prior` is the entry's current shrinkage prior (registry-seeded or
/// hydrated) — constant across a call, never mutated by reads.
pub(crate) fn window_stats(
    ring: &VecDeque<Sample>,
    prior: Prior,
    now: Instant,
    ttl: Duration,
) -> WindowStats {
    let mut success_count = 0u32;
    let mut truncated_count = 0u32;
    let mut target_fail_count = 0u32;
    let mut error_counts: Vec<(ErrorClass, u32)> = Vec::new();
    let mut returned_latencies: Vec<u32> = Vec::new();
    let mut cost_sum: u64 = 0;

    for sample in ring.iter().filter(|s| is_live(s.ts, now, ttl)) {
        match sample.class {
            OutcomeClass::Success => {
                success_count += 1;
                returned_latencies.push(sample.latency_ms);
                if let Some(cost) = sample.cost_micros {
                    cost_sum += cost;
                }
            }
            OutcomeClass::Truncated => {
                truncated_count += 1;
                returned_latencies.push(sample.latency_ms);
            }
            OutcomeClass::TargetFailure => {
                target_fail_count += 1;
                if let Some(err) = sample.err {
                    bump_error(&mut error_counts, err);
                }
            }
            OutcomeClass::Refusal
            | OutcomeClass::ClientOrAccountFault
            | OutcomeClass::Cancelled => {}
        }
    }

    let n_scoreable = success_count + truncated_count + target_fail_count;
    let p_hat = (prior.weight * prior.success_rate + success_count as f64)
        / (prior.weight + n_scoreable as f64);
    let trunc_rate = if n_scoreable == 0 {
        0.0
    } else {
        truncated_count as f64 / n_scoreable as f64
    };
    let health_factor = (p_hat * (1.0 - 0.5 * trunc_rate)).clamp(0.0, 1.0) as f32;

    returned_latencies.sort_unstable();
    let (p50_latency_ms, p95_latency_ms) = if returned_latencies.len() >= 2 {
        (
            percentile(&returned_latencies, 50.0),
            percentile(&returned_latencies, 95.0),
        )
    } else {
        (0, 0)
    };

    let cost_per_success_micros = if success_count == 0 {
        0
    } else {
        cost_sum / success_count as u64
    };

    // Last-max-wins on ties (deterministic given deterministic ring order);
    // not exercised by any spec test, which only uses unambiguous majorities.
    let dominant_error = error_counts
        .into_iter()
        .max_by_key(|&(_, n)| n)
        .map(|(e, _)| e);

    WindowStats {
        n_scoreable,
        success_count,
        truncated_count,
        target_fail_count,
        p_hat,
        trunc_rate,
        health_factor,
        p50_latency_ms,
        p95_latency_ms,
        cost_per_success_micros,
        dominant_error,
    }
}

/// Opaque JSON error histogram for persistence (§4 `ScorecardRow.error_histogram`),
/// keyed by `ErrorClass::as_str()`. Includes every class with a recorded
/// `ErrorClass` in the live window (neutral faults included) — their only
/// trace, since neutral classes never enter the scoreable denominator.
pub(crate) fn error_histogram_json(ring: &VecDeque<Sample>, now: Instant, ttl: Duration) -> String {
    let mut counts: Vec<(ErrorClass, u32)> = Vec::new();
    for sample in ring.iter().filter(|s| is_live(s.ts, now, ttl)) {
        if let Some(err) = sample.err {
            bump_error(&mut counts, err);
        }
    }
    let map: serde_json::Map<String, serde_json::Value> = counts
        .into_iter()
        .map(|(e, n)| (e.as_str().to_string(), serde_json::Value::from(n)))
        .collect();
    serde_json::Value::Object(map).to_string()
}

/// §3 hysteresis transition, applied on every `record()`. Gate-free fast path
/// (`consecutive_failures >= fast_demote_streak`) catches a dead lane before
/// `min_samples` traffic accumulates; the gated path additionally covers a
/// slow-average target failure rate or a truncation-rate breach. Recovery is
/// symmetric: a gate-free fast path (`consecutive_successes >=
/// fast_recover_streak`) lets a trickle-fed target recover well below
/// `min_samples` (a target demoted on 3 samples must be recoverable on 3
/// samples too — the amended §3 rule), OR the gated path (a qualified window
/// AND a clean current failure streak, so a single stale success mid-failure
/// run cannot flip the tier back).
pub(crate) fn apply_hysteresis(
    entry: &mut Entry,
    stats: &WindowStats,
    cfg: &DemotionConfig,
    now: Instant,
) {
    match entry.tier {
        OutcomeTier::Healthy => {
            let fast_path = entry.consecutive_failures >= cfg.fast_demote_streak;
            let gated_path = stats.n_scoreable >= cfg.min_samples
                && (stats.p_hat <= cfg.demote_success_rate
                    || stats.trunc_rate >= cfg.trunc_demote_rate);
            if fast_path || gated_path {
                entry.tier = OutcomeTier::Demoted;
                entry.demoted_since = Some(now);
            }
        }
        OutcomeTier::Demoted => {
            let fast_recover = entry.consecutive_successes >= cfg.fast_recover_streak;
            let gated_recover = stats.n_scoreable >= cfg.min_samples
                && stats.p_hat >= cfg.recover_success_rate
                && entry.consecutive_failures == 0;
            if fast_recover || gated_recover {
                entry.tier = OutcomeTier::Healthy;
                entry.demoted_since = None;
            }
        }
    }
}

/// §4 hydrate validation: fresh + internally-consistent rows become a strong
/// prior (`weight = min(scoreable_samples, 50)`); a stale row is discarded
/// (registry prior stands); ANY corrupt/invalid row discards all persisted
/// influence for that key (returns `None` — caller leaves the freshly-seeded
/// entry untouched).
pub(crate) fn hydrate_row(
    row: &ScorecardRow,
    cfg: &ScorecardConfig,
    now_epoch_ms: i64,
) -> Option<(Prior, OutcomeTier)> {
    let sum = row
        .success_count
        .checked_add(row.truncated_count)?
        .checked_add(row.target_fail_count)?;
    if sum != row.scoreable_samples {
        return None; // e.g. success_count > scoreable_samples
    }
    let tier = match row.tier {
        0 => OutcomeTier::Healthy,
        1 => OutcomeTier::Demoted,
        _ => return None,
    };
    let age_ms = now_epoch_ms.saturating_sub(row.updated_at_ms);
    let stale_ms = (cfg.persist.stale_hydrate_secs as i64).saturating_mul(1000);
    if age_ms > stale_ms {
        return None; // stale -> registry prior stands
    }
    let weight = row.scoreable_samples.min(50) as f64;
    let success_rate = if row.scoreable_samples == 0 {
        cfg.prior.default_success_rate
    } else {
        row.success_count as f64 / row.scoreable_samples as f64
    };
    Some((Prior::new(success_rate, weight), tier))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn ring_caps_at_max_samples() {
        let mut entry = Entry::new(Prior::new(0.95, 5.0));
        let now = t0();
        for i in 0..10u32 {
            entry.push(Sample::new(now, OutcomeClass::Success, i, None, None), 5);
        }
        assert_eq!(entry.ring.len(), 5);
        // Oldest 5 popped; the surviving latencies are the last 5 pushed.
        let latencies: Vec<u32> = entry.ring.iter().map(|s| s.latency_ms).collect();
        assert_eq!(latencies, vec![5, 6, 7, 8, 9]);
    }

    #[test]
    fn ttl_filter_excludes_expired_samples() {
        let now = t0();
        let ttl = Duration::from_secs(60);
        let mut ring = VecDeque::new();
        // Expired: 120s old.
        ring.push_back(Sample::new(
            now - Duration::from_secs(120),
            OutcomeClass::TargetFailure,
            50,
            None,
            Some(ErrorClass::Timeout),
        ));
        // Live: 10s old.
        ring.push_back(Sample::new(
            now - Duration::from_secs(10),
            OutcomeClass::Success,
            50,
            None,
            None,
        ));
        let stats = window_stats(&ring, Prior::new(0.95, 5.0), now, ttl);
        assert_eq!(stats.n_scoreable, 1, "expired sample must not count");
        assert_eq!(stats.success_count, 1);
        assert_eq!(stats.target_fail_count, 0);
    }

    #[test]
    fn exact_percentiles_over_returned_attempts_only() {
        let now = t0();
        let ttl = Duration::from_secs(3600);
        let mut ring = VecDeque::new();
        // 10 returned attempts (Success/Truncated) with latencies 10..=100,
        // plus TargetFailure/neutral samples that must be excluded from the
        // percentile computation entirely.
        for lat in [10, 20, 30, 40, 50, 60, 70, 80, 90, 100] {
            ring.push_back(Sample::new(now, OutcomeClass::Success, lat, None, None));
        }
        ring.push_back(Sample::new(
            now,
            OutcomeClass::TargetFailure,
            9999,
            None,
            Some(ErrorClass::ServerError),
        ));
        ring.push_back(Sample::new(
            now,
            OutcomeClass::ClientOrAccountFault,
            8888,
            None,
            Some(ErrorClass::InvalidRequest),
        ));
        let stats = window_stats(&ring, Prior::new(0.95, 5.0), now, ttl);
        assert_eq!(stats.p50_latency_ms, 50);
        assert_eq!(stats.p95_latency_ms, 100);
    }

    #[test]
    fn percentiles_require_at_least_two_returned_samples() {
        let now = t0();
        let ttl = Duration::from_secs(3600);
        let mut ring = VecDeque::new();
        ring.push_back(Sample::new(now, OutcomeClass::Success, 42, None, None));
        let stats = window_stats(&ring, Prior::new(0.95, 5.0), now, ttl);
        assert_eq!(stats.p50_latency_ms, 0);
        assert_eq!(stats.p95_latency_ms, 0);
    }

    #[test]
    fn shrinkage_cold_start_is_the_prior() {
        let now = t0();
        let ttl = Duration::from_secs(3600);
        let ring = VecDeque::new();
        let stats = window_stats(&ring, Prior::new(0.95, 5.0), now, ttl);
        assert_eq!(stats.n_scoreable, 0);
        assert!((stats.p_hat - 0.95).abs() < 1e-9, "p_hat={}", stats.p_hat);
    }

    #[test]
    fn shrinkage_washes_out_toward_all_success() {
        let now = t0();
        let ttl = Duration::from_secs(3600);
        let mut ring = VecDeque::new();
        for _ in 0..100 {
            ring.push_back(Sample::new(now, OutcomeClass::Success, 10, None, None));
        }
        let stats = window_stats(&ring, Prior::new(0.95, 5.0), now, ttl);
        assert!((stats.p_hat - 0.997).abs() < 0.001, "p_hat={}", stats.p_hat);
    }

    #[test]
    fn shrinkage_washes_out_toward_all_failure() {
        let now = t0();
        let ttl = Duration::from_secs(3600);
        let mut ring = VecDeque::new();
        for _ in 0..100 {
            ring.push_back(Sample::new(
                now,
                OutcomeClass::TargetFailure,
                10,
                None,
                Some(ErrorClass::ServerError),
            ));
        }
        let stats = window_stats(&ring, Prior::new(0.95, 5.0), now, ttl);
        assert!((stats.p_hat - 0.045).abs() < 0.001, "p_hat={}", stats.p_hat);
        assert!((stats.health_factor as f64 - 0.045).abs() < 0.001);
    }

    #[test]
    fn neutral_faults_are_excluded_from_scoreable_denominator() {
        let now = t0();
        let ttl = Duration::from_secs(3600);
        let mut ring = VecDeque::new();
        for _ in 0..10 {
            ring.push_back(Sample::new(
                now,
                OutcomeClass::ClientOrAccountFault,
                10,
                None,
                Some(ErrorClass::InvalidRequest),
            ));
        }
        for _ in 0..10 {
            ring.push_back(Sample::new(
                now,
                OutcomeClass::ClientOrAccountFault,
                10,
                None,
                Some(ErrorClass::Authentication),
            ));
        }
        let stats = window_stats(&ring, Prior::new(0.95, 5.0), now, ttl);
        assert_eq!(stats.n_scoreable, 0, "neutral classes must not count");

        let mut entry = Entry::new(Prior::new(0.95, 5.0));
        entry.ring = ring;
        let demotion_cfg = DemotionConfig {
            min_samples: 8,
            demote_success_rate: 0.60,
            recover_success_rate: 0.85,
            trunc_demote_rate: 0.25,
            fast_demote_streak: 3,
            fast_recover_streak: 3,
        };
        apply_hysteresis(&mut entry, &stats, &demotion_cfg, now);
        assert_eq!(
            entry.tier,
            OutcomeTier::Healthy,
            "neutral faults never demote"
        );
    }

    #[test]
    fn cost_per_success_sums_known_costs_over_success_count() {
        let now = t0();
        let ttl = Duration::from_secs(3600);
        let mut ring = VecDeque::new();
        ring.push_back(Sample::new(now, OutcomeClass::Success, 10, Some(100), None));
        ring.push_back(Sample::new(now, OutcomeClass::Success, 10, Some(300), None));
        ring.push_back(Sample::new(now, OutcomeClass::Success, 10, None, None));
        let stats = window_stats(&ring, Prior::new(0.95, 5.0), now, ttl);
        assert_eq!(stats.success_count, 3);
        assert_eq!(stats.cost_per_success_micros, 400 / 3);
    }

    #[test]
    fn cost_per_success_is_zero_when_all_unknown() {
        let now = t0();
        let ttl = Duration::from_secs(3600);
        let mut ring = VecDeque::new();
        ring.push_back(Sample::new(now, OutcomeClass::Success, 10, None, None));
        let stats = window_stats(&ring, Prior::new(0.95, 5.0), now, ttl);
        assert_eq!(stats.cost_per_success_micros, 0);
    }

    #[test]
    fn dominant_error_is_most_frequent_among_target_failures() {
        let now = t0();
        let ttl = Duration::from_secs(3600);
        let mut ring = VecDeque::new();
        for _ in 0..2 {
            ring.push_back(Sample::new(
                now,
                OutcomeClass::TargetFailure,
                10,
                None,
                Some(ErrorClass::Timeout),
            ));
        }
        ring.push_back(Sample::new(
            now,
            OutcomeClass::TargetFailure,
            10,
            None,
            Some(ErrorClass::ServerError),
        ));
        // A neutral fault with an even-more-frequent error class must not
        // leak into dominant_error (which is TargetFailure-scoped only).
        for _ in 0..5 {
            ring.push_back(Sample::new(
                now,
                OutcomeClass::ClientOrAccountFault,
                10,
                None,
                Some(ErrorClass::InvalidRequest),
            ));
        }
        let stats = window_stats(&ring, Prior::new(0.95, 5.0), now, ttl);
        assert_eq!(stats.dominant_error, Some(ErrorClass::Timeout));
    }

    fn demotion_cfg() -> DemotionConfig {
        DemotionConfig {
            min_samples: 8,
            demote_success_rate: 0.60,
            recover_success_rate: 0.85,
            trunc_demote_rate: 0.25,
            fast_demote_streak: 3,
            fast_recover_streak: 3,
        }
    }

    fn record_live(entry: &mut Entry, cfg: &DemotionConfig, now: Instant, class: OutcomeClass) {
        entry.push(
            Sample::new(
                now,
                class,
                10,
                None,
                matches!(class, OutcomeClass::TargetFailure).then_some(ErrorClass::ServerError),
            ),
            200,
        );
        match class {
            OutcomeClass::TargetFailure => {
                entry.consecutive_failures += 1;
                entry.consecutive_successes = 0;
            }
            OutcomeClass::Success => {
                entry.consecutive_failures = 0;
                entry.consecutive_successes += 1;
            }
            _ => {}
        }
        let stats = window_stats(&entry.ring, entry.prior, now, Duration::from_secs(86_400));
        apply_hysteresis(entry, &stats, cfg, now);
    }

    #[test]
    fn hysteresis_band_does_not_flap() {
        // Neutralize the fast-recover streak here: this test isolates the
        // *gated* recovery path (a qualified window crossing
        // `recover_success_rate`), which the fast-recover streak (its own
        // dedicated tests below) would otherwise trip early — the 13/10
        // consecutive successes below would hit `fast_recover_streak`'s
        // default of 3 long before p_hat crosses the band.
        let cfg = DemotionConfig {
            fast_recover_streak: u32::MAX,
            ..demotion_cfg()
        };
        let now = t0();
        let mut entry = Entry::new(Prior::new(0.95, 5.0));

        // 8 samples, 3 successes / 5 failures, no run of 3+ consecutive
        // failures (so this exercises the *gated* path, not the streak path):
        // p_hat = (4.75 + 3) / 13 = 0.596... <= 0.60 -> Demoted.
        for class in [
            OutcomeClass::TargetFailure,
            OutcomeClass::TargetFailure,
            OutcomeClass::Success,
            OutcomeClass::TargetFailure,
            OutcomeClass::TargetFailure,
            OutcomeClass::Success,
            OutcomeClass::TargetFailure,
            OutcomeClass::Success,
        ] {
            record_live(&mut entry, &cfg, now, class);
        }
        assert_eq!(
            entry.tier,
            OutcomeTier::Demoted,
            "gated path at p_hat<=0.60"
        );

        // 13 more successes -> p_hat = (7.75+13)/(13+13) = 0.798 -> still
        // below recover_success_rate (0.85) -> must stay Demoted.
        for _ in 0..13 {
            record_live(&mut entry, &cfg, now, OutcomeClass::Success);
        }
        assert_eq!(
            entry.tier,
            OutcomeTier::Demoted,
            "0.80 is inside the band, not recovered yet"
        );

        // 10 more successes -> p_hat = (20.75+10)/(26+10) = 0.854 >= 0.85,
        // consecutive_failures == 0 -> Healthy.
        for _ in 0..10 {
            record_live(&mut entry, &cfg, now, OutcomeClass::Success);
        }
        assert_eq!(entry.tier, OutcomeTier::Healthy, "recovered past 0.85");
    }

    #[test]
    fn fast_demote_streak_trips_below_the_sample_gate() {
        let cfg = demotion_cfg();
        let now = t0();
        let mut entry = Entry::new(Prior::new(0.95, 5.0));
        // Only 3 samples total -- well below min_samples(8) -- but 3
        // consecutive TargetFailures trips the gate-free fast path.
        for _ in 0..3 {
            record_live(&mut entry, &cfg, now, OutcomeClass::TargetFailure);
        }
        assert_eq!(entry.consecutive_failures, 3);
        assert_eq!(entry.tier, OutcomeTier::Demoted);
        assert!(entry.ring.len() < cfg.min_samples as usize);
    }

    #[test]
    fn fast_recover_streak_trickle_recovers_below_the_sample_gate() {
        let cfg = demotion_cfg();
        let now = t0();
        let mut entry = Entry::new(Prior::new(0.95, 5.0));
        // Demote via the fast-demote streak (3 failures, well below
        // min_samples).
        for _ in 0..3 {
            record_live(&mut entry, &cfg, now, OutcomeClass::TargetFailure);
        }
        assert_eq!(entry.tier, OutcomeTier::Demoted);

        // 3 consecutive successes -> fast-recover streak trips Healthy while
        // total n (6) is still well below min_samples(8), so the gated
        // recovery path (which requires n >= min_samples) could not have
        // fired here — only the streak could have.
        for _ in 0..3 {
            record_live(&mut entry, &cfg, now, OutcomeClass::Success);
        }
        assert_eq!(entry.consecutive_successes, 3);
        assert!(entry.ring.len() < cfg.min_samples as usize);
        assert_eq!(
            entry.tier,
            OutcomeTier::Healthy,
            "trickle-fed target recovers via the fast-recover streak"
        );
    }

    #[test]
    fn fast_recover_streak_does_not_flap_on_interleaved_successes() {
        let cfg = demotion_cfg();
        let now = t0();
        let mut entry = Entry::new(Prior::new(0.95, 5.0));
        // Demote via the fast-demote streak (3 failures).
        for _ in 0..3 {
            record_live(&mut entry, &cfg, now, OutcomeClass::TargetFailure);
        }
        assert_eq!(entry.tier, OutcomeTier::Demoted);

        // Successes interleaved with failures: consecutive_successes never
        // reaches fast_recover_streak(3) because every failure resets it, and
        // p_hat never reaches recover_success_rate(0.85) either — neither
        // recovery path should fire, so the target stays Demoted (no flap).
        for class in [
            OutcomeClass::Success,
            OutcomeClass::TargetFailure,
            OutcomeClass::Success,
            OutcomeClass::TargetFailure,
            OutcomeClass::Success,
        ] {
            record_live(&mut entry, &cfg, now, class);
            assert!(
                entry.consecutive_successes < cfg.fast_recover_streak,
                "an interleaved failure must reset the recovery streak"
            );
            assert_eq!(
                entry.tier,
                OutcomeTier::Demoted,
                "must not flap back to Healthy mid-sequence"
            );
        }
        assert_eq!(entry.tier, OutcomeTier::Demoted, "still demoted at the end");
    }

    #[test]
    fn truncation_rate_trigger_is_distinguishable_from_streak() {
        let cfg = demotion_cfg();
        let now = t0();
        let mut entry = Entry::new(Prior::new(0.95, 5.0));
        // 8 samples, interleaved so consecutive_failures never reaches 3,
        // trunc_rate = 3/8 = 0.375 >= 0.25 trigger; p_hat stays high so the
        // rate-based branch (not the success-rate branch) is what fires.
        let classes = [
            OutcomeClass::Truncated,
            OutcomeClass::Success,
            OutcomeClass::Truncated,
            OutcomeClass::Success,
            OutcomeClass::Truncated,
            OutcomeClass::Success,
            OutcomeClass::Success,
            OutcomeClass::Success,
        ];
        for class in classes {
            record_live(&mut entry, &cfg, now, class);
        }
        let stats = window_stats(&entry.ring, entry.prior, now, Duration::from_secs(86_400));
        assert!(stats.trunc_rate >= cfg.trunc_demote_rate);
        assert!(
            stats.p_hat > cfg.demote_success_rate,
            "p_hat={} must stay above the rate gate so truncation is the trigger",
            stats.p_hat
        );
        assert_eq!(entry.tier, OutcomeTier::Demoted);
        assert_eq!(entry.consecutive_failures, 0, "streak path did not fire");
    }

    #[test]
    fn hydrate_row_rejects_inconsistent_counts() {
        let cfg = ScorecardConfig::default();
        let row = ScorecardRow {
            target_id: "a".into(),
            class: "any".into(),
            scoreable_samples: 10,
            success_count: 11, // corrupt: success_count > scoreable_samples
            truncated_count: 0,
            target_fail_count: 0,
            p50_latency_ms: 0,
            p95_latency_ms: 0,
            cost_per_success_micros: 0,
            error_histogram: "{}".into(),
            consecutive_failures: 0,
            tier: 0,
            demoted_since_ms: None,
            quality_ewma: None,
            updated_at_ms: 0,
            schema_ver: 1,
        };
        assert!(hydrate_row(&row, &cfg, 0).is_none());
    }

    #[test]
    fn hydrate_row_rejects_stale_and_accepts_fresh() {
        let cfg = ScorecardConfig::default();
        let mut row = ScorecardRow {
            target_id: "a".into(),
            class: "any".into(),
            scoreable_samples: 20,
            success_count: 18,
            truncated_count: 1,
            target_fail_count: 1,
            p50_latency_ms: 100,
            p95_latency_ms: 200,
            cost_per_success_micros: 5,
            error_histogram: "{}".into(),
            consecutive_failures: 0,
            tier: 1,
            demoted_since_ms: Some(0),
            quality_ewma: None,
            updated_at_ms: 0,
            schema_ver: 1,
        };
        let stale_ms = (cfg.persist.stale_hydrate_secs as i64) * 1000 + 1;
        assert!(
            hydrate_row(&row, &cfg, stale_ms).is_none(),
            "older than stale_hydrate_secs must discard"
        );

        row.updated_at_ms = stale_ms - 1000;
        let (prior, tier) = hydrate_row(&row, &cfg, stale_ms).expect("fresh row hydrates");
        assert!((prior.success_rate - 0.9).abs() < 1e-9);
        assert_eq!(prior.weight, 20.0);
        assert_eq!(tier, OutcomeTier::Demoted);
    }

    #[test]
    fn hydrate_row_caps_weight_at_fifty() {
        let cfg = ScorecardConfig::default();
        let row = ScorecardRow {
            target_id: "a".into(),
            class: "any".into(),
            scoreable_samples: 500,
            success_count: 480,
            truncated_count: 10,
            target_fail_count: 10,
            p50_latency_ms: 0,
            p95_latency_ms: 0,
            cost_per_success_micros: 0,
            error_histogram: "{}".into(),
            consecutive_failures: 0,
            tier: 0,
            demoted_since_ms: None,
            quality_ewma: None,
            updated_at_ms: 0,
            schema_ver: 1,
        };
        let (prior, _) = hydrate_row(&row, &cfg, 0).expect("valid row");
        assert_eq!(prior.weight, 50.0);
    }
}
