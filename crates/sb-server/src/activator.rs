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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use sb_core::{Config, LocalExecutorConfig};
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
    BootTimeout { wake_path: String, elapsed_ms: u64 },
    WakeFailed(String),
    /// Self-heal budget exhausted after repeated health loss.
    RetriesExhausted { budget: u32, last_error: String },
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
    escalated: bool,
    poweroff_unconfigured_reported: bool,
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
            escalated: false,
            poweroff_unconfigured_reported: false,
        }
    }
}

/// One on-demand local executor lane.
pub struct LocalExecutor {
    cfg: LocalExecutorConfig,
    clock: Arc<dyn Clock>,
    runner: Arc<dyn CommandRunner>,
    probe: Arc<dyn HealthProbe>,
    runtime: Mutex<LaneRuntime>,
    /// Serializes wake attempts so concurrent submits trigger the wake exactly
    /// once (single-flight): the first waker holds this across the whole boot
    /// poll; the rest wait, then find the lane already healthy.
    wake_lock: tokio::sync::Mutex<()>,
}

impl LocalExecutor {
    fn new(
        cfg: LocalExecutorConfig,
        clock: Arc<dyn Clock>,
        runner: Arc<dyn CommandRunner>,
        probe: Arc<dyn HealthProbe>,
    ) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            clock,
            runner,
            probe,
            runtime: Mutex::new(LaneRuntime::new()),
            wake_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub fn name(&self) -> &str {
        &self.cfg.name
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LaneRuntime> {
        self.runtime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn boot_timeout_ms(&self) -> u64 {
        self.cfg.boot_timeout_secs.saturating_mul(1000)
    }

    fn idle_timeout_ms(&self) -> u64 {
        self.cfg.idle_timeout_secs.saturating_mul(1000)
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
        if self.probe.healthy(&self.cfg.health_endpoint).await {
            let mut rt = self.lock();
            self.admit_locked(&mut rt);
            return Ok(LaneJobGuard::new(self.clone()));
        }

        // Wake leg. Skip-gated when unconfigured — never faked.
        let Some(wake_command) = self.cfg.wake_command.clone() else {
            let mut rt = self.lock();
            rt.state = LaneState::Offline;
            rt.last_wake_result = Some("unconfigured".to_string());
            tracing::warn!(lane = %self.cfg.name, "wake command unconfigured; job stays queued");
            return Err(EnsureError::WakeUnconfigured);
        };

        {
            let mut rt = self.lock();
            rt.state = LaneState::Waking;
            rt.last_wake_ms = Some(self.clock.now_ms());
        }
        tracing::info!(lane = %self.cfg.name, "waking local executor");
        if let Err(error) = self.runner.run(&wake_command).await {
            let mut rt = self.lock();
            rt.state = LaneState::Offline;
            rt.last_wake_result = Some(format!("failed: {error}"));
            rt.escalated = true;
            rt.last_error = Some(format!("wake command failed: {error}"));
            return Err(EnsureError::WakeFailed(error));
        }

        // Poll for health up to the boot timeout.
        let start = self.clock.now_ms();
        loop {
            if self.probe.healthy(&self.cfg.health_endpoint).await {
                let mut rt = self.lock();
                rt.last_wake_result = Some("ok".to_string());
                self.admit_locked(&mut rt);
                tracing::info!(lane = %self.cfg.name, "local executor healthy");
                return Ok(LaneJobGuard::new(self.clone()));
            }
            let elapsed = self.clock.now_ms().saturating_sub(start);
            if elapsed >= self.boot_timeout_ms() {
                let mut rt = self.lock();
                rt.state = LaneState::Offline;
                rt.last_wake_result = Some("timeout".to_string());
                rt.escalated = true;
                rt.last_error = Some(format!(
                    "wake `{wake_command}` did not reach health in {elapsed}ms"
                ));
                tracing::error!(lane = %self.cfg.name, elapsed_ms = elapsed, "boot timeout");
                return Err(EnsureError::BootTimeout {
                    wake_path: wake_command,
                    elapsed_ms: elapsed,
                });
            }
            self.clock.sleep_ms(self.cfg.health_poll_interval_ms).await;
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
                    if self.probe.healthy(&self.cfg.health_endpoint).await {
                        let mut rt = self.lock();
                        rt.last_error = Some(format!("dispatch failed: {error}"));
                        return Err(JobError::Dispatch(error));
                    }
                    let budget = self.cfg.retry_budget;
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
        let command = self
            .cfg
            .restart_command
            .clone()
            .or_else(|| self.cfg.wake_command.clone());
        let Some(command) = command else {
            return Err("no restart_command or wake_command configured to heal".to_string());
        };
        {
            let mut rt = self.lock();
            rt.state = LaneState::Waking;
            rt.last_wake_ms = Some(self.clock.now_ms());
        }
        tracing::warn!(lane = %self.cfg.name, "self-heal: restarting local executor");
        self.runner
            .run(&command)
            .await
            .map_err(|e| format!("restart command failed: {e}"))?;
        let start = self.clock.now_ms();
        loop {
            if self.probe.healthy(&self.cfg.health_endpoint).await {
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
            self.clock.sleep_ms(self.cfg.health_poll_interval_ms).await;
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
        if rt.in_flight == 0
            && matches!(rt.state, LaneState::Draining | LaneState::Healthy)
        {
            rt.state = LaneState::IdleCountdown;
            rt.idle_since_ms = Some(self.clock.now_ms());
        }
    }

    /// Supervisor tick: power the machine off once it has been idle past the
    /// timeout. Skip-gated when `poweroff_command` is unconfigured (the lane
    /// stays healthy and the doctor reports the gap — never a fake poweroff).
    pub async fn tick(self: &Arc<Self>) {
        let due = {
            let rt = self.lock();
            match (rt.state, rt.idle_since_ms) {
                (LaneState::IdleCountdown, Some(since)) => {
                    self.clock.now_ms().saturating_sub(since) >= self.idle_timeout_ms()
                }
                _ => false,
            }
        };
        if !due {
            return;
        }

        let Some(poweroff_command) = self.cfg.poweroff_command.clone() else {
            let mut rt = self.lock();
            if !rt.poweroff_unconfigured_reported {
                rt.poweroff_unconfigured_reported = true;
                tracing::warn!(lane = %self.cfg.name, "poweroff command unconfigured; staying up");
            }
            return;
        };

        {
            let mut rt = self.lock();
            // Re-check under the lock; a new job may have cancelled the idle
            // countdown between the read above and here.
            if rt.state != LaneState::IdleCountdown {
                return;
            }
            rt.state = LaneState::PoweringOff;
        }
        tracing::info!(lane = %self.cfg.name, "idle timeout reached; powering off");
        match self.runner.run(&poweroff_command).await {
            Ok(()) => {
                let mut rt = self.lock();
                // A job could have arrived during poweroff; if so, leave it to
                // the next ensure_ready to re-wake rather than clobbering state.
                if rt.state == LaneState::PoweringOff {
                    rt.state = LaneState::Offline;
                    rt.idle_since_ms = None;
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
        let wake_configured = self.cfg.wake_command.is_some();
        let poweroff_configured = self.cfg.poweroff_command.is_some();
        let restart_configured = self.cfg.restart_command.is_some();
        let level = lane_level(&rt, wake_configured);
        let detail = lane_detail(&rt, &self.cfg.name, wake_configured, poweroff_configured);
        LocalLaneReport {
            name: self.cfg.name.clone(),
            state: rt.state,
            level,
            queue_depth: rt.waiting,
            in_flight: rt.in_flight,
            retries_used: rt.retries_used,
            last_wake_ms: rt.last_wake_ms,
            last_wake_result: rt.last_wake_result.clone(),
            last_error: rt.last_error.clone(),
            wake_configured,
            poweroff_configured,
            restart_configured,
            escalated: rt.escalated,
            detail,
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

/// Holds one job's in-flight slot; releasing it arms the idle countdown.
pub struct LaneJobGuard {
    executor: Arc<LocalExecutor>,
    released: bool,
}

impl std::fmt::Debug for LaneJobGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaneJobGuard")
            .field("lane", &self.executor.cfg.name)
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

/// Owns every managed local executor lane. Built once at startup from config;
/// held as an `Arc` in `AppState` so live state (idle timers, retry counts)
/// survives across requests.
#[derive(Default)]
pub struct CapacityActivator {
    executors: BTreeMap<String, Arc<LocalExecutor>>,
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
        let mut executors = BTreeMap::new();
        for executor_cfg in &cfg.local_executors {
            let executor = LocalExecutor::new(
                executor_cfg.clone(),
                clock.clone(),
                runner.clone(),
                probe.clone(),
            );
            executors.insert(executor_cfg.name.clone(), executor);
        }
        Arc::new(Self { executors })
    }

    /// The managed lane for a provider id, if one is configured.
    pub fn executor(&self, name: &str) -> Option<Arc<LocalExecutor>> {
        self.executors.get(name).cloned()
    }

    pub fn is_empty(&self) -> bool {
        self.executors.is_empty()
    }

    /// Metadata-only report of every managed lane (for the doctor / live
    /// capacity surface).
    pub fn reports(&self) -> Vec<LocalLaneReport> {
        self.executors
            .values()
            .map(|executor| executor.report())
            .collect()
    }

    /// Spawn a background supervisor that periodically ticks each lane so idle
    /// machines power off autonomously. No-op when there are no managed lanes.
    pub fn spawn_supervisor(self: &Arc<Self>, interval: Duration) {
        if self.executors.is_empty() {
            return;
        }
        let activator = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                for executor in activator.executors.values() {
                    executor.tick().await;
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

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

    // --- Config builder ----------------------------------------------------

    struct Legs {
        wake: Option<&'static str>,
        poweroff: Option<&'static str>,
        restart: Option<&'static str>,
        boot_timeout_secs: u64,
        idle_timeout_secs: u64,
        retry_budget: u32,
    }

    impl Default for Legs {
        fn default() -> Self {
            Self {
                wake: Some("ssh gateway wake-host executor-1"),
                poweroff: Some("ssh gateway poweroff-host executor-1"),
                restart: Some("ssh gateway restart-host executor-1"),
                boot_timeout_secs: 10,
                idle_timeout_secs: 5,
                retry_budget: 2,
            }
        }
    }

    fn build(
        host: &FakeHost,
        clock: Arc<FakeClock>,
        legs: Legs,
    ) -> Arc<LocalExecutor> {
        let cfg = LocalExecutorConfig {
            name: "comfy-local".to_string(),
            base_url: "http://executor-1.local:8188".to_string(),
            health_endpoint: "http://executor-1.local:8188/system_stats".to_string(),
            wake_command: legs.wake.map(str::to_string),
            poweroff_command: legs.poweroff.map(str::to_string),
            restart_command: legs.restart.map(str::to_string),
            boot_timeout_secs: legs.boot_timeout_secs,
            idle_timeout_secs: legs.idle_timeout_secs,
            retry_budget: legs.retry_budget,
            health_poll_interval_ms: 400,
        };
        let runner: Arc<dyn CommandRunner> = Arc::new(host.clone());
        let probe: Arc<dyn HealthProbe> = Arc::new(host.clone());
        let clock: Arc<dyn Clock> = clock;
        LocalExecutor::new(cfg, clock, runner, probe)
    }

    // --- Tests -------------------------------------------------------------

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
        assert_eq!(report.last_wake_result.as_deref(), Some("ok"));

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

        assert_eq!(host.sim().wake, 1, "single-flight: one wake for five submits");
        assert_eq!(executor.report().in_flight, 5);
        drop(guards);
        assert_eq!(executor.report().in_flight, 0);
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
        let queued: Result<(), JobError<&str>> = executor
            .run_job(|_| async { Ok(()) })
            .await;
        assert!(matches!(queued, Err(JobError::Queued)));

        let report = executor.report();
        assert!(!report.wake_configured);
        assert_eq!(report.state, LaneState::Offline);
        assert_eq!(report.level, LaneLevel::Warn);
        assert!(report.detail.contains("wake unconfigured"));
    }
}
