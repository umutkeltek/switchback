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
use sb_runtime::{LaneAcquire, LaneBusy, LaneWake, LANE_GROUP_RETRY_AFTER};
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
        // `kill_on_drop`: a command dropped at its timeout (see
        // `LocalExecutor::run_command`) is killed, not left running.
        let output = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .kill_on_drop(true)
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
    /// The lane's GPU group stayed with another lane past `group_wait_secs`,
    /// or the holder's release command failed.
    GpuUnavailable(String),
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
    /// Failed `release_command` runs since the last one that worked.
    release_failures: u32,
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
            release_failures: 0,
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
            result = self.runner.run(command) => result,
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
        let answered = self.probe.healthy(&self.cfg().health_endpoint).await;
        let mut rt = self.lock();
        // After our own power-off the host may still answer while it shuts
        // down: an answer counts as up only once a probe has failed (the
        // power-off is confirmed) or a wake ran.
        let healthy = if rt.down_unconfirmed {
            if !answered {
                rt.down_unconfirmed = false;
            }
            false
        } else {
            answered
        };
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
                    tracing::info!(lane = %self.name, group = %group, "taking the gpu group: releasing its holder");
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
        match self.run_command("release", &command).await {
            Ok(()) => {
                self.lock().release_failures = 0;
                Ok(())
            }
            Err(error) => {
                let failures = {
                    let mut rt = self.lock();
                    rt.release_failures += 1;
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
        let host = self.host();
        let _wake = host.wake_lock.lock().await;
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
        let Some((wake_lane, wake_command)) = self.host().wake_command() else {
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
                match wake_lane.run_command("wake", &wake_command).await {
                    Ok(()) => {
                        {
                            let mut rt = self.lock();
                            rt.wake_attempts += 1;
                            rt.last_wake_ms = Some(now);
                            rt.last_wake_result =
                                Some(format!("sent (attempt {})", rt.wake_attempts));
                        }
                        // A wake ran: the next healthy answer is trusted.
                        self.host().clear_down_unconfirmed();
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
        self.run_command("restart", &command)
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

    /// Supervisor tick: power the lane's machine off once every lane on it
    /// has been idle past its own timeout (see [`Host::tick`]).
    pub async fn tick(self: &Arc<Self>) {
        self.host().tick().await;
    }

    /// Metadata-only snapshot for the doctor / live capacity surface.
    pub fn report(&self) -> LocalLaneReport {
        let host = self.host();
        let wake_configured = host.wake_command().is_some();
        let poweroff_configured = host.poweroff_command().is_some();
        let poweroff_failures = host.power().failures;
        let restart_configured = self.cfg().restart_command.is_some();
        let gpu_group = self.cfg().exclusive_group.clone();
        let holds_gpu = gpu_group.as_ref().map(|group| {
            host.groups()
                .get(group)
                .is_some_and(|state| state.holder.as_deref() == Some(self.name.as_str()))
        });
        let rt = self.lock();
        let level = lane_level(&rt, wake_configured, poweroff_failures);
        let detail = lane_detail(
            &rt,
            &self.name,
            wake_configured,
            poweroff_configured,
            poweroff_failures,
        );
        LocalLaneReport {
            name: self.name.clone(),
            host: host.shared.then(|| host.id.clone()),
            gpu_group,
            holds_gpu,
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
            poweroff_failures,
            detail,
            observed_healthy: rt.observed_healthy,
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

impl Host {
    fn new(id: &str, shared: bool, members: Vec<Weak<LocalExecutor>>) -> Arc<Self> {
        Arc::new(Self {
            id: id.to_string(),
            shared,
            wake_lock: tokio::sync::Mutex::new(()),
            tick_lock: tokio::sync::Mutex::new(()),
            members: RwLock::new(members),
            power: Mutex::new(HostPower::default()),
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
    ) -> Option<u64> {
        let mut latest = None;
        for (lane, rt) in lanes.iter().zip(runtimes) {
            if rt.in_flight > 0 || rt.waiting > 0 {
                return None;
            }
            match (rt.state, rt.idle_since_ms) {
                (LaneState::IdleCountdown, Some(since)) => {
                    if now.saturating_sub(since) < lane.idle_timeout_ms() {
                        return None;
                    }
                    latest = latest.max(Some(since));
                }
                (LaneState::Offline, _) => {}
                _ => return None,
            }
        }
        latest
    }

    /// Power the machine off once it is idle (see [`Self::idle_period`]).
    /// Skip-gated when no lane sets a `poweroff_command`: the lanes stay up
    /// and the doctor reports the gap, never a fake power-off. One power-off
    /// for the machine, and every lane on it observes it. A failed (or
    /// timed-out) power-off is retried after `wake_retry_interval_secs`, up
    /// to `retry_budget` times (both read from the lane that carries the
    /// command); after that every lane is `Degraded`, escalated, and the
    /// log says so at error level.
    async fn tick(&self) {
        let Ok(_tick) = self.tick_lock.try_lock() else {
            return;
        };
        let lanes = self.lanes();
        let Some(first) = lanes.first() else {
            return;
        };
        let clock = first.clock.clone();
        let period = {
            let runtimes: Vec<_> = lanes.iter().map(|lane| lane.lock()).collect();
            Self::idle_period(&lanes, &runtimes, clock.now_ms())
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
        // Re-check under every lane's lock and mark them all powering off at
        // once: new work may have arrived since the read above.
        let before: Vec<LaneState> = {
            let mut runtimes: Vec<_> = lanes.iter().map(|lane| lane.lock()).collect();
            if Self::idle_period(&lanes, &runtimes, clock.now_ms()) != Some(period) {
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
                    // Work that arrived during the poweroff keeps its state;
                    // the next wake brings the machine back for it.
                    if rt.state == LaneState::PoweringOff {
                        rt.state = LaneState::Offline;
                        rt.idle_since_ms = None;
                        rt.down_unconfirmed = true;
                        if had_failures {
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
            hosts: Mutex::new(BTreeMap::new()),
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

    /// One power-off pass: each machine's tick once (see [`Host::tick`]).
    pub async fn tick_all(&self) {
        tick_hosts(&self.all()).await;
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
                for executor in &pass {
                    if executor.is_routed() {
                        executor.observe().await;
                    }
                }
                tick_hosts(&pass).await;
            }
        });
    }
}

/// Tick each machine under these lanes once, in lane order.
async fn tick_hosts(lanes: &[Arc<LocalExecutor>]) {
    let mut seen: Vec<Arc<Host>> = Vec::new();
    for lane in lanes {
        let host = lane.host();
        if seen.iter().any(|done| Arc::ptr_eq(done, &host)) {
            continue;
        }
        host.tick().await;
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
            hosts: Mutex::new(BTreeMap::new()),
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
            command_timeout_secs: 60,
            host: None,
            exclusive_group: None,
            release_command: None,
            group_wait_secs: 120,
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
            command_timeout_secs: 60,
            host: None,
            exclusive_group: None,
            release_command: None,
            group_wait_secs: 120,
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
        for absent in ["host", "gpu_group", "holds_gpu", "poweroff_failures"] {
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
    // escalates the lane, and later switches fail without running it.
    #[tokio::test]
    async fn a_failed_release_does_not_hand_the_gpu_over() {
        let (host, _clock, activator) = gpu_group_activator();
        let gate = Arc::new(tokio::sync::Semaphore::new(1));
        media_job(&activator, gate, Arc::new(AtomicU64::new(0)))
            .await
            .unwrap()
            .expect("comfy holds the GPU");
        host.sim().fail_release = true;
        for attempt in 1..=3 {
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
}
