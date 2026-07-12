//! outcome-routing-v1 §5 — config knobs, tuned for LOCAL traffic
//! (~60 calls/day; see spec §0 adjudications). Wiring this onto
//! `CompiledSnapshot`/`sb_core::Config` is commit 4's job; this module only
//! owns the shape + defaults so the YAML in the spec deserializes exactly.

use serde::{Deserialize, Serialize};

/// `routing.scorecard` in config. `enabled: false` ⟹ exactly today's
/// behavior (both `record` and `project` become no-ops).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScorecardConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub window: WindowConfig,
    #[serde(default)]
    pub demotion: DemotionConfig,
    #[serde(default)]
    pub prior: PriorConfig,
    /// Weight of the `outcome_health` factor in the `score` routing strategy
    /// (consumed by commit 5; carried here so the whole knob tree loads in
    /// one place).
    #[serde(default = "default_score_weight")]
    pub score_weight: f64,
    #[serde(default)]
    pub persist: PersistConfig,
}

impl Default for ScorecardConfig {
    fn default() -> Self {
        ScorecardConfig {
            enabled: default_enabled(),
            window: WindowConfig::default(),
            demotion: DemotionConfig::default(),
            prior: PriorConfig::default(),
            score_weight: default_score_weight(),
            persist: PersistConfig::default(),
        }
    }
}

fn default_enabled() -> bool {
    true
}
fn default_score_weight() -> f64 {
    0.15
}

/// Ring size + per-sample TTL (§3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowConfig {
    #[serde(default = "default_max_samples")]
    pub max_samples: usize,
    #[serde(default = "default_ttl_secs")]
    pub ttl_secs: u64,
}

impl Default for WindowConfig {
    fn default() -> Self {
        WindowConfig {
            max_samples: default_max_samples(),
            ttl_secs: default_ttl_secs(),
        }
    }
}

fn default_max_samples() -> usize {
    200
}
fn default_ttl_secs() -> u64 {
    86_400
}

/// Hysteresis gates (§3): `demote_success_rate` / `recover_success_rate` form
/// the 0.60/0.85 band; `fast_demote_streak` is the gate-free fast path for a
/// lane that fails every call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DemotionConfig {
    #[serde(default = "default_min_samples")]
    pub min_samples: u32,
    #[serde(default = "default_demote_success_rate")]
    pub demote_success_rate: f64,
    #[serde(default = "default_recover_success_rate")]
    pub recover_success_rate: f64,
    #[serde(default = "default_trunc_demote_rate")]
    pub trunc_demote_rate: f64,
    #[serde(default = "default_fast_demote_streak")]
    pub fast_demote_streak: u32,
}

impl Default for DemotionConfig {
    fn default() -> Self {
        DemotionConfig {
            min_samples: default_min_samples(),
            demote_success_rate: default_demote_success_rate(),
            recover_success_rate: default_recover_success_rate(),
            trunc_demote_rate: default_trunc_demote_rate(),
            fast_demote_streak: default_fast_demote_streak(),
        }
    }
}

fn default_min_samples() -> u32 {
    8
}
fn default_demote_success_rate() -> f64 {
    0.60
}
fn default_recover_success_rate() -> f64 {
    0.85
}
fn default_trunc_demote_rate() -> f64 {
    0.25
}
fn default_fast_demote_streak() -> u32 {
    3
}

/// Registry-fact prior (§3 shrinkage): `weight` is `w`, `default_success_rate`
/// is `p_prior`. Actual per-target registry seeding happens at wiring time
/// (commit 4); this is the fallback used when no registry fact applies.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PriorConfig {
    #[serde(default = "default_prior_weight")]
    pub weight: f64,
    #[serde(default = "default_prior_success_rate")]
    pub default_success_rate: f64,
}

impl Default for PriorConfig {
    fn default() -> Self {
        PriorConfig {
            weight: default_prior_weight(),
            default_success_rate: default_prior_success_rate(),
        }
    }
}

fn default_prior_weight() -> f64 {
    5.0
}
fn default_prior_success_rate() -> f64 {
    0.95
}

/// Persistence cadence (§4): how often the dirty flusher writes, and how old
/// a hydrated row may be before it's discarded as stale.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistConfig {
    #[serde(default = "default_flush_secs")]
    pub flush_secs: u64,
    #[serde(default = "default_stale_hydrate_secs")]
    pub stale_hydrate_secs: u64,
}

impl Default for PersistConfig {
    fn default() -> Self {
        PersistConfig {
            flush_secs: default_flush_secs(),
            stale_hydrate_secs: default_stale_hydrate_secs(),
        }
    }
}

fn default_flush_secs() -> u64 {
    30
}
fn default_stale_hydrate_secs() -> u64 {
    172_800
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_json_yields_spec_defaults() {
        let cfg: ScorecardConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.window.max_samples, 200);
        assert_eq!(cfg.window.ttl_secs, 86_400);
        assert_eq!(cfg.demotion.min_samples, 8);
        assert_eq!(cfg.demotion.demote_success_rate, 0.60);
        assert_eq!(cfg.demotion.recover_success_rate, 0.85);
        assert_eq!(cfg.demotion.trunc_demote_rate, 0.25);
        assert_eq!(cfg.demotion.fast_demote_streak, 3);
        assert_eq!(cfg.prior.weight, 5.0);
        assert_eq!(cfg.prior.default_success_rate, 0.95);
        assert_eq!(cfg.score_weight, 0.15);
        assert_eq!(cfg.persist.flush_secs, 30);
        assert_eq!(cfg.persist.stale_hydrate_secs, 172_800);
    }

    #[test]
    fn default_impl_matches_empty_json() {
        let via_default = ScorecardConfig::default();
        let via_json: ScorecardConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(
            serde_json::to_value(via_default).unwrap(),
            serde_json::to_value(via_json).unwrap()
        );
    }

    #[test]
    fn overrides_deserialize_and_disabled_is_respected() {
        let json = r#"{
            "enabled": false,
            "window": { "max_samples": 50, "ttl_secs": 3600 },
            "demotion": {
                "min_samples": 4,
                "demote_success_rate": 0.5,
                "recover_success_rate": 0.9,
                "trunc_demote_rate": 0.3,
                "fast_demote_streak": 2
            },
            "prior": { "weight": 10.0, "default_success_rate": 0.8 },
            "score_weight": 0.3,
            "persist": { "flush_secs": 5, "stale_hydrate_secs": 60 }
        }"#;
        let cfg: ScorecardConfig = serde_json::from_str(json).unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.window.max_samples, 50);
        assert_eq!(cfg.window.ttl_secs, 3600);
        assert_eq!(cfg.demotion.min_samples, 4);
        assert_eq!(cfg.demotion.demote_success_rate, 0.5);
        assert_eq!(cfg.demotion.recover_success_rate, 0.9);
        assert_eq!(cfg.demotion.trunc_demote_rate, 0.3);
        assert_eq!(cfg.demotion.fast_demote_streak, 2);
        assert_eq!(cfg.prior.weight, 10.0);
        assert_eq!(cfg.prior.default_success_rate, 0.8);
        assert_eq!(cfg.score_weight, 0.3);
        assert_eq!(cfg.persist.flush_secs, 5);
        assert_eq!(cfg.persist.stale_hydrate_secs, 60);
    }
}
