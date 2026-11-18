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
use sb_runtime::{LaneBusy, LaneWake};
use serde::Serialize;

// ---------------------------------------------------------------------------
// Seams (injected; real impls below, fakes in tests)
// ---------------------------------------------------------------------------

/// Runs an operator-configured shell command string (wake / poweroff / restart).
#[async_trait]
pub trait CommandRunner: Send + Sync {
    async fn run(&self, command: &str) -> Result<(), String>;
}

/// Decides whether a local service is reachable/healthy at an endpoint.
#[async_trait]
pub trait HealthProbe: Send + Sync {
    /// `true` = service is healthy (a 2xx at `endpoint`).
    async fn healthy(&self, endpoint: &str) -> bool;
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

#[async_trait]
impl CommandRunner for ShellCommandRunner {
    async fn run(&self, command: &str) -> Result<(), String> {
        let output = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .output()
            .await
            .map_err(|e| format!("spawn failed: {e}"))?;
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
}

impl HttpHealthProbe {
    pub fn new(timeout: Duration) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_default();
        Self { client }
    }
}

#[async_trait]
impl HealthProbe for HttpHealthProbe {
    async fn healthy(&self, endpoint: &str) -> bool {
        matches!(self.client.get(endpoint).send().await, Ok(response) if response.status().is_success())
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
    pub detail: String,
    /// Most recent health probe result; text routing skips the lane's
    /// targets while this is `false`. Absent until the first probe, and
    /// always absent for a lane without `route_health: true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_healthy: Option<bool>,
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
    /// Serializes wake attempts so concurrent submits trigger the wake exactly
    /// once (single-flight): the first waker holds this across the whole boot
    /// poll; the rest wait, then find the lane already healthy.
    wake_lock: tokio::sync::Mutex<()>,
    /// The lane opted in (`route_health: true`) and its provider can be a
    /// routed (text) target, so the supervisor probes it for routing and
    /// routing reads what it saw. Any other lane (the default, and always a
    /// media-only lane such as ComfyUI or fal) keeps its old behaviour: no
    /// periodic probe and no routing view. Recomputed on reload.
    routed: AtomicBool,
    /// Bumped whenever a request-triggered wake ends, so waiting requests
    /// wake up without polling.
    changes: tokio::sync::watch::Sender<u64>,
}

impl LocalExecutor {
    fn new(
        cfg: LocalExecutorConfig,
        clock: Arc<dyn Clock>,
        runner: Arc<dyn CommandRunner>,
        probe: Arc<dyn HealthProbe>,
        routed: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            name: cfg.name.clone(),
            cfg: RwLock::new(Arc::new(cfg)),
            clock,
            runner,
            probe,
            runtime: Mutex::new(LaneRuntime::new()),
            wake_lock: tokio::sync::Mutex::new(()),
            routed: AtomicBool::new(routed),
            changes: tokio::sync::watch::channel(0).0,
        })
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
        let ticket = {
            let mut rt = self.lock();
            rt.probe_seq += 1;
            rt.last_probe_started_ms = Some(self.clock.now_ms());
            rt.probe_seq
        };
        let healthy = self.probe.healthy(&self.cfg().health_endpoint).await;
        let mut rt = self.lock();
        if self.is_routed() && ticket > rt.observed_seq {
            rt.observed_seq = ticket;
            rt.observed_healthy = Some(healthy);
        }
        healthy
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
    /// (`wake_on_request` lanes only; the caller checks). Media jobs and
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
        if known_up {
            rt.state = LaneState::Draining;
            rt.idle_since_ms = None;
        }
        LaneJobGuard::new(self.clone())
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
        let _wake = self.wake_lock.lock().await;
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

        // Slow path: needs a wake. Count as queued while we wait for the lock.
        {
            let mut rt = self.lock();
            rt.waiting += 1;
        }
        let _wake = self.wake_lock.lock().await;
        let result = self.wake_and_admit().await;
        {
            let mut rt = self.lock();
            rt.waiting = rt.waiting.saturating_sub(1);
        }
        result
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
        if self.probe_health().await {
            let mut rt = self.lock();
            self.admit_locked(&mut rt);
            return Ok(LaneJobGuard::new(self.clone()));
        }

        // Wake leg. Skip-gated when unconfigured — never faked.
        let Some(wake_command) = self.cfg().wake_command.clone() else {
            let mut rt = self.lock();
            rt.state = LaneState::Offline;
            rt.last_wake_result = Some("unconfigured".to_string());
            tracing::warn!(lane = %self.name, "wake command unconfigured; job stays queued");
            return Err(EnsureError::WakeUnconfigured);
        };

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
                tracing::error!(lane = %self.name, elapsed_ms = elapsed, attempts, "boot timeout");
                return Err(EnsureError::BootTimeout {
                    wake_path: wake_command,
                    elapsed_ms: elapsed,
                });
            }

            // (Re-)fire the wake when due.
            if now >= next_wake_at {
                let first_attempt = self.lock().wake_attempts == 0;
                match self.runner.run(&wake_command).await {
                    Ok(()) => {
                        let mut rt = self.lock();
                        rt.wake_attempts += 1;
                        rt.last_wake_ms = Some(now);
                        rt.last_wake_result = Some(format!("sent (attempt {})", rt.wake_attempts));
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

            if self.probe_health().await {
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

    /// Run a job under managed capacity: wake if needed, then dispatch with
    /// self-heal. `op(attempt)` is the real dispatch; it runs at least once and
    /// is requeued (fresh attempt) after a health loss, up to `retry_budget`.
    pub async fn run_job<T, E, F, Fut>(self: &Arc<Self>, mut op: F) -> Result<T, JobError<E>>
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
        };

        loop {
            let attempt = self.lock().retries_used;
            match op(attempt).await {
                Ok(value) => return Ok(value),
                Err(error) => {
                    // Classify: is the service still healthy? If so this is a
                    // real job error — loud, unretried. If not, the service was
                    // lost mid-job and we self-heal + requeue.
                    if self.probe_health().await {
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
        {
            let mut rt = self.lock();
            rt.state = LaneState::Waking;
            rt.last_wake_ms = Some(self.clock.now_ms());
        }
        tracing::warn!(lane = %self.name, "self-heal: restarting local executor");
        self.runner
            .run(&command)
            .await
            .map_err(|e| format!("restart command failed: {e}"))?;
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

    /// The idle countdown runs and has passed the idle timeout.
    fn idle_due(&self, rt: &LaneRuntime) -> bool {
        match (rt.state, rt.idle_since_ms) {
            (LaneState::IdleCountdown, Some(since)) => {
                self.clock.now_ms().saturating_sub(since) >= self.idle_timeout_ms()
            }
            _ => false,
        }
    }

    /// Supervisor tick: power the machine off once it has been idle past the
    /// timeout. Skip-gated when `poweroff_command` is unconfigured (the lane
    /// stays healthy and the doctor reports the gap — never a fake poweroff).
    pub async fn tick(self: &Arc<Self>) {
        if !self.idle_due(&self.lock()) {
            return;
        }

        let Some(poweroff_command) = self.cfg().poweroff_command.clone() else {
            let mut rt = self.lock();
            if !rt.poweroff_unconfigured_reported {
                rt.poweroff_unconfigured_reported = true;
                tracing::warn!(lane = %self.name, "poweroff command unconfigured; staying up");
            }
            return;
        };

        // A lane that text requests wake holds its wake lock while it powers
        // off: no wake (a request's or a job's) probes or runs its command
        // until the poweroff command has returned, so a host that still
        // answers while it shuts down is not taken for up. A wake already
        // running skips this tick.
        let _wake = if self.cfg().wake_on_request {
            match self.wake_lock.try_lock() {
                Ok(wake) => Some(wake),
                Err(_) => return,
            }
        } else {
            None
        };
        {
            let mut rt = self.lock();
            // Re-check under the lock; a new job may have cancelled the idle
            // countdown (or ended and restarted it) since the read above.
            if !self.idle_due(&rt) {
                return;
            }
            rt.state = LaneState::PoweringOff;
        }
        tracing::info!(lane = %self.name, "idle timeout reached; powering off");
        match self.runner.run(&poweroff_command).await {
            Ok(()) => {
                let mut rt = self.lock();
                // A job could have arrived during poweroff; if so, leave it to
                // the next ensure_ready to re-wake rather than clobbering state.
                if rt.state == LaneState::PoweringOff {
                    rt.state = LaneState::Offline;
                    rt.idle_since_ms = None;
                    // A lane that text requests wake: routing must see it
                    // down now (and wake it), not dispatch into a host that
                    // is going away until the next probe. The ticket makes
                    // an older in-flight probe's "up" lose to this.
                    if self.is_routed() && self.cfg().wake_on_request {
                        rt.probe_seq += 1;
                        rt.observed_seq = rt.probe_seq;
                        rt.observed_healthy = Some(false);
                    }
                }
            }
            Err(error) => {
                let mut rt = self.lock();
                rt.state = LaneState::Degraded;
                rt.escalated = true;
                rt.last_error = Some(format!("poweroff command failed: {error}"));
            }
        }
    }

    /// Metadata-only snapshot for the doctor / live capacity surface.
    pub fn report(&self) -> LocalLaneReport {
        let rt = self.lock();
        let wake_configured = self.cfg().wake_command.is_some();
        let poweroff_configured = self.cfg().poweroff_command.is_some();
        let restart_configured = self.cfg().restart_command.is_some();
        let level = lane_level(&rt, wake_configured);
        let detail = lane_detail(&rt, &self.name, wake_configured, poweroff_configured);
        LocalLaneReport {
            name: self.name.clone(),
            state: rt.state,
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
            detail,
            observed_healthy: rt.observed_healthy,
        }
    }
}

fn lane_level(rt: &LaneRuntime, wake_configured: bool) -> LaneLevel {
    if rt.escalated || rt.state == LaneState::Degraded {
        LaneLevel::Fail
    } else if !wake_configured {
        // Managed lane with no wake wiring: it can serve only if the operator
        // happens to have the service up. Flag it so the gap is visible.
        LaneLevel::Warn
    } else if matches!(rt.state, LaneState::Waking) {
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
) -> String {
    if let Some(error) = &rt.last_error {
        if rt.escalated {
            return format!("{name} escalated: {error}");
        }
    }
    if !wake_configured {
        return format!("{name} wake unconfigured; jobs stay queued until a service is up");
    }
    if !poweroff_configured && matches!(rt.state, LaneState::IdleCountdown | LaneState::Healthy) {
        return format!("{name} poweroff unconfigured; idle machine will not be powered off");
    }
    match rt.state {
        LaneState::Offline => format!("{name} offline; will wake on next job"),
        LaneState::Waking => format!("{name} waking"),
        LaneState::Healthy => format!("{name} healthy, idle"),
        LaneState::Draining => format!("{name} draining {} job(s)", rt.in_flight),
        LaneState::IdleCountdown => format!("{name} idle; poweroff countdown running"),
        LaneState::PoweringOff => format!("{name} powering off"),
        LaneState::Degraded => format!("{name} degraded; needs operator attention"),
    }
}

/// Short human duration for decisions and errors: `850 ms`, `41 s`.
fn fmt_ms(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms} ms")
    } else {
        format!("{} s", ms.saturating_add(500) / 1_000)
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
    /// Supervisor cadence once [`Self::spawn_supervisor`] asked for one.
    supervisor_interval: OnceLock<Duration>,
    supervisor_started: AtomicBool,
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
        let activator = Self {
            executors: Arc::new(RwLock::new(BTreeMap::new())),
            clock,
            runner,
            probe,
            retired: Mutex::new(BTreeMap::new()),
            supervisor_interval: OnceLock::new(),
            supervisor_started: AtomicBool::new(false),
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
                next.insert(executor_cfg.name.clone(), executor);
            }
            for (name, removed) in lanes.iter() {
                tracing::info!(lane = %name, "local lane removed by config; running work finishes, no further poweroff");
                retired.insert(name.clone(), Arc::downgrade(removed));
            }
            *lanes = next;
        }
        self.start_supervisor_if_due();
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
            .map(|executor| executor.report())
            .collect()
    }

    /// Probe every routed lane once and record what was seen (see
    /// [`LocalExecutor::observe`]). A lane without `route_health: true`, and
    /// a media-only lane, is not probed.
    pub async fn observe_all(&self) {
        for executor in self.all() {
            if executor.is_routed() {
                executor.observe().await;
            }
        }
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
        handle.spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let pass: Vec<Arc<LocalExecutor>> = lanes
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .values()
                    .cloned()
                    .collect();
                for executor in pass {
                    if executor.is_routed() {
                        executor.observe().await;
                    }
                    executor.tick().await;
                }
            }
        });
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
        executor.schedule_refresh(&mut rt);
        Some("last health probe failed".to_string())
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
        let executor = self.executor(provider_id)?;
        if !executor.cfg().wake_on_request {
            return None;
        }
        Some(Box::new(executor.begin_request()))
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
    }

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
        async fn run(&self, command: &str) -> Result<(), String> {
            let mut sim = self.sim.lock().unwrap();
            if command.contains("restart") {
                sim.restart += 1;
                if sim.restart_makes_healthy {
                    sim.healthy = true;
                }
            } else if command.contains("poweroff") {
                sim.poweroff += 1;
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
        async fn healthy(&self, _endpoint: &str) -> bool {
            self.sim.lock().unwrap().healthy
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
            supervisor_interval: OnceLock::new(),
            supervisor_started: AtomicBool::new(false),
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
        assert_eq!(executor.report().state, LaneState::Offline);
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
        assert_eq!(report.state, LaneState::Offline, "a refresh never wakes");
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
}
