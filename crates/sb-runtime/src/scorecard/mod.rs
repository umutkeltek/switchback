//! outcome-routing-v1 — the scorecard module (spec:
//! `docs/outcome-routing-v1-spec.md`). Owns the math (§2 taxonomy, §3 state
//! model + hysteresis, §4 persistence glue, §5 config) as a standalone,
//! fully unit-tested unit. **Not wired to `Engine` yet** — commit 4 adds the
//! `finish_attempt` seam, the `Engine::scorecard` field, startup hydrate, the
//! background flusher, and the `target.outcome` stamp. Until then this
//! module has no callers and no behavior change.
//!
//! Concurrency: the map-level lock (`entries`) is only ever held to look up
//! or insert an `Arc<Mutex<Entry>>` for one key — the actual ring/hysteresis
//! math always runs under that entry's own lock, never the map's, so one hot
//! target can never block reads/writes for any other target on the map.
//!
//! Fail-open: every public method treats a disabled config, a missing entry,
//! or a poisoned lock as "no evidence" and returns `None`/no-ops rather than
//! panicking — a scorecard failure must never affect routing or responses.

mod class;
mod config;
mod entry;

pub use class::{classify, AttemptOutcome, OutcomeClass};
pub use config::{DemotionConfig, PersistConfig, PriorConfig, ScorecardConfig, WindowConfig};
pub use entry::{Prior, QualitySample, Sample};

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sb_core::{OutcomeSignal, OutcomeTier, QualityEvalConfig, QualitySignal};
use sb_store::{QualityJudgmentRecord, ScorecardRow};

use entry::{window_stats, Entry};

/// `(target_id, class)` — `class` is the capability-class component of the
/// key (`'any'` in v1, per spec §0); it is unrelated to [`OutcomeClass`],
/// which classifies one *sample's* result, not the key.
type Key = (String, String);

/// Per-target rolling outcome scorecard (outcome-routing-v1 §1/§3).
#[derive(Default)]
pub struct Scorecard {
    entries: Mutex<HashMap<Key, Arc<Mutex<Entry>>>>,
}

impl Scorecard {
    pub fn new() -> Self {
        Scorecard {
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn key(target_id: &str, class: &str) -> Key {
        (target_id.to_string(), class.to_string())
    }

    /// Look up an existing entry only — never creates one. Used by read paths
    /// that must fail-open (`project`) rather than allocate state for a
    /// target nobody has recorded an attempt for yet.
    fn get(&self, target_id: &str, class: &str) -> Option<Arc<Mutex<Entry>>> {
        let map = self.entries.lock().ok()?;
        map.get(&Self::key(target_id, class)).cloned()
    }

    /// Get-or-create, seeding a brand new entry with `seed_prior`. The map
    /// lock is held only long enough to look up/insert the `Arc` — never
    /// across the entry's own lock.
    fn get_or_create(
        &self,
        target_id: &str,
        class: &str,
        seed_prior: Prior,
    ) -> Option<Arc<Mutex<Entry>>> {
        let mut map = self.entries.lock().ok()?;
        let arc = map
            .entry(Self::key(target_id, class))
            .or_insert_with(|| Arc::new(Mutex::new(Entry::new(seed_prior))))
            .clone();
        Some(arc)
    }

    /// Record one attempt's terminal outcome (the `finish_attempt` seam,
    /// wired in commit 4). Creates the entry on first sight, seeded with
    /// `seed_prior` (a registry fact at wiring time; commit 3 just takes the
    /// value). `sample.ts` is the clock the whole call uses — there is no
    /// separate `now` parameter, so tests control time purely by choosing
    /// `Sample::ts`. Runs the §3 hysteresis transition after pushing the
    /// sample. No-ops (fail-open) when disabled or the entry lock is
    /// poisoned.
    pub fn record(
        &self,
        target_id: &str,
        class: &str,
        seed_prior: Prior,
        cfg: &ScorecardConfig,
        sample: Sample,
    ) {
        if !cfg.enabled {
            return;
        }
        let now = sample.ts;
        let Some(arc) = self.get_or_create(target_id, class, seed_prior) else {
            return;
        };
        let Ok(mut entry) = arc.lock() else {
            return;
        };
        let ttl = Duration::from_secs(cfg.window.ttl_secs);
        // TTL-lapse RESET (F3): evaluated on the window as it stood BEFORE
        // this sample lands — if it was already fully quiet, clear stale
        // hysteresis state first so it can't resurface just because a fresh
        // sample happened to arrive.
        let pre_stats = window_stats(&entry.ring, entry.prior, now, ttl);
        if entry::reset_if_ttl_lapsed(&mut entry, &pre_stats) {
            entry.dirty = true;
        }
        entry.push(sample, cfg.window.max_samples);
        match sample.class {
            OutcomeClass::TargetFailure => {
                entry.consecutive_failures += 1;
                entry.consecutive_successes = 0;
            }
            OutcomeClass::Success => {
                entry.consecutive_failures = 0;
                entry.consecutive_successes += 1;
            }
            OutcomeClass::Truncated
            | OutcomeClass::Refusal
            | OutcomeClass::ClientOrAccountFault
            | OutcomeClass::Cancelled => {}
        }
        let stats = window_stats(&entry.ring, entry.prior, now, ttl);
        entry::apply_hysteresis(&mut entry, &stats, &cfg.demotion, now, sample.class);
        entry.dirty = true;
    }

    /// Read-only projection (the router read seam, wired in commit 4). Fail-
    /// open: `None` on disabled config, a missing entry (nobody has recorded
    /// an attempt for this key), a poisoned lock, a non-finite computed
    /// posterior/health (F4), or -- per F1 -- an entry with NO scoreable
    /// evidence in the live window that has also never been hydrated from a
    /// real persisted aggregate. That last case matters: an entry can exist
    /// in the map purely from neutral events (a client abort, a safety
    /// refusal) that are recorded for observability only and must never
    /// influence routing -- such an entry must behave EXACTLY like a target
    /// nobody has recorded anything for (`None`), not project a bare
    /// registry/config prior that would rank it below an untouched peer.
    ///
    /// `project()` never *upgrades* a read to `Demoted`: the returned tier is
    /// exactly the hysteresis-decided `entry.tier`, EXCEPT when the live
    /// TTL-filtered window has gone fully quiet (`n_scoreable == 0`), in
    /// which case a TTL-lapse RESET (F3) clears the STORED tier back to
    /// `Healthy` (not merely the returned value) -- a demotion can never be
    /// permanent once its supporting evidence has entirely aged out (or,
    /// post-hydrate, before any live evidence has arrived at all).
    pub fn project(
        &self,
        target_id: &str,
        class: &str,
        cfg: &ScorecardConfig,
        now: Instant,
    ) -> Option<OutcomeSignal> {
        if !cfg.enabled {
            return None;
        }
        let arc = self.get(target_id, class)?;
        let mut entry = arc.lock().ok()?;
        let ttl = Duration::from_secs(cfg.window.ttl_secs);
        let stats = window_stats(&entry.ring, entry.prior, now, ttl);
        if entry::reset_if_ttl_lapsed(&mut entry, &stats) {
            entry.dirty = true;
        }
        if stats.n_scoreable == 0 && !entry.hydrated {
            return None;
        }
        if !stats.p_hat.is_finite() || !stats.health_factor.is_finite() {
            return None;
        }
        let demote_trigger = if entry.tier == OutcomeTier::Demoted {
            entry.demote_trigger
        } else {
            None
        };
        Some(OutcomeSignal {
            samples: stats.n_scoreable,
            success_rate: stats.p_hat as f32,
            p50_latency_ms: stats.p50_latency_ms,
            p95_latency_ms: stats.p95_latency_ms,
            cost_per_success_micros: stats.cost_per_success_micros,
            truncation_rate: stats.trunc_rate as f32,
            dominant_error: stats.dominant_error,
            tier: entry.tier,
            health_factor: stats.health_factor,
            demote_trigger,
            consecutive_failures: entry.consecutive_failures,
        })
    }

    /// Record one scored response-quality judgment without touching outcome
    /// hysteresis. Invalid scores/evaluator ids and disabled config fail open.
    pub fn record_quality(
        &self,
        target_id: &str,
        class: &str,
        evaluator_id: &str,
        scorecard_cfg: &ScorecardConfig,
        quality_cfg: &QualityEvalConfig,
        sample: QualitySample,
    ) {
        if !scorecard_cfg.enabled
            || !quality_cfg.enabled
            || evaluator_id.trim().is_empty()
            || sample.judgment_id.trim().is_empty()
            || sample.created_at_ms < 0
            || !sample.score_norm.is_finite()
            || !(0.0..=1.0).contains(&sample.score_norm)
        {
            return;
        }
        let Some(arc) = self.get_or_create(target_id, class, Prior::from_config(scorecard_cfg))
        else {
            return;
        };
        let Ok(mut entry) = arc.lock() else {
            return;
        };
        if entry.quality_evaluator_id.as_deref() != Some(evaluator_id) {
            entry.quality_ring.clear();
            entry.quality_evaluator_id = Some(evaluator_id.to_string());
        }
        entry.push_quality(sample, scorecard_cfg.window.max_samples);
        entry.dirty = true;
    }

    /// Project fresh quality evidence. Expired samples are removed so a later
    /// flush clears stale persisted influence instead of letting it linger.
    pub fn project_quality(
        &self,
        target_id: &str,
        class: &str,
        evaluator_id: &str,
        scorecard_cfg: &ScorecardConfig,
        quality_cfg: &QualityEvalConfig,
        now: Instant,
    ) -> Option<QualitySignal> {
        if !scorecard_cfg.enabled || !quality_cfg.enabled {
            return None;
        }
        let arc = self.get(target_id, class)?;
        let mut entry = arc.lock().ok()?;
        if entry.quality_evaluator_id.as_deref() != Some(evaluator_id) {
            return None;
        }
        let ttl = Duration::from_secs(scorecard_cfg.window.ttl_secs);
        if entry::prune_quality(&mut entry.quality_ring, now, ttl) {
            entry.dirty = true;
        }
        let stats = entry::quality_stats(&entry.quality_ring, now, ttl, quality_cfg.ewma_alpha)?;
        Some(QualitySignal {
            ewma: stats.ewma,
            samples: stats.samples,
            age_secs: stats.age_secs,
            evaluator_id: evaluator_id.to_string(),
        })
    }

    /// Project every target carrying fresh quality evidence for one capability
    /// class. Used only by the enabled operator usage projection.
    pub fn project_all_quality(
        &self,
        class: &str,
        evaluator_id: &str,
        scorecard_cfg: &ScorecardConfig,
        quality_cfg: &QualityEvalConfig,
        now: Instant,
    ) -> BTreeMap<String, QualitySignal> {
        let target_ids = {
            let Ok(map) = self.entries.lock() else {
                return BTreeMap::new();
            };
            map.keys()
                .filter(|(_, entry_class)| entry_class == class)
                .map(|(target_id, _)| target_id.clone())
                .collect::<Vec<_>>()
        };
        target_ids
            .into_iter()
            .filter_map(|target_id| {
                self.project_quality(
                    &target_id,
                    class,
                    evaluator_id,
                    scorecard_cfg,
                    quality_cfg,
                    now,
                )
                .map(|signal| (target_id, signal))
            })
            .collect()
    }

    /// Rebuild the current evaluator's quality rings from the audit/WAL. This
    /// is replacement replay, not append replay, so repeated startup recovery
    /// is idempotent and never doubles sample counts.
    pub fn replay_quality(
        &self,
        rows: &[QualityJudgmentRecord],
        evaluator_id: &str,
        scorecard_cfg: &ScorecardConfig,
        quality_cfg: &QualityEvalConfig,
        now: Instant,
        now_epoch_ms: i64,
    ) {
        if !scorecard_cfg.enabled || !quality_cfg.enabled || evaluator_id.trim().is_empty() {
            return;
        }
        let existing = {
            let Ok(map) = self.entries.lock() else {
                return;
            };
            map.values().cloned().collect::<Vec<_>>()
        };
        for arc in existing {
            if let Ok(mut entry) = arc.lock() {
                if !entry.quality_ring.is_empty()
                    || entry.quality_evaluator_id.as_deref() != Some(evaluator_id)
                {
                    entry.quality_ring.clear();
                    entry.quality_evaluator_id = Some(evaluator_id.to_string());
                    entry.dirty = true;
                }
            }
        }
        let ttl_ms = (scorecard_cfg.window.ttl_secs as i64).saturating_mul(1000);
        for row in rows {
            if row.status != "scored"
                || row.evaluator_id != evaluator_id
                || row.created_at_ms < now_epoch_ms.saturating_sub(ttl_ms)
            {
                continue;
            }
            let Some(score_norm) = row.score_norm else {
                continue;
            };
            let age_ms = now_epoch_ms.saturating_sub(row.created_at_ms).max(0) as u64;
            let ts = now
                .checked_sub(Duration::from_millis(age_ms))
                .unwrap_or(now);
            self.record_quality(
                &row.served_target_id,
                &row.class,
                evaluator_id,
                scorecard_cfg,
                quality_cfg,
                QualitySample {
                    judgment_id: row.judgment_id.clone(),
                    ts,
                    created_at_ms: row.created_at_ms,
                    score_norm,
                },
            );
        }
    }

    /// Startup hydrate (§4), called once before serving traffic (commit 4
    /// wires the actual `StateStore::load_scorecard()` call). A fresh,
    /// internally-consistent row becomes a strong prior; a stale/corrupt/
    /// zero-scoreable row is REJECTED (`entry::hydrate_row` returns `None`)
    /// and -- per F1/F13 -- creates NO map entry at all, so a rejected row's
    /// target is indistinguishable from one nobody has ever recorded (fully
    /// registry-prior, `project()` returns `None` until live traffic
    /// arrives), never a spurious default-prior placeholder. `now` /
    /// `now_epoch_ms` must be the same instant from two clocks: rows are
    /// stamped with SQL epoch millis while the ring itself is
    /// `Instant`-based.
    pub fn hydrate(
        &self,
        rows: &[ScorecardRow],
        cfg: &ScorecardConfig,
        now: Instant,
        now_epoch_ms: i64,
    ) {
        for row in rows {
            let Some((prior, tier)) = entry::hydrate_row(row, cfg, now_epoch_ms) else {
                continue;
            };
            let Some(arc) = self.get_or_create(&row.target_id, &row.class, prior) else {
                continue;
            };
            let Ok(mut entry) = arc.lock() else {
                continue;
            };
            entry.prior = prior;
            entry.tier = tier;
            entry.hydrated = true;
            entry.demoted_since = if tier == OutcomeTier::Demoted {
                row.demoted_since_ms.and_then(|ms| {
                    let age_ms = now_epoch_ms.saturating_sub(ms).max(0) as u64;
                    now.checked_sub(Duration::from_millis(age_ms))
                })
            } else {
                None
            };
        }
    }

    /// Flush glue (§4): return the aggregate row for every dirty entry,
    /// TTL-filtered as of `now`, and clear the dirty flag. The background
    /// flusher (wired in commit 4) upserts the result in one transaction.
    /// `tier`/`consecutive_failures` reflect the entry's true internal state
    /// (unlike `project()`, this is persistence, not a routing read, so it is
    /// never auto-lapsed).
    pub fn dirty_snapshot(
        &self,
        cfg: &ScorecardConfig,
        now: Instant,
        now_epoch_ms: i64,
    ) -> Vec<ScorecardRow> {
        self.dirty_snapshot_inner(cfg, &QualityEvalConfig::default(), now, now_epoch_ms)
    }

    pub fn dirty_snapshot_with_quality(
        &self,
        cfg: &ScorecardConfig,
        quality_cfg: &QualityEvalConfig,
        now: Instant,
        now_epoch_ms: i64,
    ) -> Vec<ScorecardRow> {
        self.dirty_snapshot_inner(cfg, quality_cfg, now, now_epoch_ms)
    }

    fn dirty_snapshot_inner(
        &self,
        cfg: &ScorecardConfig,
        quality_cfg: &QualityEvalConfig,
        now: Instant,
        now_epoch_ms: i64,
    ) -> Vec<ScorecardRow> {
        let entries: Vec<(Key, Arc<Mutex<Entry>>)> = {
            let Ok(map) = self.entries.lock() else {
                return Vec::new();
            };
            map.iter()
                .map(|(k, v)| (k.clone(), Arc::clone(v)))
                .collect()
        };
        let ttl = Duration::from_secs(cfg.window.ttl_secs);
        let mut rows = Vec::new();
        for ((target_id, class), arc) in entries {
            let Ok(mut entry) = arc.lock() else {
                continue;
            };
            if !entry.dirty {
                continue;
            }
            let stats = window_stats(&entry.ring, entry.prior, now, ttl);
            entry::prune_quality(&mut entry.quality_ring, now, ttl);
            let quality = if quality_cfg.enabled {
                entry::quality_stats(&entry.quality_ring, now, ttl, quality_cfg.ewma_alpha)
            } else {
                None
            };
            let demoted_since_ms = entry.demoted_since.and_then(|t| {
                now.checked_duration_since(t)
                    .map(|age| now_epoch_ms - age.as_millis() as i64)
            });
            rows.push(ScorecardRow {
                target_id,
                class,
                scoreable_samples: stats.n_scoreable,
                success_count: stats.success_count,
                truncated_count: stats.truncated_count,
                target_fail_count: stats.target_fail_count,
                p50_latency_ms: stats.p50_latency_ms,
                p95_latency_ms: stats.p95_latency_ms,
                cost_per_success_micros: stats.cost_per_success_micros,
                error_histogram: entry::error_histogram_json(&entry.ring, now, ttl),
                consecutive_failures: entry.consecutive_failures,
                tier: match entry.tier {
                    OutcomeTier::Healthy => 0,
                    OutcomeTier::Demoted => 1,
                },
                demoted_since_ms,
                quality_ewma: quality.map(|quality| quality.ewma),
                quality_samples: quality.map(|quality| quality.samples).unwrap_or(0),
                quality_updated_at_ms: quality.map(|quality| quality.updated_at_ms),
                quality_evaluator_id: quality.and_then(|_| entry.quality_evaluator_id.clone()),
                updated_at_ms: now_epoch_ms,
                schema_ver: 1,
            });
            entry.dirty = false;
        }
        rows
    }

    /// Re-mark keys dirty (§4, commit 4's flusher): `dirty_snapshot` clears
    /// the flag as soon as a row is READ, before the caller has actually
    /// persisted it — so when the store write fails, the flusher calls this
    /// to put the affected keys back in the dirty set, guaranteeing "failures
    /// retry next tick" instead of silently dropping the aggregate. No-op for
    /// a key whose entry no longer exists.
    pub fn mark_dirty(&self, keys: impl IntoIterator<Item = (String, String)>) {
        let Ok(map) = self.entries.lock() else {
            return;
        };
        for key in keys {
            if let Some(arc) = map.get(&key) {
                if let Ok(mut entry) = arc.lock() {
                    entry.dirty = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_core::ErrorClass;

    fn seed() -> Prior {
        Prior::new(0.95, 5.0)
    }

    fn quality_cfg() -> QualityEvalConfig {
        QualityEvalConfig {
            enabled: true,
            ..QualityEvalConfig::default()
        }
    }

    fn quality_sample(id: &str, ts: Instant, created_at_ms: i64, score: f64) -> QualitySample {
        QualitySample {
            judgment_id: id.to_string(),
            ts,
            created_at_ms,
            score_norm: score,
        }
    }

    fn quality_row(id: &str, created_at_ms: i64, score: f64) -> QualityJudgmentRecord {
        QualityJudgmentRecord {
            judgment_id: id.to_string(),
            judge_request_id: format!("judge-{id}"),
            served_request_id: format!("served-{id}"),
            served_target_id: "mock/served".to_string(),
            class: "any".to_string(),
            sample_revision: 1,
            judge_revision: 1,
            evaluator_id: "eval-a".to_string(),
            rubric_version: "quality-v1".to_string(),
            judge_target_id: Some("mock/judge".to_string()),
            status: "scored".to_string(),
            score_norm: Some(score),
            reason_code: Some("correct".to_string()),
            input_chars: 10,
            output_chars: 10,
            reserved_cost_micros: 1,
            actual_cost_micros: Some(1),
            created_at_ms,
            completed_at_ms: Some(created_at_ms + 1),
        }
    }

    #[test]
    fn quality_cold_start_alpha_ttl_and_evaluator_reset() {
        let sc = Scorecard::new();
        let mut scorecard_cfg = ScorecardConfig::default();
        scorecard_cfg.window.ttl_secs = 60;
        let quality_cfg = quality_cfg();
        let now = Instant::now();

        sc.record_quality(
            "mock/served",
            "any",
            "eval-a",
            &scorecard_cfg,
            &quality_cfg,
            quality_sample("a", now - Duration::from_secs(20), 1_000, 1.0),
        );
        sc.record_quality(
            "mock/served",
            "any",
            "eval-a",
            &scorecard_cfg,
            &quality_cfg,
            quality_sample("b", now - Duration::from_secs(10), 2_000, 0.0),
        );
        let signal = sc
            .project_quality(
                "mock/served",
                "any",
                "eval-a",
                &scorecard_cfg,
                &quality_cfg,
                now,
            )
            .unwrap();
        assert!((signal.ewma - 0.8).abs() < 1e-9);
        assert_eq!(signal.samples, 2);
        assert_eq!(signal.age_secs, 10);

        sc.record_quality(
            "mock/served",
            "any",
            "eval-b",
            &scorecard_cfg,
            &quality_cfg,
            quality_sample("c", now, 3_000, 0.25),
        );
        assert!(sc
            .project_quality(
                "mock/served",
                "any",
                "eval-a",
                &scorecard_cfg,
                &quality_cfg,
                now,
            )
            .is_none());
        let reset = sc
            .project_quality(
                "mock/served",
                "any",
                "eval-b",
                &scorecard_cfg,
                &quality_cfg,
                now,
            )
            .unwrap();
        assert_eq!(reset.samples, 1);
        assert_eq!(reset.ewma, 0.25);
        assert!(sc
            .project_quality(
                "mock/served",
                "any",
                "eval-b",
                &scorecard_cfg,
                &quality_cfg,
                now + Duration::from_secs(61),
            )
            .is_none());
    }

    #[test]
    fn quality_replay_is_replacement_idempotent_and_flushes_projection() {
        let sc = Scorecard::new();
        let scorecard_cfg = ScorecardConfig::default();
        let quality_cfg = quality_cfg();
        let now = Instant::now();
        let now_epoch_ms = 10_000;
        let rows = vec![
            quality_row("later", 9_000, 0.0),
            quality_row("earlier", 8_000, 1.0),
        ];

        for _ in 0..2 {
            sc.replay_quality(
                &rows,
                "eval-a",
                &scorecard_cfg,
                &quality_cfg,
                now,
                now_epoch_ms,
            );
        }
        let signal = sc
            .project_quality(
                "mock/served",
                "any",
                "eval-a",
                &scorecard_cfg,
                &quality_cfg,
                now,
            )
            .unwrap();
        assert_eq!(signal.samples, 2);
        assert!((signal.ewma - 0.8).abs() < 1e-9);

        let flushed =
            sc.dirty_snapshot_with_quality(&scorecard_cfg, &quality_cfg, now, now_epoch_ms);
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].quality_samples, 2);
        assert_eq!(flushed[0].quality_updated_at_ms, Some(9_000));
        assert_eq!(flushed[0].quality_evaluator_id.as_deref(), Some("eval-a"));
        assert!((flushed[0].quality_ewma.unwrap() - 0.8).abs() < 1e-9);
    }

    #[test]
    fn quality_record_never_changes_outcome_demotion_tier() {
        let sc = Scorecard::new();
        let scorecard_cfg = ScorecardConfig::default();
        let quality_cfg = quality_cfg();
        let now = Instant::now();
        for offset in 0..3 {
            sc.record(
                "mock/served",
                "any",
                seed(),
                &scorecard_cfg,
                Sample::new(
                    now + Duration::from_millis(offset),
                    OutcomeClass::TargetFailure,
                    1,
                    None,
                    Some(ErrorClass::ServerError),
                ),
            );
        }
        assert_eq!(
            sc.project("mock/served", "any", &scorecard_cfg, now)
                .unwrap()
                .tier,
            OutcomeTier::Demoted
        );
        sc.record_quality(
            "mock/served",
            "any",
            "eval-a",
            &scorecard_cfg,
            &quality_cfg,
            quality_sample("perfect", now, 1_000, 1.0),
        );
        assert_eq!(
            sc.project("mock/served", "any", &scorecard_cfg, now)
                .unwrap()
                .tier,
            OutcomeTier::Demoted
        );
    }

    #[test]
    fn disabled_config_project_returns_none() {
        let sc = Scorecard::new();
        let cfg = ScorecardConfig {
            enabled: false,
            ..ScorecardConfig::default()
        };
        let now = Instant::now();
        // Even after a recorded attempt (which itself no-ops while disabled).
        sc.record(
            "t",
            "any",
            seed(),
            &cfg,
            Sample::new(now, OutcomeClass::Success, 10, None, None),
        );
        assert!(sc.project("t", "any", &cfg, now).is_none());
    }

    #[test]
    fn missing_entry_project_returns_none() {
        let sc = Scorecard::new();
        let cfg = ScorecardConfig::default();
        assert!(sc
            .project("never-recorded", "any", &cfg, Instant::now())
            .is_none());
    }

    #[test]
    fn ttl_auto_lapse_returns_none_once_the_window_is_fully_quiet() {
        // F1 + F3: once every scoreable sample has aged out (n_scoreable ==
        // 0) and the entry was never hydrated, project() must behave EXACTLY
        // like a target nobody has recorded anything for -- None, not
        // Some(Healthy). The demotion's supporting evidence has entirely
        // aged out, so this entry now carries no more evidence than an
        // untouched peer.
        let sc = Scorecard::new();
        let cfg = ScorecardConfig::default();
        let t0 = Instant::now();
        // 3 consecutive TargetFailures -> fast-demote streak trips Demoted
        // while still below min_samples(8).
        for _ in 0..3 {
            sc.record(
                "bad",
                "any",
                seed(),
                &cfg,
                Sample::new(
                    t0,
                    OutcomeClass::TargetFailure,
                    10,
                    None,
                    Some(ErrorClass::Timeout),
                ),
            );
        }
        let signal = sc.project("bad", "any", &cfg, t0).expect("entry exists");
        assert_eq!(
            signal.tier,
            OutcomeTier::Demoted,
            "thin-but-nonzero evidence stays demoted"
        );

        // Advance the clock past the TTL: all 3 samples expire, n_scoreable
        // drops to 0 -> no scoreable evidence, never hydrated -> None.
        let past_ttl = t0 + Duration::from_secs(cfg.window.ttl_secs + 1);
        assert!(
            sc.project("bad", "any", &cfg, past_ttl).is_none(),
            "fully-expired, never-hydrated window must report no evidence at all"
        );
    }

    #[test]
    fn ttl_lapse_resets_stored_state_so_recovery_does_not_require_reearning_the_full_streak() {
        // F3: the RESET must be real (mutate the stored entry), not merely
        // masked on read -- otherwise the very next sample's own hysteresis
        // evaluation still sees the stale `Demoted` tier and demands a full
        // recovery streak/rate gate, even though the demotion's evidence has
        // entirely aged out. One fresh Success after a full TTL lapse must
        // be reported Healthy immediately.
        let sc = Scorecard::new();
        let cfg = ScorecardConfig::default();
        let t0 = Instant::now();
        for _ in 0..3 {
            sc.record(
                "bad",
                "any",
                seed(),
                &cfg,
                Sample::new(
                    t0,
                    OutcomeClass::TargetFailure,
                    10,
                    None,
                    Some(ErrorClass::Timeout),
                ),
            );
        }
        assert_eq!(
            sc.project("bad", "any", &cfg, t0).unwrap().tier,
            OutcomeTier::Demoted
        );

        let past_ttl = t0 + Duration::from_secs(cfg.window.ttl_secs + 1);
        sc.record(
            "bad",
            "any",
            seed(),
            &cfg,
            Sample::new(past_ttl, OutcomeClass::Success, 10, None, None),
        );
        let signal = sc
            .project("bad", "any", &cfg, past_ttl)
            .expect("one live scoreable sample exists");
        assert_eq!(
            signal.tier,
            OutcomeTier::Healthy,
            "TTL-lapse reset + one Success reports Healthy, not a stale Demoted"
        );
        assert_eq!(signal.samples, 1);
    }

    #[test]
    fn neutral_only_entry_projects_none_identically_to_an_untouched_peer() {
        // F1: an entry containing ONLY neutral samples (client aborts) has
        // no scoreable evidence and was never hydrated -- it must project
        // None, exactly like a peer nobody has recorded anything for. Today
        // it would instead project the bare registry/config prior (0.95),
        // ranking it below an untouched peer and letting a client abort
        // reorder score routing.
        let sc = Scorecard::new();
        let cfg = ScorecardConfig::default();
        let now = Instant::now();
        for _ in 0..5 {
            sc.record(
                "aborted-a-lot",
                "any",
                seed(),
                &cfg,
                Sample::new(now, OutcomeClass::Cancelled, 10, None, None),
            );
        }
        assert!(
            sc.project("aborted-a-lot", "any", &cfg, now).is_none(),
            "neutral-only entry must project None"
        );
        assert!(
            sc.project("never-touched-peer", "any", &cfg, now).is_none(),
            "peer without any entry also projects None"
        );
    }

    #[test]
    fn hydrate_fresh_row_seeds_a_strong_prior_that_influences_p_hat() {
        let sc = Scorecard::new();
        let cfg = ScorecardConfig::default();
        let now = Instant::now();
        let now_epoch_ms: i64 = 1_700_000_000_000;
        let row = ScorecardRow {
            target_id: "openrouter/llama".into(),
            class: "any".into(),
            scoreable_samples: 40,
            success_count: 36,
            truncated_count: 2,
            target_fail_count: 2,
            p50_latency_ms: 500,
            p95_latency_ms: 900,
            cost_per_success_micros: 10,
            error_histogram: "{}".into(),
            consecutive_failures: 0,
            tier: 0,
            demoted_since_ms: None,
            quality_ewma: None,
            quality_samples: 0,
            quality_updated_at_ms: None,
            quality_evaluator_id: None,
            updated_at_ms: now_epoch_ms - 1000, // 1s old -> well within stale window
            schema_ver: 1,
        };
        sc.hydrate(&[row], &cfg, now, now_epoch_ms);

        // Zero LIVE samples recorded yet -> project() reads purely the
        // hydrated prior (n_scoreable == 0 in window_stats), so p_hat must
        // equal the hydrated success_rate (36/40 = 0.9) exactly, matching
        // the cold-start shrinkage identity (see entry::tests).
        let signal = sc
            .project("openrouter/llama", "any", &cfg, now)
            .expect("hydrated entry exists");
        assert!((signal.success_rate as f64 - 0.9).abs() < 1e-6);
        // Hydrated tier was Healthy here; a Demoted-hydrate case is covered
        // by the "re-earns demotion" case below.
        assert_eq!(signal.tier, OutcomeTier::Healthy);
    }

    #[test]
    fn hydrate_demoted_tier_re_earns_demotion_before_reporting_demoted() {
        let sc = Scorecard::new();
        let cfg = ScorecardConfig::default();
        let now = Instant::now();
        let now_epoch_ms: i64 = 1_700_000_000_000;
        let row = ScorecardRow {
            target_id: "nvidia/minimax".into(),
            class: "any".into(),
            scoreable_samples: 40,
            success_count: 2,
            truncated_count: 0,
            target_fail_count: 38,
            p50_latency_ms: 0,
            p95_latency_ms: 0,
            cost_per_success_micros: 0,
            error_histogram: "{}".into(),
            consecutive_failures: 5,
            tier: 1, // Demoted
            demoted_since_ms: Some(now_epoch_ms - 5_000),
            quality_ewma: None,
            quality_samples: 0,
            quality_updated_at_ms: None,
            quality_evaluator_id: None,
            updated_at_ms: now_epoch_ms - 1000,
            schema_ver: 1,
        };
        sc.hydrate(&[row], &cfg, now, now_epoch_ms);

        // Fresh process, zero live samples yet -> n_scoreable == 0 ->
        // auto-lapse reports Healthy even though the hydrated tier is
        // Demoted (kept internally "for observability").
        let signal = sc
            .project("nvidia/minimax", "any", &cfg, now)
            .expect("hydrated entry exists");
        assert_eq!(
            signal.tier,
            OutcomeTier::Healthy,
            "must re-earn demotion live"
        );

        // Live traffic reproduces the same failure pattern -> re-demotes.
        for _ in 0..3 {
            sc.record(
                "nvidia/minimax",
                "any",
                seed(),
                &cfg,
                Sample::new(
                    now,
                    OutcomeClass::TargetFailure,
                    10,
                    None,
                    Some(ErrorClass::ServerError),
                ),
            );
        }
        let signal = sc.project("nvidia/minimax", "any", &cfg, now).unwrap();
        assert_eq!(
            signal.tier,
            OutcomeTier::Demoted,
            "re-earned from live evidence"
        );
    }

    #[test]
    fn hydrate_stale_row_is_discarded_and_leaves_no_map_entry() {
        // F1/F13: a rejected row (here: stale) must leave NO map entry --
        // not a default-prior placeholder. project() must therefore return
        // None (identical to a target nobody has ever recorded), not
        // Some(registry-default-prior).
        let sc = Scorecard::new();
        let cfg = ScorecardConfig::default();
        let now = Instant::now();
        let now_epoch_ms: i64 = 1_700_000_000_000;
        let stale_age_ms = (cfg.persist.stale_hydrate_secs as i64) * 1000 + 1;
        let row = ScorecardRow {
            target_id: "zai/glm".into(),
            class: "any".into(),
            scoreable_samples: 40,
            success_count: 2, // would read as a very unhealthy prior if honored
            truncated_count: 0,
            target_fail_count: 38,
            p50_latency_ms: 0,
            p95_latency_ms: 0,
            cost_per_success_micros: 0,
            error_histogram: "{}".into(),
            consecutive_failures: 0,
            tier: 1,
            demoted_since_ms: None,
            quality_ewma: None,
            quality_samples: 0,
            quality_updated_at_ms: None,
            quality_evaluator_id: None,
            updated_at_ms: now_epoch_ms - stale_age_ms,
            schema_ver: 1,
        };
        sc.hydrate(&[row], &cfg, now, now_epoch_ms);

        assert!(
            sc.project("zai/glm", "any", &cfg, now).is_none(),
            "a stale/rejected row must create no entry at all"
        );
    }

    #[test]
    fn hydrate_corrupt_row_discards_all_persisted_influence_and_leaves_no_entry() {
        // F1/F4/F13: a corrupt row (here: success_count > scoreable_samples)
        // must be rejected and create NO map entry -- project() reports None,
        // not a default-prior placeholder.
        let sc = Scorecard::new();
        let cfg = ScorecardConfig::default();
        let now = Instant::now();
        let now_epoch_ms: i64 = 1_700_000_000_000;
        let row = ScorecardRow {
            target_id: "fireworks/x".into(),
            class: "any".into(),
            scoreable_samples: 10,
            success_count: 999, // corrupt: exceeds scoreable_samples
            truncated_count: 0,
            target_fail_count: 0,
            p50_latency_ms: 0,
            p95_latency_ms: 0,
            cost_per_success_micros: 0,
            error_histogram: "{}".into(),
            consecutive_failures: 0,
            tier: 1,
            demoted_since_ms: None,
            quality_ewma: None,
            quality_samples: 0,
            quality_updated_at_ms: None,
            quality_evaluator_id: None,
            updated_at_ms: now_epoch_ms - 1000,
            schema_ver: 1,
        };
        sc.hydrate(&[row], &cfg, now, now_epoch_ms);

        assert!(
            sc.project("fireworks/x", "any", &cfg, now).is_none(),
            "a corrupt/rejected row must create no entry at all"
        );
    }

    #[test]
    fn hydrate_zero_scoreable_row_has_no_routing_influence() {
        // F4: a row with zero scoreable samples carries no real evidence --
        // must be rejected (no entry created), not seed a weight-0 prior.
        let sc = Scorecard::new();
        let cfg = ScorecardConfig::default();
        let now = Instant::now();
        let now_epoch_ms: i64 = 1_700_000_000_000;
        let row = ScorecardRow {
            target_id: "quiet/target".into(),
            class: "any".into(),
            scoreable_samples: 0,
            success_count: 0,
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
            quality_samples: 0,
            quality_updated_at_ms: None,
            quality_evaluator_id: None,
            updated_at_ms: now_epoch_ms - 1000,
            schema_ver: 1,
        };
        sc.hydrate(&[row], &cfg, now, now_epoch_ms);

        assert!(
            sc.project("quiet/target", "any", &cfg, now).is_none(),
            "zero-sample row must have no routing influence"
        );
    }

    #[test]
    fn project_surfaces_demote_trigger_and_consecutive_failures() {
        // F12: OutcomeSignal.demote_trigger/consecutive_failures are
        // populated by project() directly from the entry's own hysteresis
        // state, not guessed downstream.
        let sc = Scorecard::new();
        let cfg = ScorecardConfig::default();
        let now = Instant::now();
        for _ in 0..3 {
            sc.record(
                "streaky",
                "any",
                seed(),
                &cfg,
                Sample::new(
                    now,
                    OutcomeClass::TargetFailure,
                    10,
                    None,
                    Some(ErrorClass::Timeout),
                ),
            );
        }
        let signal = sc.project("streaky", "any", &cfg, now).unwrap();
        assert_eq!(signal.tier, OutcomeTier::Demoted);
        assert_eq!(signal.demote_trigger, Some(sb_core::DemoteTrigger::Streak));
        assert_eq!(signal.consecutive_failures, 3);

        // A Healthy signal never carries a demote_trigger.
        let healthy = sc.project("never-touched", "any", &cfg, now);
        assert!(healthy.is_none());
    }

    #[test]
    fn dirty_snapshot_clears_dirty_flag_and_reflects_true_tier() {
        let sc = Scorecard::new();
        let cfg = ScorecardConfig::default();
        let t0 = Instant::now();
        for _ in 0..3 {
            sc.record(
                "bad",
                "any",
                seed(),
                &cfg,
                Sample::new(
                    t0,
                    OutcomeClass::TargetFailure,
                    10,
                    None,
                    Some(ErrorClass::Timeout),
                ),
            );
        }
        let now_epoch_ms = 1_700_000_000_000;
        let rows = sc.dirty_snapshot(&cfg, t0, now_epoch_ms);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].target_id, "bad");
        assert_eq!(
            rows[0].tier, 1,
            "dirty_snapshot reflects true tier, unlike project()'s auto-lapse"
        );
        assert_eq!(rows[0].consecutive_failures, 3);

        // Second call with no new writes: nothing dirty, nothing returned.
        let rows2 = sc.dirty_snapshot(&cfg, t0, now_epoch_ms);
        assert!(
            rows2.is_empty(),
            "dirty flag was cleared by the first flush"
        );
    }

    #[test]
    fn mark_dirty_requeues_a_failed_flush_for_the_next_tick() {
        let sc = Scorecard::new();
        let cfg = ScorecardConfig::default();
        let t0 = Instant::now();
        sc.record(
            "bad",
            "any",
            seed(),
            &cfg,
            Sample::new(t0, OutcomeClass::Success, 10, None, None),
        );
        let now_epoch_ms = 1_700_000_000_000;

        // Simulate the flusher's read: dirty_snapshot already clears the
        // flag, as if the row were about to be upserted.
        let rows = sc.dirty_snapshot(&cfg, t0, now_epoch_ms);
        assert_eq!(rows.len(), 1);
        assert!(
            sc.dirty_snapshot(&cfg, t0, now_epoch_ms).is_empty(),
            "flag cleared by the read"
        );

        // The store write "fails" -> the flusher re-marks the keys dirty so
        // the next tick retries instead of silently dropping the aggregate.
        sc.mark_dirty(rows.into_iter().map(|r| (r.target_id, r.class)));
        let retried = sc.dirty_snapshot(&cfg, t0, now_epoch_ms);
        assert_eq!(
            retried.len(),
            1,
            "a failed flush must be retried on the next tick"
        );
        assert_eq!(retried[0].target_id, "bad");
    }

    #[test]
    fn record_creates_entry_with_seed_prior_on_first_sight() {
        let sc = Scorecard::new();
        let cfg = ScorecardConfig::default();
        let now = Instant::now();
        let custom_seed = Prior::new(0.5, 20.0);
        sc.record(
            "custom",
            "any",
            custom_seed,
            &cfg,
            Sample::new(now, OutcomeClass::Success, 10, None, None),
        );
        let signal = sc.project("custom", "any", &cfg, now).unwrap();
        // p_hat = (20*0.5 + 1) / (20 + 1) = 11/21
        assert!((signal.success_rate as f64 - (11.0 / 21.0)).abs() < 1e-6);
    }
}
