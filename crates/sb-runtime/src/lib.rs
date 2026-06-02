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
mod hedge;
mod helpers;
mod outcome;
mod profiles;
mod snapshot;
mod stream;
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
    /// Serializes config publishes/reloads so the revision read→build→swap is
    /// atomic. Without it two concurrent publishers both read revision N, both
    /// compute N+1, and the later store silently overwrites the earlier (a lost
    /// update + a duplicate revision number). Held only across a swap.
    reload_lock: Mutex<()>,
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
