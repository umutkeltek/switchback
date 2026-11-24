//! The engine side of token-pool admission ([`sb_runtime::token_pool`]):
//! what a lane's engine model really runs with, and the engine's own token
//! count.
//!
//! A model's pool comes from the engine's last model listing (its argument
//! list, read by [`crate::engine_truth`]): `--ctx-size` is the pool, and one
//! request may use all of it with `--kv-unified`, else `ctx-size /
//! parallel`. A model the engine has not listed loaded yet falls back to its
//! settings document (the load settings the overlay gives the engine). When
//! neither knows the context size the pool is unknown and requests go
//! unchecked.
//!
//! The listing is read again (bounded, at most once a second per lane, the
//! calls of one lane one at a time) only when admission needs a model it
//! does not list as loaded, and only while the lane's machine is up: never a
//! wake. That read is kept here, never as the engine control's listing, so
//! the engine control's own decisions do not change with request traffic.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use serde_json::Value;

use sb_runtime::token_pool::{PoolSource, PoolTruth, PoolView};
use sb_runtime::BoxFuture;

use crate::activator::{CapacityActivator, LaneState};
use crate::engine_control::{engine_lane, EngineControls, EngineCtx};

/// The shortest gap between two listing reads for one lane.
const REFRESH_GAP: Duration = Duration::from_secs(1);
/// The longest a listing read for admission may take.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(2);
/// The longest an engine token count may take before the estimate is used.
const COUNT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long admission's own listing read stands in for the engine
/// control's (the poller's next pass replaces it).
const OWN_READ_KEEP: Duration = Duration::from_secs(30);

/// Admission's own last listing read of a lane (when, and its rows); one
/// read at a time.
type OwnRead = tokio::sync::Mutex<Option<(Instant, Vec<Value>)>>;

/// The pool of `model` in listing `rows` when it is loaded (or loading)
/// with its arguments.
fn loaded_truth(rows: &[Value], model: &str) -> Option<PoolTruth> {
    let row = rows.iter().find(|row| {
        row["id"].as_str() == Some(model)
            && matches!(row["status"]["value"].as_str(), Some("loaded" | "loading"))
    })?;
    truth_of(&crate::engine_truth::effective(row), PoolSource::Engine)
}

/// The pool of a truth view ([`crate::engine_truth::effective`]): the set
/// context is the pool; one request may use what the view says, or the
/// whole pool when the engine picks its slots itself.
pub(crate) fn truth_of(view: &Value, source: PoolSource) -> Option<PoolTruth> {
    let pool = view["ctx_size"].as_u64().filter(|ctx| *ctx > 0)?;
    let per_request = view["per_request_ctx"]
        .as_u64()
        .filter(|per| *per > 0)
        .unwrap_or(pool)
        .min(pool);
    Some(PoolTruth {
        pool_tokens: pool,
        per_request_tokens: per_request,
        slots: view["slots"].as_u64(),
        kv_unified: view["kv_unified"].as_bool(),
        source,
    })
}

/// A settings document's preset section as engine arguments, for the truth
/// view (`kv-unified = false` is `--no-kv-unified`).
fn preset_args(pairs: &std::collections::BTreeMap<String, String>) -> Vec<String> {
    let mut args = vec!["llama-server".to_string()];
    for (key, value) in pairs {
        match value.to_ascii_lowercase().as_str() {
            "true" | "on" | "1" if key == "kv-unified" => args.push("--kv-unified".into()),
            "false" | "off" | "0" if key == "kv-unified" => args.push("--no-kv-unified".into()),
            _ => {
                args.push(format!("--{key}"));
                args.push(value.clone());
            }
        }
    }
    args
}

/// [`PoolView`] over the engine control's listing and the settings
/// registry. Holds the engine weakly (the engine holds this view).
pub(crate) struct EnginePoolView {
    pub engine: Weak<sb_runtime::Engine>,
    pub capacity: Arc<CapacityActivator>,
    pub engines: Arc<EngineControls>,
    /// Per lane: one listing read at a time, and the last one (when, and
    /// its rows). Kept here, apart from the engine control's listing.
    refreshes: Mutex<HashMap<String, Arc<OwnRead>>>,
}

impl EnginePoolView {
    pub(crate) fn new(
        engine: Weak<sb_runtime::Engine>,
        capacity: Arc<CapacityActivator>,
        engines: Arc<EngineControls>,
    ) -> Self {
        Self {
            engine,
            capacity,
            engines,
            refreshes: Mutex::new(HashMap::new()),
        }
    }

    fn ctx(&self) -> Option<EngineCtx> {
        Some(EngineCtx {
            engine: self.engine.upgrade()?,
            capacity: self.capacity.clone(),
            engines: self.engines.clone(),
        })
    }

    /// The model's pool from the engine control's last listing, else from
    /// admission's own last read (when it is recent), when either lists the
    /// model loaded (or loading) with its arguments.
    fn listed(&self, provider_id: &str, model: &str) -> Option<PoolTruth> {
        let (_, rows) = self.engines.cached_models(provider_id).unwrap_or_default();
        loaded_truth(&rows, model).or_else(|| {
            let lock = self.refresh_lock(provider_id);
            let last = lock.try_lock().ok()?;
            let (at, rows) = last.as_ref()?;
            (at.elapsed() <= OWN_READ_KEEP)
                .then(|| loaded_truth(rows, model))
                .flatten()
        })
    }

    /// The model's pool from its settings document's load settings.
    fn settings_truth(&self, provider_id: &str, model: &str) -> Option<PoolTruth> {
        let engine = self.engine.upgrade()?;
        let snap = engine.snapshot();
        let lane = engine_lane(&snap.config, provider_id)?;
        let entry = engine
            .model_settings()
            .get(&format!("{provider_id}/{model}"))?;
        let pairs = EngineControls::preset_of(lane.engine.kind, &entry.doc);
        let args = preset_args(&pairs);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        truth_of(
            &crate::engine_truth::effective_from_args(&args),
            PoolSource::Settings,
        )
    }

    /// Whether the lane's machine is up (a listing read may be made): the
    /// lifecycle says so, or its last probe saw it answer. Never a call.
    fn lane_up(&self, lane: &str) -> bool {
        self.capacity.executor(lane).is_some_and(|executor| {
            let report = executor.report();
            matches!(
                report.state,
                LaneState::Healthy | LaneState::Draining | LaneState::IdleCountdown
            ) || (report.state == LaneState::Offline && report.observed_healthy == Some(true))
        })
    }

    fn refresh_lock(&self, lane: &str) -> Arc<OwnRead> {
        self.refreshes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(lane.to_string())
            .or_default()
            .clone()
    }
}

impl PoolView for EnginePoolView {
    fn cached(&self, provider_id: &str, model: &str) -> Option<PoolTruth> {
        self.listed(provider_id, model)
            .or_else(|| self.settings_truth(provider_id, model))
    }

    fn fresh<'a>(
        &'a self,
        provider_id: &'a str,
        model: &'a str,
    ) -> BoxFuture<'a, Option<PoolTruth>> {
        Box::pin(async move {
            if let Some(truth) = self.listed(provider_id, model) {
                return Some(truth);
            }
            if self.lane_up(provider_id) {
                if let Some(ctx) = self.ctx() {
                    if let Some(lane) = engine_lane(&ctx.engine.snapshot().config, provider_id) {
                        // Callers of one lane share one read: the later ones
                        // wait for it and then use what it found.
                        let lock = self.refresh_lock(provider_id);
                        let mut last = lock.lock().await;
                        if last
                            .as_ref()
                            .is_none_or(|(at, _)| at.elapsed() >= REFRESH_GAP)
                        {
                            match self
                                .engines
                                .read_listing_for_pool(&ctx, &lane, REFRESH_TIMEOUT)
                                .await
                            {
                                Ok(rows) => *last = Some((Instant::now(), rows)),
                                Err(e) => {
                                    tracing::debug!(lane = %provider_id, error = %e, "token pool: engine listing not read");
                                    // Keep the last rows; try again after the gap.
                                    let rows =
                                        last.take().map(|(_, rows)| rows).unwrap_or_default();
                                    *last = Some((Instant::now(), rows));
                                }
                            }
                        }
                        if let Some(truth) = last
                            .as_ref()
                            .and_then(|(_, rows)| loaded_truth(rows, model))
                        {
                            return Some(truth);
                        }
                    }
                }
            }
            self.cached(provider_id, model)
        })
    }

    fn count_tokens<'a>(
        &'a self,
        provider_id: &'a str,
        model: &'a str,
        text: String,
    ) -> BoxFuture<'a, Option<u64>> {
        Box::pin(async move {
            let ctx = self.ctx()?;
            let lane = engine_lane(&ctx.engine.snapshot().config, provider_id)?;
            let started = Instant::now();
            let counted = self
                .engines
                .count_tokens(&ctx, &lane, model, text, COUNT_TIMEOUT)
                .await;
            tracing::debug!(
                lane = %provider_id,
                model,
                counted = ?counted,
                ms = started.elapsed().as_millis() as u64,
                "token pool: engine token count"
            );
            counted
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_listed_arguments_give_the_pool() {
        let unified = crate::engine_truth::effective_from_args(&[
            "s",
            "--ctx-size",
            "98304",
            "--parallel",
            "3",
            "--kv-unified",
        ]);
        let truth = truth_of(&unified, PoolSource::Engine).unwrap();
        assert_eq!(
            (truth.pool_tokens, truth.per_request_tokens),
            (98_304, 98_304)
        );
        let split = crate::engine_truth::effective_from_args(&["s", "-c", "98304", "-np", "3"]);
        let truth = truth_of(&split, PoolSource::Engine).unwrap();
        assert_eq!(
            (truth.pool_tokens, truth.per_request_tokens),
            (98_304, 32_768)
        );
        // The engine picks its slots: one request may use the whole pool.
        let picked = crate::engine_truth::effective_from_args(&["s", "-c", "65536"]);
        let truth = truth_of(&picked, PoolSource::Engine).unwrap();
        assert_eq!(
            (truth.pool_tokens, truth.per_request_tokens),
            (65_536, 65_536)
        );
        // No context size: unknown.
        assert!(truth_of(&json!({"ctx_size": null}), PoolSource::Engine).is_none());
    }

    #[test]
    fn a_settings_preset_reads_as_arguments() {
        let pairs = std::collections::BTreeMap::from([
            ("ctx-size".to_string(), "131072".to_string()),
            ("parallel".to_string(), "4".to_string()),
            ("kv-unified".to_string(), "false".to_string()),
        ]);
        let args = preset_args(&pairs);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let truth = truth_of(
            &crate::engine_truth::effective_from_args(&args),
            PoolSource::Settings,
        )
        .unwrap();
        assert_eq!(
            (truth.pool_tokens, truth.per_request_tokens),
            (131_072, 32_768)
        );
        assert_eq!(truth.kv_unified, Some(false));
    }
}
