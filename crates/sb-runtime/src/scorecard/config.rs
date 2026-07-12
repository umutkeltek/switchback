//! outcome-routing-v1 §5 — config knobs, tuned for LOCAL traffic
//! (~60 calls/day; see spec §0 adjudications).
//!
//! The actual struct definitions live in `sb-core` (see
//! `sb_core::ScorecardConfig` and friends) so `ScorecardConfig` can be a
//! field of `sb_core::ServerConfig` and ride the compiled snapshot the way
//! `retry`/`circuit_breaker`/`budget`/`hedge` do (commit 4's wiring). This
//! module just re-exports them under the scorecard module's own path so the
//! rest of this crate (and its tests) can keep using the short names.
//! `enabled: false` ⟹ exactly today's behavior (both `record` and `project`
//! become no-ops).

pub use sb_core::{
    ScorecardConfig, ScorecardDemotionConfig as DemotionConfig,
    ScorecardPersistConfig as PersistConfig, ScorecardPriorConfig as PriorConfig,
    ScorecardWindowConfig as WindowConfig,
};

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
        assert_eq!(cfg.demotion.fast_recover_streak, 3);
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
                "fast_demote_streak": 2,
                "fast_recover_streak": 4
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
        assert_eq!(cfg.demotion.fast_recover_streak, 4);
        assert_eq!(cfg.prior.weight, 10.0);
        assert_eq!(cfg.prior.default_success_rate, 0.8);
        assert_eq!(cfg.score_weight, 0.3);
        assert_eq!(cfg.persist.flush_secs, 5);
        assert_eq!(cfg.persist.stale_hydrate_secs, 60);
    }
}
