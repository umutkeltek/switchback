//! On-demand local-capacity activator.
//!
//! A normally-powered-off batch machine runs local executor services (ComfyUI
//! for image jobs, a local vLLM for OpenAI-compatible text). This module owns a
//! per-executor state machine that wakes the machine when a job targets its
//! lane, waits for service health, drains the queued work, and powers the
//! machine off after an idle timeout. If a service crashes or wedges mid-job it
//! self-heals (restart / re-wake, requeue, retry budget) and only fails loud
//! when the budget is exhausted.
//!
//! Every side effect goes through a seam so tests inject fakes: shell commands
//! through [`CommandRunner`], health through [`HealthProbe`], time through
//! [`Clock`]. An unconfigured command leg is *skip-gated* — the state machine
//! still runs, jobs stay queued, and the doctor reports the gap. It is never
//! faked into a success.
//!
//! Tracing is metadata-only (lane name, state, elapsed, command *kind*) — never
//! a prompt, a media body, or a raw command string in a structured field.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use sb_core::{Config, LocalExecutorConfig, ProviderKind};

use crate::alerts::{AlertSink, Alerter, CommandAlertSink};
use crate::holds::{Hold, HoldError, HoldState, HoldView, Holds};
use crate::scheme_auth::SchemeCredential;
use sb_runtime::{LaneAcquire, LaneBusy, LaneWake, LANE_GROUP_RETRY_AFTER};
use serde::Serialize;

// ---------------------------------------------------------------------------
// Seams (injected; real impls below, fakes in tests)
// ---------------------------------------------------------------------------

/// Runs an operator-configured shell command string (wake / poweroff / restart).
#[async_trait]
pub trait CommandRunner: Send + Sync {
    async fn run(&self, command: &str) -> Result<(), String>;

    /// Run a command whose exit code is its answer (the power-off guard):
    /// `Ok(code)` when it ran to an exit, `Err` when it could not run or was
    /// killed. The default knows only success and failure, so a failure is
    /// "could not run" and never an answer: never a reason to power off.
    async fn exit_code(&self, command: &str) -> Result<i32, String> {
        self.run(command).await.map(|()| 0)
    }
}

/// Decides whether a local service is reachable/healthy at an endpoint.
#[async_trait]
pub trait HealthProbe: Send + Sync {
    /// `true` = service is healthy (a 2xx at `endpoint`).
    async fn healthy(&self, endpoint: &str) -> bool;

    /// Probe carrying a credential (the lane provider's, for an engine
    /// behind a token proxy) and tell a refused credential from a down
    /// engine. The default ignores the credential and asks
    /// [`Self::healthy`], so it never answers `Refused`.
    async fn check(&self, endpoint: &str, credential: Option<&SchemeCredential>) -> ProbeAnswer {
        let _ = credential;
        if self.healthy(endpoint).await {
            ProbeAnswer::Up
        } else {
            ProbeAnswer::Down
        }
    }
}

/// What one health probe saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeAnswer {
    /// A 2xx.
    Up,
    /// No answer, or any other status.
    Down,
    /// 401 or 403: the engine answers but refuses the credential (missing
    /// or wrong). Only lanes whose provider carries one on plain calls
    /// (`comfyui`) read it so; for any other lane it counts as `Down`.
    Refused(u16),
}

/// Leases the credential a lane's health probe carries: the one of the
/// provider whose id is the lane's name. Real: the engine's live resolver,
/// so a vault or env key and a reload behave as for the provider's calls.
#[async_trait]
pub trait LaneCredentials: Send + Sync {
    async fn credential(&self, provider_id: &str) -> Option<SchemeCredential>;
}

/// [`LaneCredentials`] over the engine's current snapshot. Holds the engine
/// weakly: the engine holds the activator, not the other way round.
pub struct EngineCredentials(pub Weak<sb_runtime::Engine>);

#[async_trait]
impl LaneCredentials for EngineCredentials {
    async fn credential(&self, provider_id: &str) -> Option<SchemeCredential> {
        let snapshot = self.0.upgrade()?.snapshot();
        let provider = snapshot
            .config
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)?;
        match SchemeCredential::resolve(&snapshot.resolver, provider, provider_id).await {
            Ok(credential) => credential,
            Err(error) => {
                tracing::warn!(lane = %provider_id, %error, "health probe sent without the provider's credential");
                None
            }
        }
    }
}

/// Monotonic-ish wall clock plus sleep. The one seam tests need to make boot
/// timeouts and idle countdowns deterministic.
#[async_trait]
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
    async fn sleep_ms(&self, ms: u64);
}

/// Real shell runner: `sh -c "<command>"`, non-zero exit is a loud error.
pub struct ShellCommandRunner;

impl ShellCommandRunner {
    async fn spawn(command: &str) -> Result<std::process::Output, String> {
        // `kill_on_drop`: a command dropped at its timeout (see
        // `LocalExecutor::run_command`) is killed, not left running.
        tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| format!("spawn failed: {e}"))
    }
}

#[async_trait]
impl CommandRunner for ShellCommandRunner {
    /// Exit codes 126 and 127 are the shell's own "found but not
    /// executable" and "not found": the command never ran, so they count
    /// as "could not run", not as an answer.
    async fn exit_code(&self, command: &str) -> Result<i32, String> {
        match Self::spawn(command).await?.status.code() {
            Some(code @ (126 | 127)) => Err(format!("exit {code}: the shell could not run it")),
            Some(code) => Ok(code),
            None => Err("killed by a signal".to_string()),
        }
    }

    async fn run(&self, command: &str) -> Result<(), String> {
        let output = Self::spawn(command).await?;
        if output.status.success() {
            Ok(())
        } else {
            // Keep stderr out of structured fields; a short tail is enough to
            // debug an operator command without leaking a media/prompt body.
            let code = output
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string());
            Err(format!("exit {code}"))
        }
    }
}

/// Real health probe: a GET that treats any 2xx as healthy. A short timeout
/// keeps an offline host from blocking the poll loop.
pub struct HttpHealthProbe {
    client: reqwest::Client,
    /// For a probe that carries a credential: follows no redirect, so the
    /// credential never reaches another origin (see
    /// [`crate::scheme_auth::redirect_policy`]).
    credentialed: reqwest::Client,
}

impl HttpHealthProbe {
    pub fn new(timeout: Duration) -> Self {
        let client = |redirect| {
            reqwest::Client::builder()
                .timeout(timeout)
                .redirect(redirect)
                .build()
                .unwrap_or_default()
        };
        Self {
            client: client(reqwest::redirect::Policy::default()),
            credentialed: client(reqwest::redirect::Policy::none()),
        }
    }
}

#[async_trait]
impl HealthProbe for HttpHealthProbe {
    async fn healthy(&self, endpoint: &str) -> bool {
        matches!(self.client.get(endpoint).send().await, Ok(response) if response.status().is_success())
    }

    async fn check(&self, endpoint: &str, credential: Option<&SchemeCredential>) -> ProbeAnswer {
        let client = match credential {
            Some(_) => &self.credentialed,
            None => &self.client,
        };
        let request = crate::scheme_auth::with_credential(client.get(endpoint), credential);
        match request.send().await {
            Ok(response) if response.status().is_success() => ProbeAnswer::Up,
            Ok(response) if crate::scheme_auth::refused(response.status()) => {
                ProbeAnswer::Refused(response.status().as_u16())
            }
            _ => ProbeAnswer::Down,
        }
    }
}

/// Real clock over `SystemTime` + `tokio::time::sleep`.
pub struct SystemClock;

#[async_trait]
impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
    async fn sleep_ms(&self, ms: u64) {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
}

// ---------------------------------------------------------------------------
// Observable state (doctor / pulse surface)
// ---------------------------------------------------------------------------

/// Lifecycle position of one lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneState {
    /// Machine off / service unreachable, no wake in flight.
    Offline,
    /// Wake issued; polling for health.
    Waking,
    /// Healthy and idle (no jobs in flight, no idle countdown yet).
    Healthy,
    /// Healthy with at least one job in flight.
    Draining,
    /// Healthy, no jobs; idle timer running toward poweroff.
    IdleCountdown,
    /// Poweroff command executing.
    PoweringOff,
    /// Escalated: could not heal a wedged service. Needs operator attention.
    Degraded,
}

/// Aggregate signal level, mirroring the fal balance probe's ok/warn/fail/skip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneLevel {
    Ok,
    Warn,
    Fail,
    Skip,
}

impl LaneLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
            Self::Skip => "skip",
        }
    }
}

/// A metadata-only snapshot of one lane for `sb doctor` / the live capacity
/// surface. No prompt/media bodies, no raw command strings.
#[derive(Debug, Clone, Serialize)]
pub struct LocalLaneReport {
    pub name: String,
    /// The machine the lane shares with other lanes (`host` in config);
    /// absent for a lane with a machine of its own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// The GPU group the lane shares (`exclusive_group` in config).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu_group: Option<String>,
    /// Whether the lane holds its GPU group now; absent outside a group.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub holds_gpu: Option<bool>,
    pub state: LaneState,
    pub level: LaneLevel,
    /// Callers currently blocked waiting for capacity (queue depth).
    pub queue_depth: u32,
    pub in_flight: u32,
    pub retries_used: u32,
    /// Wake commands issued in the current/most-recent waking cycle. Wake
    /// transports are lossy, so a healthy boot may take several sends.
    pub wake_attempts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_wake_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_wake_result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub wake_configured: bool,
    pub poweroff_configured: bool,
    pub restart_configured: bool,
    pub escalated: bool,
    /// Power-off attempts that failed in the machine's current idle period.
    /// Non-zero while the power-off is retried (`level` warn) and after the
    /// retries ran out (`level` fail, `escalated`).
    #[serde(skip_serializing_if = "is_zero")]
    pub poweroff_failures: u32,
    /// Busy answers in a row from the machine's power-off guard
    /// (`poweroff_guard_command`); while non-zero the idle power-off is
    /// postponed. Absent at zero.
    #[serde(skip_serializing_if = "is_zero")]
    pub poweroff_guard_busy: u32,
    /// When the guard's current run of busy answers began (clock ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poweroff_guard_busy_since_ms: Option<u64>,
    /// Guard runs in a row that gave no answer (timed out or could not
    /// run). `level` is `warn` from `retry_budget` of them on, until the
    /// guard answers again. Absent at zero.
    #[serde(skip_serializing_if = "is_zero")]
    pub poweroff_guard_failures: u32,
    /// The guard gave no answer `retry_budget` times in a row (for alerts;
    /// the detail says so).
    #[serde(skip)]
    pub poweroff_guard_broken: bool,
    pub detail: String,
    /// Most recent health probe result; text routing skips the lane's
    /// targets while this is `false`. Absent until the first probe, and
    /// always absent for a lane without `route_health: true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_healthy: Option<bool>,
    /// Seconds until the idle power-off of the lane's machine; `null` while
    /// the machine has work, is held, is off, or has no power-off command.
    /// Zero once it is due (a busy guard may still postpone it).
    pub poweroff_in_secs: Option<u64>,
    /// The active holds on the lane's machine (`/v1/hosts/{host}/holds`,
    /// where `host` is the lane's `host`, or its name when it has a machine
    /// of its own).
    pub holds: Vec<HoldView>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why capacity could not be secured for a job.
#[derive(Debug, Clone)]
pub enum EnsureError {
    /// Offline and no `wake_command` configured — skip-gated; the job stays
    /// queued, never a fake success.
    WakeUnconfigured,
    /// Wake ran but health never came up within `boot_timeout`.
    BootTimeout { wake_path: String, elapsed_ms: u64 },
    /// The wake command itself failed.
    WakeFailed(String),
    /// The engine answers but its health endpoint refused the credential
    /// (a lane whose provider carries one): failed at once, never waited
    /// out as a boot timeout.
    Refused(String),
}

/// Terminal outcome of running a job under managed capacity.
#[derive(Debug)]
pub enum JobError<E> {
    /// Wake is unconfigured: the job is queued, awaiting operator wiring.
    Queued,
    BootTimeout {
        wake_path: String,
        elapsed_ms: u64,
    },
    WakeFailed(String),
    /// Self-heal budget exhausted after repeated health loss.
    RetriesExhausted {
        budget: u32,
        last_error: String,
    },
    /// The dispatch itself failed while the service was healthy (a real job
    /// error, not a capacity problem) — propagated loud, unretried.
    Dispatch(E),
    /// The lane's GPU group stayed with another lane past `group_wait_secs`,
    /// or the holder's release command failed.
    GpuUnavailable(String),
    /// The engine's health endpoint refused the credential.
    Refused(String),
}

// ---------------------------------------------------------------------------
// Runtime state
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct LaneRuntime {
    state: LaneState,
    in_flight: u32,
    waiting: u32,
    idle_since_ms: Option<u64>,
    last_wake_ms: Option<u64>,
    last_wake_result: Option<String>,
    last_error: Option<String>,
    retries_used: u32,
    wake_attempts: u32,
    escalated: bool,
    poweroff_unconfigured_reported: bool,
    /// Result of the most recent health probe (`None` = never probed, or the
    /// lane is not routed). Read by text routing; never drives the lifecycle
    /// state.
    observed_healthy: Option<bool>,
    /// Probes overlap (supervisor, routing refresh, a job's wake poll). Each
    /// takes a ticket when it starts; only a result whose ticket is newer than
    /// `observed_seq` is recorded, so a slow probe that began while the lane
    /// was down cannot overwrite a newer "up".
    probe_seq: u64,
    observed_seq: u64,
    /// When the most recent probe started (clock ms).
    last_probe_started_ms: Option<u64>,
    /// A routing-triggered refresh probe is running (single-flight).
    refresh_in_flight: bool,
    /// A request-triggered wake task is running (single-flight: every
    /// waiting request joins it).
    request_wake_running: bool,
    /// How the most recent request-triggered wake ended (`None` while one
    /// runs). `Err` carries a metadata-only reason.
    last_request_wake: Option<Result<(), String>>,
    /// Waking cycles that ran the wake command, so a waiter can tell "woken"
    /// from "answered a fresh probe".
    wake_cycles: u64,
    /// When the running waking cycle started (clock ms); `None` when no cycle
    /// runs. Feeds the `Retry-After` hint.
    wake_cycle_started_ms: Option<u64>,
    /// When the most recent request-triggered wake ended (clock ms).
    last_request_wake_ended_ms: Option<u64>,
    /// This lane's machine was powered off by us and no probe has failed
    /// since: a host may still answer while it shuts down, so a healthy
    /// answer is not trusted until a probe has failed once or a wake ran.
    down_unconfirmed: bool,
    /// When our last power-off command returned (clock ms). A host still
    /// answering `boot_timeout_secs` later did not power off, and is
    /// trusted again (see [`LocalExecutor::probe_health`]).
    poweroff_returned_ms: Option<u64>,
    /// Failed `release_command` runs since the last one that worked.
    release_failures: u32,
    /// The latest probe was refused (401/403) on a lane that reads that.
    refused: Option<u16>,
    /// After a failed `release_command`, no new run before this moment
    /// (clock ms), so a burst of waiting work cannot spend the lane's
    /// whole retry budget within seconds.
    release_retry_at_ms: Option<u64>,
    /// The latest trusted probe saw the engine up (any lane, routed or
    /// not). A lane offline to the lifecycle whose engine answers (its
    /// machine was woken outside Switchback) reads `healthy` in the report.
    answered_up: bool,
}

impl LaneRuntime {
    fn new() -> Self {
        Self {
            state: LaneState::Offline,
            in_flight: 0,
            waiting: 0,
            idle_since_ms: None,
            last_wake_ms: None,
            last_wake_result: None,
            last_error: None,
            retries_used: 0,
            wake_attempts: 0,
            escalated: false,
            poweroff_unconfigured_reported: false,
            observed_healthy: None,
            probe_seq: 0,
            observed_seq: 0,
            last_probe_started_ms: None,
            refresh_in_flight: false,
            request_wake_running: false,
            last_request_wake: None,
            wake_cycles: 0,
            wake_cycle_started_ms: None,
            last_request_wake_ended_ms: None,
            down_unconfirmed: false,
            poweroff_returned_ms: None,
            release_failures: 0,
            refused: None,
            release_retry_at_ms: None,
            answered_up: false,
        }
    }
}

/// Minimum gap between the start of the last probe and a routing-triggered
/// refresh of the same lane.
const REFRESH_MIN_INTERVAL_MS: u64 = 1_000;

/// One on-demand local executor lane.
pub struct LocalExecutor {
    /// Lane id (the provider id). Fixed for the executor's life; a reload
    /// that renames a lane removes the old one and adds a new one.
    name: String,
    /// The lane's settings. Replaced in place by a config reload, so live
    /// state (in-flight count, queued waiters, the wake lock) survives it.
    cfg: RwLock<Arc<LocalExecutorConfig>>,
    clock: Arc<dyn Clock>,
    runner: Arc<dyn CommandRunner>,
    probe: Arc<dyn HealthProbe>,
    runtime: Mutex<LaneRuntime>,
    /// The machine the lane runs on. Holds the wake lock (single-flight
    /// wake), the power commands and the power-off decision. A lane without
    /// `host` has a machine of its own, so it behaves as a lane always did.
    host: RwLock<Arc<Host>>,
    /// The lane opted in (`route_health: true`) and its provider can be a
    /// routed (text) target, so the supervisor probes it for routing and
    /// routing reads what it saw. Any other lane (the default, and always a
    /// media-only lane such as ComfyUI or fal) keeps its old behaviour: no
    /// periodic probe and no routing view. Recomputed on reload.
    routed: AtomicBool,
    /// Bumped whenever a request-triggered wake ends, so waiting requests
    /// wake up without polling.
    changes: tokio::sync::watch::Sender<u64>,
    /// How the health probe treats the provider's credential (set from
    /// config on every reconcile).
    probe_auth: RwLock<ProbeAuth>,
    /// Where the probe leases that credential (the engine's resolver once
    /// the server attached it).
    credentials: RwLock<Option<Arc<dyn LaneCredentials>>>,
}

/// How a lane's health probe treats its provider's credential.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ProbeAuth {
    /// The provider carries a credential on plain calls (`comfyui`): a 401
    /// or 403 at the health endpoint is a refused credential, not a down
    /// engine.
    reads_refusal: bool,
    /// ... and the health endpoint is on the provider's `base_url` origin,
    /// so the probe may carry the credential. Never sent anywhere else.
    sends_credential: bool,
}

/// The probe rules for `lane` under `cfg` (see [`ProbeAuth`]).
fn probe_auth_for(cfg: &Config, lane: &LocalExecutorConfig) -> ProbeAuth {
    let Some(provider) = cfg.providers.iter().find(|p| p.id == lane.name) else {
        return ProbeAuth::default();
    };
    let ProviderKind::ComfyUi { base_url, .. } = &provider.kind else {
        return ProbeAuth::default();
    };
    if crate::scheme_auth::plain_call_scheme(provider).is_none() {
        return ProbeAuth::default();
    }
    let origin = |url: &str| {
        reqwest::Url::parse(url).ok().map(|url| {
            (
                url.scheme().to_string(),
                url.host_str().map(str::to_ascii_lowercase),
                url.port_or_known_default(),
            )
        })
    };
    let same_origin =
        origin(base_url).is_some() && origin(base_url) == origin(&lane.health_endpoint);
    ProbeAuth {
        reads_refusal: true,
        sends_credential: same_origin,
    }
}

impl LocalExecutor {
    fn new(
        cfg: LocalExecutorConfig,
        clock: Arc<dyn Clock>,
        runner: Arc<dyn CommandRunner>,
        probe: Arc<dyn HealthProbe>,
        routed: bool,
    ) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            host: RwLock::new(Host::new(&cfg.name, false, vec![me.clone()])),
            name: cfg.name.clone(),
            cfg: RwLock::new(Arc::new(cfg)),
            clock,
            runner,
            probe,
            runtime: Mutex::new(LaneRuntime::new()),
            routed: AtomicBool::new(routed),
            changes: tokio::sync::watch::channel(0).0,
            probe_auth: RwLock::new(ProbeAuth::default()),
            credentials: RwLock::new(None),
        })
    }

    /// The machine this lane runs on right now (a reload may move it).
    fn host(&self) -> Arc<Host> {
        self.host
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn set_host(&self, host: Arc<Host>) {
        *self
            .host
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = host;
    }

    /// Run one operator command of this lane, bounded by
    /// `command_timeout_secs` (the wake command by `boot_timeout_secs` when
    /// that is longer) on the clock seam. A command still running
    /// then is dropped (the shell runner kills it) and reported as failed,
    /// so no lock is held waiting on it.
    async fn run_command(&self, kind: &'static str, command: &str) -> Result<(), String> {
        self.bounded(kind, self.runner.run(command)).await
    }

    /// Ask this lane's power-off guard, bounded like any other command
    /// (`command_timeout_secs`). `Ok(code)` is its answer; a guard that
    /// timed out or could not run is `Err`, never an answer.
    async fn ask_guard(&self, command: &str) -> Result<i32, String> {
        self.bounded("poweroff_guard", self.runner.exit_code(command))
            .await
    }

    async fn bounded<T>(
        &self,
        kind: &'static str,
        command: impl std::future::Future<Output = Result<T, String>>,
    ) -> Result<T, String> {
        let cfg = self.cfg();
        // A wake command may block until the machine answers; it gets the
        // boot timeout when that is longer, so a wake that worked before
        // commands were bounded still works.
        let timeout_secs = if kind == "wake" {
            cfg.command_timeout_secs.max(cfg.boot_timeout_secs)
        } else {
            cfg.command_timeout_secs
        };
        let timeout_ms = timeout_secs.saturating_mul(1000);
        tokio::select! {
            biased;
            result = command => result,
            () = self.clock.sleep_ms(timeout_ms) => {
                tracing::error!(
                    lane = %self.name,
                    command = kind,
                    timeout_ms,
                    "operator command timed out; treated as failed"
                );
                Err(format!("timed out after {}", fmt_ms(timeout_ms)))
            }
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The lane's current settings (cheap `Arc` clone; never held across a
    /// reload's write).
    fn cfg(&self) -> Arc<LocalExecutorConfig> {
        self.cfg
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn is_routed(&self) -> bool {
        self.routed.load(Ordering::SeqCst)
    }

    /// Apply a reloaded entry for this lane in place. Live state is kept:
    /// in-flight requests and jobs, queued waiters, a wake in progress (its
    /// loop reads the new timings on its next pass). Turning routing off
    /// clears the routing observation, so the report stays honest.
    fn update_config(&self, cfg: LocalExecutorConfig, routed: bool) {
        *self
            .cfg
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::new(cfg);
        self.routed.store(routed, Ordering::SeqCst);
        if !routed {
            self.lock().observed_healthy = None;
        }
    }

    fn set_probe_auth(&self, auth: ProbeAuth, credentials: Option<Arc<dyn LaneCredentials>>) {
        *self
            .probe_auth
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = auth;
        if credentials.is_some() {
            *self
                .credentials
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = credentials;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LaneRuntime> {
        self.runtime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn boot_timeout_ms(&self) -> u64 {
        self.cfg().boot_timeout_secs.saturating_mul(1000)
    }

    fn idle_timeout_ms(&self) -> u64 {
        self.cfg().idle_timeout_secs.saturating_mul(1000)
    }

    /// Probe the service's health endpoint and, on a routed lane, remember
    /// what was seen. Every probe in this module goes through here, so text
    /// routing (which reads the observation through
    /// [`sb_runtime::LaneStatus`]) always sees the latest evidence. On any
    /// other lane nothing is recorded, so routing never reads it.
    async fn probe_health(&self) -> bool {
        self.probe_answer().await == ProbeAnswer::Up
    }

    /// [`Self::probe_health`] that also tells a refused credential from a
    /// down engine, on a lane whose provider carries one (`comfyui`). The
    /// probe carries the provider's credential when it has one and the
    /// health endpoint is on the provider's own origin.
    async fn probe_answer(&self) -> ProbeAnswer {
        let ticket = {
            let mut rt = self.lock();
            rt.probe_seq += 1;
            rt.last_probe_started_ms = Some(self.clock.now_ms());
            rt.probe_seq
        };
        let auth = self.probe_auth();
        let credential = match (auth.sends_credential, self.credentials()) {
            (true, Some(source)) => source.credential(&self.name).await,
            _ => None,
        };
        let mut answer = self
            .probe
            .check(&self.cfg().health_endpoint, credential.as_ref())
            .await;
        if !auth.reads_refusal && matches!(answer, ProbeAnswer::Refused(_)) {
            answer = ProbeAnswer::Down;
        }
        let answered = answer != ProbeAnswer::Down;
        let adopts = self.host().adopts();
        let mut rt = self.lock();
        // After our own power-off the host may still answer while it shuts
        // down: an answer counts as up only once a probe has failed (the
        // power-off is confirmed) or a wake ran.
        let trusted = if rt.down_unconfirmed {
            let now = self.clock.now_ms();
            let never_went_down = answered
                && rt
                    .poweroff_returned_ms
                    .is_some_and(|at| now.saturating_sub(at) >= self.boot_timeout_ms());
            if !answered {
                rt.down_unconfirmed = false;
            } else if never_went_down {
                // Still answering a whole boot timeout after the power-off
                // returned: it did not take effect. Trust the host again
                // (otherwise a lane that does not wake on request stays
                // known down for good) and say so.
                rt.down_unconfirmed = false;
                tracing::warn!(
                    lane = %self.name,
                    "host still answers after its poweroff command returned; taken for up again"
                );
                rt.last_error = Some(format!(
                    "host still answered {} after its poweroff command returned; the poweroff may not have taken effect",
                    fmt_ms(self.boot_timeout_ms())
                ));
            }
            never_went_down
        } else {
            true
        };
        let answer = if trusted { answer } else { ProbeAnswer::Down };
        let healthy = answer == ProbeAnswer::Up;
        if self.is_routed() && ticket > rt.observed_seq {
            rt.observed_seq = ticket;
            rt.observed_healthy = Some(healthy);
        }
        rt.answered_up = healthy;
        // Up while the lifecycle thinks the machine is off (no wake of ours
        // runs: that would be `waking`): it was woken outside Switchback.
        // An adopting machine takes it as its own, so its idle countdown
        // starts now and it powers off like one we woke.
        if healthy && adopts && rt.state == LaneState::Offline {
            tracing::info!(
                lane = %self.name,
                "machine answers without a wake of ours; adopted, idle countdown starts"
            );
            self.mark_up_locked(&mut rt);
        }
        rt.refused = match answer {
            ProbeAnswer::Refused(status) => Some(status),
            _ => None,
        };
        answer
    }

    /// The lane fails at once because its health endpoint refused the
    /// credential: loud (a config problem), never a boot timeout.
    fn refused(&self, status: u16) -> EnsureError {
        let auth = self.probe_auth();
        let detail = format!(
            "health endpoint answered {status}: the engine refuses the credential (missing or wrong; {})",
            if auth.sends_credential {
                format!(
                    "set api_key_env and auth_scheme on provider `{}`",
                    self.name
                )
            } else {
                "the credential is only sent to a health endpoint on the provider's base_url origin"
                    .to_string()
            }
        );
        let mut rt = self.lock();
        rt.state = LaneState::Offline;
        rt.wake_cycle_started_ms = None;
        rt.escalated = true;
        rt.last_error = Some(detail.clone());
        tracing::error!(lane = %self.name, status, "health endpoint refused the credential");
        EnsureError::Refused(detail)
    }

    fn probe_auth(&self) -> ProbeAuth {
        *self
            .probe_auth
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn credentials(&self) -> Option<Arc<dyn LaneCredentials>> {
        self.credentials
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Routing just skipped this lane as known down: start one background
    /// probe so the next request sees fresh evidence instead of waiting up to
    /// a whole supervisor interval. Never blocks the caller and never touches
    /// the lifecycle. Single-flight, and at most one start per
    /// [`REFRESH_MIN_INTERVAL_MS`] after any probe, so rejected traffic cannot
    /// turn into a probe flood. Outside a Tokio runtime it does nothing.
    fn schedule_refresh(self: &Arc<Self>, rt: &mut LaneRuntime) {
        if rt.refresh_in_flight {
            return;
        }
        let now = self.clock.now_ms();
        if rt
            .last_probe_started_ms
            .is_some_and(|started| now.saturating_sub(started) < REFRESH_MIN_INTERVAL_MS)
        {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        rt.refresh_in_flight = true;
        let executor = self.clone();
        handle.spawn(async move {
            executor.probe_health().await;
            executor.lock().refresh_in_flight = false;
        });
    }

    /// Liveness probe for routing. It records the result and never changes
    /// the lifecycle state: wake, drain and poweroff stay owned by jobs and
    /// [`Self::tick`]. The supervisor calls it on every tick.
    pub async fn observe(&self) {
        self.probe_health().await;
    }

    /// Take a lane from healthy/idle into draining for one accepted job. Resets
    /// the per-job self-heal budget and clears any prior escalation.
    fn admit_locked(&self, rt: &mut LaneRuntime) {
        rt.in_flight += 1;
        rt.idle_since_ms = None;
        rt.escalated = false;
        rt.last_error = None;
        rt.retries_used = 0;
        rt.poweroff_unconfigured_reported = false;
        rt.state = LaneState::Draining;
    }

    fn is_ready(state: LaneState) -> bool {
        matches!(
            state,
            LaneState::Healthy | LaneState::Draining | LaneState::IdleCountdown
        )
    }

    /// The lifecycle learns the lane is up: draining when work is already
    /// counted on it, else idle with the countdown starting now (so a lane
    /// woken for a request that then never arrives still powers off).
    fn mark_up_locked(&self, rt: &mut LaneRuntime) {
        if rt.in_flight > 0 {
            rt.state = LaneState::Draining;
            rt.idle_since_ms = None;
        } else {
            rt.state = LaneState::IdleCountdown;
            rt.idle_since_ms = Some(self.clock.now_ms());
        }
    }

    /// Count one text or embeddings request as in-flight work on the lane
    /// (lanes that wake on request, share a host or a GPU group; the caller
    /// checks). Media jobs and
    /// requests share the one `in_flight` count, so the idle timer and the
    /// poweroff wait for both, and the countdown starts when the last of
    /// either ends. Never waits and never wakes. A lane the lifecycle does
    /// not know to be up is counted but left in its state, except that a
    /// routing probe that last saw it up brings it into the lifecycle (so a
    /// lane started by hand is powered off after idle like a woken one).
    pub fn begin_request(self: &Arc<Self>) -> LaneJobGuard {
        let mut rt = self.lock();
        rt.in_flight += 1;
        let known_up = Self::is_ready(rt.state)
            || (rt.state == LaneState::Offline && rt.observed_healthy == Some(true));
        // A power-off whose retries ran out left the machine on and the lane
        // degraded. New work on it starts a new idle period with a fresh
        // retry budget, as a media job's does (`admit_locked`); left
        // degraded, the lane would hold the machine's power-off off for good.
        let degraded = rt.state == LaneState::Degraded;
        if degraded {
            rt.escalated = false;
            rt.last_error = None;
        }
        if known_up || degraded {
            rt.state = LaneState::Draining;
            rt.idle_since_ms = None;
        }
        LaneJobGuard::new(self.clone())
    }

    /// A text or embeddings request on a lane in an exclusive GPU group,
    /// before its first byte: wait up to `group_wait_secs` for the group
    /// (counted as queued, so the machine stays busy), then hold the group
    /// and the lane until the returned guard is dropped.
    pub async fn acquire_for_request(self: &Arc<Self>) -> LaneAcquire {
        let wait_ms = self.cfg().group_wait_secs.saturating_mul(1000);
        let queued = QueuedSlot::new(self.clone());
        let taken = self.take_group(wait_ms).await;
        let outcome = match taken {
            Ok((permit, detail)) => LaneAcquire::Ready {
                busy: Some(Box::new((self.begin_request(), permit))),
                detail: Some(detail),
            },
            Err((detail, retry_after)) => LaneAcquire::NotReady {
                detail,
                retry_after,
            },
        };
        drop(queued);
        outcome
    }

    /// Take the lane's GPU group without waiting: only while the lane holds
    /// it and no other lane waits for it. `Err` says why not.
    fn try_take_group(&self) -> Result<GroupPermit, String> {
        let Some(group) = self.cfg().exclusive_group.clone() else {
            return Err("is in no gpu group".to_string());
        };
        let host = self.host();
        let mut groups = host.groups();
        let state = groups.entry(group.clone()).or_default();
        if state.holder.as_deref() == Some(self.name.as_str()) && state.switch_to.is_none() {
            state.active += 1;
            drop(groups);
            return Ok(GroupPermit { host, group });
        }
        Err(match (&state.holder, &state.switch_to) {
            (_, Some(next)) => {
                format!("does not take gpu group `{group}` now (lane `{next}` is next for it)")
            }
            (Some(holder), None) => {
                format!("does not hold gpu group `{group}` (lane `{holder}` does)")
            }
            (None, None) => format!(
                "does not hold gpu group `{group}` yet (only a route's first choice takes it)"
            ),
        })
    }

    /// Take the lane's GPU group, waiting up to `wait_ms` on the clock seam.
    /// Work for the holder takes it at once. Otherwise the first waiter
    /// asks for a switch (once), waits until the holder has nothing in
    /// flight, runs the holder's `release_command` once (every other group
    /// lane's when the holder is unknown, after start-up or a power-off)
    /// and becomes the holder. Fairness: while a switch is pending, new work
    /// of the holder waits behind it, so the holder cannot starve the
    /// waiter; once the switch is done, the old holder's waiting work asks
    /// for the next one, so the new holder cannot starve it either. With
    /// three or more lanes the next switch goes to whichever lane asks first.
    async fn take_group(
        self: &Arc<Self>,
        wait_ms: u64,
    ) -> Result<(GroupPermit, String), (String, Duration)> {
        let Some(group) = self.cfg().exclusive_group.clone() else {
            return Err(("is in no gpu group".to_string(), LANE_GROUP_RETRY_AFTER));
        };
        let host = self.host();
        let others: Vec<String> = host
            .lanes()
            .iter()
            .filter(|lane| {
                lane.name != self.name
                    && lane.cfg().exclusive_group.as_deref() == Some(group.as_str())
            })
            .map(|lane| lane.name.clone())
            .collect();
        let started = self.clock.now_ms();
        let mut changes = host.group_changes.subscribe();
        let mut claim = SwitchClaim {
            host: host.clone(),
            group: group.clone(),
            lane: self.name.clone(),
            claimed: false,
        };
        loop {
            changes.borrow_and_update();
            let step = host.group_step(&group, &self.name, &others, &mut claim.claimed);
            let waited = self.clock.now_ms().saturating_sub(started);
            match step {
                GroupStep::Took => {
                    let detail = if waited == 0 {
                        format!("holds gpu group `{group}`")
                    } else {
                        format!(
                            "waited {} for gpu group `{group}`, then held it",
                            fmt_ms(waited)
                        )
                    };
                    return Ok((GroupPermit { host, group }, detail));
                }
                GroupStep::Release { from, lanes } => {
                    let giver = from.clone().unwrap_or_else(|| lanes.join(","));
                    tracing::info!(lane = %giver, taker = %self.name, group = %group, "taking the gpu group: releasing its holder");
                    let released = host.release(&lanes).await;
                    claim.claimed = false;
                    host.finish_switch(&group, &self.name, released.is_ok());
                    let waited = self.clock.now_ms().saturating_sub(started);
                    return match released {
                        Ok(()) => {
                            let how = match from {
                                Some(holder) => format!("took it from lane `{holder}`"),
                                None if lanes.is_empty() => "took it".to_string(),
                                None => format!(
                                    "took it (holder unknown, released lane `{}`)",
                                    lanes.join("`, `")
                                ),
                            };
                            Ok((
                                GroupPermit {
                                    host,
                                    group: group.clone(),
                                },
                                format!("waited {} for gpu group `{group}`, {how}", fmt_ms(waited)),
                            ))
                        }
                        Err(error) => Err((
                            format!("{error}; gpu group `{group}` not handed over"),
                            LANE_GROUP_RETRY_AFTER,
                        )),
                    };
                }
                GroupStep::Wait(why) => {
                    if waited >= wait_ms {
                        return Err((
                            format!("waited {} for gpu group `{group}`; {why}", fmt_ms(wait_ms)),
                            LANE_GROUP_RETRY_AFTER,
                        ));
                    }
                    tokio::select! {
                        _ = changes.changed() => {}
                        () = self.clock.sleep_ms(wait_ms - waited) => {}
                    }
                }
            }
        }
    }

    /// Run this lane's `release_command` so its engine frees the GPU. A
    /// failure counts against the lane's `retry_budget`; past it the lane
    /// is escalated and later switches away from it fail at once, so the
    /// GPU stays with it until an operator acts.
    async fn release_gpu(&self) -> Result<(), String> {
        let cfg = self.cfg();
        let failures = self.lock().release_failures;
        if failures > cfg.retry_budget {
            let message = format!(
                "lane `{}` could not release the gpu ({failures} failed release commands); needs operator attention",
                self.name
            );
            // Stay loud: new work on the lane clears its escalation
            // (`admit_locked`), but the GPU is still stuck with it.
            self.escalate(message.clone());
            return Err(message);
        }
        let Some(command) = cfg.release_command.clone() else {
            return Err(format!("lane `{}` has no release_command", self.name));
        };
        // Spaced like every other repeated power step of a lane: a failed
        // release is tried again only after `wake_retry_interval_secs`.
        let now = self.clock.now_ms();
        if let Some(at) = self.lock().release_retry_at_ms.filter(|at| now < *at) {
            return Err(format!(
                "release command of lane `{}` failed {failures} time(s); next try in {}",
                self.name,
                fmt_ms(at - now)
            ));
        }
        match self.run_command("release", &command).await {
            Ok(()) => {
                let mut rt = self.lock();
                rt.release_failures = 0;
                rt.release_retry_at_ms = None;
                Ok(())
            }
            Err(error) => {
                let gap_ms = cfg.wake_retry_interval_secs.saturating_mul(1000);
                let failures = {
                    let mut rt = self.lock();
                    rt.release_failures += 1;
                    rt.release_retry_at_ms = Some(self.clock.now_ms().saturating_add(gap_ms));
                    rt.release_failures
                };
                let message = format!("release command of lane `{}` failed: {error}", self.name);
                if failures > cfg.retry_budget {
                    tracing::error!(
                        lane = %self.name,
                        failures,
                        "release command failed and its retries ran out; the gpu stays with this lane"
                    );
                    self.escalate(format!(
                        "release command failed {failures} times (last: {error}); the gpu group stays with this lane until an operator acts"
                    ));
                } else {
                    tracing::warn!(lane = %self.name, attempt = failures, "release command failed; gpu not handed over");
                    self.lock().last_error = Some(message.clone());
                }
                Err(message)
            }
        }
    }

    /// Wake the lane for a request that routing was about to skip, and wait
    /// up to `wake_wait_secs` (on the [`Clock`] seam) for the result. The
    /// wake runs in a background task: it is shared by every waiting
    /// request (single-flight, and serialized with media wakes by the wake
    /// lock) and keeps going up to `boot_timeout_secs` after a request
    /// stops waiting. It starts with a fresh probe, so a lane that is only
    /// believed down is used at once, with no wake command.
    pub async fn wake_for_request(self: &Arc<Self>) -> LaneWake {
        let cfg = self.cfg();
        let wait_ms = cfg.wake_wait_secs.saturating_mul(1000);
        let started = self.clock.now_ms();
        let mut changes = self.changes.subscribe();
        let cycles_before = {
            let mut rt = self.lock();
            self.start_request_wake_locked(&mut rt);
            // A waking cycle already under way (a media job's, or an earlier
            // request's that outlived its wait) counts as a wake for this
            // request when it brings the lane up.
            rt.wake_cycles
                .saturating_sub(u64::from(rt.wake_cycle_started_ms.is_some()))
        };
        // Counted as queued until this future ends, including when it is
        // dropped mid-wait because the client went away.
        let _queued = QueuedSlot::new(self.clone());
        // `Ok(woken)` or `Err((reason, wake still running))`.
        let outcome: Result<bool, (String, bool)> = loop {
            changes.borrow_and_update();
            {
                let rt = self.lock();
                if !rt.request_wake_running {
                    break match &rt.last_request_wake {
                        Some(Ok(())) => Ok(rt.wake_cycles != cycles_before),
                        Some(Err(reason)) => Err((reason.clone(), false)),
                        None => Err(("wake did not report".to_string(), false)),
                    };
                }
            }
            let elapsed = self.clock.now_ms().saturating_sub(started);
            if elapsed >= wait_ms {
                break Err((format!("wake wait of {} passed", fmt_ms(wait_ms)), true));
            }
            tokio::select! {
                _ = changes.changed() => {}
                _ = self.clock.sleep_ms(wait_ms - elapsed) => {}
            }
        };
        let elapsed = self.clock.now_ms().saturating_sub(started);
        let rt = self.lock();
        match outcome {
            Ok(true) => LaneWake::Ready {
                detail: format!("woken for this request, healthy after {}", fmt_ms(elapsed)),
            },
            Ok(false) => LaneWake::Ready {
                detail: format!(
                    "answered a fresh probe after {}, used without a wake",
                    fmt_ms(elapsed)
                ),
            },
            Err((reason, still_waking)) => {
                // While the wake still runs, retry once its boot deadline
                // has passed; otherwise after the lane's wake re-send gap.
                let retry_ms = match (still_waking, rt.wake_cycle_started_ms) {
                    (true, Some(cycle_started)) => cycle_started
                        .saturating_add(cfg.boot_timeout_secs.saturating_mul(1000))
                        .saturating_sub(self.clock.now_ms()),
                    _ => cfg.wake_retry_interval_secs.saturating_mul(1000),
                };
                LaneWake::NotReady {
                    detail: if still_waking {
                        format!("{reason}, wake still running")
                    } else {
                        reason
                    },
                    retry_after: Duration::from_millis(retry_ms.max(1_000)),
                }
            }
        }
    }

    /// Start the shared request-triggered wake unless one already runs.
    /// After a failed one, no new wake starts until `wake_retry_interval_secs`
    /// has passed: requests in that gap get the last failure at once, so
    /// steady traffic cannot turn a broken wake command into a command
    /// flood. Outside a Tokio runtime it records a failure instead.
    fn start_request_wake_locked(self: &Arc<Self>, rt: &mut LaneRuntime) {
        if rt.request_wake_running {
            return;
        }
        if let (Some(Err(_)), Some(ended)) = (&rt.last_request_wake, rt.last_request_wake_ended_ms)
        {
            let gap = self.cfg().wake_retry_interval_secs.saturating_mul(1000);
            if self.clock.now_ms().saturating_sub(ended) < gap {
                return;
            }
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            rt.last_request_wake = Some(Err("no async runtime to run the wake".to_string()));
            return;
        };
        rt.request_wake_running = true;
        rt.last_request_wake = None;
        let reset = RequestWakeReset {
            executor: self.clone(),
        };
        handle.spawn(async move {
            let result = reset.executor.wake_for_routing().await;
            {
                let mut rt = reset.executor.lock();
                rt.last_request_wake = Some(result);
                rt.last_request_wake_ended_ms = Some(reset.executor.clock.now_ms());
            }
            drop(reset);
        });
    }

    /// The body of a request-triggered wake, under the wake lock. A fresh
    /// probe first; if it fails, the lifecycle is corrected (a lane it
    /// thought up is down) and the normal waking cycle runs.
    async fn wake_for_routing(self: &Arc<Self>) -> Result<(), String> {
        let host = self.host();
        let _wake = host.wake_lock.lock().await;
        if self.probe_health().await {
            let mut rt = self.lock();
            if !Self::is_ready(rt.state) {
                self.mark_up_locked(&mut rt);
            }
            return Ok(());
        }
        {
            let mut rt = self.lock();
            if Self::is_ready(rt.state) {
                rt.state = LaneState::Offline;
                rt.idle_since_ms = None;
            }
        }
        match self.wake_and_admit().await {
            // The request takes its own slot when it is dispatched; this
            // one only proves the lane is up and arms the idle countdown.
            Ok(guard) => {
                drop(guard);
                Ok(())
            }
            Err(EnsureError::WakeUnconfigured) => Err("wake command unconfigured".to_string()),
            Err(EnsureError::BootTimeout { elapsed_ms, .. }) => Err(format!(
                "wake did not reach health within its boot timeout ({})",
                fmt_ms(elapsed_ms)
            )),
            Err(EnsureError::WakeFailed(error)) => Err(format!("wake command failed: {error}")),
            Err(EnsureError::Refused(detail)) => Err(detail),
        }
    }

    /// Ensure the lane has healthy capacity for one job, waking the machine if
    /// needed. On success returns a guard that holds the job's in-flight slot;
    /// dropping it releases the slot and arms the idle countdown when the lane
    /// falls idle. Single-flight: concurrent callers trigger at most one wake.
    pub async fn ensure_ready(self: &Arc<Self>) -> Result<LaneJobGuard, EnsureError> {
        // Fast path: already healthy — dispatch concurrently, no wake lock.
        {
            let mut rt = self.lock();
            if Self::is_ready(rt.state) {
                self.admit_locked(&mut rt);
                return Ok(LaneJobGuard::new(self.clone()));
            }
        }

        // Slow path: needs a wake. Count as queued while we wait for the lock
        // and the wake. The slot is released on drop, so a job cancelled (or
        // a client gone) mid-wait never stays counted and never keeps the
        // machine on.
        let _queued = QueuedSlot::new(self.clone());
        let host = self.host();
        let _wake = host.wake_lock.lock().await;
        self.wake_and_admit().await
    }

    /// Runs under the wake lock: double-checks, probes, and (if still offline)
    /// runs the single wake + boot poll.
    async fn wake_and_admit(self: &Arc<Self>) -> Result<LaneJobGuard, EnsureError> {
        // Another waker may have brought the lane up while we queued.
        {
            let mut rt = self.lock();
            if Self::is_ready(rt.state) {
                self.admit_locked(&mut rt);
                return Ok(LaneJobGuard::new(self.clone()));
            }
        }

        // The service may already be up without our wake (operator started it).
        match self.probe_answer().await {
            ProbeAnswer::Up => {
                let mut rt = self.lock();
                self.admit_locked(&mut rt);
                return Ok(LaneJobGuard::new(self.clone()));
            }
            ProbeAnswer::Refused(status) => return Err(self.refused(status)),
            ProbeAnswer::Down => {}
        }

        // Wake leg. Skip-gated when unconfigured — never faked.
        let Some((wake_lane, wake_command)) = self.host().wake_command() else {
            let mut rt = self.lock();
            rt.state = LaneState::Offline;
            rt.last_wake_result = Some("unconfigured".to_string());
            tracing::warn!(lane = %self.name, "wake command unconfigured; job stays queued");
            return Err(EnsureError::WakeUnconfigured);
        };

        // A machine that another lane already brought up only needs this
        // lane's engine: never send the wake again (a power toggle would
        // switch it off).
        if let Some(evidence) = self.host_up_without_me(true).await {
            return self.come_up_on_running_host(evidence).await;
        }

        {
            let mut rt = self.lock();
            rt.state = LaneState::Waking;
            rt.wake_attempts = 0;
            rt.wake_cycles += 1;
            rt.wake_cycle_started_ms = Some(self.clock.now_ms());
        }
        tracing::info!(lane = %self.name, "waking local executor");

        // One waking cycle. The wake command is re-issued on a fixed interval —
        // Wake-on-LAN and similar transports are lossy fire-and-forget UDP, so a
        // single send is unreliable — until health passes or the boot deadline
        // expires. Single-flight holds at the state-machine level: only the
        // wake-lock holder runs this cycle, and the retries are inside it.
        let start = self.clock.now_ms();
        let wake_retry_ms = self.cfg().wake_retry_interval_secs.saturating_mul(1000);
        let mut next_wake_at = start; // fire immediately, then every interval
        loop {
            let now = self.clock.now_ms();

            // Boot deadline first, so we never fire a wake past the timeout.
            let elapsed = now.saturating_sub(start);
            if elapsed >= self.boot_timeout_ms() {
                let mut rt = self.lock();
                let attempts = rt.wake_attempts;
                rt.state = LaneState::Offline;
                rt.last_wake_result = Some("timeout".to_string());
                rt.wake_cycle_started_ms = None;
                rt.escalated = true;
                rt.last_error = Some(format!(
                    "wake `{wake_command}` did not reach health in {elapsed}ms after {attempts} attempt(s)"
                ));
                drop(rt);
                // The wake brought nothing up: it is no evidence that the
                // machine runs, so the next lane that needs it wakes it.
                self.host().forget_wake_sent();
                tracing::error!(lane = %self.name, elapsed_ms = elapsed, attempts, "boot timeout");
                return Err(EnsureError::BootTimeout {
                    wake_path: wake_command,
                    elapsed_ms: elapsed,
                });
            }

            // (Re-)fire the wake when due. A re-send is skipped while another
            // lane of the machine answers: the machine is up, only this
            // lane's engine is still starting.
            let first_attempt = self.lock().wake_attempts == 0;
            let resend_skipped = if now >= next_wake_at && !first_attempt {
                self.host_up_without_me(false).await
            } else {
                None
            };
            if let Some(evidence) = resend_skipped {
                self.lock().last_wake_result = Some(format!("re-send skipped: {evidence}"));
                next_wake_at = now.saturating_add(wake_retry_ms);
            } else if now >= next_wake_at {
                match wake_lane.run_command("wake", &wake_command).await {
                    Ok(()) => {
                        {
                            let mut rt = self.lock();
                            rt.wake_attempts += 1;
                            rt.last_wake_ms = Some(now);
                            rt.last_wake_result =
                                Some(format!("sent (attempt {})", rt.wake_attempts));
                        }
                        // A wake ran: the next healthy answer is trusted, and
                        // the machine counts as coming up for its other lanes.
                        self.host().clear_down_unconfirmed();
                        self.host().note_wake_sent(now);
                    }
                    Err(error) if first_attempt => {
                        // The very first send failing means the wake mechanism
                        // itself is broken (e.g. a bad command) — fail loud
                        // rather than spin until the boot deadline.
                        let mut rt = self.lock();
                        rt.wake_attempts += 1;
                        rt.last_wake_ms = Some(now);
                        rt.state = LaneState::Offline;
                        rt.last_wake_result = Some(format!("failed: {error}"));
                        rt.wake_cycle_started_ms = None;
                        rt.escalated = true;
                        rt.last_error = Some(format!("wake command failed: {error}"));
                        return Err(EnsureError::WakeFailed(error));
                    }
                    Err(error) => {
                        // A later send failing is tolerated (transient) — keep
                        // polling and re-firing until the boot deadline.
                        let mut rt = self.lock();
                        rt.wake_attempts += 1;
                        rt.last_wake_ms = Some(now);
                        rt.last_wake_result = Some(format!(
                            "retry send failed (attempt {}): {error}",
                            rt.wake_attempts
                        ));
                    }
                }
                next_wake_at = now.saturating_add(wake_retry_ms);
            }

            let answer = self.probe_answer().await;
            if let ProbeAnswer::Refused(status) = answer {
                return Err(self.refused(status));
            }
            if answer == ProbeAnswer::Up {
                let mut rt = self.lock();
                rt.last_wake_result = Some(format!("ok after {} attempt(s)", rt.wake_attempts));
                rt.wake_cycle_started_ms = None;
                self.admit_locked(&mut rt);
                tracing::info!(lane = %self.name, "local executor healthy");
                return Ok(LaneJobGuard::new(self.clone()));
            }

            self.clock
                .sleep_ms(self.cfg().health_poll_interval_ms)
                .await;
        }
    }

    /// Why this lane's shared machine is known to be up without it, or
    /// `None`. Evidence, in order: the machine's wake command succeeded in
    /// this power cycle less than this lane's `boot_timeout_secs` ago (only
    /// with `with_wake`; a wake that brought nothing up within its boot
    /// timeout, or our power-off, ends that), or another lane of the
    /// machine answers a fresh probe (a refused credential is an answer
    /// too). A lane without `host` never has any: it wakes its machine as
    /// it always did.
    async fn host_up_without_me(&self, with_wake: bool) -> Option<String> {
        let host = self.host();
        if !host.shared {
            return None;
        }
        let now = self.clock.now_ms();
        if with_wake
            && host
                .wake_sent_ms()
                .is_some_and(|sent| now.saturating_sub(sent) < self.boot_timeout_ms())
        {
            return Some(format!(
                "host `{}` was woken {} ago",
                host.id,
                fmt_ms(now.saturating_sub(host.wake_sent_ms().unwrap_or(now)))
            ));
        }
        for lane in host.lanes() {
            if lane.name != self.name && lane.probe_answer().await != ProbeAnswer::Down {
                return Some(format!(
                    "lane `{}` of host `{}` answers",
                    lane.name, host.id
                ));
            }
        }
        None
    }

    /// Bring this lane up on a machine that is already up: no wake command.
    /// Wait for its engine up to `boot_timeout_secs`; if it does not answer,
    /// run `restart_command` when one is configured and wait once more.
    /// Otherwise the lane fails like a boot timeout, with the reason.
    async fn come_up_on_running_host(
        self: &Arc<Self>,
        evidence: String,
    ) -> Result<LaneJobGuard, EnsureError> {
        let started = self.clock.now_ms();
        {
            let mut rt = self.lock();
            rt.state = LaneState::Waking;
            rt.wake_attempts = 0;
            rt.last_wake_result = Some(format!("not sent: {evidence}"));
        }
        tracing::info!(lane = %self.name, "host already up; waiting for this lane's engine without a wake");
        let boot_ms = self.boot_timeout_ms();
        let mut answer = self.wait_for_health(boot_ms).await;
        let mut restart_note = String::new();
        if answer == ProbeAnswer::Down {
            match self.cfg().restart_command.clone() {
                Some(restart) => match self.run_command("restart", &restart).await {
                    Ok(()) => {
                        restart_note = ", then after its restart_command".to_string();
                        answer = self.wait_for_health(boot_ms).await;
                    }
                    Err(error) => restart_note = format!("; its restart_command failed: {error}"),
                },
                None => restart_note = "; no restart_command to try".to_string(),
            }
        }
        if let ProbeAnswer::Refused(status) = answer {
            return Err(self.refused(status));
        }
        let elapsed = self.clock.now_ms().saturating_sub(started);
        let mut rt = self.lock();
        if answer == ProbeAnswer::Up {
            rt.last_wake_result = Some(format!(
                "up without a wake after {} ({evidence})",
                fmt_ms(elapsed)
            ));
            self.admit_locked(&mut rt);
            tracing::info!(lane = %self.name, "local executor healthy");
            return Ok(LaneJobGuard::new(self.clone()));
        }
        rt.state = LaneState::Offline;
        rt.last_wake_result = Some("timeout".to_string());
        rt.escalated = true;
        rt.last_error = Some(format!(
            "did not answer within {} while its host was up ({evidence}){restart_note}; no wake command sent",
            fmt_ms(boot_ms)
        ));
        tracing::error!(lane = %self.name, elapsed_ms = elapsed, "engine did not come up on a running host");
        Err(EnsureError::BootTimeout {
            wake_path: "none (host already up)".to_string(),
            elapsed_ms: elapsed,
        })
    }

    /// Poll this lane's health every `health_poll_interval_ms` until it
    /// answers (`Up`, or `Refused` at once) or `budget_ms` has passed
    /// (`Down`).
    async fn wait_for_health(&self, budget_ms: u64) -> ProbeAnswer {
        let start = self.clock.now_ms();
        loop {
            let answer = self.probe_answer().await;
            if answer != ProbeAnswer::Down {
                return answer;
            }
            if self.clock.now_ms().saturating_sub(start) >= budget_ms {
                return ProbeAnswer::Down;
            }
            self.clock
                .sleep_ms(self.cfg().health_poll_interval_ms)
                .await;
        }
    }

    /// Run a job under managed capacity: wake if needed, then dispatch with
    /// self-heal. `op(attempt)` is the real dispatch; it runs at least once and
    /// is requeued (fresh attempt) after a health loss, up to `retry_budget`.
    pub async fn run_job<T, E, F, Fut>(self: &Arc<Self>, op: F) -> Result<T, JobError<E>>
    where
        F: FnMut(u32) -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
        E: std::fmt::Display,
    {
        self.run_job_noted(op).await.map(|(value, _)| value)
    }

    /// [`Self::run_job`], plus what taking the lane's GPU group did, for the
    /// job's decision (`None` outside a group). A job on a lane in an
    /// exclusive GPU group waits for the group after its wake, up to the
    /// lane's `group_wait_secs`, and holds it until it ends.
    pub async fn run_job_noted<T, E, F, Fut>(
        self: &Arc<Self>,
        mut op: F,
    ) -> Result<(T, Option<String>), JobError<E>>
    where
        F: FnMut(u32) -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
        E: std::fmt::Display,
    {
        let _guard = match self.ensure_ready().await {
            Ok(guard) => guard,
            Err(EnsureError::WakeUnconfigured) => return Err(JobError::Queued),
            Err(EnsureError::BootTimeout {
                wake_path,
                elapsed_ms,
            }) => {
                return Err(JobError::BootTimeout {
                    wake_path,
                    elapsed_ms,
                })
            }
            Err(EnsureError::WakeFailed(error)) => return Err(JobError::WakeFailed(error)),
            Err(EnsureError::Refused(detail)) => return Err(JobError::Refused(detail)),
        };
        let mut note = None;
        let _permit = if self.cfg().exclusive_group.is_some() {
            let wait_ms = self.cfg().group_wait_secs.saturating_mul(1000);
            match self.take_group(wait_ms).await {
                Ok((permit, detail)) => {
                    note = Some(detail);
                    Some(permit)
                }
                Err((detail, _)) => return Err(JobError::GpuUnavailable(detail)),
            }
        } else {
            None
        };

        loop {
            let attempt = self.lock().retries_used;
            match op(attempt).await {
                Ok(value) => return Ok((value, note)),
                Err(error) => {
                    // Classify: is the service still healthy? If so this is a
                    // real job error — loud, unretried. If not, the service was
                    // lost mid-job and we self-heal + requeue.
                    // A refused probe also means the engine answers.
                    if self.probe_answer().await != ProbeAnswer::Down {
                        let mut rt = self.lock();
                        rt.last_error = Some(format!("dispatch failed: {error}"));
                        return Err(JobError::Dispatch(error));
                    }
                    let budget = self.cfg().retry_budget;
                    if self.lock().retries_used >= budget {
                        self.escalate(format!(
                            "retry budget {budget} exhausted after health loss: {error}"
                        ));
                        return Err(JobError::RetriesExhausted {
                            budget,
                            last_error: error.to_string(),
                        });
                    }
                    self.lock().retries_used += 1;
                    if let Err(heal_error) = self.heal().await {
                        self.escalate(format!("self-heal failed: {heal_error}"));
                        return Err(JobError::RetriesExhausted {
                            budget,
                            last_error: heal_error,
                        });
                    }
                }
            }
        }
    }

    /// Recover a wedged service in place: `restart_command` when configured,
    /// else fall back to `wake_command`. Polls health up to the boot timeout.
    async fn heal(self: &Arc<Self>) -> Result<(), String> {
        let cfg = self.cfg();
        let command = cfg
            .restart_command
            .clone()
            .or_else(|| cfg.wake_command.clone());
        let Some(command) = command else {
            return Err("no restart_command or wake_command configured to heal".to_string());
        };
        // Falling back to the wake command on a shared machine that is
        // known up would send a wake to a running machine: only wait.
        let wake_skipped = cfg.restart_command.is_none() && {
            let evidence = self.host_up_without_me(true).await;
            if let Some(evidence) = &evidence {
                self.lock().last_wake_result = Some(format!("not sent: {evidence}"));
            }
            evidence.is_some()
        };
        self.lock().state = LaneState::Waking;
        if !wake_skipped {
            self.lock().last_wake_ms = Some(self.clock.now_ms());
            tracing::warn!(lane = %self.name, "self-heal: restarting local executor");
            self.run_command("restart", &command)
                .await
                .map_err(|e| format!("restart command failed: {e}"))?;
        }
        let start = self.clock.now_ms();
        loop {
            if self.probe_health().await {
                let mut rt = self.lock();
                // The job still owns its in-flight slot; return to draining.
                rt.state = LaneState::Draining;
                rt.last_wake_result = Some("healed".to_string());
                return Ok(());
            }
            let elapsed = self.clock.now_ms().saturating_sub(start);
            if elapsed >= self.boot_timeout_ms() {
                let mut rt = self.lock();
                rt.state = LaneState::Degraded;
                return Err(format!("service did not recover within {elapsed}ms"));
            }
            self.clock
                .sleep_ms(self.cfg().health_poll_interval_ms)
                .await;
        }
    }

    fn escalate(&self, error: String) {
        let mut rt = self.lock();
        rt.escalated = true;
        rt.last_error = Some(error);
    }

    /// Release one in-flight slot; arm the idle countdown when the lane falls
    /// idle. Called from the job guard's `Drop`.
    fn release(&self) {
        let mut rt = self.lock();
        rt.in_flight = rt.in_flight.saturating_sub(1);
        if rt.in_flight == 0 && matches!(rt.state, LaneState::Draining | LaneState::Healthy) {
            rt.state = LaneState::IdleCountdown;
            rt.idle_since_ms = Some(self.clock.now_ms());
        }
    }

    /// Supervisor tick: power the lane's machine off once every lane on it
    /// has been idle past its own timeout (see [`Host::tick`]).
    pub async fn tick(self: &Arc<Self>) {
        self.host().tick(None).await;
    }

    /// Metadata-only snapshot for the doctor / live capacity surface.
    pub fn report(&self) -> LocalLaneReport {
        self.report_with(None)
    }

    /// [`Self::report`] with the machine's holds.
    fn report_with(&self, holds: Option<&Holds>) -> LocalLaneReport {
        let host = self.host();
        let now = self.clock.now_ms();
        let poweroff_in_secs = host
            .poweroff_in_ms(holds, now)
            .map(|ms| ms.saturating_add(999) / 1_000);
        let hold_views = holds
            .map(|holds| {
                holds
                    .list(Some(&host.id), now)
                    .iter()
                    .map(Hold::view)
                    .collect()
            })
            .unwrap_or_default();
        let wake_configured = host.wake_command().is_some();
        let poweroff_configured = host.poweroff_command().is_some();
        let poweroff_failures = host.power().failures;
        let guard = host.guard_view(self.clock.now_ms());
        let restart_configured = self.cfg().restart_command.is_some();
        let gpu_group = self.cfg().exclusive_group.clone();
        let holds_gpu = gpu_group.as_ref().map(|group| {
            host.groups()
                .get(group)
                .is_some_and(|state| state.holder.as_deref() == Some(self.name.as_str()))
        });
        let rt = self.lock();
        let mut level = lane_level(&rt, wake_configured, poweroff_failures);
        if guard.broken && level == LaneLevel::Ok {
            level = LaneLevel::Warn;
        }
        let detail = lane_detail(
            &rt,
            &self.name,
            wake_configured,
            poweroff_configured,
            poweroff_failures,
        );
        let detail = guard.detail(&self.name, &rt, detail);
        LocalLaneReport {
            name: self.name.clone(),
            host: host.shared.then(|| host.id.clone()),
            gpu_group,
            holds_gpu,
            state: if rt.state == LaneState::Offline && rt.answered_up {
                LaneState::Healthy
            } else {
                rt.state
            },
            level,
            queue_depth: rt.waiting,
            in_flight: rt.in_flight,
            retries_used: rt.retries_used,
            wake_attempts: rt.wake_attempts,
            last_wake_ms: rt.last_wake_ms,
            last_wake_result: rt.last_wake_result.clone(),
            last_error: rt.last_error.clone(),
            wake_configured,
            poweroff_configured,
            restart_configured,
            escalated: rt.escalated,
            poweroff_failures,
            poweroff_guard_busy: guard.busy,
            poweroff_guard_busy_since_ms: guard.busy_since_ms,
            poweroff_guard_failures: guard.failures,
            poweroff_guard_broken: guard.broken,
            detail,
            observed_healthy: rt.observed_healthy,
            poweroff_in_secs,
            holds: hold_views,
        }
    }
}

fn lane_level(rt: &LaneRuntime, wake_configured: bool, poweroff_failures: u32) -> LaneLevel {
    if rt.escalated || rt.state == LaneState::Degraded {
        LaneLevel::Fail
    } else if !wake_configured {
        // Managed lane with no wake wiring: it can serve only if the operator
        // happens to have the service up. Flag it so the gap is visible.
        LaneLevel::Warn
    } else if matches!(rt.state, LaneState::Waking) || poweroff_failures > 0 {
        LaneLevel::Warn
    } else {
        LaneLevel::Ok
    }
}

fn lane_detail(
    rt: &LaneRuntime,
    name: &str,
    wake_configured: bool,
    poweroff_configured: bool,
    poweroff_failures: u32,
) -> String {
    if let Some(error) = &rt.last_error {
        if rt.escalated {
            return format!("{name} escalated: {error}");
        }
        if poweroff_failures > 0 {
            return format!("{name} {error}");
        }
    }
    if !wake_configured {
        return format!("{name} wake unconfigured; jobs stay queued until a service is up");
    }
    if !poweroff_configured && matches!(rt.state, LaneState::IdleCountdown | LaneState::Healthy) {
        return format!("{name} poweroff unconfigured; idle machine will not be powered off");
    }
    match rt.state {
        LaneState::Offline if rt.answered_up => format!(
            "{name} healthy, idle; its machine was woken outside Switchback, so no idle countdown runs (adopt_external_wake adopts it)"
        ),
        LaneState::Offline => format!("{name} offline; will wake on next job"),
        LaneState::Waking => format!("{name} waking"),
        LaneState::Healthy => format!("{name} healthy, idle"),
        LaneState::Draining => format!("{name} draining {} job(s)", rt.in_flight),
        LaneState::IdleCountdown => format!("{name} idle; poweroff countdown running"),
        LaneState::PoweringOff => format!("{name} powering off"),
        LaneState::Degraded => format!("{name} degraded; needs operator attention"),
    }
}

// ---------------------------------------------------------------------------
// Host: the powered machine under one or more lanes
// ---------------------------------------------------------------------------

/// The powered machine one or more lanes run on. It owns what belongs to
/// the machine rather than to one engine: the wake lock, the power commands
/// and the power-off decision.
pub(crate) struct Host {
    id: String,
    /// Named by `host:` in config (shared by its lanes), as opposed to the
    /// machine of its own a lane without `host` gets.
    shared: bool,
    /// Serializes the machine's wakes, so one wake runs however many lanes
    /// and requests ask, and keeps a wake off the machine while a power-off
    /// command runs.
    wake_lock: tokio::sync::Mutex<()>,
    /// One power-off decision at a time.
    tick_lock: tokio::sync::Mutex<()>,
    /// The lanes on the machine, in config order.
    members: RwLock<Vec<Weak<LocalExecutor>>>,
    power: Mutex<HostPower>,
    guard: Mutex<GuardState>,
    /// When the machine's wake command last succeeded in this power cycle
    /// (clock ms). Ended by our power-off and by a wake that brought
    /// nothing up within its boot timeout.
    wake_sent: Mutex<Option<u64>>,
    /// The machine's exclusive GPU groups, by `exclusive_group` id.
    groups: Mutex<BTreeMap<String, GroupState>>,
    /// Bumped whenever a group changes, so waiters wake without polling.
    group_changes: tokio::sync::watch::Sender<u64>,
}

/// One exclusive GPU group on a machine.
#[derive(Debug, Default)]
struct GroupState {
    /// The lane whose engine may hold the GPU. `None` = unknown (after
    /// start-up or a power-off): taking the group then releases every other
    /// lane of the group once.
    holder: Option<String>,
    /// Work of the holder (requests, jobs) that holds the group now.
    active: u32,
    /// The lane a switch was asked for (set once, by its first waiter).
    switch_to: Option<String>,
    /// The switcher is running release commands.
    releasing: bool,
}

/// What one look at a GPU group told a lane that wants it.
enum GroupStep {
    /// The lane holds the group; one unit of work was counted.
    Took,
    /// This waiter runs the switch now: release these lanes.
    Release {
        from: Option<String>,
        lanes: Vec<String>,
    },
    /// Not yet; why, for the decision if the wait runs out.
    Wait(String),
}

/// One unit of work holding a GPU group; dropping it lets a waiting lane
/// take the group once the holder has nothing left in flight.
struct GroupPermit {
    host: Arc<Host>,
    group: String,
}

impl Drop for GroupPermit {
    fn drop(&mut self) {
        if let Some(state) = self.host.groups().get_mut(&self.group) {
            state.active = state.active.saturating_sub(1);
        }
        self.host.group_changed();
    }
}

/// The switch a waiter asked for; a waiter that goes away (wait over,
/// client gone, task dropped mid-release) withdraws it, so the holder's
/// work can flow again.
struct SwitchClaim {
    host: Arc<Host>,
    group: String,
    lane: String,
    claimed: bool,
}

impl Drop for SwitchClaim {
    fn drop(&mut self) {
        if !self.claimed {
            return;
        }
        if let Some(state) = self.host.groups().get_mut(&self.group) {
            if state.switch_to.as_deref() == Some(self.lane.as_str()) {
                state.switch_to = None;
                state.releasing = false;
            }
        }
        self.host.group_changed();
    }
}

/// Power-off failures of the machine's current idle period.
#[derive(Debug, Default)]
struct HostPower {
    failures: u32,
    /// The idle period the failures belong to: the latest moment one of the
    /// machine's lanes went idle. New work starts a new period, which gets
    /// a fresh retry budget.
    failed_period: Option<u64>,
    /// No new attempt before this moment (clock ms).
    retry_at_ms: Option<u64>,
}

/// What the machine's power-off guard (`poweroff_guard_command`) said
/// lately. Only the guard's own runs change it, never new work or a new
/// idle period, so a guard that stays busy or broken for a month stays
/// visible in the report. It belongs to one command: a reload that
/// changes or removes the guard starts it afresh (see [`Host::guard`]).
#[derive(Debug, Default)]
struct GuardState {
    /// The guard command these answers came from.
    command: Option<String>,
    /// Busy answers in a row, and when the first of them came (clock ms).
    busy: u32,
    busy_since_ms: Option<u64>,
    /// Runs in a row that gave no answer (timed out, could not run), and
    /// why the last one gave none.
    failures: u32,
    last_failure: Option<String>,
    /// Do not ask before this moment (clock ms).
    ask_at_ms: Option<u64>,
}

/// The guard's state as a lane's report shows it.
#[derive(Debug, Default)]
struct GuardView {
    busy: u32,
    busy_since_ms: Option<u64>,
    busy_for_ms: u64,
    failures: u32,
    last_failure: Option<String>,
    /// `retry_budget` (at least one) failures in a row: the guard itself
    /// is broken, and the lane reports `warn`.
    broken: bool,
}

impl GuardView {
    /// The lane's detail with what the guard says on top: a broken guard
    /// whatever the lane does (an escalation stays first), a postponed
    /// power-off while the lane counts down to idle.
    fn detail(&self, name: &str, rt: &LaneRuntime, detail: String) -> String {
        if rt.escalated {
            return detail;
        }
        if self.broken {
            return format!(
                "{name} power-off guard is broken: no answer {} times in a row (last: {}); the machine stays on until it answers",
                self.failures,
                self.last_failure.as_deref().unwrap_or("unknown")
            );
        }
        if self.busy > 0 && rt.state == LaneState::IdleCountdown {
            return format!(
                "{name} idle; power-off postponed: guard reports the machine busy ({} answers in a row, busy for {})",
                self.busy,
                fmt_span(self.busy_for_ms)
            );
        }
        detail
    }
}

impl Host {
    fn new(id: &str, shared: bool, members: Vec<Weak<LocalExecutor>>) -> Arc<Self> {
        Arc::new(Self {
            id: id.to_string(),
            shared,
            wake_lock: tokio::sync::Mutex::new(()),
            tick_lock: tokio::sync::Mutex::new(()),
            members: RwLock::new(members),
            power: Mutex::new(HostPower::default()),
            guard: Mutex::new(GuardState::default()),
            wake_sent: Mutex::new(None),
            groups: Mutex::new(BTreeMap::new()),
            group_changes: tokio::sync::watch::channel(0).0,
        })
    }

    fn groups(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, GroupState>> {
        self.groups
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn group_changed(&self) {
        self.group_changes
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    /// One look at `group` for `lane` (see [`LocalExecutor::take_group`]).
    fn group_step(
        &self,
        group: &str,
        lane: &str,
        others: &[String],
        claimed: &mut bool,
    ) -> GroupStep {
        let mut groups = self.groups();
        let state = groups.entry(group.to_string()).or_default();
        if state.holder.as_deref() == Some(lane) && state.switch_to.is_none() {
            state.active += 1;
            return GroupStep::Took;
        }
        match state.switch_to.as_deref() {
            None => {
                state.switch_to = Some(lane.to_string());
                *claimed = true;
            }
            Some(next) if next == lane && *claimed => {}
            Some(next) if next == lane => {
                return GroupStep::Wait("a switch to this lane is under way".to_string())
            }
            Some(next) => return GroupStep::Wait(format!("lane `{next}` is next for it")),
        }
        if state.releasing || state.active > 0 {
            return GroupStep::Wait(match &state.holder {
                Some(holder) => {
                    format!(
                        "lane `{holder}` still holds it ({} in flight)",
                        state.active
                    )
                }
                None => "it is being released".to_string(),
            });
        }
        state.releasing = true;
        GroupStep::Release {
            from: state.holder.clone(),
            lanes: match &state.holder {
                Some(holder) => vec![holder.clone()],
                None => others.to_vec(),
            },
        }
    }

    /// The switcher is done: on success `lane` holds the group with one
    /// unit of work counted; otherwise the holder keeps it.
    fn finish_switch(&self, group: &str, lane: &str, released: bool) {
        if let Some(state) = self.groups().get_mut(group) {
            state.releasing = false;
            state.switch_to = None;
            if released {
                state.holder = Some(lane.to_string());
                state.active += 1;
            }
        }
        self.group_changed();
    }

    /// Run the release command of each named lane on this machine, in
    /// order; the first failure stops the switch.
    async fn release(&self, names: &[String]) -> Result<(), String> {
        for name in names {
            let Some(lane) = self.lanes().into_iter().find(|lane| lane.name == *name) else {
                continue;
            };
            lane.release_gpu().await?;
        }
        Ok(())
    }

    /// The machine's lanes still alive, in config order.
    fn lanes(&self) -> Vec<Arc<LocalExecutor>> {
        self.members
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter_map(Weak::upgrade)
            .collect()
    }

    fn set_members(&self, members: Vec<Weak<LocalExecutor>>) {
        *self
            .members
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = members;
    }

    fn power(&self) -> std::sync::MutexGuard<'_, HostPower> {
        self.power
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The machine's wake command and the lane that carries it: the first
    /// lane in config order that sets one.
    fn wake_command(&self) -> Option<(Arc<LocalExecutor>, String)> {
        self.lanes().into_iter().find_map(|lane| {
            let command = lane.cfg().wake_command.clone()?;
            Some((lane, command))
        })
    }

    /// The machine's power-off command and the lane that carries it: the
    /// first lane in config order that sets one.
    fn poweroff_command(&self) -> Option<(Arc<LocalExecutor>, String)> {
        self.lanes().into_iter().find_map(|lane| {
            let command = lane.cfg().poweroff_command.clone()?;
            Some((lane, command))
        })
    }

    /// The machine adopts a wake from outside Switchback: one of its lanes
    /// sets `adopt_external_wake`.
    fn adopts(&self) -> bool {
        self.lanes()
            .iter()
            .any(|lane| lane.cfg().adopt_external_wake)
    }

    /// The machine's power-off guard and the lane that carries it: the
    /// first lane in config order that sets one. Its timings and budget are
    /// that lane's.
    fn poweroff_guard_command(&self) -> Option<(Arc<LocalExecutor>, String)> {
        self.lanes().into_iter().find_map(|lane| {
            let command = lane.cfg().poweroff_guard_command.clone()?;
            Some((lane, command))
        })
    }

    /// The state of the guard `command` (`None`: the machine has no guard
    /// now). Answers of another command, or of a guard a reload removed,
    /// are dropped: after an operator fixes a broken guard and reloads, the
    /// report no longer says it is broken, and the fixed guard is asked at
    /// the next idle tick instead of after the old one's back-off.
    fn guard(&self, command: Option<&str>) -> std::sync::MutexGuard<'_, GuardState> {
        let mut guard = self
            .guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.command.as_deref() != command {
            *guard = GuardState {
                command: command.map(str::to_string),
                ..GuardState::default()
            };
        }
        guard
    }

    /// What the guard said, for a lane's report; empty when the machine
    /// has no guard (any more).
    fn guard_view(&self, now: u64) -> GuardView {
        let Some((carrier, command)) = self.poweroff_guard_command() else {
            drop(self.guard(None));
            return GuardView::default();
        };
        let threshold = carrier.cfg().retry_budget.max(1);
        let guard = self.guard(Some(&command));
        GuardView {
            busy: guard.busy,
            busy_since_ms: guard.busy_since_ms,
            busy_for_ms: guard
                .busy_since_ms
                .map_or(0, |since| now.saturating_sub(since)),
            failures: guard.failures,
            last_failure: guard.last_failure.clone(),
            broken: guard.failures >= threshold,
        }
    }

    /// Ask the guard whether the machine is free and record its answer.
    /// `true` only for exit 0. Busy (any other exit) and no answer (timed
    /// out, could not run) both keep the machine up and space the next
    /// question by the carrier's `wake_retry_interval_secs`, the gap every
    /// other repeated power step of a lane already uses.
    async fn guard_says_free(&self, carrier: &LocalExecutor, command: &str) -> bool {
        let answer = carrier.ask_guard(command).await;
        let now = carrier.clock.now_ms();
        let cfg = carrier.cfg();
        let gap_ms = cfg.wake_retry_interval_secs.saturating_mul(1000);
        let threshold = cfg.retry_budget.max(1);
        let mut guard = self.guard(Some(command));
        match answer {
            Ok(0) => {
                if guard.busy > 0 || guard.failures > 0 {
                    tracing::info!(host = %self.id, "power-off guard reports the machine free");
                }
                *guard = GuardState {
                    command: guard.command.take(),
                    ..GuardState::default()
                };
                true
            }
            Ok(code) => {
                guard.busy += 1;
                guard.busy_since_ms.get_or_insert(now);
                guard.failures = 0;
                guard.last_failure = None;
                guard.ask_at_ms = Some(now.saturating_add(gap_ms));
                if guard.busy == 1 {
                    tracing::info!(
                        host = %self.id,
                        exit = code,
                        "power-off postponed: guard reports the machine busy"
                    );
                }
                false
            }
            Err(error) => {
                guard.failures += 1;
                guard.last_failure = Some(error);
                guard.ask_at_ms = Some(now.saturating_add(gap_ms));
                if guard.failures == threshold {
                    tracing::error!(
                        host = %self.id,
                        failures = guard.failures,
                        "power-off guard gave no answer again; it is broken and the machine stays on until it answers"
                    );
                } else {
                    tracing::warn!(
                        host = %self.id,
                        failures = guard.failures,
                        "power-off guard gave no answer; no power-off"
                    );
                }
                false
            }
        }
    }

    /// Wake the machine for a hold: no wake when any of its lanes already
    /// answers (a power toggle would switch a running machine off);
    /// otherwise the lane that carries the wake command wakes it as for a
    /// job, and its idle countdown starts once it is up.
    async fn wake_for_hold(&self) -> (HoldState, String) {
        for lane in self.lanes() {
            if lane.probe_answer().await != ProbeAnswer::Down {
                return (
                    HoldState::Active,
                    format!(
                        "wake not sent: lane `{}` of host `{}` answers",
                        lane.name, self.id
                    ),
                );
            }
        }
        let Some((carrier, _)) = self.wake_command() else {
            return (
                HoldState::WakeFailed,
                format!("host `{}` has no wake_command", self.id),
            );
        };
        match carrier.ensure_ready().await {
            Ok(guard) => {
                drop(guard);
                (
                    HoldState::Active,
                    format!("woken: lane `{}` is healthy", carrier.name),
                )
            }
            Err(error) => (HoldState::WakeFailed, ensure_error_text(&error)),
        }
    }

    fn wake_sent_slot(&self) -> std::sync::MutexGuard<'_, Option<u64>> {
        self.wake_sent
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn wake_sent_ms(&self) -> Option<u64> {
        *self.wake_sent_slot()
    }

    fn note_wake_sent(&self, now: u64) {
        *self.wake_sent_slot() = Some(now);
    }

    fn forget_wake_sent(&self) {
        *self.wake_sent_slot() = None;
    }

    /// A wake ran: every lane on the machine trusts its next healthy answer.
    fn clear_down_unconfirmed(&self) {
        for lane in self.lanes() {
            lane.lock().down_unconfirmed = false;
        }
    }

    /// The machine's idle period when it is due for power-off, else `None`.
    /// Due means: no lane has work in flight or queued, every lane counts
    /// down to idle or is offline, at least one counts down, and every
    /// countdown has passed its own lane's `idle_timeout_secs`. The period
    /// is the latest moment a lane went idle, so with equal timeouts the
    /// timer runs from the moment the LAST lane went idle.
    fn idle_period(
        lanes: &[Arc<LocalExecutor>],
        runtimes: &[std::sync::MutexGuard<'_, LaneRuntime>],
        now: u64,
        quiet_since: Option<u64>,
    ) -> Option<u64> {
        Self::idle_due(lanes, runtimes, quiet_since)
            .filter(|(_, due)| now >= *due)
            .map(|(period, _)| period)
    }

    /// The machine's idle period and the moment its power-off falls due,
    /// or `None` while it has work, is not counting down, or is off (see
    /// [`Self::idle_period`]). Each countdown runs from no earlier than
    /// `quiet_since`, the end of the machine's last hold.
    fn idle_due(
        lanes: &[Arc<LocalExecutor>],
        runtimes: &[std::sync::MutexGuard<'_, LaneRuntime>],
        quiet_since: Option<u64>,
    ) -> Option<(u64, u64)> {
        let mut latest = None;
        let mut due = 0;
        for (lane, rt) in lanes.iter().zip(runtimes) {
            if rt.in_flight > 0 || rt.waiting > 0 {
                return None;
            }
            match (rt.state, rt.idle_since_ms) {
                (LaneState::IdleCountdown, Some(since)) => {
                    let start = since.max(quiet_since.unwrap_or(0));
                    due = due.max(start.saturating_add(lane.idle_timeout_ms()));
                    latest = latest.max(Some(start));
                }
                (LaneState::Offline, _) => {}
                _ => return None,
            }
        }
        latest.map(|period| (period, due))
    }

    /// Milliseconds until the machine's idle power-off, or `None` while it
    /// has work, is held, is off, or has no power-off command. Zero once it
    /// is due (a busy guard or a retry gap may still postpone it).
    fn poweroff_in_ms(&self, holds: Option<&Holds>, now: u64) -> Option<u64> {
        self.poweroff_command()?;
        let held = holds
            .map(|holds| holds.host(&self.id, now))
            .unwrap_or_default();
        if held.held {
            return None;
        }
        let lanes = self.lanes();
        let runtimes: Vec<_> = lanes.iter().map(|lane| lane.lock()).collect();
        let (_, due) = Self::idle_due(&lanes, &runtimes, held.ended_at_ms)?;
        Some(due.saturating_sub(now))
    }

    /// Power the machine off once it is idle (see [`Self::idle_period`]).
    /// Skip-gated when no lane sets a `poweroff_command`: the lanes stay up
    /// and the doctor reports the gap, never a fake power-off. One power-off
    /// for the machine, and every lane on it observes it. A failed (or
    /// timed-out) power-off is retried after `wake_retry_interval_secs`, up
    /// to `retry_budget` times (both read from the lane that carries the
    /// command); after that every lane is `Degraded`, escalated, and the
    /// log says so at error level. With a `poweroff_guard_command`, the
    /// guard is asked right before each attempt and only exit 0 lets it run
    /// (see [`Self::guard_says_free`]).
    async fn tick(&self, holds: Option<&Holds>) {
        let Ok(_tick) = self.tick_lock.try_lock() else {
            return;
        };
        let lanes = self.lanes();
        let Some(first) = lanes.first() else {
            return;
        };
        let clock = first.clock.clone();
        // A hold keeps the machine on; once the last one ends the idle
        // countdown runs from that moment.
        let held = holds
            .map(|holds| holds.host(&self.id, clock.now_ms()))
            .unwrap_or_default();
        if held.held {
            return;
        }
        let period = {
            let runtimes: Vec<_> = lanes.iter().map(|lane| lane.lock()).collect();
            Self::idle_period(&lanes, &runtimes, clock.now_ms(), held.ended_at_ms)
        };
        let Some(period) = period else {
            return;
        };
        {
            let mut power = self.power();
            if power.failed_period != Some(period) {
                *power = HostPower::default();
            }
            if power.retry_at_ms.is_some_and(|at| clock.now_ms() < at) {
                return;
            }
        }
        let guard = self.poweroff_guard_command();
        let command = guard.as_ref().map(|(_, command)| command.as_str());
        if self
            .guard(command)
            .ask_at_ms
            .is_some_and(|at| clock.now_ms() < at)
        {
            return;
        }

        let Some((carrier, poweroff_command)) = self.poweroff_command() else {
            for lane in &lanes {
                let mut rt = lane.lock();
                if rt.state == LaneState::IdleCountdown && !rt.poweroff_unconfigured_reported {
                    rt.poweroff_unconfigured_reported = true;
                    tracing::warn!(lane = %lane.name, "poweroff command unconfigured; staying up");
                }
            }
            return;
        };

        // A machine with a lane that text requests wake holds its wake lock
        // while it powers off: no wake (a request's or a job's) probes or
        // runs its command until the poweroff command has returned, so a
        // host that still answers while it shuts down is not taken for up.
        // A wake already running skips this tick.
        let _wake = if lanes.iter().any(|lane| lane.cfg().wake_on_request) {
            match self.wake_lock.try_lock() {
                Ok(wake) => Some(wake),
                Err(_) => return,
            }
        } else {
            None
        };
        // Work Switchback cannot see (a client that reaches an engine
        // directly) holds the power-off off too: the guard is asked last,
        // under the same locks, and work that arrives while it runs is
        // caught by the re-check below.
        if let Some((guard_lane, guard_command)) = &guard {
            if !self.guard_says_free(guard_lane, guard_command).await {
                return;
            }
        }
        // Re-check under every lane's lock and mark them all powering off at
        // once: new work may have arrived since the read above.
        // A hold taken while the guard ran keeps the machine on too.
        if holds.is_some_and(|holds| holds.host(&self.id, clock.now_ms()).held) {
            return;
        }
        let before: Vec<LaneState> = {
            let mut runtimes: Vec<_> = lanes.iter().map(|lane| lane.lock()).collect();
            if Self::idle_period(&lanes, &runtimes, clock.now_ms(), held.ended_at_ms)
                != Some(period)
            {
                return;
            }
            runtimes
                .iter_mut()
                .map(|rt| std::mem::replace(&mut rt.state, LaneState::PoweringOff))
                .collect()
        };
        tracing::info!(host = %self.id, lanes = lanes.len(), "idle timeout reached; powering off");
        match carrier.run_command("poweroff", &poweroff_command).await {
            Ok(()) => {
                let had_failures = std::mem::take(&mut *self.power()).failures > 0;
                // The power cycle ends: the next lane that needs the machine
                // wakes it again.
                self.forget_wake_sent();
                // The machine is off: after the next boot nobody is known to
                // hold its GPU.
                for state in self.groups().values_mut() {
                    if state.active == 0 && state.switch_to.is_none() {
                        state.holder = None;
                    }
                }
                for lane in &lanes {
                    let mut rt = lane.lock();
                    // The engines restart with the machine and hold no GPU
                    // memory: release commands that failed before say
                    // nothing about the next boot, so each gets its full
                    // budget again (otherwise one bad moment, such as an
                    // engine still starting, stops switches for good).
                    rt.release_failures = 0;
                    rt.release_retry_at_ms = None;
                    // Work that arrived during the poweroff keeps its state;
                    // the next wake brings the machine back for it.
                    if rt.state == LaneState::PoweringOff {
                        rt.state = LaneState::Offline;
                        rt.idle_since_ms = None;
                        rt.down_unconfirmed = true;
                        rt.answered_up = false;
                        rt.poweroff_returned_ms = Some(clock.now_ms());
                        // A clean power-off is a fresh start for the next
                        // boot, as for the release budget above: an error of
                        // the last cycle (a failed job, a failed release or
                        // power-off) is cleared. An escalation still needs an
                        // operator and keeps its error.
                        if had_failures || !rt.escalated {
                            rt.last_error = None;
                        }
                        // A lane that text requests wake: routing must see
                        // it down now (and wake it), not dispatch into a
                        // host that is going away until the next probe. The
                        // ticket makes an older in-flight probe's "up" lose.
                        if lane.is_routed() && lane.cfg().wake_on_request {
                            rt.probe_seq += 1;
                            rt.observed_seq = rt.probe_seq;
                            rt.observed_healthy = Some(false);
                        }
                    }
                }
            }
            Err(error) => {
                let cfg = carrier.cfg();
                let failures = {
                    let mut power = self.power();
                    power.failures += 1;
                    power.failed_period = Some(period);
                    power.failures
                };
                let attempts = cfg.retry_budget.saturating_add(1);
                let retry = failures < attempts;
                let gap_ms = cfg.wake_retry_interval_secs.saturating_mul(1000);
                let message = if retry {
                    self.power().retry_at_ms = Some(clock.now_ms().saturating_add(gap_ms));
                    tracing::warn!(
                        host = %self.id,
                        attempt = failures,
                        attempts,
                        retry_in_ms = gap_ms,
                        "poweroff command failed; will retry"
                    );
                    format!(
                        "poweroff command failed (attempt {failures} of {attempts}): {error}; retrying in {}",
                        fmt_ms(gap_ms)
                    )
                } else {
                    tracing::error!(
                        host = %self.id,
                        failures,
                        "poweroff command failed and its retries ran out; the machine stays on and needs operator attention"
                    );
                    format!(
                        "poweroff command failed {failures} times (last: {error}); the machine stays on until an operator acts"
                    )
                };
                for (lane, before) in lanes.iter().zip(before) {
                    let mut rt = lane.lock();
                    if rt.state == LaneState::PoweringOff {
                        rt.state = if retry { before } else { LaneState::Degraded };
                    }
                    if !retry {
                        rt.escalated = true;
                    }
                    rt.last_error = Some(message.clone());
                }
            }
        }
    }
}

/// Why a wake failed, without the command line.
fn ensure_error_text(error: &EnsureError) -> String {
    match error {
        EnsureError::WakeUnconfigured => "wake command unconfigured".to_string(),
        EnsureError::BootTimeout { elapsed_ms, .. } => format!(
            "wake did not reach health within its boot timeout ({})",
            fmt_ms(*elapsed_ms)
        ),
        EnsureError::WakeFailed(error) => format!("wake command failed: {error}"),
        EnsureError::Refused(detail) => detail.clone(),
    }
}

/// Serde helper: leave a zero count out of the report, so a lane that
/// never failed reports exactly what it did before the count existed.
fn is_zero(count: &u32) -> bool {
    *count == 0
}

/// Short human duration for decisions and errors: `850 ms`, `41 s`.
fn fmt_ms(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms} ms")
    } else {
        format!("{} s", ms.saturating_add(500) / 1_000)
    }
}

/// A span that may last hours, for a report: `40 s`, `12 min`, `3 h 5 min`.
fn fmt_span(ms: u64) -> String {
    let secs = ms / 1_000;
    match secs {
        0..=59 => format!("{secs} s"),
        60..=3_599 => format!("{} min", secs / 60),
        _ if (secs % 3_600) / 60 == 0 => format!("{} h", secs / 3_600),
        _ => format!("{} h {} min", secs / 3_600, (secs % 3_600) / 60),
    }
}

/// One request waiting for capacity, shown as `queue_depth`; released on
/// drop so a request abandoned mid-wait never stays counted.
struct QueuedSlot {
    executor: Arc<LocalExecutor>,
}

impl QueuedSlot {
    fn new(executor: Arc<LocalExecutor>) -> Self {
        executor.lock().waiting += 1;
        Self { executor }
    }
}

impl Drop for QueuedSlot {
    fn drop(&mut self) {
        let mut rt = self.executor.lock();
        rt.waiting = rt.waiting.saturating_sub(1);
    }
}

/// Ends a request-triggered wake on every path (result, panic, runtime
/// shutdown): clears the single-flight flag and wakes the waiters, so a
/// lost task can never leave requests waiting past their bound for nothing.
struct RequestWakeReset {
    executor: Arc<LocalExecutor>,
}

impl Drop for RequestWakeReset {
    fn drop(&mut self) {
        {
            let mut rt = self.executor.lock();
            rt.request_wake_running = false;
            if rt.last_request_wake.is_none() {
                rt.last_request_wake = Some(Err("wake task ended early".to_string()));
                rt.last_request_wake_ended_ms = Some(self.executor.clock.now_ms());
            }
        }
        self.executor
            .changes
            .send_modify(|version| *version = version.wrapping_add(1));
    }
}

/// Holds one job's or request's in-flight slot; releasing it arms the idle
/// countdown when the lane falls idle.
pub struct LaneJobGuard {
    executor: Arc<LocalExecutor>,
    released: bool,
}

impl std::fmt::Debug for LaneJobGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaneJobGuard")
            .field("lane", &self.executor.name)
            .finish()
    }
}

impl LaneJobGuard {
    fn new(executor: Arc<LocalExecutor>) -> Self {
        Self {
            executor,
            released: false,
        }
    }
}

impl Drop for LaneJobGuard {
    fn drop(&mut self) {
        if !self.released {
            self.released = true;
            self.executor.release();
        }
    }
}

// ---------------------------------------------------------------------------
// Activator: the map of managed lanes
// ---------------------------------------------------------------------------

/// Owns every managed local executor lane. Built at startup from config and
/// kept in step with every later publish ([`CapacityActivator::reconcile`]);
/// held as an `Arc` in `AppState` so live state (idle timers, retry counts,
/// in-flight requests) survives across requests and reloads.
pub struct CapacityActivator {
    executors: Arc<RwLock<BTreeMap<String, Arc<LocalExecutor>>>>,
    /// Seams kept for lanes added by a reload.
    clock: Arc<dyn Clock>,
    runner: Arc<dyn CommandRunner>,
    probe: Arc<dyn HealthProbe>,
    /// Lanes a reload removed, as weak handles: one that still has work
    /// running on it (a stream, a job, a wake holds a strong handle) is
    /// taken back, with that work, if a later reload adds it again.
    retired: Mutex<BTreeMap<String, Weak<LocalExecutor>>>,
    /// Machines named by `host:`, kept across reloads so a wake in
    /// progress and a power-off retry keep their state.
    hosts: Mutex<BTreeMap<String, Arc<Host>>>,
    /// Supervisor cadence once [`Self::spawn_supervisor`] asked for one.
    supervisor_interval: OnceLock<Duration>,
    supervisor_started: AtomicBool,
    /// Where lane probes lease their provider's credential; handed to
    /// every lane, including lanes a reload adds.
    credentials: RwLock<Option<Arc<dyn LaneCredentials>>>,
    /// Host holds (`/v1/hosts/{host}/holds`): an active one keeps its
    /// machine on.
    holds: Arc<Holds>,
    /// Lane alerts (`server.alert_command`), checked after every
    /// supervisor pass.
    alerts: Arc<Alerter>,
}

/// The lane is read by routing: it opted in and its provider can be a routed
/// (text) target, not a media-only one.
fn routed_lane(cfg: &Config, executor_cfg: &LocalExecutorConfig) -> bool {
    executor_cfg.route_health
        && cfg
            .providers
            .iter()
            .find(|provider| provider.id == executor_cfg.name)
            .is_some_and(|provider| {
                !matches!(
                    provider.kind,
                    ProviderKind::ComfyUi { .. } | ProviderKind::Fal { .. }
                )
            })
}

impl CapacityActivator {
    /// Build from config with real seams (system clock, shell runner, HTTP
    /// health probe). Lanes come from `config.local_executors`.
    pub fn from_config(cfg: &Config) -> Arc<Self> {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let runner: Arc<dyn CommandRunner> = Arc::new(ShellCommandRunner);
        let probe: Arc<dyn HealthProbe> = Arc::new(HttpHealthProbe::new(Duration::from_secs(3)));
        Self::with_seams(cfg, clock, runner, probe)
    }

    /// Build with explicit seams (tests inject fakes).
    pub fn with_seams(
        cfg: &Config,
        clock: Arc<dyn Clock>,
        runner: Arc<dyn CommandRunner>,
        probe: Arc<dyn HealthProbe>,
    ) -> Arc<Self> {
        Self::with_alert_sink(cfg, clock, runner, probe, Arc::new(CommandAlertSink))
    }

    /// [`Self::with_seams`] with the program that delivers alerts as a
    /// seam too.
    pub fn with_alert_sink(
        cfg: &Config,
        clock: Arc<dyn Clock>,
        runner: Arc<dyn CommandRunner>,
        probe: Arc<dyn HealthProbe>,
        sink: Arc<dyn AlertSink>,
    ) -> Arc<Self> {
        let activator = Self {
            alerts: Arc::new(Alerter::new(clock.clone(), sink)),
            executors: Arc::new(RwLock::new(BTreeMap::new())),
            clock,
            runner,
            probe,
            retired: Mutex::new(BTreeMap::new()),
            hosts: Mutex::new(BTreeMap::new()),
            supervisor_interval: OnceLock::new(),
            supervisor_started: AtomicBool::new(false),
            credentials: RwLock::new(None),
            holds: Arc::new(Holds::new()),
        };
        activator.reconcile(cfg);
        Arc::new(activator)
    }

    fn lanes(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<String, Arc<LocalExecutor>>> {
        self.executors
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Every lane right now, cloned out so no lock is held across an await.
    fn all(&self) -> Vec<Arc<LocalExecutor>> {
        self.lanes().values().cloned().collect()
    }

    /// Bring the lane set in line with a published config. A lane that stays
    /// is updated in place and keeps its live state (in-flight requests and
    /// jobs, queued waiters, a wake in progress). A new lane starts offline,
    /// as at startup. A removed lane leaves the map at once: routing, wakes
    /// and the supervisor no longer see it, so it is never probed, ticked or
    /// powered off again; work already running on it (a stream, a media job,
    /// a wake cycle) holds its own handle and finishes normally. Re-adding
    /// it while such work still runs takes that same lane back, so the work
    /// keeps holding the idle poweroff off; once it has ended, re-adding
    /// starts a fresh lane.
    pub fn reconcile(&self, cfg: &Config) {
        self.alerts.configure(cfg);
        {
            let mut lanes = self
                .executors
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut retired = self
                .retired
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            retired.retain(|_, lane| lane.strong_count() > 0);
            let mut next = BTreeMap::new();
            for executor_cfg in &cfg.local_executors {
                let routed = routed_lane(cfg, executor_cfg);
                let existing = lanes.remove(&executor_cfg.name).or_else(|| {
                    retired
                        .remove(&executor_cfg.name)
                        .and_then(|lane| lane.upgrade())
                });
                let executor = match existing {
                    Some(existing) => {
                        existing.update_config(executor_cfg.clone(), routed);
                        existing
                    }
                    None => LocalExecutor::new(
                        executor_cfg.clone(),
                        self.clock.clone(),
                        self.runner.clone(),
                        self.probe.clone(),
                        routed,
                    ),
                };
                executor
                    .set_probe_auth(probe_auth_for(cfg, executor_cfg), self.credentials_source());
                next.insert(executor_cfg.name.clone(), executor);
            }
            for (name, removed) in lanes.iter() {
                tracing::info!(lane = %name, "local lane removed by config; running work finishes, no further poweroff");
                retired.insert(name.clone(), Arc::downgrade(removed));
            }
            self.assign_hosts(cfg, &next);
            *lanes = next;
        }
        self.start_supervisor_if_due();
    }

    /// Put every lane on its machine. Lanes that name the same `host` share
    /// one (kept across reloads); a lane without `host` keeps, or gets, a
    /// machine of its own. A lane a reload removed while work still runs on
    /// it stays on its shared machine until that work ends, so the work
    /// keeps holding the machine's power-off off.
    fn assign_hosts(&self, cfg: &Config, lanes: &BTreeMap<String, Arc<LocalExecutor>>) {
        let mut hosts = self
            .hosts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut members: BTreeMap<&str, Vec<Arc<LocalExecutor>>> = BTreeMap::new();
        for executor_cfg in &cfg.local_executors {
            let Some(executor) = lanes.get(&executor_cfg.name) else {
                continue;
            };
            match executor_cfg.host.as_deref() {
                Some(host_id) => members.entry(host_id).or_default().push(executor.clone()),
                None if executor.host().shared => executor.set_host(Host::new(
                    &executor.name,
                    false,
                    vec![Arc::downgrade(executor)],
                )),
                None => {}
            }
        }
        let mut kept = BTreeMap::new();
        for (host_id, mut on_host) in members {
            let host = hosts
                .remove(host_id)
                .unwrap_or_else(|| Host::new(host_id, true, Vec::new()));
            for old in host.lanes() {
                let still_listed = on_host.iter().any(|lane| Arc::ptr_eq(lane, &old));
                if !still_listed && !lanes.contains_key(old.name()) {
                    on_host.push(old);
                }
            }
            host.set_members(on_host.iter().map(Arc::downgrade).collect());
            for lane in &on_host {
                lane.set_host(host.clone());
            }
            kept.insert(host_id.to_string(), host);
        }
        *hosts = kept;
    }

    /// Let lane probes carry their provider's credential, leased from
    /// `source` (the server attaches the engine's resolver once it exists).
    pub fn attach_credentials(&self, source: Arc<dyn LaneCredentials>) {
        *self
            .credentials
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(source.clone());
        for lane in self.all() {
            *lane
                .credentials
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(source.clone());
        }
    }

    fn credentials_source(&self) -> Option<Arc<dyn LaneCredentials>> {
        self.credentials
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Lane alerts: delivery state for the report, and the test send.
    pub fn alerts(&self) -> &Arc<Alerter> {
        &self.alerts
    }

    /// Check every lane for alerts and start delivering the due ones in
    /// the background (what the supervisor does after each pass).
    pub fn check_alerts(&self) {
        self.alerts.check(&self.reports());
        self.alerts.kick();
    }

    /// The host holds and whether they survive a restart.
    pub fn holds(&self) -> &Arc<Holds> {
        &self.holds
    }

    /// Keep host holds in `dir` (they survive a restart); see
    /// [`Holds::attach_dir`].
    pub fn keep_holds_in(&self, dir: &std::path::Path) {
        self.holds.attach_dir(dir, self.clock.now_ms());
    }

    /// The activator's clock (ms).
    pub fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    /// The machine named `id`: a `host:` of the config, or the name of a
    /// lane with a machine of its own.
    fn host_by_id(&self, id: &str) -> Option<Arc<Host>> {
        self.all()
            .into_iter()
            .map(|lane| lane.host())
            .find(|host| host.id == id)
    }

    /// Take a hold on machine `host` for `ttl_ms` (see [`Holds::create`]).
    /// `Ok(None)`: no such machine. With `wake`, the machine is woken in the
    /// background with its lanes' wake command, under the same rules as any
    /// wake (none while another lane of a shared machine answers); the
    /// hold's `state` says how it went.
    pub fn take_hold(
        &self,
        host: &str,
        reason: &str,
        ttl_ms: u64,
        owner: (Option<String>, Option<String>),
        wake: bool,
    ) -> Result<Option<Hold>, HoldError> {
        let Some(machine) = self.host_by_id(host) else {
            return Ok(None);
        };
        let now = self.clock.now_ms();
        let state = if wake {
            HoldState::Waking
        } else {
            HoldState::Active
        };
        let hold = self
            .holds
            .create(host, reason, ttl_ms, owner.0, owner.1, state, now)?;
        if wake {
            let holds = self.holds.clone();
            let clock = self.clock.clone();
            let id = hold.id.clone();
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    handle.spawn(async move {
                        let (state, detail) = machine.wake_for_hold().await;
                        holds.set_state(&id, state, Some(detail), clock.now_ms());
                    });
                }
                Err(_) => self.holds.set_state(
                    &id,
                    HoldState::WakeFailed,
                    Some("no async runtime to run the wake".to_string()),
                    now,
                ),
            }
        }
        Ok(Some(hold))
    }

    /// `true` when `host` names a managed machine.
    pub fn has_host(&self, host: &str) -> bool {
        self.host_by_id(host).is_some()
    }

    /// The managed lane for a provider id, if one is configured.
    pub fn executor(&self, name: &str) -> Option<Arc<LocalExecutor>> {
        self.lanes().get(name).cloned()
    }

    pub fn is_empty(&self) -> bool {
        self.lanes().is_empty()
    }

    /// Metadata-only report of every managed lane (for the doctor / live
    /// capacity surface).
    pub fn reports(&self) -> Vec<LocalLaneReport> {
        self.all()
            .iter()
            .map(|executor| executor.report_with(Some(&self.holds)))
            .collect()
    }

    /// One power-off pass: each machine's tick once (see [`Host::tick`]).
    pub async fn tick_all(&self) {
        tick_hosts(&self.all(), &self.holds).await;
    }

    /// Probe every routed lane once and record what was seen (see
    /// [`LocalExecutor::observe`]). A lane without `route_health: true`, and
    /// a media-only lane, is not probed.
    pub async fn observe_all(&self) {
        observe_lanes(&self.all()).await;
    }

    /// Ask for a background supervisor that periodically probes each routed
    /// lane (so text routing knows which lanes are down) and ticks every lane
    /// (so idle machines power off autonomously). It starts once there is at
    /// least one managed lane, including one added later by a reload, and
    /// each pass works on the lanes configured at that moment.
    pub fn spawn_supervisor(self: &Arc<Self>, interval: Duration) {
        let _ = self.supervisor_interval.set(interval);
        self.start_supervisor_if_due();
    }

    fn start_supervisor_if_due(&self) {
        let Some(interval) = self.supervisor_interval.get().copied() else {
            return;
        };
        if self.is_empty() || self.supervisor_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.supervisor_started.store(false, Ordering::SeqCst);
            return;
        };
        // The task shares the lane map, so each pass sees the lanes a
        // reload left, not the ones that existed when it started.
        let lanes = self.executors.clone();
        let holds = self.holds.clone();
        let alerts = self.alerts.clone();
        handle.spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let pass: Vec<Arc<LocalExecutor>> = lanes
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .values()
                    .cloned()
                    .collect();
                observe_lanes(&pass).await;
                tick_hosts(&pass, &holds).await;
                // Alerts last: delivery runs in its own task, so a hanging
                // alert command never delays the next probe or power-off.
                let reports: Vec<LocalLaneReport> = pass
                    .iter()
                    .map(|lane| lane.report_with(Some(&holds)))
                    .collect();
                alerts.check(&reports);
                alerts.kick();
            }
        });
    }
}

/// One probe pass of the supervisor. Probed: every lane routing reads
/// (`route_health`), every lane of a machine that adopts outside wakes
/// (`adopt_external_wake`), and then any other lane of a shared machine
/// where one lane was just seen up, so the report shows which engines run.
/// Any other lane (the default media lane, a lane without opt-in) is never
/// probed by the supervisor, as before.
async fn observe_lanes(lanes: &[Arc<LocalExecutor>]) {
    let probed = |lane: &Arc<LocalExecutor>| lane.is_routed() || lane.host().adopts();
    for lane in lanes.iter().filter(|lane| probed(lane)) {
        lane.observe().await;
    }
    for lane in lanes.iter().filter(|lane| !probed(lane)) {
        let host = lane.host();
        let sibling_up = host.shared
            && host
                .lanes()
                .iter()
                .any(|other| other.name != lane.name && other.lock().answered_up);
        if sibling_up {
            lane.observe().await;
        }
    }
}

/// Tick each machine under these lanes once, in lane order.
async fn tick_hosts(lanes: &[Arc<LocalExecutor>], holds: &Holds) {
    let mut seen: Vec<Arc<Host>> = Vec::new();
    for lane in lanes {
        let host = lane.host();
        if seen.iter().any(|done| Arc::ptr_eq(done, &host)) {
            continue;
        }
        host.tick(Some(holds)).await;
        seen.push(host);
    }
}

/// Text routing reads managed-lane state from here: a lane is known down only
/// when it opted in (`route_health: true`) and its most recent health probe
/// failed. Not opted in, not probed yet, or up, is not down — the router then
/// behaves exactly as without managed lanes. Reporting
/// a lane down also schedules a background refresh of that evidence (see
/// [`LocalExecutor::schedule_refresh`]); the answer itself never waits on it.
impl sb_runtime::LaneStatus for CapacityActivator {
    fn known_down(&self, provider_id: &str) -> Option<String> {
        let executor = self.executor(provider_id)?;
        let mut rt = executor.lock();
        // A lane that text requests wake is down while its poweroff command
        // runs, whatever the last probe saw: a request wakes it again rather
        // than being dispatched into a host that is going away.
        if rt.state == LaneState::PoweringOff
            && executor.is_routed()
            && executor.cfg().wake_on_request
        {
            return Some("powering off".to_string());
        }
        if rt.observed_healthy != Some(false) {
            return None;
        }
        let refused = rt.refused;
        executor.schedule_refresh(&mut rt);
        Some(match refused {
            Some(status) => {
                format!("last health probe was refused ({status}): credential missing or wrong")
            }
            None => "last health probe failed".to_string(),
        })
    }

    /// Opt-in lanes only (`wake_on_request: true`); bounded by the lane's
    /// `wake_wait_secs` on the [`Clock`] seam.
    fn wake_for_request<'a>(&'a self, provider_id: &'a str) -> sb_runtime::BoxFuture<'a, LaneWake> {
        let executor = self.executor(provider_id);
        Box::pin(async move {
            match executor {
                Some(executor) if executor.cfg().wake_on_request => {
                    executor.wake_for_request().await
                }
                _ => LaneWake::NotReady {
                    detail: "is not a lane that wakes on request".to_string(),
                    retry_after: Duration::from_secs(1),
                },
            }
        })
    }

    fn begin_request(&self, provider_id: &str) -> Option<LaneBusy> {
        self.try_begin_request(provider_id).ok().flatten()
    }

    /// A lane in an exclusive GPU group counts the request only while it
    /// holds its group (no waiting here); any other opted-in lane always. A
    /// lane on a shared host counts it even without `wake_on_request`: the
    /// machine's power-off, which another lane's idle timer may ask for,
    /// must wait for it.
    fn try_begin_request(&self, provider_id: &str) -> Result<Option<LaneBusy>, String> {
        let Some(executor) = self.executor(provider_id) else {
            return Ok(None);
        };
        let cfg = executor.cfg();
        if cfg.exclusive_group.is_some() {
            let permit = executor.try_take_group()?;
            return Ok(Some(Box::new((executor.begin_request(), permit))));
        }
        if !cfg.wake_on_request && cfg.host.is_none() {
            return Ok(None);
        }
        Ok(Some(Box::new(executor.begin_request())))
    }

    /// Bounded by the lane's `group_wait_secs` on the [`Clock`] seam.
    fn acquire_for_request<'a>(
        &'a self,
        provider_id: &'a str,
    ) -> sb_runtime::BoxFuture<'a, LaneAcquire> {
        let executor = self.executor(provider_id);
        Box::pin(async move {
            match executor {
                Some(executor) if executor.cfg().exclusive_group.is_some() => {
                    executor.acquire_for_request().await
                }
                _ => match self.try_begin_request(provider_id) {
                    Ok(busy) => LaneAcquire::Ready { busy, detail: None },
                    Err(detail) => LaneAcquire::NotReady {
                        detail,
                        retry_after: LANE_GROUP_RETRY_AFTER,
                    },
                },
            }
        })
    }

    fn config_published(&self, config: &Config) {
        self.reconcile(config);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    // --- Fakes -------------------------------------------------------------

    struct FakeClock {
        now: AtomicU64,
    }

    impl FakeClock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                now: AtomicU64::new(0),
            })
        }
        fn advance(&self, ms: u64) {
            self.now.fetch_add(ms, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.now.load(Ordering::SeqCst)
        }
        async fn sleep_ms(&self, ms: u64) {
            // Advance deterministically and return: boot timeouts and idle
            // countdowns are driven by the test, not by wall time.
            self.now.fetch_add(ms, Ordering::SeqCst);
        }
    }

    #[derive(Default)]
    struct Sim {
        healthy: bool,
        wake: u32,
        poweroff: u32,
        restart: u32,
        wake_makes_healthy: bool,
        /// Health only comes up once this many wake sends have landed (models a
        /// lossy Wake-on-LAN transport where the first packets are dropped).
        healthy_after_wakes: Option<u32>,
        poweroff_makes_unhealthy: bool,
        restart_makes_healthy: bool,
        fail_wake: bool,
        fail_poweroff: bool,
        fail_release: bool,
        /// Every release command run, in order.
        releases: Vec<String>,
        /// What the power-off guard answers, and how often it was asked.
        guard: GuardAnswer,
        guard_runs: u32,
        /// An engine still starting on a machine that is up: its health
        /// endpoint fails this many more probes (counted only while the
        /// machine is up). A restart that heals clears it.
        fail_probes: BTreeMap<String, u32>,
        /// While set, a release command runs until the gate gets a permit.
        release_gate: Option<Arc<tokio::sync::Semaphore>>,
    }

    #[derive(Default, Clone, Copy)]
    enum GuardAnswer {
        #[default]
        Free,
        Busy,
        /// The command cannot run (a missing script).
        Broken,
    }

    const GUARD: &str = "ssh gateway check-idle executor-1";

    #[derive(Clone)]
    struct FakeHost {
        sim: Arc<Mutex<Sim>>,
    }

    impl FakeHost {
        fn new(sim: Sim) -> Self {
            Self {
                sim: Arc::new(Mutex::new(sim)),
            }
        }
        fn sim(&self) -> std::sync::MutexGuard<'_, Sim> {
            self.sim.lock().unwrap()
        }
    }

    #[async_trait]
    impl CommandRunner for FakeHost {
        async fn exit_code(&self, command: &str) -> Result<i32, String> {
            if command.contains("check-idle") {
                let mut sim = self.sim.lock().unwrap();
                sim.guard_runs += 1;
                return match sim.guard {
                    GuardAnswer::Free => Ok(0),
                    GuardAnswer::Busy => Ok(1),
                    GuardAnswer::Broken => Err("exit 127: the shell could not run it".to_string()),
                };
            }
            self.run(command).await.map(|()| 0)
        }

        async fn run(&self, command: &str) -> Result<(), String> {
            if command.contains("release") {
                let gate = self.sim.lock().unwrap().release_gate.clone();
                if let Some(gate) = gate {
                    gate.acquire().await.unwrap().forget();
                }
            }
            let mut sim = self.sim.lock().unwrap();
            if command.contains("release") {
                sim.releases.push(command.to_string());
                if sim.fail_release {
                    return Err("exit 1".to_string());
                }
                return Ok(());
            }
            if command.contains("restart") {
                sim.restart += 1;
                if sim.restart_makes_healthy {
                    sim.healthy = true;
                    sim.fail_probes.clear();
                }
            } else if command.contains("poweroff") {
                sim.poweroff += 1;
                if sim.fail_poweroff {
                    return Err("exit 255".to_string());
                }
                if sim.poweroff_makes_unhealthy {
                    sim.healthy = false;
                }
            } else if command.contains("wake") {
                sim.wake += 1;
                if sim.fail_wake {
                    return Err("wake failed".to_string());
                }
                if sim.wake_makes_healthy {
                    sim.healthy = true;
                }
                if let Some(threshold) = sim.healthy_after_wakes {
                    if sim.wake >= threshold {
                        sim.healthy = true;
                    }
                }
            } else {
                return Err(format!("unexpected command: {command}"));
            }
            Ok(())
        }
    }

    #[async_trait]
    impl HealthProbe for FakeHost {
        async fn healthy(&self, endpoint: &str) -> bool {
            let mut sim = self.sim.lock().unwrap();
            if !sim.healthy {
                return false;
            }
            match sim.fail_probes.get_mut(endpoint) {
                Some(left) if *left > 0 => {
                    *left -= 1;
                    false
                }
                _ => true,
            }
        }
    }

    /// An activator over one already-built lane (fake seams for lanes a
    /// reload would add).
    fn activator_of(executor: &Arc<LocalExecutor>) -> CapacityActivator {
        let host = FakeHost::new(Sim::default());
        CapacityActivator {
            executors: Arc::new(RwLock::new(BTreeMap::from([(
                executor.name.clone(),
                executor.clone(),
            )]))),
            clock: FakeClock::new(),
            runner: Arc::new(host.clone()),
            probe: Arc::new(host),
            retired: Mutex::new(BTreeMap::new()),
            hosts: Mutex::new(BTreeMap::new()),
            supervisor_interval: OnceLock::new(),
            supervisor_started: AtomicBool::new(false),
            credentials: RwLock::new(None),
            holds: Arc::new(Holds::new()),
            alerts: Arc::new(Alerter::new(FakeClock::new(), Arc::new(CommandAlertSink))),
        }
    }

    // --- Config builder ----------------------------------------------------

    struct Legs {
        wake: Option<&'static str>,
        poweroff: Option<&'static str>,
        restart: Option<&'static str>,
        boot_timeout_secs: u64,
        wake_retry_interval_secs: u64,
        idle_timeout_secs: u64,
        retry_budget: u32,
        health_poll_interval_ms: u64,
        guard: Option<&'static str>,
    }

    impl Default for Legs {
        fn default() -> Self {
            Self {
                wake: Some("ssh gateway wake-host executor-1"),
                poweroff: Some("ssh gateway poweroff-host executor-1"),
                restart: Some("ssh gateway restart-host executor-1"),
                boot_timeout_secs: 10,
                wake_retry_interval_secs: 30,
                idle_timeout_secs: 5,
                retry_budget: 2,
                health_poll_interval_ms: 400,
                guard: None,
            }
        }
    }

    fn build(host: &FakeHost, clock: Arc<FakeClock>, legs: Legs) -> Arc<LocalExecutor> {
        let cfg = LocalExecutorConfig {
            name: "comfy-local".to_string(),
            base_url: "http://executor-1.example:8188".to_string(),
            health_endpoint: "http://executor-1.example:8188/system_stats".to_string(),
            wake_command: legs.wake.map(str::to_string),
            poweroff_command: legs.poweroff.map(str::to_string),
            restart_command: legs.restart.map(str::to_string),
            boot_timeout_secs: legs.boot_timeout_secs,
            wake_retry_interval_secs: legs.wake_retry_interval_secs,
            idle_timeout_secs: legs.idle_timeout_secs,
            retry_budget: legs.retry_budget,
            health_poll_interval_ms: legs.health_poll_interval_ms,
            route_health: true,
            wake_on_request: false,
            wake_wait_secs: 90,
            command_timeout_secs: 60,
            host: None,
            exclusive_group: None,
            release_command: None,
            group_wait_secs: 120,
            poweroff_guard_command: legs.guard.map(str::to_string),
            adopt_external_wake: false,
        };
        let runner: Arc<dyn CommandRunner> = Arc::new(host.clone());
        let probe: Arc<dyn HealthProbe> = Arc::new(host.clone());
        let clock: Arc<dyn Clock> = clock;
        LocalExecutor::new(cfg, clock, runner, probe, true)
    }

    // --- Tests -------------------------------------------------------------

    // Routing view: a probe records health for text routing, and only a
    // probe that FAILED makes a lane known-down. The lifecycle never moves.
    #[tokio::test]
    async fn observe_records_health_for_routing_without_moving_the_lifecycle() {
        use sb_runtime::LaneStatus;
        let host = FakeHost::new(Sim {
            healthy: false,
            ..Default::default()
        });
        let executor = build(&host, FakeClock::new(), Legs::default());
        let activator = activator_of(&executor);

        assert_eq!(
            activator.known_down("comfy-local"),
            None,
            "never probed is not down"
        );
        activator.observe_all().await;
        assert!(activator.known_down("comfy-local").is_some());
        let report = executor.report();
        assert_eq!(report.observed_healthy, Some(false));
        assert_eq!(report.state, LaneState::Offline, "observe never wakes");
        assert_eq!(host.sim().wake, 0);

        host.sim().healthy = true;
        activator.observe_all().await;
        assert_eq!(activator.known_down("comfy-local"), None);
        // The engine answers: the report reads healthy, but the lifecycle
        // has not moved (no wake of ours, no idle countdown).
        assert_eq!(executor.report().state, LaneState::Healthy);
        assert_eq!(executor.lock().state, LaneState::Offline);
        assert_eq!(activator.known_down("not-a-lane"), None);
    }

    /// Let spawned background probes run (current-thread test runtime).
    async fn settle() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }

    // Stale window: a lane that came back after its last failed probe is
    // re-probed in the background as soon as routing skips it (rate-limited),
    // instead of staying "down" until the next supervisor tick.
    #[tokio::test]
    async fn skipping_a_down_lane_refreshes_its_evidence_in_the_background() {
        use sb_runtime::LaneStatus;
        let host = FakeHost::new(Sim::default());
        let clock = FakeClock::new();
        let executor = build(&host, clock.clone(), Legs::default());
        let activator = activator_of(&executor);
        activator.observe_all().await;
        host.sim().healthy = true; // the lane comes back right after the probe

        // Within the minimum gap no refresh starts: rejected traffic cannot
        // turn into a probe flood.
        assert!(activator.known_down("comfy-local").is_some());
        settle().await;
        assert!(activator.known_down("comfy-local").is_some());

        // Once the evidence is older than the gap, the skip itself answers
        // from the old evidence but starts one refresh probe.
        clock.advance(REFRESH_MIN_INTERVAL_MS);
        assert!(
            activator.known_down("comfy-local").is_some(),
            "the request that triggers the refresh never waits for it"
        );
        settle().await;
        assert_eq!(activator.known_down("comfy-local"), None, "fresh evidence");
        let report = executor.report();
        assert_eq!(report.observed_healthy, Some(true));
        assert_eq!(
            executor.lock().state,
            LaneState::Offline,
            "a refresh never wakes or moves the lifecycle"
        );
        assert_eq!(report.state, LaneState::Healthy, "the engine answers");
        assert_eq!(host.sim().wake, 0);
    }

    /// A probe whose answers are scripted; an answer can be held back until
    /// the test releases it.
    struct ScriptedProbe {
        answers: Mutex<std::collections::VecDeque<(bool, Option<Arc<tokio::sync::Notify>>)>>,
    }

    #[async_trait]
    impl HealthProbe for ScriptedProbe {
        async fn healthy(&self, _endpoint: &str) -> bool {
            let (healthy, gate) = self
                .answers
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted answer");
            if let Some(gate) = gate {
                gate.notified().await;
            }
            healthy
        }
    }

    // Overlapping probes: a slow probe that started while the lane was down
    // must not overwrite the newer "up" a later probe already recorded.
    #[tokio::test]
    async fn a_slow_stale_probe_never_overwrites_newer_evidence() {
        use sb_runtime::LaneStatus;
        let gate = Arc::new(tokio::sync::Notify::new());
        let probe = Arc::new(ScriptedProbe {
            answers: Mutex::new(std::collections::VecDeque::from([
                (false, Some(gate.clone())),
                (true, None),
            ])),
        });
        let host = FakeHost::new(Sim::default());
        let cfg = LocalExecutorConfig {
            name: "lane".to_string(),
            base_url: "http://executor-1.example:8080".to_string(),
            health_endpoint: "http://executor-1.example:8080/health".to_string(),
            wake_command: None,
            poweroff_command: None,
            restart_command: None,
            boot_timeout_secs: 10,
            wake_retry_interval_secs: 30,
            idle_timeout_secs: 5,
            retry_budget: 2,
            health_poll_interval_ms: 400,
            route_health: true,
            wake_on_request: false,
            wake_wait_secs: 90,
            command_timeout_secs: 60,
            host: None,
            exclusive_group: None,
            release_command: None,
            group_wait_secs: 120,
            poweroff_guard_command: None,
            adopt_external_wake: false,
        };
        let executor = LocalExecutor::new(cfg, FakeClock::new(), Arc::new(host), probe, true);
        let activator = activator_of(&executor);

        let slow = tokio::spawn({
            let executor = executor.clone();
            async move { executor.observe().await }
        });
        settle().await; // the slow probe has started and is held
        executor.observe().await; // a later probe sees the lane up
        assert_eq!(activator.known_down("lane"), None);
        gate.notify_one();
        slow.await.unwrap();
        assert_eq!(
            activator.known_down("lane"),
            None,
            "the older probe's `down` arrived last but is not recorded"
        );
        assert_eq!(executor.report().observed_healthy, Some(true));
    }

    /// Records every endpoint it is asked to probe.
    #[derive(Default)]
    struct RecordingProbe {
        seen: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl HealthProbe for RecordingProbe {
        async fn healthy(&self, endpoint: &str) -> bool {
            self.seen.lock().unwrap().push(endpoint.to_string());
            false
        }
    }

    // A media-only lane (powered off on purpose between jobs) gets no
    // periodic routing probe even with `route_health: true`; only a lane
    // whose provider can be a routed target is probed.
    #[tokio::test]
    async fn the_supervisor_probes_only_lanes_that_routing_reads() {
        let cfg = Config::from_yaml(
            r#"
providers:
  - id: comfy-local
    type: comfyui
    base_url: "http://executor-1.example:8188"
  - id: lane
    type: openai_compatible
    base_url: "http://executor-1.example:8080/v1"
local_executors:
  - name: comfy-local
    base_url: "http://executor-1.example:8188"
    health_endpoint: "http://executor-1.example:8188/system_stats"
    route_health: true
  - name: lane
    base_url: "http://executor-1.example:8080"
    health_endpoint: "http://executor-1.example:8080/health"
    route_health: true
"#,
        )
        .unwrap();
        let probe = Arc::new(RecordingProbe::default());
        let host = FakeHost::new(Sim::default());
        let activator =
            CapacityActivator::with_seams(&cfg, FakeClock::new(), Arc::new(host), probe.clone());
        activator.spawn_supervisor(Duration::from_millis(5));
        tokio::time::sleep(Duration::from_millis(100)).await;

        let seen = probe.seen.lock().unwrap().clone();
        assert!(
            seen.iter()
                .any(|endpoint| endpoint == "http://executor-1.example:8080/health"),
            "{seen:?}"
        );
        assert!(
            !seen
                .iter()
                .any(|endpoint| endpoint.ends_with("/system_stats")),
            "a media-only lane is never probed by the supervisor: {seen:?}"
        );
    }

    /// A text lane that opted in (`lane`) and one that did not (`quiet`).
    fn opted_in_and_default_lanes() -> Config {
        Config::from_yaml(
            r#"
providers:
  - id: lane
    type: openai_compatible
    base_url: "http://executor-1.example:8080/v1"
  - id: quiet
    type: openai_compatible
    base_url: "http://executor-2.example:8080/v1"
local_executors:
  - name: lane
    base_url: "http://executor-1.example:8080"
    health_endpoint: "http://executor-1.example:8080/health"
    route_health: true
  - name: quiet
    base_url: "http://executor-2.example:8080"
    health_endpoint: "http://executor-2.example:8080/health"
"#,
        )
        .unwrap()
    }

    // Opt-in: a text lane without `route_health` gets no health GET from the
    // supervisor, so a host that wakes on ordinary traffic can still power
    // off. The opted-in lane on the same supervisor is probed every pass.
    #[tokio::test]
    async fn the_supervisor_never_probes_a_lane_without_route_health() {
        let probe = Arc::new(RecordingProbe::default());
        let host = FakeHost::new(Sim::default());
        let activator = CapacityActivator::with_seams(
            &opted_in_and_default_lanes(),
            FakeClock::new(),
            Arc::new(host),
            probe.clone(),
        );
        activator.spawn_supervisor(Duration::from_millis(5));
        tokio::time::sleep(Duration::from_millis(100)).await;
        activator.observe_all().await;

        let seen = probe.seen.lock().unwrap().clone();
        let count = |endpoint: &str| seen.iter().filter(|e| *e == endpoint).count();
        assert!(
            count("http://executor-1.example:8080/health") >= 2,
            "the supervisor ran passes: {seen:?}"
        );
        assert_eq!(
            count("http://executor-2.example:8080/health"),
            0,
            "no health GET to a lane without route_health: {seen:?}"
        );
    }

    // Opt-in: a lane without `route_health` is never in the routing view,
    // even after a probe (here a job's wake path) saw it down, and the
    // capacity report carries no `observed_healthy` for it.
    #[tokio::test]
    async fn a_lane_without_route_health_is_never_known_down() {
        use sb_runtime::LaneStatus;
        let probe = Arc::new(RecordingProbe::default());
        let host = FakeHost::new(Sim::default());
        let activator = CapacityActivator::with_seams(
            &opted_in_and_default_lanes(),
            FakeClock::new(),
            Arc::new(host),
            probe.clone(),
        );
        assert_eq!(activator.known_down("quiet"), None, "never probed");

        let quiet = activator.executor("quiet").unwrap();
        assert!(
            quiet.ensure_ready().await.is_err(),
            "down and no wake command"
        );
        assert_eq!(
            probe.seen.lock().unwrap().as_slice(),
            ["http://executor-2.example:8080/health"],
            "the job's own probe ran and failed"
        );
        assert_eq!(activator.known_down("quiet"), None, "last probe failed");
        assert_eq!(quiet.report().observed_healthy, None);
        let report = serde_json::to_value(quiet.report()).unwrap();
        assert!(report.get("observed_healthy").is_none(), "{report}");

        // The opted-in lane on the same activator is read by routing.
        activator.observe_all().await;
        assert!(activator.known_down("lane").is_some());
        assert_eq!(activator.known_down("quiet"), None);
    }

    // (a) queue-while-offline -> wake -> healthy -> drain
    #[tokio::test]
    async fn offline_job_wakes_then_drains() {
        let host = FakeHost::new(Sim {
            healthy: false,
            wake_makes_healthy: true,
            ..Default::default()
        });
        let executor = build(&host, FakeClock::new(), Legs::default());

        let guard = executor.ensure_ready().await.expect("capacity");
        assert_eq!(host.sim().wake, 1, "wake ran exactly once");
        let report = executor.report();
        assert_eq!(report.state, LaneState::Draining);
        assert_eq!(report.in_flight, 1);
        assert_eq!(report.wake_attempts, 1);
        assert!(report
            .last_wake_result
            .as_deref()
            .is_some_and(|r| r.starts_with("ok")));

        drop(guard);
        let report = executor.report();
        assert_eq!(report.state, LaneState::IdleCountdown);
        assert_eq!(report.in_flight, 0);
        assert_eq!(report.level, LaneLevel::Ok);
    }

    // (b) single-flight wake under concurrent submits
    #[tokio::test]
    async fn concurrent_submits_trigger_one_wake() {
        let host = FakeHost::new(Sim {
            healthy: false,
            wake_makes_healthy: true,
            ..Default::default()
        });
        let executor = build(&host, FakeClock::new(), Legs::default());

        let futures = (0..5).map(|_| executor.ensure_ready());
        let guards: Vec<_> = futures::future::join_all(futures)
            .await
            .into_iter()
            .map(|r| r.expect("capacity"))
            .collect();

        assert_eq!(
            host.sim().wake,
            1,
            "single-flight: one wake for five submits"
        );
        assert_eq!(executor.report().in_flight, 5);
        drop(guards);
        assert_eq!(executor.report().in_flight, 0);
    }

    // (a2) lossy wake: re-fire on interval within one waking cycle until the
    // Nth send lands and health comes up. Single-flight cycle, N sends inside.
    #[tokio::test]
    async fn lossy_wake_refires_until_healthy() {
        let host = FakeHost::new(Sim {
            healthy: false,
            healthy_after_wakes: Some(3), // first two sends are "lost"
            ..Default::default()
        });
        let legs = Legs {
            boot_timeout_secs: 100,
            wake_retry_interval_secs: 5,
            health_poll_interval_ms: 5_000, // one poll == one retry interval
            ..Default::default()
        };
        let executor = build(&host, FakeClock::new(), legs);

        let guard = executor
            .ensure_ready()
            .await
            .expect("capacity after retries");
        assert_eq!(
            host.sim().wake,
            3,
            "wake re-fired until the Nth send landed"
        );
        let report = executor.report();
        assert_eq!(report.wake_attempts, 3);
        assert_eq!(report.state, LaneState::Draining);
        drop(guard);
    }

    // (c) boot timeout -> loud failure naming the wake path + elapsed
    #[tokio::test]
    async fn boot_timeout_fails_loud() {
        let host = FakeHost::new(Sim {
            healthy: false,
            wake_makes_healthy: false, // wake never brings health up
            ..Default::default()
        });
        let legs = Legs {
            boot_timeout_secs: 1,
            ..Default::default()
        };
        let executor = build(&host, FakeClock::new(), legs);

        let error = executor.ensure_ready().await.expect_err("must time out");
        match error {
            EnsureError::BootTimeout {
                wake_path,
                elapsed_ms,
            } => {
                assert!(wake_path.contains("wake-host"));
                assert!(elapsed_ms >= 1000, "elapsed {elapsed_ms} >= boot timeout");
            }
            other => panic!("expected BootTimeout, got {other:?}"),
        }
        assert_eq!(host.sim().wake, 1);
        let report = executor.report();
        assert_eq!(report.state, LaneState::Offline);
        assert_eq!(report.level, LaneLevel::Fail);
        assert!(report.escalated);
        assert_eq!(report.last_wake_result.as_deref(), Some("timeout"));
    }

    // (d) crash mid-job -> restart -> requeue -> success
    #[tokio::test]
    async fn crash_mid_job_heals_and_succeeds() {
        let host = FakeHost::new(Sim {
            healthy: false,
            wake_makes_healthy: true,
            restart_makes_healthy: true,
            ..Default::default()
        });
        let executor = build(&host, FakeClock::new(), Legs::default());

        let sim = host.sim.clone();
        let result: Result<i32, JobError<&str>> = executor
            .run_job(move |attempt| {
                let sim = sim.clone();
                async move {
                    if attempt == 0 {
                        sim.lock().unwrap().healthy = false; // service wedged
                        Err("wedged")
                    } else {
                        Ok(42)
                    }
                }
            })
            .await;

        assert_eq!(result.expect("job succeeds after heal"), 42);
        assert_eq!(host.sim().restart, 1, "restart ran once");
        assert_eq!(host.sim().wake, 1);
        let report = executor.report();
        assert_eq!(report.retries_used, 1);
        assert_eq!(report.in_flight, 0);
    }

    // (e) retry-budget exhaustion -> loud failure + doctor escalation
    #[tokio::test]
    async fn retry_budget_exhaustion_fails_loud_and_escalates() {
        let host = FakeHost::new(Sim {
            healthy: false,
            wake_makes_healthy: true,
            restart_makes_healthy: true, // heal always restores health...
            ..Default::default()
        });
        let executor = build(&host, FakeClock::new(), Legs::default()); // budget 2

        let sim = host.sim.clone();
        let result: Result<i32, JobError<&str>> = executor
            .run_job(move |_attempt| {
                let sim = sim.clone();
                async move {
                    sim.lock().unwrap().healthy = false; // ...but every dispatch re-wedges
                    Err("always wedges")
                }
            })
            .await;

        match result {
            Err(JobError::RetriesExhausted { budget, .. }) => assert_eq!(budget, 2),
            other => panic!("expected RetriesExhausted, got {other:?}"),
        }
        assert_eq!(host.sim().restart, 2, "healed budget times");
        let report = executor.report();
        assert_eq!(report.retries_used, 2);
        assert!(report.escalated);
        assert_eq!(report.level, LaneLevel::Fail);
        assert!(report.last_error.is_some());
    }

    // (f) idle expiry -> poweroff
    #[tokio::test]
    async fn idle_expiry_powers_off() {
        let host = FakeHost::new(Sim {
            healthy: false,
            wake_makes_healthy: true,
            poweroff_makes_unhealthy: true,
            ..Default::default()
        });
        let clock = FakeClock::new();
        let executor = build(&host, clock.clone(), Legs::default()); // idle 5s

        let guard = executor.ensure_ready().await.expect("capacity");
        drop(guard); // idle countdown armed at now=0
        assert_eq!(executor.report().state, LaneState::IdleCountdown);

        clock.advance(5_000);
        executor.tick().await;

        assert_eq!(host.sim().poweroff, 1);
        assert_eq!(executor.report().state, LaneState::Offline);
    }

    // (g) a new job cancels the idle countdown
    #[tokio::test]
    async fn new_job_cancels_idle_countdown() {
        let host = FakeHost::new(Sim {
            healthy: false,
            wake_makes_healthy: true,
            poweroff_makes_unhealthy: true,
            ..Default::default()
        });
        let clock = FakeClock::new();
        let executor = build(&host, clock.clone(), Legs::default()); // idle 5s

        let guard = executor.ensure_ready().await.expect("capacity");
        drop(guard); // idle countdown armed at now=0

        clock.advance(3_000); // still within idle window
        let guard2 = executor.ensure_ready().await.expect("capacity");
        assert_eq!(executor.report().state, LaneState::Draining);

        clock.advance(5_000); // past the original expiry
        executor.tick().await;
        assert_eq!(host.sim().poweroff, 0, "new job cancelled the countdown");

        drop(guard2);
        executor.tick().await; // now=8000, idle_since=8000 -> not due
        assert_eq!(host.sim().poweroff, 0);
    }

    // (h) unconfigured wake -> jobs stay queued + doctor reports unconfigured
    #[tokio::test]
    async fn unconfigured_wake_keeps_jobs_queued() {
        let host = FakeHost::new(Sim {
            healthy: false,
            ..Default::default()
        });
        let legs = Legs {
            wake: None,
            ..Default::default()
        };
        let executor = build(&host, FakeClock::new(), legs);

        let error = executor.ensure_ready().await.expect_err("no capacity");
        assert!(matches!(error, EnsureError::WakeUnconfigured));
        assert_eq!(host.sim().wake, 0, "never faked a wake");

        // run_job maps the same gate to a queued job, not a failure.
        let queued: Result<(), JobError<&str>> = executor.run_job(|_| async { Ok(()) }).await;
        assert!(matches!(queued, Err(JobError::Queued)));

        let report = executor.report();
        assert!(!report.wake_configured);
        assert_eq!(report.state, LaneState::Offline);
        assert_eq!(report.level, LaneLevel::Warn);
        assert!(report.detail.contains("wake unconfigured"));
    }

    // --- Text requests on a lane with `wake_on_request` --------------------

    /// A clock that moves only when the test moves it; `sleep_ms` waits for
    /// that. Lets a test hold a wake mid-boot and decide when time passes,
    /// with no wall-clock sleeping.
    struct ManualClock {
        now: AtomicU64,
        moved: tokio::sync::watch::Sender<u64>,
    }

    impl ManualClock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                now: AtomicU64::new(0),
                moved: tokio::sync::watch::channel(0).0,
            })
        }
        fn advance(&self, ms: u64) {
            self.now.fetch_add(ms, Ordering::SeqCst);
            self.moved.send_modify(|version| *version += 1);
        }
    }

    #[async_trait]
    impl Clock for ManualClock {
        fn now_ms(&self) -> u64 {
            self.now.load(Ordering::SeqCst)
        }
        async fn sleep_ms(&self, ms: u64) {
            let deadline = self.now_ms().saturating_add(ms);
            let mut moved = self.moved.subscribe();
            while self.now_ms() < deadline {
                if moved.changed().await.is_err() {
                    return;
                }
            }
        }
    }

    /// A text lane that wakes on request: boot timeout 180 s, wake wait
    /// 90 s, re-send gap 60 s (longer than any step a test takes, so one
    /// cycle sends exactly one wake), idle 5 s.
    fn text_lane(
        host: &FakeHost,
        clock: Arc<dyn Clock>,
        wake_on_request: bool,
    ) -> Arc<LocalExecutor> {
        let cfg = LocalExecutorConfig {
            name: "lane".to_string(),
            base_url: "http://executor-1.example:8080".to_string(),
            health_endpoint: "http://executor-1.example:8080/health".to_string(),
            wake_command: Some("ssh gateway wake-host executor-1".to_string()),
            poweroff_command: Some("ssh gateway poweroff-host executor-1".to_string()),
            restart_command: None,
            boot_timeout_secs: 180,
            wake_retry_interval_secs: 60,
            idle_timeout_secs: 5,
            retry_budget: 2,
            health_poll_interval_ms: 400,
            route_health: true,
            wake_on_request,
            wake_wait_secs: 90,
            command_timeout_secs: 60,
            host: None,
            exclusive_group: None,
            release_command: None,
            group_wait_secs: 120,
            poweroff_guard_command: None,
            adopt_external_wake: false,
        };
        LocalExecutor::new(
            cfg,
            clock,
            Arc::new(host.clone()),
            Arc::new(host.clone()),
            true,
        )
    }

    fn detail(wake: &LaneWake) -> &str {
        match wake {
            LaneWake::Ready { detail } | LaneWake::NotReady { detail, .. } => detail,
        }
    }

    // N requests that all find the lane down share ONE wake; each is told
    // the lane is ready once health comes up, and how long it took.
    #[tokio::test]
    async fn concurrent_requests_trigger_exactly_one_wake() {
        use sb_runtime::LaneStatus;
        let host = FakeHost::new(Sim::default());
        let clock = ManualClock::new();
        let lane = text_lane(&host, clock.clone(), true);
        let activator = Arc::new(activator_of(&lane));
        lane.observe().await;
        assert!(activator.known_down("lane").is_some());

        let waiters = tokio::spawn({
            let activator = activator.clone();
            async move {
                let calls = (0..5).map(|_| activator.wake_for_request("lane"));
                futures::future::join_all(calls).await
            }
        });
        settle().await;
        assert_eq!(host.sim().wake, 1, "one wake for five requests");
        assert_eq!(lane.report().state, LaneState::Waking);
        assert_eq!(
            lane.report().queue_depth,
            5,
            "the waiting requests are queued"
        );

        host.sim().healthy = true; // the host finishes booting
        clock.advance(41_000);
        let results = waiters.await.unwrap();
        for wake in &results {
            assert_eq!(
                wake,
                &LaneWake::Ready {
                    detail: "woken for this request, healthy after 41 s".to_string()
                }
            );
        }
        assert_eq!(host.sim().wake, 1, "still one wake");
        let report = lane.report();
        assert_eq!(
            report.state,
            LaneState::IdleCountdown,
            "idle until a request lands"
        );
        assert_eq!(report.queue_depth, 0);
        assert_eq!(report.in_flight, 0);
        assert_eq!(activator.known_down("lane"), None, "routing sees it up");
    }

    // The bound passes before the host answers: the request is told so at
    // once (with a Retry-After hint), and the wake keeps going for the
    // next request.
    #[tokio::test]
    async fn the_wait_bound_passes_while_the_wake_keeps_going() {
        use sb_runtime::LaneStatus;
        let host = FakeHost::new(Sim::default());
        let clock = ManualClock::new();
        let lane = text_lane(&host, clock.clone(), true);
        let activator = Arc::new(activator_of(&lane));

        let waiter = tokio::spawn({
            let activator = activator.clone();
            async move { activator.wake_for_request("lane").await }
        });
        settle().await;
        clock.advance(90_000);
        let wake = waiter.await.unwrap();
        // One waking cycle; inside it the lossy wake is re-sent every 60 s.
        let sent = host.sim().wake;
        assert_eq!(sent, 2);
        assert_eq!(
            wake,
            LaneWake::NotReady {
                detail: "wake wait of 90 s passed, wake still running".to_string(),
                retry_after: Duration::from_secs(90),
            },
            "retry once the 180 s boot deadline has passed"
        );
        assert_eq!(lane.report().state, LaneState::Waking, "the wake goes on");
        assert_eq!(lane.report().queue_depth, 0);

        // The host comes up; the background wake sees it. The next request
        // finds a lane that answers its fresh probe: no second wake.
        host.sim().healthy = true;
        clock.advance(400);
        settle().await;
        assert_eq!(lane.report().state, LaneState::IdleCountdown);
        let wake = activator.wake_for_request("lane").await;
        assert!(
            detail(&wake).starts_with("answered a fresh probe"),
            "{wake:?}"
        );
        assert_eq!(host.sim().wake, sent, "no second waking cycle");
    }

    // A request that joins a wake already under way (here an earlier
    // request's, which outlived that request's wait) is told the lane was
    // woken, not that it answered a fresh probe without one.
    #[tokio::test]
    async fn a_request_that_joins_a_running_wake_is_told_the_lane_was_woken() {
        let host = FakeHost::new(Sim::default());
        let clock = ManualClock::new();
        let lane = text_lane(&host, clock.clone(), true);
        let first = tokio::spawn({
            let lane = lane.clone();
            async move { lane.wake_for_request().await }
        });
        settle().await;
        clock.advance(90_000);
        assert!(matches!(first.await.unwrap(), LaneWake::NotReady { .. }));
        assert_eq!(lane.report().state, LaneState::Waking);

        let second = tokio::spawn({
            let lane = lane.clone();
            async move { lane.wake_for_request().await }
        });
        settle().await;
        host.sim().healthy = true; // the host finishes booting
        clock.advance(400);
        let wake = second.await.unwrap();
        assert_eq!(
            wake,
            LaneWake::Ready {
                detail: "woken for this request, healthy after 400 ms".to_string()
            }
        );
        assert_eq!(host.sim().wake, 2, "one waking cycle, re-sent once");
    }

    // A request abandoned mid-wait (the client went away) is no longer
    // counted as queued, and the wake it started goes on.
    #[tokio::test]
    async fn an_abandoned_wait_leaves_the_queue_and_the_wake_goes_on() {
        let host = FakeHost::new(Sim::default());
        let clock = ManualClock::new();
        let lane = text_lane(&host, clock.clone(), true);
        let waiter = tokio::spawn({
            let lane = lane.clone();
            async move { lane.wake_for_request().await }
        });
        settle().await;
        assert_eq!(lane.report().queue_depth, 1);
        waiter.abort();
        let _ = waiter.await;
        assert_eq!(lane.report().queue_depth, 0);
        assert_eq!(lane.report().state, LaneState::Waking);
        assert_eq!(host.sim().wake, 1);
    }

    // Slice 1's stale window: a lane believed down that answers a fresh
    // probe is used at once, with no wake command.
    #[tokio::test]
    async fn a_lane_believed_down_that_answers_is_used_without_a_wake() {
        let host = FakeHost::new(Sim::default());
        let lane = text_lane(&host, ManualClock::new(), true);
        lane.observe().await; // the last probe saw it down
        host.sim().healthy = true; // ... then it came back
        let wake = lane.wake_for_request().await;
        assert!(matches!(wake, LaneWake::Ready { .. }), "{wake:?}");
        assert!(
            detail(&wake).starts_with("answered a fresh probe"),
            "{wake:?}"
        );
        assert_eq!(host.sim().wake, 0);
        assert_eq!(lane.report().observed_healthy, Some(true));
    }

    // A wake that cannot work says why, and a request in the next re-send
    // gap gets that answer at once instead of running the command again.
    #[tokio::test]
    async fn a_failed_wake_is_reported_and_not_rerun_inside_the_resend_gap() {
        let host = FakeHost::new(Sim {
            fail_wake: true,
            ..Default::default()
        });
        let clock = ManualClock::new();
        let lane = text_lane(&host, clock.clone(), true);
        let wake = lane.wake_for_request().await;
        assert_eq!(
            detail(&wake),
            "wake command failed: wake failed",
            "{wake:?}"
        );
        assert_eq!(host.sim().wake, 1);
        let again = lane.wake_for_request().await;
        assert_eq!(detail(&again), "wake command failed: wake failed");
        assert_eq!(host.sim().wake, 1, "not rerun inside the 60 s gap");
        clock.advance(60_000);
        lane.wake_for_request().await;
        assert_eq!(host.sim().wake, 2, "rerun after the gap");
    }

    // The busy guard: while a request is in flight the idle poweroff
    // cannot fire; the countdown starts when it ends; routing then sees
    // the lane down so the next request wakes it instead of failing.
    #[tokio::test]
    async fn idle_poweroff_waits_for_an_open_request_and_fires_after_it() {
        use sb_runtime::LaneStatus;
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            poweroff_makes_unhealthy: true,
            ..Default::default()
        });
        let clock = ManualClock::new();
        let lane = text_lane(&host, clock.clone(), true);
        let activator = activator_of(&lane);
        assert!(matches!(
            lane.wake_for_request().await,
            LaneWake::Ready { .. }
        ));

        let stream = activator.begin_request("lane").expect("opted in");
        assert_eq!(lane.report().state, LaneState::Draining);
        assert_eq!(lane.report().in_flight, 1);
        clock.advance(60_000); // far past the 5 s idle timeout
        lane.tick().await;
        assert_eq!(host.sim().poweroff, 0, "never while a stream is open");

        drop(stream); // last byte sent, or the client went away
        assert_eq!(lane.report().state, LaneState::IdleCountdown);
        lane.tick().await;
        assert_eq!(host.sim().poweroff, 0, "the countdown starts at release");
        clock.advance(5_000);
        lane.tick().await;
        assert_eq!(host.sim().poweroff, 1);
        assert_eq!(lane.report().state, LaneState::Offline);
        assert!(
            activator.known_down("lane").is_some(),
            "routing sees the poweroff at once"
        );
    }

    /// A host whose poweroff command blocks until the test opens the gate,
    /// so a test can act while the lane is powering off.
    #[derive(Clone)]
    struct SlowPoweroff {
        host: FakeHost,
        gate: Arc<tokio::sync::Semaphore>,
    }

    #[async_trait]
    impl CommandRunner for SlowPoweroff {
        async fn run(&self, command: &str) -> Result<(), String> {
            if command.contains("poweroff") {
                self.gate
                    .acquire()
                    .await
                    .map_err(|error| error.to_string())?
                    .forget();
            }
            self.host.run(command).await
        }
    }

    // While the poweroff command runs, routing sees the lane down and no
    // wake races it: a request that arrives then is not dispatched into a
    // host that is going away, and its wake (fresh probe included) waits
    // for the poweroff to finish, then wakes the host again.
    #[tokio::test]
    async fn a_request_during_poweroff_waits_for_it_and_wakes_the_host_again() {
        use sb_runtime::LaneStatus;
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            poweroff_makes_unhealthy: true,
            ..Default::default()
        });
        let clock = ManualClock::new();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let cfg = (*text_lane(&host, clock.clone(), true).cfg()).clone();
        let lane = LocalExecutor::new(
            cfg,
            clock.clone(),
            Arc::new(SlowPoweroff {
                host: host.clone(),
                gate: gate.clone(),
            }),
            Arc::new(host.clone()),
            true,
        );
        let activator = Arc::new(activator_of(&lane));
        assert!(matches!(
            lane.wake_for_request().await,
            LaneWake::Ready { .. }
        ));
        assert_eq!(lane.report().state, LaneState::IdleCountdown);

        clock.advance(5_000); // the idle timeout passes
        let ticking = tokio::spawn({
            let lane = lane.clone();
            async move { lane.tick().await }
        });
        settle().await;
        assert_eq!(lane.report().state, LaneState::PoweringOff);
        assert!(
            activator.known_down("lane").is_some(),
            "never dispatched into a lane that is powering off"
        );

        let waiter = tokio::spawn({
            let activator = activator.clone();
            async move { activator.wake_for_request("lane").await }
        });
        settle().await;
        assert!(!waiter.is_finished(), "the wake waits for the poweroff");
        assert_eq!(lane.report().state, LaneState::PoweringOff);

        gate.add_permits(1); // the poweroff command returns; the host is off
        ticking.await.unwrap();
        let wake = waiter.await.unwrap();
        assert!(
            detail(&wake).starts_with("woken for this request"),
            "{wake:?}"
        );
        assert_eq!(host.sim().poweroff, 1);
        assert_eq!(host.sim().wake, 2, "woken again after the poweroff");
        assert_eq!(lane.report().state, LaneState::IdleCountdown);
    }

    // Media jobs and requests share one in-flight count.
    #[tokio::test]
    async fn media_jobs_and_requests_share_one_in_flight_count() {
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            ..Default::default()
        });
        let clock = FakeClock::new();
        let lane = text_lane(&host, clock.clone(), true);
        let job = lane.ensure_ready().await.expect("capacity");
        let request = lane.begin_request();
        assert_eq!(lane.report().in_flight, 2);
        drop(job);
        assert_eq!(
            lane.report().state,
            LaneState::Draining,
            "the request still runs"
        );
        drop(request);
        assert_eq!(lane.report().in_flight, 0);
        assert_eq!(lane.report().state, LaneState::IdleCountdown);
    }

    // A guard is released while a panic unwinds.
    #[tokio::test]
    async fn a_request_guard_is_released_when_its_holder_panics() {
        let host = FakeHost::new(Sim::default());
        let lane = text_lane(&host, FakeClock::new(), true);
        let held = lane.clone();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = held.begin_request();
            panic!("handler panicked mid-stream");
        }));
        assert!(outcome.is_err());
        assert_eq!(lane.report().in_flight, 0);
    }

    // Opt-out (the default): no guard, no wake command, nothing counted.
    #[tokio::test]
    async fn a_lane_without_wake_on_request_holds_nothing_and_never_wakes() {
        use sb_runtime::LaneStatus;
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            ..Default::default()
        });
        let lane = text_lane(&host, FakeClock::new(), false);
        let activator = activator_of(&lane);
        assert!(activator.begin_request("lane").is_none());
        let wake = activator.wake_for_request("lane").await;
        assert!(matches!(wake, LaneWake::NotReady { .. }), "{wake:?}");
        assert_eq!(host.sim().wake, 0);
        let report = lane.report();
        assert_eq!(report.in_flight, 0);
        assert_eq!(report.state, LaneState::Offline);
    }

    fn lanes_config(lanes: &str) -> Config {
        Config::from_yaml(&format!(
            r#"
providers:
  - id: lane
    type: openai_compatible
    base_url: "http://executor-1.example:8080/v1"
  - id: added
    type: openai_compatible
    base_url: "http://executor-2.example:8080/v1"
local_executors:
{lanes}"#
        ))
        .unwrap()
    }

    const LANE: &str = r#"  - name: lane
    base_url: "http://executor-1.example:8080"
    health_endpoint: "http://executor-1.example:8080/health"
    route_health: true
    wake_on_request: true
"#;

    const ADDED: &str = r#"  - name: added
    base_url: "http://executor-2.example:8080"
    health_endpoint: "http://executor-2.example:8080/health"
    route_health: true
    wake_on_request: true
"#;

    // Reload: a lane that stays keeps its live state and takes the new
    // settings; an added lane is used; a removed one leaves routing and
    // the supervisor, while work already on it finishes.
    #[tokio::test]
    async fn a_reload_adds_changes_and_removes_lanes_without_dropping_live_state() {
        use sb_runtime::LaneStatus;
        let host = FakeHost::new(Sim::default());
        let activator = CapacityActivator::with_seams(
            &lanes_config(LANE),
            FakeClock::new(),
            Arc::new(host.clone()),
            Arc::new(host.clone()),
        );
        assert!(activator.executor("added").is_none());
        let in_flight = activator.begin_request("lane").expect("opted in");

        let changed = LANE.replace(
            "wake_on_request: true",
            "wake_on_request: true\n    wake_wait_secs: 7",
        );
        activator.config_published(&lanes_config(&format!("{changed}{ADDED}")));
        let lane = activator.executor("lane").unwrap();
        assert_eq!(lane.report().in_flight, 1, "the open request is kept");
        assert_eq!(lane.cfg().wake_wait_secs, 7, "new settings apply");
        let added = activator
            .begin_request("added")
            .expect("the added lane is used");
        assert_eq!(activator.executor("added").unwrap().report().in_flight, 1);
        drop(added);

        activator.observe_all().await; // both down
        activator.config_published(&lanes_config(ADDED));
        assert!(activator.executor("lane").is_none());
        assert_eq!(
            activator.known_down("lane"),
            None,
            "routing no longer reads it"
        );
        assert!(activator.begin_request("lane").is_none());
        drop(in_flight); // the request on the removed lane finishes cleanly
        assert_eq!(lane.report().in_flight, 0);
        assert_eq!(
            activator
                .reports()
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            ["added"]
        );
    }

    // Opt-out cost: a config without `local_executors` gets an activator
    // (always attached so a reload can add a lane) but no supervisor task,
    // no lane and no probe, before and after a reload that keeps it so. A
    // reload that adds a lane starts the supervisor then.
    #[tokio::test]
    async fn no_lanes_means_no_supervisor_until_a_reload_adds_one() {
        let host = FakeHost::new(Sim::default());
        let no_lanes = Config::from_yaml("providers:\n  - id: lane\n    type: mock\n").unwrap();
        let activator = CapacityActivator::with_seams(
            &no_lanes,
            FakeClock::new(),
            Arc::new(host.clone()),
            Arc::new(host.clone()),
        );
        activator.spawn_supervisor(Duration::from_secs(10));
        assert!(activator.is_empty());
        assert!(!activator.supervisor_started.load(Ordering::SeqCst));
        activator.reconcile(&no_lanes);
        assert!(!activator.supervisor_started.load(Ordering::SeqCst));

        activator.reconcile(&lanes_config(LANE));
        assert!(activator.supervisor_started.load(Ordering::SeqCst));
        let sim = host.sim();
        assert_eq!((sim.wake, sim.poweroff), (0, 0));
    }

    // A lane removed by a reload and added back while work still runs on
    // it is the same lane again: the open stream keeps holding the idle
    // poweroff off, so the host is not powered off under it.
    #[tokio::test]
    async fn a_lane_removed_and_added_back_keeps_the_work_still_running_on_it() {
        use sb_runtime::LaneStatus;
        let host = FakeHost::new(Sim {
            healthy: true,
            ..Default::default()
        });
        let clock = FakeClock::new();
        let lane_cfg = LANE.replace(
            "wake_on_request: true",
            "wake_on_request: true\n    poweroff_command: \"ssh gateway poweroff-host executor-1\"",
        );
        let activator = CapacityActivator::with_seams(
            &lanes_config(&lane_cfg),
            clock.clone(),
            Arc::new(host.clone()),
            Arc::new(host.clone()),
        );
        activator.observe_all().await; // the host is up
        let long_stream = activator.begin_request("lane").expect("opted in");

        activator.config_published(&lanes_config(ADDED));
        activator.config_published(&lanes_config(&format!("{lane_cfg}{ADDED}")));
        activator.observe_all().await;
        let lane = activator.executor("lane").unwrap();
        assert_eq!(lane.report().in_flight, 1, "the open stream still counts");

        // Another request comes and goes, then the idle timeout passes
        // while the first stream is still open.
        drop(activator.begin_request("lane").expect("opted in"));
        clock.advance(3_600_000);
        lane.tick().await;
        assert_eq!(host.sim().poweroff, 0, "never under the open stream");

        drop(long_stream);
        clock.advance(3_600_000);
        lane.tick().await;
        assert_eq!(host.sim().poweroff, 1, "idle once the stream ended");
    }

    // --- Hardening: power-off retry, command timeout, own power-off ---------

    // A failed power-off is retried after the re-send gap, up to the lane's
    // retry budget (2 here, so 3 attempts), then the lane is Degraded and
    // the report says so loudly. Nothing runs between the attempts.
    #[tokio::test]
    async fn a_failed_poweroff_is_retried_then_degraded_loudly() {
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            fail_poweroff: true,
            ..Default::default()
        });
        let clock = FakeClock::new();
        let executor = build(&host, clock.clone(), Legs::default()); // gap 30 s
        drop(executor.ensure_ready().await.expect("capacity"));
        clock.advance(5_000);
        executor.tick().await;
        assert_eq!(host.sim().poweroff, 1);
        let report = executor.report();
        assert_eq!(report.state, LaneState::IdleCountdown, "{report:?}");
        assert_eq!(report.level, LaneLevel::Warn);
        assert_eq!(report.poweroff_failures, 1);
        assert!(!report.escalated);
        assert!(
            report.detail.contains("attempt 1 of 3") && report.detail.contains("retrying in 30 s"),
            "{}",
            report.detail
        );

        executor.tick().await;
        clock.advance(29_000);
        executor.tick().await;
        assert_eq!(host.sim().poweroff, 1, "no retry inside the gap");
        clock.advance(1_000);
        executor.tick().await;
        assert_eq!(host.sim().poweroff, 2, "retried after the gap");
        clock.advance(30_000);
        executor.tick().await;
        assert_eq!(host.sim().poweroff, 3);

        let report = executor.report();
        assert_eq!(report.state, LaneState::Degraded);
        assert_eq!(report.level, LaneLevel::Fail);
        assert!(report.escalated);
        assert_eq!(report.poweroff_failures, 3);
        assert!(
            report.detail.contains("poweroff command failed 3 times")
                && report.detail.contains("until an operator acts"),
            "{}",
            report.detail
        );
        clock.advance(600_000);
        executor.tick().await;
        assert_eq!(
            host.sim().poweroff,
            3,
            "bounded: no attempt after the budget"
        );
    }

    // A retry that works powers the machine off and clears the warning;
    // new work after a failure starts a fresh budget.
    #[tokio::test]
    async fn a_poweroff_retry_that_works_powers_off_and_clears_the_warning() {
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            fail_poweroff: true,
            poweroff_makes_unhealthy: true,
            ..Default::default()
        });
        let clock = FakeClock::new();
        let executor = build(&host, clock.clone(), Legs::default());
        drop(executor.ensure_ready().await.expect("capacity"));
        clock.advance(5_000);
        executor.tick().await;
        assert_eq!(executor.report().poweroff_failures, 1);

        host.sim().fail_poweroff = false; // the transport is back
        clock.advance(30_000);
        executor.tick().await;
        assert_eq!(host.sim().poweroff, 2);
        let report = executor.report();
        assert_eq!(report.state, LaneState::Offline);
        assert_eq!(report.level, LaneLevel::Ok);
        assert_eq!(report.poweroff_failures, 0);
        assert_eq!(report.last_error, None);
    }

    // A text request on a lane that a power-off left degraded starts a new
    // idle period: the lane leaves `Degraded`, and once idle again the
    // machine gets a fresh power-off attempt instead of staying on for good.
    #[tokio::test]
    async fn new_text_work_after_a_degraded_poweroff_gets_a_fresh_attempt() {
        use sb_runtime::LaneStatus;
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            fail_poweroff: true,
            ..Default::default()
        });
        let clock = ManualClock::new();
        let lane = text_lane(&host, clock.clone(), true); // budget 2, gap 60 s
        let activator = activator_of(&lane);
        assert!(matches!(
            lane.wake_for_request().await,
            LaneWake::Ready { .. }
        ));
        clock.advance(5_000);
        lane.tick().await;
        clock.advance(60_000);
        lane.tick().await;
        clock.advance(60_000);
        lane.tick().await;
        assert_eq!(host.sim().poweroff, 3);
        assert_eq!(lane.report().state, LaneState::Degraded);

        lane.observe().await; // the machine stayed on
        drop(activator.begin_request("lane").expect("counted"));
        let report = lane.report();
        assert_eq!(report.state, LaneState::IdleCountdown, "{report:?}");
        assert!(!report.escalated, "{report:?}");
        clock.advance(5_000);
        lane.tick().await;
        assert_eq!(
            host.sim().poweroff,
            4,
            "a fresh attempt for the new idle period"
        );
    }

    /// A runner whose first command of a kind hangs forever (a stuck ssh
    /// session); every later command goes to the fake host.
    #[derive(Clone)]
    struct HangOnce {
        host: FakeHost,
        kind: &'static str,
        hung: Arc<AtomicBool>,
    }

    #[async_trait]
    impl CommandRunner for HangOnce {
        async fn run(&self, command: &str) -> Result<(), String> {
            if command.contains(self.kind) && !self.hung.swap(true, Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            self.host.run(command).await
        }
    }

    // A hung command is stopped at `command_timeout_secs` (on the clock
    // seam), treated as failed, and frees the wake lock: the next job wakes
    // the machine instead of waiting behind it forever.
    #[tokio::test]
    async fn a_hung_command_times_out_and_frees_the_wake_lock() {
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            ..Default::default()
        });
        let clock = ManualClock::new();
        let mut cfg = (*text_lane(&host, clock.clone(), false).cfg()).clone();
        cfg.command_timeout_secs = 20;
        cfg.boot_timeout_secs = 20; // a wake command gets the longer of the two
        let lane = LocalExecutor::new(
            cfg,
            clock.clone(),
            Arc::new(HangOnce {
                host: host.clone(),
                kind: "wake",
                hung: Arc::new(AtomicBool::new(false)),
            }),
            Arc::new(host.clone()),
            true,
        );
        let first = tokio::spawn({
            let lane = lane.clone();
            async move { lane.ensure_ready().await.map(drop) }
        });
        settle().await;
        assert_eq!(lane.report().state, LaneState::Waking);
        let second = tokio::spawn({
            let lane = lane.clone();
            async move { lane.ensure_ready().await.map(drop) }
        });
        settle().await;
        assert!(!second.is_finished(), "queued behind the hung wake");

        clock.advance(20_000);
        let first = tokio::time::timeout(Duration::from_secs(5), first)
            .await
            .expect("the hung command is stopped at its timeout")
            .unwrap();
        match first {
            Err(EnsureError::WakeFailed(error)) => {
                assert_eq!(error, "timed out after 20 s")
            }
            other => panic!("expected a timed-out wake, got {other:?}"),
        }
        let second = tokio::time::timeout(Duration::from_secs(5), second)
            .await
            .expect("the lock was freed")
            .unwrap();
        assert!(second.is_ok(), "{second:?}");
        assert_eq!(host.sim().wake, 1, "the second job's wake ran");
    }

    /// A runner whose wake command blocks for `ms` on the clock seam (an
    /// operator script that returns once the machine answers).
    struct SlowWake {
        host: FakeHost,
        clock: Arc<ManualClock>,
        ms: u64,
    }

    #[async_trait]
    impl CommandRunner for SlowWake {
        async fn run(&self, command: &str) -> Result<(), String> {
            if command.contains("wake") {
                self.clock.sleep_ms(self.ms).await;
            }
            self.host.run(command).await
        }
    }

    // A wake command may run for the boot timeout (180 s here) when that
    // is longer than `command_timeout_secs` (60 s): a slow wake that worked
    // before commands were bounded still works.
    #[tokio::test]
    async fn a_slow_wake_command_gets_the_boot_timeout() {
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            ..Default::default()
        });
        let clock = ManualClock::new();
        let cfg = (*text_lane(&host, clock.clone(), false).cfg()).clone();
        assert_eq!((cfg.command_timeout_secs, cfg.boot_timeout_secs), (60, 180));
        let lane = LocalExecutor::new(
            cfg,
            clock.clone(),
            Arc::new(SlowWake {
                host: host.clone(),
                clock: clock.clone(),
                ms: 90_000,
            }),
            Arc::new(host.clone()),
            true,
        );
        let job = tokio::spawn({
            let lane = lane.clone();
            async move { lane.ensure_ready().await.map(drop) }
        });
        settle().await;
        clock.advance(60_000);
        settle().await;
        assert!(!job.is_finished(), "not cut at command_timeout_secs");
        clock.advance(30_000);
        let result = tokio::time::timeout(Duration::from_secs(5), job)
            .await
            .expect("the wake returns")
            .unwrap();
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(host.sim().wake, 1);
    }

    // After our own power-off the host may still answer while it shuts
    // down: neither routing nor a request trusts that answer until a probe
    // has failed once or a wake ran.
    #[tokio::test]
    async fn up_is_not_trusted_right_after_our_own_poweroff() {
        use sb_runtime::LaneStatus;
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            ..Default::default() // the poweroff leaves the host answering
        });
        let clock = ManualClock::new();
        let lane = text_lane(&host, clock.clone(), true);
        let activator = Arc::new(activator_of(&lane));
        assert!(matches!(
            lane.wake_for_request().await,
            LaneWake::Ready { .. }
        ));
        clock.advance(5_000);
        lane.tick().await;
        assert_eq!(host.sim().poweroff, 1);
        assert!(host.sim().healthy, "the host still answers");

        lane.observe().await;
        assert_eq!(lane.report().observed_healthy, Some(false));
        assert!(activator.known_down("lane").is_some(), "not taken for up");

        let wake = activator.wake_for_request("lane").await;
        assert!(
            detail(&wake).starts_with("woken for this request"),
            "a wake ran, not a fresh probe: {wake:?}"
        );
        assert_eq!(host.sim().wake, 2);
        assert_eq!(activator.known_down("lane"), None, "up after the wake");
    }

    // A power-off that returned success but did nothing: a lane that does
    // not wake on request is known down only until the boot timeout has
    // passed; then the answer is trusted again, with a warning.
    #[tokio::test]
    async fn a_host_that_never_powered_off_is_trusted_again_after_the_boot_timeout() {
        use sb_runtime::LaneStatus;
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            ..Default::default() // the poweroff leaves the host answering
        });
        let clock = ManualClock::new();
        let lane = text_lane(&host, clock.clone(), false); // boot timeout 180 s
        let activator = activator_of(&lane);
        drop(lane.ensure_ready().await.expect("capacity"));
        clock.advance(5_000);
        lane.tick().await;
        assert_eq!(host.sim().poweroff, 1);

        clock.advance(179_000);
        lane.observe().await;
        assert!(activator.known_down("lane").is_some(), "still unconfirmed");
        clock.advance(1_000);
        lane.observe().await;
        assert_eq!(activator.known_down("lane"), None, "trusted again");
        let report = lane.report();
        assert!(
            report
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("may not have taken effect")),
            "{report:?}"
        );
        assert_eq!(host.sim().wake, 1, "no wake needed");
    }

    // A probe that fails confirms our power-off; after that, an answer is
    // trusted again (a host someone started by hand is used).
    #[tokio::test]
    async fn a_failed_probe_confirms_our_poweroff() {
        use sb_runtime::LaneStatus;
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            poweroff_makes_unhealthy: true,
            ..Default::default()
        });
        let clock = ManualClock::new();
        let lane = text_lane(&host, clock.clone(), true);
        let activator = activator_of(&lane);
        assert!(matches!(
            lane.wake_for_request().await,
            LaneWake::Ready { .. }
        ));
        clock.advance(5_000);
        lane.tick().await;
        lane.observe().await; // the host is down: confirmed
        host.sim().healthy = true; // someone starts it by hand
        lane.observe().await;
        assert_eq!(activator.known_down("lane"), None);
        assert_eq!(host.sim().wake, 1, "no wake needed");
    }

    // --- Shared machine power (`host:`) -------------------------------------

    /// Two lanes on one machine `gpu-box`: a text lane that wakes on request
    /// (it carries the power commands) and a ComfyUI lane (media jobs). Each
    /// lane's idle timeout is 5 s. `extra` is appended to both lane entries.
    fn shared_host_config(extra: &str) -> Config {
        Config::from_yaml(&format!(
            r#"
providers:
  - id: text
    type: openai_compatible
    base_url: "http://executor-1.example:8080/v1"
  - id: comfy
    type: comfyui
    base_url: "http://executor-1.example:8188"
local_executors:
  - name: text
    host: gpu-box
    base_url: "http://executor-1.example:8080"
    health_endpoint: "http://executor-1.example:8080/health"
    route_health: true
    wake_on_request: true
    wake_command: "ssh gateway wake-host executor-1"
    poweroff_command: "ssh gateway poweroff-host executor-1"
    idle_timeout_secs: 5
    boot_timeout_secs: 180
    wake_retry_interval_secs: 60
{extra}  - name: comfy
    host: gpu-box
    base_url: "http://executor-1.example:8188"
    health_endpoint: "http://executor-1.example:8188/system_stats"
    idle_timeout_secs: 5
    boot_timeout_secs: 180
    wake_retry_interval_secs: 60
{extra}"#
        ))
        .unwrap()
    }

    fn shared_activator(
        cfg: &Config,
        host: &FakeHost,
        clock: Arc<dyn Clock>,
    ) -> Arc<CapacityActivator> {
        CapacityActivator::with_seams(cfg, clock, Arc::new(host.clone()), Arc::new(host.clone()))
    }

    // Work on one lane keeps the shared machine up while the other lane is
    // idle far past its timeout; the machine is powered off once, after
    // the last lane has been idle for its timeout, and every lane on it
    // goes offline together.
    #[tokio::test]
    async fn work_on_one_lane_keeps_the_shared_machine_up_until_both_are_idle() {
        use sb_runtime::LaneStatus;
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            poweroff_makes_unhealthy: true,
            ..Default::default()
        });
        let clock = FakeClock::new();
        let activator = shared_activator(&shared_host_config(""), &host, clock.clone());
        let text = activator.executor("text").unwrap();
        let comfy = activator.executor("comfy").unwrap();

        let job = comfy.ensure_ready().await.expect("capacity"); // wakes the machine
        assert_eq!(host.sim().wake, 1);
        text.observe().await;
        drop(activator.begin_request("text").expect("opted in"));
        assert_eq!(text.report().state, LaneState::IdleCountdown);

        clock.advance(60_000); // the text lane is idle far past its 5 s
        activator.tick_all().await;
        assert_eq!(host.sim().poweroff, 0, "never under the running media job");
        assert_eq!(comfy.report().state, LaneState::Draining);

        drop(job); // the last lane goes idle now
        clock.advance(4_000);
        activator.tick_all().await;
        assert_eq!(host.sim().poweroff, 0, "the timer runs from the last lane");
        clock.advance(1_000);
        activator.tick_all().await;
        activator.tick_all().await;
        assert_eq!(host.sim().poweroff, 1, "exactly one power-off");
        assert_eq!(text.report().state, LaneState::Offline);
        assert_eq!(comfy.report().state, LaneState::Offline);
        assert!(
            activator.known_down("text").is_some(),
            "routing sees it down"
        );
        let reports = activator.reports();
        assert!(
            reports.iter().all(|r| r.host.as_deref() == Some("gpu-box")),
            "{reports:?}"
        );
        assert!(
            reports
                .iter()
                .all(|r| r.wake_configured && r.poweroff_configured),
            "the comfy lane uses the host's commands: {reports:?}"
        );
    }

    // The reverse: an open text request keeps the machine up for it while
    // the media lane is idle past its timeout.
    #[tokio::test]
    async fn an_open_request_keeps_the_shared_machine_up_for_it() {
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            ..Default::default()
        });
        let clock = FakeClock::new();
        let activator = shared_activator(&shared_host_config(""), &host, clock.clone());
        let text = activator.executor("text").unwrap();
        drop(
            activator
                .executor("comfy")
                .unwrap()
                .ensure_ready()
                .await
                .expect("capacity"),
        );
        text.observe().await;
        let stream = sb_runtime::LaneStatus::begin_request(&*activator, "text").expect("opted in");
        clock.advance(60_000);
        activator.tick_all().await;
        assert_eq!(host.sim().poweroff, 0, "never under the open stream");
        drop(stream);
        clock.advance(5_000);
        activator.tick_all().await;
        assert_eq!(host.sim().poweroff, 1);
    }

    // A text lane on the machine that does not wake on request: its open
    // request still keeps the machine up while the media lane's idle timer
    // runs out, and the power-off fires once it ends.
    #[tokio::test]
    async fn an_open_request_on_a_lane_that_does_not_wake_keeps_the_shared_machine_up() {
        use sb_runtime::LaneStatus;
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            ..Default::default()
        });
        let clock = FakeClock::new();
        let mut cfg = shared_host_config("");
        cfg.local_executors[0].wake_on_request = false;
        let activator = shared_activator(&cfg, &host, clock.clone());
        drop(
            activator
                .executor("comfy")
                .unwrap()
                .ensure_ready()
                .await
                .expect("capacity"),
        );
        let stream = activator
            .try_begin_request("text")
            .expect("never refused")
            .expect("counted on a shared machine");
        clock.advance(60_000);
        activator.tick_all().await;
        assert_eq!(host.sim().poweroff, 0, "never under the open stream");
        drop(stream);
        activator.tick_all().await;
        assert_eq!(host.sim().poweroff, 1, "fires once the stream ends");
    }

    // Five requests across two lanes of one sleeping machine: one wake.
    #[tokio::test]
    async fn five_requests_across_two_lanes_of_a_sleeping_host_cause_one_wake() {
        use sb_runtime::LaneStatus;
        let host = FakeHost::new(Sim::default());
        let clock = ManualClock::new();
        let twin = shared_host_config("").local_executors[0].clone();
        let mut cfg = shared_host_config("");
        let mut second = twin.clone();
        second.name = "comfy".to_string();
        second.wake_command = None;
        second.poweroff_command = None;
        second.health_endpoint = "http://executor-1.example:8081/health".to_string();
        cfg.local_executors = vec![twin, second];
        // Both lanes are text lanes here (wake on request).
        cfg.providers[1] = cfg.providers[0].clone();
        cfg.providers[1].id = "comfy".to_string();
        let activator = shared_activator(&cfg, &host, clock.clone());
        activator.observe_all().await;
        assert!(activator.known_down("text").is_some());
        assert!(activator.known_down("comfy").is_some());

        let waiters = tokio::spawn({
            let activator = activator.clone();
            async move {
                let calls = ["text", "comfy", "text", "comfy", "text"]
                    .map(|lane| activator.wake_for_request(lane));
                futures::future::join_all(calls).await
            }
        });
        settle().await;
        assert_eq!(host.sim().wake, 1, "one wake for the machine");

        host.sim().healthy = true;
        clock.advance(41_000);
        let results = waiters.await.unwrap();
        assert!(
            results.iter().all(|w| matches!(w, LaneWake::Ready { .. })),
            "{results:?}"
        );
        assert_eq!(host.sim().wake, 1, "still one wake");
        assert_eq!(activator.known_down("text"), None);
        assert_eq!(activator.known_down("comfy"), None);
    }

    // Opt-out: two lanes without `host` are two machines, exactly as
    // before. No shared state: each powers off on its own timer.
    #[tokio::test]
    async fn lanes_without_a_host_keep_a_machine_each() {
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            ..Default::default()
        });
        let clock = FakeClock::new();
        let cfg = shared_host_config("").clone();
        let mut cfg = cfg;
        for lane in &mut cfg.local_executors {
            lane.host = None;
            lane.wake_command = Some("ssh gateway wake-host executor-1".to_string());
            lane.poweroff_command = Some("ssh gateway poweroff-host executor-1".to_string());
        }
        let activator = shared_activator(&cfg, &host, clock.clone());
        let job = activator
            .executor("comfy")
            .unwrap()
            .ensure_ready()
            .await
            .expect("capacity");
        drop(
            activator
                .executor("text")
                .unwrap()
                .ensure_ready()
                .await
                .expect("capacity"),
        );
        clock.advance(5_000);
        activator.tick_all().await;
        assert_eq!(
            host.sim().poweroff,
            1,
            "the idle lane powers off its own machine"
        );
        drop(job);
        clock.advance(5_000);
        activator.tick_all().await;
        assert_eq!(host.sim().poweroff, 2);
        assert!(activator.reports().iter().all(|r| r.host.is_none()));
        let json = serde_json::to_value(activator.reports()).unwrap();
        for absent in [
            "host",
            "gpu_group",
            "holds_gpu",
            "poweroff_failures",
            "poweroff_guard_busy",
            "poweroff_guard_busy_since_ms",
            "poweroff_guard_failures",
        ] {
            assert!(
                json[0].get(absent).is_none(),
                "{absent} absent, not null: {json}"
            );
        }
    }

    // A reload that puts two lanes on one host moves them onto it, and a
    // reload that takes `host` away gives each a machine of its own again.
    #[tokio::test]
    async fn a_reload_moves_lanes_onto_and_off_a_shared_host() {
        let host = FakeHost::new(Sim::default());
        let shared = shared_host_config("");
        let mut separate = shared.clone();
        for lane in &mut separate.local_executors {
            lane.host = None;
        }
        let activator = shared_activator(&separate, &host, FakeClock::new());
        let text = activator.executor("text").unwrap();
        let comfy = activator.executor("comfy").unwrap();
        assert!(!Arc::ptr_eq(&text.host(), &comfy.host()));

        activator.reconcile(&shared);
        assert!(Arc::ptr_eq(&text.host(), &comfy.host()));
        assert_eq!(text.host().id, "gpu-box");

        activator.reconcile(&separate);
        assert!(!Arc::ptr_eq(&text.host(), &comfy.host()));
        assert!(activator.reports().iter().all(|r| r.host.is_none()));
    }

    // --- Exclusive GPU group (`exclusive_group`) ----------------------------

    const RELEASE_TEXT: &str = "ssh gateway release-gpu text";
    const RELEASE_COMFY: &str = "ssh gateway release-gpu comfy";

    /// The machine of [`shared_host_config`] with both lanes in GPU group
    /// `gpu0`, each with its own release command; a lane waits up to 30 s.
    fn gpu_group_config() -> Config {
        let mut cfg = shared_host_config("");
        for lane in &mut cfg.local_executors {
            lane.exclusive_group = Some("gpu0".to_string());
            lane.group_wait_secs = 30;
            lane.release_command = Some(format!("ssh gateway release-gpu {}", lane.name));
        }
        cfg
    }

    /// The machine is up; both engines answer.
    fn gpu_group_activator() -> (FakeHost, Arc<ManualClock>, Arc<CapacityActivator>) {
        let host = FakeHost::new(Sim {
            healthy: true,
            ..Default::default()
        });
        let clock = ManualClock::new();
        let activator = shared_activator(&gpu_group_config(), &host, clock.clone());
        (host, clock, activator)
    }

    /// A media job on `comfy` whose dispatch runs until `gate` gets a
    /// permit; `started` counts dispatches that began.
    fn media_job(
        activator: &Arc<CapacityActivator>,
        gate: Arc<tokio::sync::Semaphore>,
        started: Arc<AtomicU64>,
    ) -> tokio::task::JoinHandle<Result<(&'static str, Option<String>), String>> {
        let comfy = activator.executor("comfy").unwrap();
        tokio::spawn(async move {
            comfy
                .run_job_noted(|_attempt| {
                    let gate = gate.clone();
                    let started = started.clone();
                    async move {
                        started.fetch_add(1, Ordering::SeqCst);
                        gate.acquire().await.unwrap().forget();
                        Ok::<_, String>("image")
                    }
                })
                .await
                .map_err(|error| format!("{error:?}"))
        })
    }

    fn acquire(
        activator: &Arc<CapacityActivator>,
        lane: &'static str,
    ) -> tokio::task::JoinHandle<LaneAcquire> {
        let activator = activator.clone();
        tokio::spawn(
            async move { sb_runtime::LaneStatus::acquire_for_request(&*activator, lane).await },
        )
    }

    fn ready(acquired: LaneAcquire) -> (LaneBusy, String) {
        match acquired {
            LaneAcquire::Ready {
                busy: Some(busy),
                detail: Some(detail),
            } => (busy, detail),
            LaneAcquire::Ready { .. } => panic!("ready without a guard or a detail"),
            LaneAcquire::NotReady { detail, .. } => panic!("not ready: {detail}"),
        }
    }

    fn releases(host: &FakeHost) -> Vec<String> {
        host.sim().releases.clone()
    }

    // A text request that arrives while a media job holds the GPU waits;
    // when the job ends the media lane's release command runs exactly once,
    // the text lane takes the GPU, and the detail says what happened.
    #[tokio::test]
    async fn a_text_request_waits_for_a_media_job_then_one_release_runs() {
        let (host, clock, activator) = gpu_group_activator();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let job = media_job(&activator, gate.clone(), Arc::new(AtomicU64::new(0)));
        settle().await;
        assert_eq!(
            releases(&host),
            [RELEASE_TEXT],
            "the first holder after start-up releases the other lane"
        );
        assert_eq!(
            activator.executor("comfy").unwrap().report().holds_gpu,
            Some(true)
        );

        let request = acquire(&activator, "text");
        settle().await;
        assert!(!request.is_finished(), "waits while the job holds the GPU");
        assert_eq!(activator.executor("text").unwrap().report().queue_depth, 1);
        clock.advance(12_000);
        settle().await;
        assert!(!request.is_finished());

        gate.add_permits(1); // the image is done
        let (job_result, job_note) = job.await.unwrap().expect("the job succeeds");
        assert_eq!(job_result, "image");
        assert!(
            job_note
                .as_deref()
                .unwrap()
                .contains("released lane `text`"),
            "{job_note:?}"
        );
        let (busy, detail) = ready(request.await.unwrap());
        assert_eq!(
            detail,
            "waited 12 s for gpu group `gpu0`, took it from lane `comfy`"
        );
        assert_eq!(
            releases(&host),
            [RELEASE_TEXT, RELEASE_COMFY],
            "exactly one release for the switch"
        );
        let text = activator.executor("text").unwrap().report();
        assert_eq!((text.holds_gpu, text.in_flight), (Some(true), 1));
        drop(busy);
        assert_eq!(activator.executor("text").unwrap().report().in_flight, 0);
    }

    // The reverse: a media job that arrives while text streams are open
    // waits until the last one ends; new text work waits behind the switch.
    #[tokio::test]
    async fn a_media_job_waits_until_the_last_text_stream_ends() {
        use sb_runtime::LaneStatus;
        let (host, _clock, activator) = gpu_group_activator();
        let (first, _) = ready(acquire(&activator, "text").await.unwrap());
        let second = activator
            .try_begin_request("text")
            .expect("the holder's work runs at once")
            .expect("counted");
        assert_eq!(releases(&host), [RELEASE_COMFY]);

        let started = Arc::new(AtomicU64::new(0));
        let gate = Arc::new(tokio::sync::Semaphore::new(1));
        let job = media_job(&activator, gate, started.clone());
        settle().await;
        assert!(!job.is_finished(), "two streams are open");
        drop(first);
        settle().await;
        assert!(!job.is_finished(), "one stream is still open");
        assert_eq!(started.load(Ordering::SeqCst), 0);
        let refused = activator
            .try_begin_request("text")
            .err()
            .expect("waits behind the switch");
        assert!(refused.contains("lane `comfy` is next"), "{refused}");

        drop(second); // the last stream ends
        let (_, note) = job.await.unwrap().expect("the job runs");
        assert!(
            note.as_deref()
                .unwrap()
                .contains("took it from lane `text`"),
            "{note:?}"
        );
        assert_eq!(releases(&host), [RELEASE_COMFY, RELEASE_TEXT]);
        assert_eq!(
            activator.executor("comfy").unwrap().report().holds_gpu,
            Some(true)
        );
    }

    // Fairness: a switch is asked once; new work of the holder that arrives
    // after it waits behind it, and the lane that lost the GPU gets the
    // next turn. Neither side starves the other.
    #[tokio::test]
    async fn the_gpu_group_alternates_between_waiting_lanes() {
        let (host, _clock, activator) = gpu_group_activator();
        let (text_stream, _) = ready(acquire(&activator, "text").await.unwrap());

        let gate_a = Arc::new(tokio::sync::Semaphore::new(0));
        let started_a = Arc::new(AtomicU64::new(0));
        let job_a = media_job(&activator, gate_a.clone(), started_a.clone());
        settle().await;
        let next_text = acquire(&activator, "text"); // after job A's switch
        settle().await;
        assert!(
            !next_text.is_finished(),
            "new holder work waits behind the switch"
        );
        drop(text_stream);
        settle().await;
        assert_eq!(started_a.load(Ordering::SeqCst), 1, "job A got the GPU");
        assert!(!next_text.is_finished());

        let gate_b = Arc::new(tokio::sync::Semaphore::new(1));
        let started_b = Arc::new(AtomicU64::new(0));
        let job_b = media_job(&activator, gate_b, started_b.clone()); // after the text switch
        settle().await;
        assert_eq!(
            started_b.load(Ordering::SeqCst),
            0,
            "job B waits behind the text turn"
        );

        gate_a.add_permits(1);
        job_a.await.unwrap().expect("job A");
        let (text_busy, detail) = ready(next_text.await.unwrap());
        assert!(detail.ends_with("took it from lane `comfy`"), "{detail}");
        settle().await;
        assert_eq!(started_b.load(Ordering::SeqCst), 0, "still the text turn");
        drop(text_busy);
        job_b.await.unwrap().expect("job B");
        assert_eq!(
            releases(&host),
            [RELEASE_COMFY, RELEASE_TEXT, RELEASE_COMFY, RELEASE_TEXT]
        );
    }

    // A wait that runs out says why, and withdraws its switch so the
    // holder's work flows again; no release command ran for it.
    #[tokio::test]
    async fn a_group_wait_that_runs_out_says_so_and_withdraws_the_switch() {
        use sb_runtime::LaneStatus;
        let (host, clock, activator) = gpu_group_activator();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let job = media_job(&activator, gate.clone(), Arc::new(AtomicU64::new(0)));
        settle().await;
        let request = acquire(&activator, "text");
        settle().await;
        clock.advance(30_000);
        match request.await.unwrap() {
            LaneAcquire::NotReady {
                detail,
                retry_after,
            } => {
                assert_eq!(
                    detail,
                    "waited 30 s for gpu group `gpu0`; lane `comfy` still holds it (1 in flight)"
                );
                assert_eq!(retry_after, LANE_GROUP_RETRY_AFTER);
            }
            LaneAcquire::Ready { .. } => panic!("the bound passed"),
        }
        assert_eq!(activator.executor("text").unwrap().report().queue_depth, 0);
        assert!(
            activator
                .try_begin_request("comfy")
                .expect("the switch is withdrawn")
                .is_some(),
            "the holder's work flows again"
        );
        gate.add_permits(1);
        job.await.unwrap().expect("job");
        assert_eq!(
            releases(&host),
            [RELEASE_TEXT],
            "no release for the expired wait"
        );
    }

    // A failed release command is reported, does not hand the GPU over,
    // and counts against the holder's retry budget (2): the third failure
    // (each one a re-send gap after the last) escalates the lane, and later
    // switches fail without running it.
    #[tokio::test]
    async fn a_failed_release_does_not_hand_the_gpu_over() {
        let (host, clock, activator) = gpu_group_activator();
        let gate = Arc::new(tokio::sync::Semaphore::new(1));
        media_job(&activator, gate, Arc::new(AtomicU64::new(0)))
            .await
            .unwrap()
            .expect("comfy holds the GPU");
        host.sim().fail_release = true;
        for attempt in 1..=3 {
            if attempt > 1 {
                clock.advance(60_000);
            }
            match acquire(&activator, "text").await.unwrap() {
                LaneAcquire::NotReady { detail, .. } => assert_eq!(
                    detail,
                    "release command of lane `comfy` failed: exit 1; gpu group `gpu0` not handed over",
                    "attempt {attempt}"
                ),
                LaneAcquire::Ready { .. } => panic!("handed over after a failed release"),
            }
        }
        let comfy = activator.executor("comfy").unwrap().report();
        assert_eq!(comfy.holds_gpu, Some(true), "the GPU stays with comfy");
        assert!(comfy.escalated, "{comfy:?}");
        assert_eq!(comfy.level, LaneLevel::Fail);
        assert!(
            comfy.detail.contains("release command failed 3 times"),
            "{}",
            comfy.detail
        );
        assert_eq!(
            activator.executor("text").unwrap().report().holds_gpu,
            Some(false)
        );

        let runs = releases(&host).len();
        match acquire(&activator, "text").await.unwrap() {
            LaneAcquire::NotReady { detail, .. } => {
                assert!(detail.contains("needs operator attention"), "{detail}")
            }
            LaneAcquire::Ready { .. } => panic!("handed over"),
        }
        assert_eq!(releases(&host).len(), runs, "not run again past the budget");
    }

    // A waiter that goes away while the holder's release command runs (its
    // client hung up) withdraws the switch: the holder keeps the GPU and its
    // work runs at once again; the next waiter runs the release again.
    #[tokio::test]
    async fn a_waiter_dropped_mid_release_leaves_the_holder_in_place() {
        let host = FakeHost::new(Sim {
            healthy: true,
            ..Default::default()
        });
        let clock = ManualClock::new();
        let activator = CapacityActivator::with_seams(
            &gpu_group_config(),
            clock.clone(),
            Arc::new(HangOnce {
                host: host.clone(),
                kind: RELEASE_COMFY,
                hung: Arc::new(AtomicBool::new(false)),
            }),
            Arc::new(host.clone()),
        );
        let gate = Arc::new(tokio::sync::Semaphore::new(2));
        media_job(&activator, gate.clone(), Arc::new(AtomicU64::new(0)))
            .await
            .unwrap()
            .expect("comfy holds the GPU");

        let request = acquire(&activator, "text");
        settle().await;
        assert!(!request.is_finished(), "the release command hangs");
        request.abort();
        let _ = request.await;
        settle().await;
        let text = activator.executor("text").unwrap().report();
        assert_eq!((text.holds_gpu, text.queue_depth), (Some(false), 0));
        assert_eq!(
            activator.executor("comfy").unwrap().report().holds_gpu,
            Some(true),
            "the holder stays"
        );
        let (_, note) = media_job(&activator, gate, Arc::new(AtomicU64::new(0)))
            .await
            .unwrap()
            .expect("the holder's work runs at once");
        assert_eq!(note.as_deref(), Some("holds gpu group `gpu0`"));

        let (_busy, detail) = ready(acquire(&activator, "text").await.unwrap());
        assert!(detail.ends_with("took it from lane `comfy`"), "{detail}");
        assert_eq!(releases(&host), [RELEASE_TEXT, RELEASE_COMFY]);
    }

    // A reload that removes the lane holding the GPU: its release command
    // left with it, so the next taker gets the GPU without running one (the
    // removed engine is no longer Switchback's to unload).
    #[tokio::test]
    async fn a_lane_removed_while_it_holds_the_gpu_hands_it_over_without_a_release() {
        let (host, _clock, activator) = gpu_group_activator();
        media_job(
            &activator,
            Arc::new(tokio::sync::Semaphore::new(1)),
            Arc::new(AtomicU64::new(0)),
        )
        .await
        .unwrap()
        .expect("comfy holds the GPU");
        let mut without_comfy = gpu_group_config();
        without_comfy.local_executors.truncate(1);
        activator.reconcile(&without_comfy);
        assert!(activator.executor("comfy").is_none());

        let (_busy, detail) = ready(acquire(&activator, "text").await.unwrap());
        assert!(detail.ends_with("took it from lane `comfy`"), "{detail}");
        assert_eq!(releases(&host), [RELEASE_TEXT], "no release could run");
        assert_eq!(
            activator.executor("text").unwrap().report().holds_gpu,
            Some(true)
        );
    }

    // Past its budget a stuck release stays loud: a media job clears the
    // lane's escalation, the next refused switch raises it again. A power-off
    // restarts the engines, so the next boot gets a fresh budget and the
    // first taker (holder unknown) runs the release again.
    #[tokio::test]
    async fn a_stuck_release_stays_loud_and_a_poweroff_gives_it_a_fresh_budget() {
        let (host, clock, activator) = gpu_group_activator();
        let gate = Arc::new(tokio::sync::Semaphore::new(2));
        media_job(&activator, gate.clone(), Arc::new(AtomicU64::new(0)))
            .await
            .unwrap()
            .expect("comfy holds the GPU");
        host.sim().fail_release = true;
        for _ in 1..=3 {
            assert!(matches!(
                acquire(&activator, "text").await.unwrap(),
                LaneAcquire::NotReady { .. }
            ));
            clock.advance(60_000);
        }
        media_job(&activator, gate, Arc::new(AtomicU64::new(0)))
            .await
            .unwrap()
            .expect("the holder's work runs");
        assert!(!activator.executor("comfy").unwrap().report().escalated);
        assert!(matches!(
            acquire(&activator, "text").await.unwrap(),
            LaneAcquire::NotReady { .. }
        ));
        let comfy = activator.executor("comfy").unwrap().report();
        assert!(comfy.escalated, "loud again: {comfy:?}");
        assert_eq!(comfy.level, LaneLevel::Fail);

        host.sim().fail_release = false;
        clock.advance(5_000);
        activator.tick_all().await;
        assert_eq!(host.sim().poweroff, 1);
        assert_eq!(
            activator.executor("comfy").unwrap().report().holds_gpu,
            Some(false),
            "nobody is known to hold it after a power-off"
        );
        let (_busy, detail) = ready(acquire(&activator, "text").await.unwrap());
        assert!(
            detail.ends_with("took it (holder unknown, released lane `comfy`)"),
            "{detail}"
        );
    }

    // Opt-out: lanes without `exclusive_group` never touch a GPU group and
    // never run a release command.
    #[tokio::test]
    async fn lanes_without_a_gpu_group_never_wait_or_release() {
        use sb_runtime::LaneStatus;
        let host = FakeHost::new(Sim {
            healthy: true,
            ..Default::default()
        });
        let activator = shared_activator(&shared_host_config(""), &host, ManualClock::new());
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let job = media_job(&activator, gate.clone(), Arc::new(AtomicU64::new(0)));
        settle().await;
        let request = activator.try_begin_request("text").expect("never refused");
        assert!(request.is_some(), "counted, as before");
        assert!(matches!(
            activator.acquire_for_request("text").await,
            LaneAcquire::Ready { detail: None, .. }
        ));
        gate.add_permits(1);
        let (_, note) = job.await.unwrap().expect("job");
        assert_eq!(note, None);
        assert!(releases(&host).is_empty());
        let host_of = activator.executor("text").unwrap().host();
        assert!(host_of.groups().is_empty(), "no group state");
        assert!(activator.reports().iter().all(|r| r.gpu_group.is_none()));
    }

    // --- Leftovers from the rehearsal ---------------------------------------

    fn comfy_releases(host: &FakeHost) -> usize {
        releases(host)
            .iter()
            .filter(|run| *run == RELEASE_COMFY)
            .count()
    }

    // A burst of requests right after a failed release runs it once per
    // re-send gap (60 s here), so it cannot spend the holder's retry budget
    // within seconds; after the gap it runs again.
    #[tokio::test]
    async fn a_burst_after_a_failed_release_runs_it_once_per_gap() {
        let (host, clock, activator) = gpu_group_activator();
        media_job(
            &activator,
            Arc::new(tokio::sync::Semaphore::new(1)),
            Arc::new(AtomicU64::new(0)),
        )
        .await
        .unwrap()
        .expect("comfy holds the GPU");
        host.sim().fail_release = true;
        let mut details = Vec::new();
        for _ in 0..5 {
            match acquire(&activator, "text").await.unwrap() {
                LaneAcquire::NotReady { detail, .. } => details.push(detail),
                LaneAcquire::Ready { .. } => panic!("handed over after a failed release"),
            }
            clock.advance(2_000);
        }
        assert_eq!(comfy_releases(&host), 1, "one run in the burst");
        assert_eq!(
            details[1],
            "release command of lane `comfy` failed 1 time(s); next try in 58 s; gpu group `gpu0` not handed over"
        );
        assert!(!activator.executor("comfy").unwrap().report().escalated);

        host.sim().fail_release = false;
        clock.advance(50_000);
        let (_busy, detail) = ready(acquire(&activator, "text").await.unwrap());
        assert!(detail.ends_with("took it from lane `comfy`"), "{detail}");
        assert_eq!(comfy_releases(&host), 2);
    }

    // A failed job leaves its error on the lane; a clean power-off is a
    // fresh start and clears it.
    #[tokio::test]
    async fn a_clean_poweroff_clears_the_error_of_a_failed_job() {
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            poweroff_makes_unhealthy: true,
            ..Default::default()
        });
        let clock = FakeClock::new();
        let executor = build(&host, clock.clone(), Legs::default());
        let result = executor
            .run_job(|_attempt| async { Err::<(), _>("comfyui prompt failed".to_string()) })
            .await;
        assert!(matches!(result, Err(JobError::Dispatch(_))));
        assert_eq!(
            executor.report().last_error.as_deref(),
            Some("dispatch failed: comfyui prompt failed")
        );
        clock.advance(5_000);
        executor.tick().await;
        assert_eq!(host.sim().poweroff, 1);
        assert_eq!(executor.report().last_error, None);
    }

    #[derive(Clone, Default)]
    struct LogSink(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // The switch log line names the lane that gives the GPU up as `lane`,
    // and the lane that takes it as `taker`.
    #[tokio::test]
    async fn the_release_log_names_the_lane_that_gives_the_gpu_up() {
        // Scoped to this test: `#[tokio::test]` runs the switch on this one
        // thread. A global subscriber would collect every other test's
        // lines for the rest of the run. With a single dispatcher registered,
        // tracing computes a log site's interest with the default of the
        // thread that hits it first (another test's, which takes nothing),
        // so a second one is kept alive, and the interest cache is rebuilt
        // for the log sites other tests registered before.
        let _second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::new());
        let sink = LogSink::default();
        let writer = sink.clone();
        let _logs = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_max_level(tracing::Level::INFO)
                .with_writer(move || writer.clone())
                .finish(),
        );
        tracing::callsite::rebuild_interest_cache();
        let (_host, _clock, activator) = gpu_group_activator();
        media_job(
            &activator,
            Arc::new(tokio::sync::Semaphore::new(1)),
            Arc::new(AtomicU64::new(0)),
        )
        .await
        .unwrap()
        .expect("comfy holds the GPU");
        let (_busy, _) = ready(acquire(&activator, "text").await.unwrap());
        let logged = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        let switch = logged
            .lines()
            .find(|line| line.contains("releasing its holder") && line.contains("taker=text"))
            .unwrap_or_else(|| panic!("no switch line in {logged}"));
        assert!(switch.contains("lane=comfy"), "{switch}");
    }

    // --- A ComfyUI lane behind a token proxy ------------------------------

    /// A health endpoint behind a token proxy: up with a credential, else
    /// refused.
    struct ProxyProbe;

    #[async_trait]
    impl HealthProbe for ProxyProbe {
        async fn healthy(&self, _endpoint: &str) -> bool {
            false
        }
        async fn check(
            &self,
            _endpoint: &str,
            credential: Option<&SchemeCredential>,
        ) -> ProbeAnswer {
            match credential {
                Some(_) => ProbeAnswer::Up,
                None => ProbeAnswer::Refused(401),
            }
        }
    }

    // A health endpoint that redirects to another origin never gets the
    // probe's credential forwarded there (reqwest would forward a custom
    // header); without a credential the probe follows redirects as before.
    #[tokio::test]
    async fn a_credentialed_probe_follows_no_redirect() {
        const SECRET: &str = "probe-secret-not-real-41";
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let log = seen.clone();
        let other = axum::Router::new().fallback(move |request: axum::extract::Request| {
            let log = log.clone();
            async move {
                log.lock()
                    .unwrap()
                    .push(format!("{} {:?}", request.uri(), request.headers()));
                "ok"
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let other_url = format!("http://{}/system_stats", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, other).await.unwrap() });
        let redirecting = axum::Router::new().route(
            "/system_stats",
            axum::routing::get(move || {
                let to = other_url.clone();
                async move { axum::response::Redirect::temporary(&to) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/system_stats", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, redirecting).await.unwrap() });

        let probe = HttpHealthProbe::new(Duration::from_secs(3));
        let credential = SchemeCredential::from_lease(
            sb_core::AuthScheme::Header {
                name: "x-comfy-key".into(),
            },
            sb_core::CredentialLease::bearer("default", SECRET),
        );
        assert_eq!(
            probe.check(&endpoint, credential.as_ref()).await,
            ProbeAnswer::Down
        );
        assert!(
            seen.lock().unwrap().is_empty(),
            "{:?}",
            seen.lock().unwrap()
        );
        assert_eq!(probe.check(&endpoint, None).await, ProbeAnswer::Up);
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(!seen[0].contains(SECRET), "{seen:?}");
    }

    struct OneKey;

    #[async_trait]
    impl LaneCredentials for OneKey {
        async fn credential(&self, _provider_id: &str) -> Option<SchemeCredential> {
            SchemeCredential::from_lease(
                sb_core::AuthScheme::Bearer,
                sb_core::CredentialLease::bearer("default", "proxy-key"),
            )
        }
    }

    fn comfy_lane_config(health_host: &str) -> Config {
        Config::from_yaml(&format!(
            r#"
providers:
  - id: comfy
    type: comfyui
    base_url: "http://executor-1.example:8188"
    api_key_env: COMFY_PROXY_KEY
  - id: text
    type: openai_compatible
    base_url: "http://executor-1.example:8080/v1"
local_executors:
  - name: comfy
    base_url: "http://executor-1.example:8188"
    health_endpoint: "http://{health_host}/system_stats"
    wake_command: "ssh gateway wake-host executor-1"
  - name: text
    base_url: "http://executor-1.example:8080"
    health_endpoint: "http://executor-1.example:8080/health"
    wake_command: "ssh gateway wake-host executor-1"
"#
        ))
        .unwrap()
    }

    // A refused probe means the engine answers but wants the credential:
    // the job fails at once and loudly, with no wake against a running
    // machine. With the credential the same lane is used.
    #[tokio::test]
    async fn a_refused_probe_fails_at_once_and_never_wakes() {
        let host = FakeHost::new(Sim::default());
        let cfg = comfy_lane_config("executor-1.example:8188");
        let activator = CapacityActivator::with_seams(
            &cfg,
            FakeClock::new(),
            Arc::new(host.clone()),
            Arc::new(ProxyProbe),
        );
        let comfy = activator.executor("comfy").unwrap();
        match comfy.ensure_ready().await {
            Err(EnsureError::Refused(detail)) => assert_eq!(
                detail,
                "health endpoint answered 401: the engine refuses the credential (missing or wrong; set api_key_env and auth_scheme on provider `comfy`)"
            ),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert_eq!(host.sim().wake, 0, "no wake for an engine that answers");
        assert_eq!(comfy.report().level, LaneLevel::Fail);

        activator.attach_credentials(Arc::new(OneKey));
        drop(
            comfy
                .ensure_ready()
                .await
                .expect("the probe carries the key"),
        );
        assert_eq!(host.sim().wake, 0);
    }

    // The credential goes only to a health endpoint on the provider's own
    // origin, and a lane whose provider carries none (a text engine) reads
    // a 401 as down, as before.
    #[test]
    fn the_probe_credential_stays_on_the_provider_origin() {
        let same = comfy_lane_config("executor-1.example:8188");
        let other = comfy_lane_config("monitor.example:8188");
        assert_eq!(
            probe_auth_for(&same, &same.local_executors[0]),
            ProbeAuth {
                reads_refusal: true,
                sends_credential: true
            }
        );
        assert_eq!(
            probe_auth_for(&other, &other.local_executors[0]),
            ProbeAuth {
                reads_refusal: true,
                sends_credential: false
            }
        );
        assert_eq!(
            probe_auth_for(&same, &same.local_executors[1]),
            ProbeAuth::default()
        );
    }

    // --- No wake for a machine that is already up ---------------------------

    const TEXT_HEALTH: &str = "http://executor-1.example:8080/health";
    const COMFY_HEALTH: &str = "http://executor-1.example:8188/system_stats";

    fn wakes_and_restarts(host: &FakeHost) -> (u32, u32) {
        let sim = host.sim();
        (sim.wake, sim.restart)
    }

    // Seen live: the text lane woke the shared machine, ComfyUI was not
    // answering yet, and the ComfyUI lane ran the wake command again 6 s
    // later. Now it only waits for its own engine.
    #[tokio::test]
    async fn a_lane_whose_host_another_lane_woke_waits_for_its_engine_without_a_wake() {
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            poweroff_makes_unhealthy: true,
            fail_probes: BTreeMap::from([(COMFY_HEALTH.to_string(), 3)]),
            ..Default::default()
        });
        let clock = FakeClock::new();
        let activator = shared_activator(&shared_host_config(""), &host, clock.clone());
        let text = activator.executor("text").unwrap();
        let comfy = activator.executor("comfy").unwrap();
        drop(
            text.ensure_ready()
                .await
                .expect("the text lane wakes the machine"),
        );
        assert_eq!(host.sim().wake, 1);

        clock.advance(6_000); // ComfyUI is still starting
        let job = comfy.ensure_ready().await.expect("comfy comes up");
        assert_eq!(
            wakes_and_restarts(&host),
            (1, 0),
            "no wake for a machine that is up"
        );
        let result = comfy.report().last_wake_result.unwrap();
        assert_eq!(
            result,
            "up without a wake after 4 s (host `gpu-box` was woken 6 s ago)"
        );
        drop(job);

        // The power cycle ends with our power-off: the next lane that needs
        // the machine wakes it again.
        clock.advance(5_000);
        activator.tick_all().await;
        assert_eq!(host.sim().poweroff, 1);
        drop(comfy.ensure_ready().await.expect("woken again"));
        assert_eq!(host.sim().wake, 2, "a new power cycle starts with a wake");
    }

    // The other order: ComfyUI woke the machine long ago; a text request
    // finds its engine not answering yet. The other lane answers, so the
    // request waits for the text engine and sends no wake.
    #[tokio::test]
    async fn a_text_request_on_a_host_that_answers_sends_no_wake() {
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            fail_probes: BTreeMap::from([(TEXT_HEALTH.to_string(), 2)]),
            ..Default::default()
        });
        let clock = ManualClock::new();
        let activator = shared_activator(&shared_host_config(""), &host, clock.clone());
        let text = activator.executor("text").unwrap();
        let job = activator
            .executor("comfy")
            .unwrap()
            .ensure_ready()
            .await
            .expect("comfy wakes the machine");
        assert_eq!(host.sim().wake, 1);
        clock.advance(200_000); // past the boot timeout: only a probe tells

        let wake = text.wake_for_request().await;
        assert!(matches!(wake, LaneWake::Ready { .. }), "{wake:?}");
        assert_eq!(wakes_and_restarts(&host), (1, 0));
        assert_eq!(
            text.report().last_wake_result.as_deref(),
            Some("up without a wake after 0 ms (lane `comfy` of host `gpu-box` answers)")
        );
        drop(job);
    }

    // A lane whose engine does not answer on a running machine gets its
    // restart_command, never the wake command; without one it fails like a
    // boot timeout and says why.
    #[tokio::test]
    async fn an_engine_that_stays_down_on_a_running_host_is_restarted_not_woken() {
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            restart_makes_healthy: true,
            fail_probes: BTreeMap::from([(COMFY_HEALTH.to_string(), u32::MAX)]),
            ..Default::default()
        });
        let clock = FakeClock::new();
        let mut cfg = shared_host_config("");
        cfg.local_executors[1].restart_command = Some("ssh gateway restart-host comfy".into());
        let activator = shared_activator(&cfg, &host, clock.clone());
        drop(
            activator
                .executor("text")
                .unwrap()
                .ensure_ready()
                .await
                .expect("up"),
        );
        let comfy = activator.executor("comfy").unwrap();
        drop(comfy.ensure_ready().await.expect("up after its restart"));
        assert_eq!(wakes_and_restarts(&host), (1, 1));

        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            fail_probes: BTreeMap::from([(COMFY_HEALTH.to_string(), u32::MAX)]),
            ..Default::default()
        });
        let activator = shared_activator(&shared_host_config(""), &host, FakeClock::new());
        drop(
            activator
                .executor("text")
                .unwrap()
                .ensure_ready()
                .await
                .expect("up"),
        );
        let comfy = activator.executor("comfy").unwrap();
        match comfy.ensure_ready().await {
            Err(EnsureError::BootTimeout { wake_path, .. }) => {
                assert_eq!(wake_path, "none (host already up)")
            }
            other => panic!("expected a boot timeout, got {other:?}"),
        }
        assert_eq!(wakes_and_restarts(&host), (1, 0), "never the wake command");
        let report = comfy.report();
        assert!(report.escalated);
        assert!(
            report
                .last_error
                .as_deref()
                .unwrap()
                .ends_with("; no restart_command to try; no wake command sent"),
            "{report:?}"
        );
    }

    // Within one waking cycle the wake is re-sent on its interval (lossy
    // transports), but not while another lane of the machine answers.
    #[tokio::test]
    async fn the_wake_is_not_re_sent_while_another_lane_of_the_host_answers() {
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            fail_probes: BTreeMap::from([(TEXT_HEALTH.to_string(), 40)]),
            ..Default::default()
        });
        let clock = FakeClock::new();
        let activator = shared_activator(&shared_host_config(""), &host, clock.clone());
        let text = activator.executor("text").unwrap();
        drop(text.ensure_ready().await.expect("up after about 80 s"));
        assert_eq!(host.sim().wake, 1, "the 60 s re-send was skipped");
        assert_eq!(
            text.report().last_wake_result.as_deref(),
            Some("ok after 1 attempt(s)")
        );
    }

    /// The shared machine's probes as a fake host answers them, except that
    /// ComfyUI sits behind a token proxy and refuses a probe without one.
    struct ComfyBehindProxy(FakeHost);

    #[async_trait]
    impl HealthProbe for ComfyBehindProxy {
        async fn healthy(&self, endpoint: &str) -> bool {
            self.0.healthy(endpoint).await
        }
        async fn check(
            &self,
            endpoint: &str,
            _credential: Option<&SchemeCredential>,
        ) -> ProbeAnswer {
            match (endpoint == COMFY_HEALTH, self.0.healthy(endpoint).await) {
                (true, true) => ProbeAnswer::Refused(401),
                (_, true) => ProbeAnswer::Up,
                (_, false) => ProbeAnswer::Down,
            }
        }
    }

    // A lane whose probe is refused still shows that its machine is up: the
    // text lane waits for its engine and sends no wake.
    #[tokio::test]
    async fn a_refused_probe_on_another_lane_still_shows_the_host_is_up() {
        let host = FakeHost::new(Sim {
            healthy: true,
            fail_probes: BTreeMap::from([(TEXT_HEALTH.to_string(), 2)]),
            ..Default::default()
        });
        let activator = CapacityActivator::with_seams(
            &shared_host_config(""),
            FakeClock::new(),
            Arc::new(host.clone()),
            Arc::new(ComfyBehindProxy(host.clone())),
        );
        let text = activator.executor("text").unwrap();
        drop(text.ensure_ready().await.expect("up without a wake"));
        assert_eq!(host.sim().wake, 0);
        assert_eq!(
            text.report().last_wake_result.as_deref(),
            Some("up without a wake after 2 s (lane `comfy` of host `gpu-box` answers)")
        );
    }

    // Opt-out: lanes without `host` keep a machine each and wake it as
    // before, whatever another lane did.
    #[tokio::test]
    async fn a_lane_without_a_host_still_sends_its_own_wake() {
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            fail_probes: BTreeMap::from([(COMFY_HEALTH.to_string(), 1)]),
            ..Default::default()
        });
        let mut cfg = shared_host_config("");
        for lane in &mut cfg.local_executors {
            lane.host = None;
            lane.wake_command = Some("ssh gateway wake-host executor-1".to_string());
        }
        let activator = shared_activator(&cfg, &host, FakeClock::new());
        drop(
            activator
                .executor("text")
                .unwrap()
                .ensure_ready()
                .await
                .expect("up"),
        );
        drop(
            activator
                .executor("comfy")
                .unwrap()
                .ensure_ready()
                .await
                .expect("up"),
        );
        assert_eq!(host.sim().wake, 2, "each lane wakes its own machine");
    }

    // --- Power-off guard (`poweroff_guard_command`) -------------------------

    const GUARD_FIELDS: [&str; 3] = [
        "poweroff_guard_busy",
        "poweroff_guard_busy_since_ms",
        "poweroff_guard_failures",
    ];

    /// A single lane with a guard: idle 5 s, gap 30 s, budget 2.
    fn guarded(host: &FakeHost, clock: Arc<FakeClock>) -> Arc<LocalExecutor> {
        build(
            host,
            clock,
            Legs {
                guard: Some(GUARD),
                ..Legs::default()
            },
        )
    }

    fn guard_runs_and_poweroffs(host: &FakeHost) -> (u32, u32) {
        let sim = host.sim();
        (sim.guard_runs, sim.poweroff)
    }

    // A guard that reports the machine free lets the idle power-off run:
    // one question, one power-off, and the report shows nothing new.
    #[tokio::test]
    async fn a_free_guard_lets_the_idle_poweroff_run_once() {
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            poweroff_makes_unhealthy: true,
            ..Default::default()
        });
        let clock = FakeClock::new();
        let executor = guarded(&host, clock.clone());
        drop(executor.ensure_ready().await.expect("capacity"));
        clock.advance(4_000);
        executor.tick().await;
        assert_eq!(guard_runs_and_poweroffs(&host), (0, 0), "not before idle");
        clock.advance(1_000);
        executor.tick().await;
        executor.tick().await;
        assert_eq!(guard_runs_and_poweroffs(&host), (1, 1));
        let report = executor.report();
        assert_eq!(
            (report.state, report.level),
            (LaneState::Offline, LaneLevel::Ok)
        );
        let json = serde_json::to_value(&report).unwrap();
        for absent in GUARD_FIELDS {
            assert!(json.get(absent).is_none(), "{absent} absent: {json}");
        }
    }

    // Busy: no power-off, the machine stays up, the report says so, and
    // the guard is asked again only after the gap. Free then powers off.
    #[tokio::test]
    async fn a_busy_guard_postpones_the_poweroff_until_it_reports_free() {
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            poweroff_makes_unhealthy: true,
            guard: GuardAnswer::Busy,
            ..Default::default()
        });
        let clock = FakeClock::new();
        let executor = guarded(&host, clock.clone());
        drop(executor.ensure_ready().await.expect("capacity"));
        clock.advance(5_000);
        executor.tick().await;
        assert_eq!(guard_runs_and_poweroffs(&host), (1, 0));
        let report = executor.report();
        assert_eq!(report.state, LaneState::IdleCountdown);
        assert_eq!(report.level, LaneLevel::Ok, "busy is not an error");
        assert_eq!(report.poweroff_guard_busy, 1);
        assert_eq!(report.poweroff_guard_busy_since_ms, Some(5_000));
        assert_eq!(
            report.detail,
            "comfy-local idle; power-off postponed: guard reports the machine busy (1 answers in a row, busy for 0 s)"
        );

        executor.tick().await;
        clock.advance(29_000);
        executor.tick().await;
        assert_eq!(
            guard_runs_and_poweroffs(&host),
            (1, 0),
            "not inside the gap"
        );
        clock.advance(1_000);
        executor.tick().await;
        assert_eq!(guard_runs_and_poweroffs(&host), (2, 0));
        assert_eq!(executor.report().poweroff_guard_busy_since_ms, Some(5_000));

        host.sim().guard = GuardAnswer::Free;
        clock.advance(30_000);
        executor.tick().await;
        assert_eq!(guard_runs_and_poweroffs(&host), (3, 1));
        let report = executor.report();
        assert_eq!(report.state, LaneState::Offline);
        assert_eq!(
            (
                report.poweroff_guard_busy,
                report.poweroff_guard_busy_since_ms
            ),
            (0, None)
        );
    }

    // Busy for hours is still not an error: level ok, with a count and
    // since when, and never a power-off.
    #[tokio::test]
    async fn a_guard_busy_for_hours_stays_ok_and_says_since_when() {
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            guard: GuardAnswer::Busy,
            ..Default::default()
        });
        let clock = FakeClock::new();
        let executor = guarded(&host, clock.clone());
        drop(executor.ensure_ready().await.expect("capacity"));
        clock.advance(5_000);
        executor.tick().await;
        for _ in 0..240 {
            clock.advance(30_000);
            executor.tick().await;
        }
        assert_eq!(guard_runs_and_poweroffs(&host), (241, 0));
        let report = executor.report();
        assert_eq!(
            (report.state, report.level),
            (LaneState::IdleCountdown, LaneLevel::Ok)
        );
        assert!(
            report
                .detail
                .ends_with("guard reports the machine busy (241 answers in a row, busy for 2 h)"),
            "{}",
            report.detail
        );
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["poweroff_guard_busy"], 241);
        assert_eq!(json["poweroff_guard_busy_since_ms"], 5_000);
        assert!(json.get("poweroff_guard_failures").is_none(), "{json}");
    }

    // A guard that cannot run is never "free": no power-off ever. After
    // `retry_budget` (2) failures in a row the lane is `warn` and says the
    // guard is broken; new work does not hide it. An answer clears it.
    #[tokio::test]
    async fn a_broken_guard_goes_loud_and_never_powers_off() {
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            poweroff_makes_unhealthy: true,
            guard: GuardAnswer::Broken,
            ..Default::default()
        });
        let clock = FakeClock::new();
        let executor = guarded(&host, clock.clone());
        drop(executor.ensure_ready().await.expect("capacity"));
        clock.advance(5_000);
        executor.tick().await;
        let report = executor.report();
        assert_eq!(guard_runs_and_poweroffs(&host), (1, 0));
        assert_eq!(
            (report.level, report.poweroff_guard_failures),
            (LaneLevel::Ok, 1)
        );

        clock.advance(30_000);
        executor.tick().await;
        let report = executor.report();
        assert_eq!(
            (report.level, report.poweroff_guard_failures),
            (LaneLevel::Warn, 2)
        );
        assert_eq!(
            report.detail,
            "comfy-local power-off guard is broken: no answer 2 times in a row (last: exit 127: the shell could not run it); the machine stays on until it answers"
        );

        let job = executor.ensure_ready().await.expect("new work");
        assert_eq!(
            executor.report().level,
            LaneLevel::Warn,
            "work does not hide it"
        );
        drop(job);
        for _ in 0..100 {
            clock.advance(30_000);
            executor.tick().await;
        }
        assert_eq!(
            guard_runs_and_poweroffs(&host),
            (102, 0),
            "never a power-off"
        );
        assert_eq!(executor.report().poweroff_guard_failures, 102);

        host.sim().guard = GuardAnswer::Free;
        clock.advance(30_000);
        executor.tick().await;
        assert_eq!(guard_runs_and_poweroffs(&host), (103, 1));
        let report = executor.report();
        assert_eq!(
            (report.level, report.poweroff_guard_failures),
            (LaneLevel::Ok, 0)
        );
    }

    // The guard's answers belong to its command. An operator who fixes a
    // broken guard and reloads sees the lane `ok` at once, and the fixed
    // guard is asked at the next idle tick, not after the old one's
    // back-off. A guard that a reload removed and a later one re-adds
    // starts afresh too.
    #[tokio::test]
    async fn a_reload_that_changes_the_guard_starts_its_answers_afresh() {
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            poweroff_makes_unhealthy: true,
            guard: GuardAnswer::Broken,
            ..Default::default()
        });
        let clock = FakeClock::new();
        let executor = guarded(&host, clock.clone());
        let reload = |guard: Option<String>| {
            let mut cfg = (*executor.cfg()).clone();
            cfg.poweroff_guard_command = guard;
            executor.update_config(cfg, executor.is_routed());
        };
        drop(executor.ensure_ready().await.expect("capacity"));
        clock.advance(5_000);
        executor.tick().await;
        clock.advance(30_000);
        executor.tick().await;
        assert_eq!(executor.report().level, LaneLevel::Warn);

        reload(None);
        assert_eq!(executor.report().poweroff_guard_failures, 0);
        reload(Some(GUARD.to_string()));
        let report = executor.report();
        assert_eq!(
            (report.level, report.poweroff_guard_failures),
            (LaneLevel::Ok, 0),
            "re-added: {}",
            report.detail
        );
        clock.advance(1_000);
        executor.tick().await;
        clock.advance(30_000);
        executor.tick().await;
        assert_eq!(executor.report().level, LaneLevel::Warn, "still broken");
        assert_eq!(guard_runs_and_poweroffs(&host), (4, 0));

        reload(Some(format!("{GUARD} --fixed")));
        host.sim().guard = GuardAnswer::Free;
        let report = executor.report();
        assert_eq!(
            (report.level, report.poweroff_guard_failures),
            (LaneLevel::Ok, 0),
            "fixed: {}",
            report.detail
        );
        assert!(!report.detail.contains("broken"), "{}", report.detail);
        clock.advance(1_000);
        executor.tick().await;
        assert_eq!(
            guard_runs_and_poweroffs(&host),
            (5, 1),
            "the fixed guard is asked at once"
        );
    }

    /// A runner whose power-off guard never returns (a hung ssh session).
    struct HangingGuard(FakeHost);

    #[async_trait]
    impl CommandRunner for HangingGuard {
        async fn run(&self, command: &str) -> Result<(), String> {
            self.0.run(command).await
        }
        async fn exit_code(&self, command: &str) -> Result<i32, String> {
            if command.contains("check-idle") {
                std::future::pending::<()>().await;
            }
            self.0.exit_code(command).await
        }
    }

    // A guard that hangs is stopped at `command_timeout_secs` and counts as
    // no answer: no power-off.
    #[tokio::test]
    async fn a_hung_guard_times_out_and_is_not_taken_for_free() {
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            ..Default::default()
        });
        let clock = FakeClock::new();
        let cfg = (*guarded(&host, clock.clone()).cfg()).clone();
        let executor = LocalExecutor::new(
            cfg,
            clock.clone(),
            Arc::new(HangingGuard(host.clone())),
            Arc::new(host.clone()),
            true,
        );
        drop(executor.ensure_ready().await.expect("capacity"));
        clock.advance(5_000);
        executor.tick().await;
        assert_eq!(host.sim().poweroff, 0);
        let report = executor.report();
        assert_eq!(report.poweroff_guard_failures, 1);
        assert_eq!(report.state, LaneState::IdleCountdown);
        clock.advance(30_000);
        executor.tick().await;
        let report = executor.report();
        assert_eq!(report.level, LaneLevel::Warn);
        assert!(
            report.detail.contains("(last: timed out after 60 s)"),
            "{}",
            report.detail
        );
        assert_eq!(host.sim().poweroff, 0);
    }

    /// A guard that says "free" while, at that moment, a client starts a
    /// text request on the shared machine.
    struct WorkWhileAsked {
        host: FakeHost,
        activator: OnceLock<Weak<CapacityActivator>>,
        request: Mutex<Option<LaneBusy>>,
    }

    #[async_trait]
    impl CommandRunner for WorkWhileAsked {
        async fn run(&self, command: &str) -> Result<(), String> {
            self.host.run(command).await
        }
        async fn exit_code(&self, command: &str) -> Result<i32, String> {
            let first = self.host.sim().guard_runs == 0;
            if command.contains("check-idle") && first {
                let activator = self.activator.get().and_then(Weak::upgrade).unwrap();
                let request = sb_runtime::LaneStatus::begin_request(&*activator, "text");
                *self.request.lock().unwrap() = request;
            }
            self.host.exit_code(command).await
        }
    }

    // On a shared machine the guard may sit on any lane and is asked once
    // per power-off attempt. Work that starts while it runs is caught by
    // the re-check: no power-off under it; once it ends, the next pass asks
    // again and powers off.
    #[tokio::test]
    async fn work_that_starts_while_the_guard_runs_holds_the_poweroff_off() {
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            ..Default::default()
        });
        let clock = FakeClock::new();
        let mut cfg = shared_host_config("");
        cfg.local_executors[1].poweroff_guard_command = Some(GUARD.to_string());
        let runner = Arc::new(WorkWhileAsked {
            host: host.clone(),
            activator: OnceLock::new(),
            request: Mutex::new(None),
        });
        let activator = CapacityActivator::with_seams(
            &cfg,
            clock.clone(),
            runner.clone(),
            Arc::new(host.clone()),
        );
        runner
            .activator
            .set(Arc::downgrade(&activator))
            .ok()
            .unwrap();
        let text = activator.executor("text").unwrap();
        drop(
            activator
                .executor("comfy")
                .unwrap()
                .ensure_ready()
                .await
                .expect("capacity"),
        );
        text.observe().await;
        clock.advance(5_000);
        activator.tick_all().await;
        assert_eq!(
            guard_runs_and_poweroffs(&host),
            (1, 0),
            "the re-check saw the request"
        );
        assert_eq!(text.report().state, LaneState::Draining);

        let request = runner.request.lock().unwrap().take();
        assert!(request.is_some(), "the request was counted");
        drop(request);
        clock.advance(4_000);
        activator.tick_all().await;
        assert_eq!(
            guard_runs_and_poweroffs(&host),
            (1, 0),
            "idle timer from its end"
        );
        clock.advance(1_000);
        activator.tick_all().await;
        assert_eq!(
            guard_runs_and_poweroffs(&host),
            (2, 1),
            "one question, one power-off"
        );
        assert_eq!(text.report().state, LaneState::Offline);
    }

    // The real runner gives the guard's exit code as its answer, and the
    // shell's own "cannot run" codes (126, 127) as no answer.
    #[tokio::test]
    async fn the_shell_runner_reads_a_guard_answer_from_its_exit_code() {
        let shell = ShellCommandRunner;
        assert_eq!(shell.exit_code("exit 0").await, Ok(0));
        assert_eq!(shell.exit_code("exit 3").await, Ok(3));
        let missing = shell
            .exit_code("switchback-test-no-such-command-here")
            .await
            .unwrap_err();
        assert_eq!(missing, "exit 127: the shell could not run it");
    }

    // Opt-out: without a guard nothing is asked and the report is as before.
    #[tokio::test]
    async fn without_a_guard_the_poweroff_runs_as_before() {
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            guard: GuardAnswer::Busy,
            ..Default::default()
        });
        let clock = FakeClock::new();
        let executor = build(&host, clock.clone(), Legs::default());
        drop(executor.ensure_ready().await.expect("capacity"));
        clock.advance(5_000);
        executor.tick().await;
        assert_eq!(guard_runs_and_poweroffs(&host), (0, 1));
        let json = serde_json::to_value(executor.report()).unwrap();
        for absent in GUARD_FIELDS {
            assert!(json.get(absent).is_none(), "{absent} absent: {json}");
        }
    }

    // --- Host holds (`/v1/hosts/{host}/holds`) -------------------------------

    /// A hold of tenant `agents` on `host`, taken as the HTTP handler does.
    fn hold(activator: &CapacityActivator, host: &str, ttl_ms: u64, wake: bool) -> Hold {
        activator
            .take_hold(
                host,
                "benchmark",
                ttl_ms,
                (Some("agents".to_string()), None),
                wake,
            )
            .expect("under the cap")
            .expect("a managed machine")
    }

    fn report_of(activator: &CapacityActivator, lane: &str) -> LocalLaneReport {
        activator
            .reports()
            .into_iter()
            .find(|report| report.name == lane)
            .expect("lane")
    }

    // A hold keeps an idle machine on far past its idle timeout. When it
    // ends the countdown runs from that moment, not from when the lanes
    // went idle, and the report counts it down.
    #[tokio::test]
    async fn a_hold_blocks_the_idle_poweroff_and_its_expiry_restarts_the_countdown() {
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            ..Default::default()
        });
        let clock = FakeClock::new();
        let activator = shared_activator(&shared_host_config(""), &host, clock.clone());
        let job = activator
            .executor("comfy")
            .unwrap()
            .ensure_ready()
            .await
            .expect("capacity");
        assert_eq!(
            report_of(&activator, "comfy").poweroff_in_secs,
            None,
            "busy"
        );
        drop(job);
        assert_eq!(report_of(&activator, "comfy").poweroff_in_secs, Some(5));
        let held = hold(&activator, "gpu-box", 60_000, false);
        assert_eq!(held.state, HoldState::Active);
        let report = report_of(&activator, "text");
        assert_eq!(report.poweroff_in_secs, None, "held");
        assert_eq!(report.holds.len(), 1, "both lanes show the machine's hold");
        assert_eq!(report.holds[0].owner.as_deref(), Some("agents"));

        clock.advance(59_000);
        activator.tick_all().await;
        assert_eq!(host.sim().poweroff, 0, "never while held");
        clock.advance(1_000); // the hold expires now
        activator.tick_all().await;
        assert_eq!(
            host.sim().poweroff,
            0,
            "the countdown starts at the hold's end"
        );
        let report = report_of(&activator, "comfy");
        assert!(report.holds.is_empty(), "an expired hold ends by itself");
        assert_eq!(report.poweroff_in_secs, Some(5));
        clock.advance(4_999);
        activator.tick_all().await;
        assert_eq!(host.sim().poweroff, 0);
        clock.advance(1);
        activator.tick_all().await;
        assert_eq!(host.sim().poweroff, 1, "the normal idle power-off");
        assert_eq!(report_of(&activator, "comfy").poweroff_in_secs, None, "off");
    }

    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("sb-{name}-{}-{nanos}", std::process::id()))
    }

    // A gateway restart with a state directory keeps the holds: the new run
    // does not power off a machine an agent holds. Without one, holds are
    // memory only and say so.
    #[tokio::test]
    async fn a_hold_survives_a_restart_with_a_state_directory() {
        let dir = scratch_dir("holds");
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            ..Default::default()
        });
        let clock = FakeClock::new();
        let cfg = shared_host_config("");
        let first = shared_activator(&cfg, &host, clock.clone());
        assert!(
            !first.holds().durable(),
            "memory only until a directory is given"
        );
        first.keep_holds_in(&dir);
        assert!(first.holds().durable());
        let held = hold(&first, "gpu-box", 3_600_000, false);
        drop(first);

        let second = shared_activator(&cfg, &host, clock.clone());
        second.keep_holds_in(&dir);
        assert_eq!(second.holds().list(None, clock.now_ms()), vec![held]);
        drop(
            second
                .executor("comfy")
                .unwrap()
                .ensure_ready()
                .await
                .expect("capacity"),
        );
        clock.advance(600_000);
        second.tick_all().await;
        assert_eq!(host.sim().poweroff, 0, "the held machine stays on");
    }

    // `wake: true` on a shared machine one of whose lanes already answers
    // sends no wake (a power toggle would switch it off); on a sleeping
    // machine it wakes it once, in the background.
    #[tokio::test]
    async fn a_hold_that_wakes_sends_no_wake_while_another_lane_of_the_host_answers() {
        let host = FakeHost::new(Sim {
            healthy: true,
            wake_makes_healthy: true,
            ..Default::default()
        });
        // The text engine (it carries the wake command) is down; the media
        // engine answers.
        host.sim()
            .fail_probes
            .insert("http://executor-1.example:8080/health".to_string(), 1_000);
        let clock = FakeClock::new();
        let activator = shared_activator(&shared_host_config(""), &host, clock.clone());
        let held = hold(&activator, "gpu-box", 60_000, true);
        assert_eq!(held.state, HoldState::Waking, "answers at once");
        settle().await;
        let now = activator
            .holds()
            .get("gpu-box", &held.id, clock.now_ms())
            .unwrap();
        assert_eq!(now.state, HoldState::Active);
        assert!(
            now.wake_detail
                .as_deref()
                .unwrap()
                .contains("lane `comfy` of host `gpu-box` answers"),
            "{now:?}"
        );
        assert_eq!(host.sim().wake, 0, "no wake against a running machine");

        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            ..Default::default()
        });
        let activator = shared_activator(&shared_host_config(""), &host, clock.clone());
        let held = hold(&activator, "gpu-box", 60_000, true);
        settle().await;
        assert_eq!(host.sim().wake, 1, "a sleeping machine is woken once");
        let now = activator
            .holds()
            .get("gpu-box", &held.id, clock.now_ms())
            .unwrap();
        assert_eq!(now.state, HoldState::Active, "{now:?}");
        assert!(activator
            .take_hold("no-such-box", "x", 1_000, (None, None), true)
            .unwrap()
            .is_none());
    }

    // Slice 5's open item: on a shared machine where one lane waits for the
    // GPU (the holder's release command still runs) while the other lane
    // counts down idle far past its timeout, the machine is not powered off
    // under the waiting lane; once that work is done it powers off as usual.
    #[tokio::test]
    async fn a_lane_waiting_for_the_gpu_keeps_the_machine_on_while_the_other_counts_down() {
        let (host, clock, activator) = gpu_group_activator();
        let gate = Arc::new(tokio::sync::Semaphore::new(1));
        media_job(&activator, gate, Arc::new(AtomicU64::new(0)))
            .await
            .unwrap()
            .expect("the job runs");
        let comfy = activator.executor("comfy").unwrap().report();
        assert_eq!(comfy.state, LaneState::IdleCountdown);
        assert_eq!(comfy.holds_gpu, Some(true));

        let release = Arc::new(tokio::sync::Semaphore::new(0));
        host.sim().release_gate = Some(release.clone());
        let request = acquire(&activator, "text");
        settle().await;
        assert!(!request.is_finished(), "waits while comfy's release runs");
        assert_eq!(activator.executor("text").unwrap().report().queue_depth, 1);
        clock.advance(20_000); // comfy idle four times its timeout
        activator.tick_all().await;
        assert_eq!(host.sim().poweroff, 0, "never under the waiting lane");
        assert_eq!(report_of(&activator, "comfy").poweroff_in_secs, None);

        release.add_permits(1);
        let (busy, _) = ready(request.await.unwrap());
        drop(busy);
        clock.advance(5_000);
        activator.tick_all().await;
        assert_eq!(host.sim().poweroff, 1, "powers off once the work is done");
    }

    // --- Machines woken outside Switchback -----------------------------------

    /// The machine of [`shared_host_config`], already on (woken by another
    /// tool); `extra` goes on both lanes.
    fn woken_outside(extra: &str) -> (FakeHost, Arc<FakeClock>, Arc<CapacityActivator>) {
        let host = FakeHost::new(Sim {
            healthy: true,
            ..Default::default()
        });
        let clock = FakeClock::new();
        let activator = shared_activator(&shared_host_config(extra), &host, clock.clone());
        (host, clock, activator)
    }

    /// Adoption as the config allows it: with a power-off guard (the guard
    /// of the fake host answers free unless a test says otherwise).
    const ADOPT_WITH_GUARD: &str =
        "    adopt_external_wake: true\n    poweroff_guard_command: \"ssh gateway check-idle executor-1\"\n";

    // A lane whose engine answers although Switchback never woke its
    // machine reads `healthy` (it read `offline`), and so does the media
    // lane on the same machine once the supervisor sees the machine up.
    // Without adoption nothing else changes: no countdown, no power-off.
    #[tokio::test]
    async fn a_directly_woken_lane_reads_healthy_and_without_adoption_stays_on() {
        let (host, clock, activator) = woken_outside("");
        activator.observe_all().await;
        for lane in ["text", "comfy"] {
            let report = report_of(&activator, lane);
            assert_eq!(report.state, LaneState::Healthy, "{report:?}");
            assert_eq!(report.poweroff_in_secs, None, "no countdown");
            assert!(
                report.detail.contains("woken outside Switchback"),
                "{report:?}"
            );
        }
        clock.advance(6 * 60 * 60 * 1000);
        activator.observe_all().await;
        activator.tick_all().await;
        assert_eq!(host.sim().poweroff, 0, "as before: it stays on");
        assert_eq!(host.sim().wake, 0);
    }

    // `adopt_external_wake`: the machine woken by another tool is taken as
    // one Switchback woke: its countdown shows and it powers off after the
    // idle timeout, with no wake sent.
    #[tokio::test]
    async fn an_adopted_machine_powers_off_after_its_idle_timeout() {
        let (host, clock, activator) = woken_outside(ADOPT_WITH_GUARD);
        activator.observe_all().await;
        let report = report_of(&activator, "text");
        assert_eq!(report.state, LaneState::IdleCountdown, "{report:?}");
        assert_eq!(report.poweroff_in_secs, Some(5));
        assert_eq!(
            report_of(&activator, "comfy").state,
            LaneState::IdleCountdown,
            "the media lane is probed on an adopting machine"
        );
        clock.advance(4_000);
        activator.tick_all().await;
        assert_eq!(host.sim().poweroff, 0, "not before the idle timeout");
        clock.advance(1_000);
        activator.tick_all().await;
        assert_eq!(host.sim().poweroff, 1);
        assert_eq!(host.sim().wake, 0, "never woken by us");
    }

    // The guard still decides: a person or a direct client it sees keeps
    // the adopted machine on.
    #[tokio::test]
    async fn an_adopted_machine_whose_guard_says_busy_stays_on() {
        let (host, clock, activator) = woken_outside(
            "    adopt_external_wake: true\n    poweroff_guard_command: \"ssh gateway check-idle executor-1\"\n",
        );
        host.sim().guard = GuardAnswer::Busy;
        activator.observe_all().await;
        clock.advance(600_000);
        activator.tick_all().await;
        assert_eq!(
            guard_runs_and_poweroffs(&host),
            (1, 0),
            "asked, busy, stays on"
        );
        host.sim().guard = GuardAnswer::Free;
        clock.advance(60_000); // past the guard's re-ask gap
        activator.tick_all().await;
        assert_eq!(guard_runs_and_poweroffs(&host), (2, 1));
    }

    // A hold keeps an adopted machine on; once it ends the countdown runs.
    #[tokio::test]
    async fn a_hold_keeps_an_adopted_machine_on() {
        let (host, clock, activator) = woken_outside(ADOPT_WITH_GUARD);
        let held = hold(&activator, "gpu-box", 3_600_000, false);
        activator.observe_all().await;
        clock.advance(600_000);
        activator.tick_all().await;
        assert_eq!(host.sim().poweroff, 0, "held");
        activator
            .holds()
            .delete("gpu-box", &held.id, clock.now_ms())
            .unwrap();
        assert_eq!(report_of(&activator, "text").poweroff_in_secs, Some(5));
        clock.advance(5_000);
        activator.tick_all().await;
        assert_eq!(host.sim().poweroff, 1);
    }

    // --- Lane alerts (`server.alert_command`) --------------------------------

    #[derive(Clone, Copy, PartialEq)]
    enum SinkMode {
        Ok,
        Hang,
    }

    /// Records every alert delivery: (payload, summary).
    struct FakeSink {
        mode: Mutex<SinkMode>,
        sent: Mutex<Vec<(serde_json::Value, String)>>,
    }

    impl FakeSink {
        fn new(mode: SinkMode) -> Arc<Self> {
            Arc::new(Self {
                mode: Mutex::new(mode),
                sent: Mutex::new(Vec::new()),
            })
        }
        fn kinds(&self) -> Vec<String> {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .map(|(payload, _)| payload["kind"].as_str().unwrap().to_string())
                .collect()
        }
    }

    #[async_trait]
    impl crate::alerts::AlertSink for FakeSink {
        async fn deliver(
            &self,
            _argv: &[String],
            payload: &str,
            summary: &str,
            _timeout_ms: u64,
        ) -> Result<i32, String> {
            self.sent
                .lock()
                .unwrap()
                .push((serde_json::from_str(payload).unwrap(), summary.to_string()));
            if *self.mode.lock().unwrap() == SinkMode::Hang {
                std::future::pending::<()>().await;
            }
            Ok(0)
        }
    }

    const ALERT_KEY: &str = "local-key-7f3a9c2b";

    /// The shared machine with alerts on (after 180 s, repeat 6 h) and an
    /// inline provider key; `extra` goes on both lanes.
    fn alerting(
        host: &FakeHost,
        clock: Arc<dyn Clock>,
        sink: Arc<FakeSink>,
        extra: &str,
    ) -> Arc<CapacityActivator> {
        let mut cfg = shared_host_config(extra);
        cfg.server.alert_command = Some(vec!["/usr/local/bin/lane-alert".to_string()]);
        if let ProviderKind::OpenaiCompatible { api_key, .. } = &mut cfg.providers[0].kind {
            *api_key = Some(ALERT_KEY.to_string());
        }
        CapacityActivator::with_alert_sink(
            &cfg,
            clock,
            Arc::new(host.clone()),
            Arc::new(host.clone()),
            sink,
        )
    }

    async fn check(activator: &CapacityActivator) {
        activator.check_alerts();
        settle().await;
    }

    // A normal wake shows `warn` for a minute or two: that sends nothing.
    #[tokio::test]
    async fn a_normal_wake_shorter_than_alert_after_sends_no_alert() {
        let host = FakeHost::new(Sim::default());
        let clock = FakeClock::new();
        let sink = FakeSink::new(SinkMode::Ok);
        let activator = alerting(&host, clock.clone(), sink.clone(), "");
        let text = activator.executor("text").unwrap();
        text.lock().state = LaneState::Waking;
        assert_eq!(text.report().level, LaneLevel::Warn);
        for _ in 0..12 {
            check(&activator).await;
            clock.advance(10_000); // 120 s of waking in all
        }
        text.lock().state = LaneState::IdleCountdown;
        check(&activator).await;
        assert!(sink.kinds().is_empty(), "{:?}", sink.kinds());
    }

    // A lasting failure sends exactly one alert, repeats it after
    // `alert_repeat_secs` (not before), and one `recovered` when it ends.
    #[tokio::test]
    async fn a_persistent_failure_alerts_once_repeats_and_recovers_once() {
        let host = FakeHost::new(Sim::default());
        let clock = FakeClock::new();
        let sink = FakeSink::new(SinkMode::Ok);
        let activator = alerting(&host, clock.clone(), sink.clone(), "");
        let text = activator.executor("text").unwrap();
        text.lock().state = LaneState::Degraded;
        for _ in 0..30 {
            check(&activator).await;
            clock.advance(60_000); // 30 min of checks
        }
        assert_eq!(sink.kinds(), ["escalated"]);
        clock.advance(6 * 60 * 60 * 1000);
        check(&activator).await;
        check(&activator).await;
        assert_eq!(sink.kinds(), ["escalated", "escalated"], "one repeat");
        text.lock().state = LaneState::IdleCountdown;
        check(&activator).await;
        check(&activator).await;
        assert_eq!(sink.kinds(), ["escalated", "escalated", "recovered"]);
        let (recovered, _) = sink.sent.lock().unwrap()[2].clone();
        assert_eq!(recovered["level"], "ok");
        assert_eq!(recovered["previous_level"], "fail");
        let status = activator.alerts().status();
        assert!(status.last_sent_at.is_some() && status.last_error.is_none());
        assert_eq!(status.pending, 0);
    }

    // A wake that keeps failing leaves the lane escalated, and each new
    // wake attempt changes its last wake result ("sent", then "timeout"):
    // the alert kind would flip between `escalated` and `wake_failed` with
    // every attempt. One problem alerts once until `alert_repeat_secs`, not
    // once per wake attempt.
    #[tokio::test]
    async fn a_wake_that_keeps_failing_does_not_alert_on_every_attempt() {
        let host = FakeHost::new(Sim::default());
        let clock = FakeClock::new();
        let sink = FakeSink::new(SinkMode::Ok);
        let activator = alerting(&host, clock.clone(), sink.clone(), "");
        let text = activator.executor("text").unwrap();
        text.escalate("wake did not reach health".to_string());
        for attempt in 0..12 {
            text.lock().last_wake_result = Some("timeout".to_string());
            check(&activator).await;
            clock.advance(60_000);
            text.lock().last_wake_result = Some(format!("sent (attempt {attempt})"));
            check(&activator).await;
            clock.advance(240_000); // a five-minute wake cycle, for an hour
        }
        assert_eq!(sink.kinds(), ["wake_failed"], "one alert for one problem");
        clock.advance(6 * 60 * 60 * 1000);
        check(&activator).await;
        assert_eq!(sink.kinds().len(), 2, "the repeat after alert_repeat_secs");
    }

    // An alert command that hangs never delays a due power-off: delivery
    // runs in its own task, and its state shows in the report.
    #[tokio::test]
    async fn a_hanging_alert_command_does_not_delay_a_due_poweroff() {
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            ..Default::default()
        });
        let clock = FakeClock::new();
        let sink = FakeSink::new(SinkMode::Hang);
        let activator = alerting(&host, clock.clone(), sink.clone(), "");
        drop(
            activator
                .executor("comfy")
                .unwrap()
                .ensure_ready()
                .await
                .expect("capacity"),
        );
        activator
            .executor("text")
            .unwrap()
            .escalate("probe refused".to_string());
        check(&activator).await;
        assert_eq!(sink.kinds(), ["escalated"], "delivery started and hangs");
        clock.advance(5_000);
        tokio::time::timeout(Duration::from_secs(5), async {
            activator.check_alerts();
            activator.tick_all().await;
        })
        .await
        .expect("never waits for the alert command");
        assert_eq!(host.sim().poweroff, 1, "the power-off ran on time");
        assert_eq!(activator.alerts().status().pending, 1, "still pending");
    }

    // A guard that stays broken alerts once it has been broken for
    // `alert_after_secs`, once for the machine (not once per lane).
    #[tokio::test]
    async fn a_broken_guard_alerts() {
        let host = FakeHost::new(Sim {
            wake_makes_healthy: true,
            guard: GuardAnswer::Broken,
            ..Default::default()
        });
        let clock = FakeClock::new();
        let sink = FakeSink::new(SinkMode::Ok);
        let activator = alerting(
            &host,
            clock.clone(),
            sink.clone(),
            "    poweroff_guard_command: \"ssh gateway check-idle executor-1\"\n",
        );
        drop(
            activator
                .executor("comfy")
                .unwrap()
                .ensure_ready()
                .await
                .expect("capacity"),
        );
        for _ in 0..4 {
            clock.advance(60_000);
            activator.tick_all().await;
            check(&activator).await;
        }
        assert!(
            report_of(&activator, "comfy").poweroff_guard_broken,
            "broken by now"
        );
        assert!(sink.kinds().is_empty(), "not before alert_after_secs");
        clock.advance(180_000);
        check(&activator).await;
        assert_eq!(sink.kinds(), ["guard_broken"], "one event for the machine");
        assert_eq!(host.sim().poweroff, 0);
    }

    // The payload never carries a key, a credential in a URL or an operator
    // command line, even when the lane's error text holds them. Its fields
    // follow the documented schema.
    #[tokio::test]
    async fn the_alert_payload_carries_no_key_and_no_command() {
        let host = FakeHost::new(Sim::default());
        let clock = FakeClock::new();
        let sink = FakeSink::new(SinkMode::Ok);
        let activator = alerting(&host, clock.clone(), sink.clone(), "");
        // The user info is joined at run time: no credential-shaped URL in
        // the source.
        let userinfo = ["user", ALERT_KEY].join(":");
        activator.executor("text").unwrap().escalate(format!(
            "wake `ssh gateway wake-host executor-1` failed; upstream said: invalid key {ALERT_KEY} for http://{userinfo}@executor-1.example:8080/v1?key={ALERT_KEY}"
        ));
        check(&activator).await;
        let sent = sink.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        let (payload, summary) = &sent[0];
        for text in [payload.to_string(), summary.clone()] {
            assert!(!text.contains(ALERT_KEY), "{text}");
            assert!(!text.contains("ssh gateway"), "{text}");
            assert!(!text.contains("user:"), "{text}");
        }
        assert!(payload["detail"].as_str().unwrap().contains("<command>"));
        for field in [
            "schema",
            "kind",
            "lane",
            "host",
            "level",
            "previous_level",
            "detail",
            "since",
            "sent_at",
            "gateway",
        ] {
            assert!(payload.get(field).is_some(), "{field}: {payload}");
        }
        assert_eq!(payload["schema"], "switchback/lane-alert@1");
        assert_eq!(payload["kind"], "escalated");
        assert_eq!(payload["lane"], "text");
        assert_eq!(payload["host"], "gpu-box");
        assert_eq!(payload["level"], "fail");
        assert!(summary.starts_with("switchback escalated: text on gpu-box is fail"));
    }
}
