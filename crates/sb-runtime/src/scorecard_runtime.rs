//! outcome-routing-v1 §4 — the background scorecard flusher (build plan
//! commit 4). Periodically writes dirty aggregates to the configured state
//! store; a store failure is logged and retried on the next tick, never
//! affects request handling. Startup hydrate lives in `snapshot.rs`
//! (`with_store_policy`, where the store is first attached).

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::Engine;

impl Engine {
    /// One flush cycle: snapshot dirty scorecard rows (from the pinned
    /// snapshot's `ScorecardConfig`) and upsert them via the configured
    /// store, if any. No-op when no store is attached, the pinned config
    /// disables the scorecard, or there is nothing dirty. A store failure is
    /// logged (metadata only) and the affected keys are re-marked dirty so
    /// the next tick retries — `dirty_snapshot` itself already cleared the
    /// flag on read, so without this the aggregate would otherwise be
    /// silently dropped on a failed write.
    pub fn flush_scorecard_once(&self) {
        let Some(store) = self.store() else {
            return;
        };
        let snap = self.snapshot();
        let cfg = &snap.config.server.scorecard;
        if !cfg.enabled {
            return;
        }
        let now = Instant::now();
        let now_epoch_ms = sb_store::now_millis();
        let rows = self.scorecard().dirty_snapshot_with_quality(
            cfg,
            &snap.config.server.quality_eval,
            now,
            now_epoch_ms,
        );
        if rows.is_empty() {
            return;
        }
        if let Err(e) = store.upsert_scorecard(&rows) {
            tracing::warn!(
                error = %e,
                rows = rows.len(),
                "scorecard flush failed; will retry next tick"
            );
            let keys = rows.into_iter().map(|r| (r.target_id, r.class));
            self.scorecard().mark_dirty(keys);
        }
    }

    /// Spawn the periodic scorecard flusher: every `persist.flush_secs` (read
    /// fresh from the pinned snapshot each tick, so a reload's new cadence
    /// takes effect on the next cycle) flush dirty aggregates. Runs for the
    /// life of the tokio runtime — the same lifecycle as the other
    /// background listeners `sb-server::serve_gateway` spawns (this process
    /// has no graceful-shutdown signal for any of them yet). Takes `Arc<Self>`
    /// because the loop must own a strong reference across `.await` points
    /// that can long outlive the caller's stack frame.
    pub fn spawn_scorecard_flusher(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                let flush_secs = self
                    .snapshot()
                    .config
                    .server
                    .scorecard
                    .persist
                    .flush_secs
                    .max(1);
                tokio::time::sleep(Duration::from_secs(flush_secs)).await;
                self.flush_scorecard_once();
            }
        })
    }
}
