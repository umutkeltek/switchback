//! The Switchback execution runtime.
//!
//! This crate owns the request execution state machine that used to live inside
//! the Axum handlers: route selection, account resolution, retries, two-level
//! (target × account) fallback, hedging, budget enforcement, trace emission, and
//! attempt lifecycle. `sb-server` is reduced to HTTP ingress/egress + protocol
//! translation; it hands a canonical `AiRequest` to [`Engine::execute`] and
//! renders the [`ExecOutcome`] back in the client's wire format.
//!
//! Configuration is compiled into an immutable, revisioned [`Snapshot`] held
//! behind an `ArcSwap`. Each request pins ONE snapshot for its whole lifetime,
//! so a config publish (hot-swap) never tears a request across revisions:
//! in-flight requests finish on the old revision, new ones start on the new.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use arc_swap::ArcSwap;
use sb_core::Config;

mod audit;
mod collect;
mod denial;
mod embeddings;
mod execute;
mod execution_meta;
mod finish_attempt;
mod hedge;
mod helpers;
mod lanes;
mod outcome;
mod profiles;
pub(crate) mod quality_eval;
pub mod scorecard;
mod scorecard_runtime;
mod snapshot;
mod stream;
mod trace_persist;
mod usage;

#[cfg(test)]
mod tests;

pub use audit::AuditContext;
pub use outcome::{EmbeddingsOutcome, ExecError, ExecOutcome};

pub(crate) use denial::DenialTrace;
#[cfg(test)]
pub(crate) use snapshot::config_hash;

/// Live, runtime-toggleable operational knobs — the subset of `server` config a
/// control-plane client (dashboard / CLI) can flip without a restart. Read per
/// request; updated through `PATCH /v1/runtime`. Structural config (providers,
/// routes, accounts) is NOT live and stays file-driven.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Runtime {
    pub cost_aware: bool,
    pub latency_aware: bool,
    pub hedge_enabled: bool,
    pub retry_max: u32,
    pub budget_max_usd: Option<f64>,
}

impl Runtime {
    pub fn from_config(cfg: &Config) -> Self {
        Runtime {
            cost_aware: cfg.server.cost_aware,
            latency_aware: cfg.server.latency_aware,
            hedge_enabled: cfg.server.hedge.enabled,
            retry_max: cfg.server.retry.max_retries,
            budget_max_usd: cfg.server.budget.max_usd,
        }
    }
}

/// View of managed local lanes (`local_executors`), consulted while resolving
/// and dispatching route candidates. The server's capacity activator
/// implements it from its own health probes and lifecycle; without one,
/// routing is unchanged. Every method except [`LaneStatus::known_down`] has a
/// do-nothing default, and the runtime calls them only for a lane whose entry
/// in the request's pinned config sets `wake_on_request: true`.
pub trait LaneStatus: Send + Sync {
    /// `Some(detail)` only when the provider's managed lane is KNOWN to be
    /// down (a health probe saw it down). Up, not yet probed, or not a managed
    /// lane: `None`. Called on the request path: it must answer from evidence
    /// already held and never wait on I/O (it may start a background refresh).
    fn known_down(&self, provider_id: &str) -> Option<String>;

    /// Start the lane's wake (single-flight: one wake however many requests
    /// ask) and wait up to the lane's own bound for it to be ready. Called
    /// before the first byte, so a lane that stays down still allows
    /// fallback. Must always return within the bound.
    fn wake_for_request<'a>(&'a self, _provider_id: &'a str) -> BoxFuture<'a, LaneWake> {
        Box::pin(std::future::ready(LaneWake::NotReady {
            detail: "wake on request is not available".to_string(),
            retry_after: std::time::Duration::from_secs(1),
        }))
    }

    /// Count one request as in-flight work on the lane until the returned
    /// guard is dropped (the response is complete, the stream ended, the
    /// client went away, or the attempt failed). `None` = the lane does not
    /// track requests. Never waits.
    fn begin_request(&self, _provider_id: &str) -> Option<LaneBusy> {
        None
    }

    /// Like [`Self::begin_request`] for a dispatch that must not wait: a
    /// lane in an exclusive GPU group (`exclusive_group`) that does not hold
    /// its group right now, or a lane at its `max_in_flight`, refuses with
    /// `Err(reason)`, and the target is skipped. Never waits. The runtime calls this, not `begin_request`.
    fn try_begin_request(&self, provider_id: &str) -> Result<Option<LaneBusy>, String> {
        Ok(self.begin_request(provider_id))
    }

    /// For a lane with `max_in_flight`: wait in the lane's FIFO queue,
    /// bounded by its `queue_timeout_secs`, for a place. For a lane in an
    /// exclusive GPU group: then wait, bounded by the lane's
    /// `group_wait_secs`, until the lane holds the group (the previous
    /// holder's `release_command` runs once when it has nothing in flight).
    /// Then hold the lane busy like [`Self::try_begin_request`]. Called
    /// before the first byte for the target a route would choose first.
    fn acquire_for_request<'a>(&'a self, provider_id: &'a str) -> BoxFuture<'a, LaneAcquire> {
        Box::pin(std::future::ready(
            match self.try_begin_request(provider_id) {
                Ok(busy) => LaneAcquire::Ready { busy, detail: None },
                Err(detail) => LaneAcquire::NotReady {
                    detail,
                    retry_after: LANE_GROUP_RETRY_AFTER,
                },
            },
        ))
    }

    /// `Some(detail)` while new work on the provider's lane would have to
    /// wait for a place under its `max_in_flight` (the limit is reached, or
    /// others already wait). `None` with room, without a limit, or for a
    /// provider that is not a managed lane. Never waits; the answer may be
    /// stale by the time the request dispatches (the dispatch then takes
    /// its place, or waits for one, through [`Self::acquire_for_request`]).
    fn lane_full(&self, _provider_id: &str) -> Option<String> {
        None
    }

    /// The engine published a new config (reload or control-plane publish).
    /// Lanes added, changed or removed by it take effect from here.
    fn config_published(&self, _config: &Config) {}
}

/// Holds a managed lane busy while one request is in flight on it; dropping
/// it releases the lane. Type-erased so the runtime never sees lifecycle
/// internals.
pub type LaneBusy = Box<dyn Send + Sync>;

/// `Retry-After` hint for a request refused because a lane's GPU group is
/// busy: about one short media job.
pub const LANE_GROUP_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(10);

/// `Retry-After` hint for a request that waited its lane's whole
/// `queue_timeout_secs` for a place under `max_in_flight`.
pub const LANE_QUEUE_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(5);

/// What waiting for a lane's GPU group ended with.
pub enum LaneAcquire {
    /// The lane holds its group; `busy` holds it (and the lane) until
    /// dropped. `detail` says how, for the `RouteDecision` (for example
    /// `waited 12 s for gpu group `gpu0`, took it from lane `comfy``).
    Ready {
        busy: Option<LaneBusy>,
        detail: Option<String>,
    },
    /// The lane did not get its group; `detail` says why.
    NotReady {
        detail: String,
        retry_after: std::time::Duration,
    },
    /// The lane stayed full (`max_in_flight`) for its whole
    /// `queue_timeout_secs`; `detail` says how long the request waited.
    QueueTimeout {
        detail: String,
        retry_after: std::time::Duration,
    },
}

/// What a request-triggered wake found within its bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneWake {
    /// The lane is ready; `detail` says how, for the `RouteDecision` (for
    /// example `woken for this request, healthy after 41 s`).
    Ready { detail: String },
    /// The lane is not ready; `detail` says why (for example `wake wait of
    /// 90 s passed`). `retry_after` is a hint for a 503 when no other target
    /// can serve.
    NotReady {
        detail: String,
        retry_after: std::time::Duration,
    },
}

/// Boxed future returned by [`LaneStatus::wake_for_request`].
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// An immutable, revisioned compilation of config into everything a request
/// needs: the parsed config, the adapter registry, the credential resolver, and
/// the live knobs. Each request pins ONE snapshot for its whole lifetime, so a
/// config publish (hot-swap) never tears a request across revisions.
pub struct Snapshot {
    pub revision: u64,
    pub config: Arc<Config>,
    pub registry: Arc<sb_adapters::AdapterRegistry>,
    pub resolver: Arc<sb_credentials::CredentialResolver>,
    pub runtime: Runtime,
    /// Built-in plugins, compiled from `config.plugins` at publish time and run
    /// on the hot path (pre_route / post_route / select_egress / post_attempt).
    pub plugins: sb_plugin::PluginHost,
}

/// The execution runtime: holds the current compiled snapshot (swapped
/// atomically on publish/reload) plus the persistent sinks (usage ledger +
/// trace log) that accumulate across reloads — they are NOT config, so they
/// survive a hot-swap. `sb-server` wraps this in its Axum `AppState`.
pub struct Engine {
    /// The current compiled snapshot, swapped atomically on publish/reload.
    snapshot: ArcSwap<Snapshot>,
    /// Persistent across reloads (usage + traces accumulate; not config).
    ledger: Arc<sb_ledger::UsageLedger>,
    traces: Arc<sb_trace::TraceLog>,
    /// Per-target rolling outcome scorecard (outcome-routing-v1 §1/§3).
    /// Persistent across reloads, like `ledger`/`traces` — NOT part of
    /// `Snapshot` — because its ring/hysteresis state must survive a config
    /// hot-swap. Only its *thresholds* (`ScorecardConfig`) ride the pinned
    /// snapshot (`config.server.scorecard`).
    scorecard: Arc<scorecard::Scorecard>,
    /// Bounded live-traffic quality capture queue and serial evaluator state.
    /// Like the scorecard, this survives snapshot swaps; each operation still
    /// re-reads the pinned/fresh config before accepting sampled bodies.
    quality_eval: Arc<quality_eval::QualityEval>,
    /// Config file path, for `reload_from_file` (unset when built from memory).
    config_path: OnceLock<PathBuf>,
    /// Durable control-plane state (config revisions + audit). `None` = in-memory
    /// only (persistence disabled). Optional stores are best-effort; required
    /// stores fail control-plane mutations before a runtime swap.
    store: Option<Arc<dyn sb_store::StateStore>>,
    store_required: bool,
    /// Per-combo target cursor for `strategy: round_robin`. Runtime state, not
    /// config, so it survives hot reload like latency and breaker state.
    combo_rr: Mutex<HashMap<String, usize>>,
    /// Metadata-only exact request fingerprints seen by this process.
    /// This is a hit-rate signal, not response replay.
    exact_cache: Mutex<sb_core::ExactRequestCache>,
    /// Serializes config publishes/reloads so the revision read→build→swap is
    /// atomic. Without it two concurrent publishers both read revision N, both
    /// compute N+1, and the later store silently overwrites the earlier (a lost
    /// update + a duplicate revision number). Held only across a swap.
    reload_lock: Mutex<()>,
    /// Managed local lane state. A target on a lane known to be down is
    /// rejected before dispatch, so a lane stopped on purpose never collects
    /// failures. `None` = no managed lanes (routing unchanged).
    lanes: Option<Arc<dyn LaneStatus>>,
}

/// Outcome of an optimistic-concurrency publish ([`Engine::publish_with_audit`]).
#[derive(Debug)]
pub enum PublishError {
    /// The caller's expected revision no longer matches the current one (someone
    /// else published first) — map to HTTP 409.
    Conflict { expected: u64, current: u64 },
    /// The publish itself failed (invalid config, state-store write error, …).
    Failed(String),
}
