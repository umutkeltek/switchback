//! Local-lane alerts: tell someone when a managed lane has a problem.
//!
//! After each supervisor pass the lane reports are checked. A problem is a
//! lane in `warn` or `fail` for `server.alert_after_secs` (a normal wake
//! shows `warn` for a minute or two and must not alert), or at once an
//! escalation: a power-off whose retries ran out, a wake that never came
//! up, or any other escalated or degraded lane. A broken power-off guard
//! alerts once it has been broken for `alert_after_secs`. While a problem
//! stays it repeats every `alert_repeat_secs`; when a lane that alerted is
//! `ok` again, one `recovered` event follows.
//!
//! Each event runs `server.alert_command` (argv, no shell) with the JSON
//! event on stdin and a one-line summary in `SWITCHBACK_ALERT_SUMMARY`. The
//! payload schema is `switchback/lane-alert@1` (see the docs). Delivery runs
//! in a background task: a failing or hanging command never blocks or
//! delays lane work, a wake or a power-off. A failed delivery stays pending
//! and is tried again later, with a growing gap.
//!
//! Detail text goes through [`redact_text`]: no key, no credential in a
//! URL, no operator command line ever reaches the payload.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use sb_core::Config;
use serde::Serialize;

use crate::activator::{Clock, LaneLevel, LaneState, LocalLaneReport};
use crate::holds::rfc3339_ms;

/// The payload's schema id; a change to the payload's fields is a new id.
pub const ALERT_SCHEMA: &str = "switchback/lane-alert@1";
/// The env var that carries the one-line summary.
pub const SUMMARY_ENV: &str = "SWITCHBACK_ALERT_SUMMARY";
/// At most this many undelivered events are kept (oldest dropped first).
const MAX_PENDING: usize = 64;
/// The first retry of a failed delivery waits this long; each further
/// failure doubles it, up to [`MAX_RETRY_GAP_MS`].
const FIRST_RETRY_GAP_MS: u64 = 60_000;
const MAX_RETRY_GAP_MS: u64 = 60 * 60 * 1000;

/// Runs the alert command. Real: [`CommandAlertSink`]; tests use fakes.
#[async_trait]
pub trait AlertSink: Send + Sync {
    /// Run `argv` with `payload` on stdin and `summary` in
    /// [`SUMMARY_ENV`], stopped after `timeout_ms`. `Ok(code)` once it ran
    /// to an exit; `Err` when it could not run, was killed, or timed out.
    async fn deliver(
        &self,
        argv: &[String],
        payload: &str,
        summary: &str,
        timeout_ms: u64,
    ) -> Result<i32, String>;
}

/// The real sink: spawns the program directly (no shell), writes the event
/// to its stdin, and kills it at the timeout.
pub struct CommandAlertSink;

#[async_trait]
impl AlertSink for CommandAlertSink {
    async fn deliver(
        &self,
        argv: &[String],
        payload: &str,
        summary: &str,
        timeout_ms: u64,
    ) -> Result<i32, String> {
        use tokio::io::AsyncWriteExt;
        let Some((program, args)) = argv.split_first() else {
            return Err("alert_command is empty".to_string());
        };
        // Its own process group, so the timeout stops what it started too
        // (a script's `curl` or `sleep`), not only the program itself.
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .env(SUMMARY_ENV, summary)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("could not run the alert command: {error}"))?;
        let group = child.id().and_then(|pid| i32::try_from(pid).ok());
        let stdin = child.stdin.take();
        let run = async {
            if let Some(mut stdin) = stdin {
                // A program that exits without reading its input is not a
                // failed delivery; its exit code decides.
                let _ = stdin.write_all(payload.as_bytes()).await;
            }
            child
                .wait()
                .await
                .map_err(|error| format!("alert command did not finish: {error}"))
        };
        let outcome = tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), run).await;
        match outcome {
            Ok(status) => status?
                .code()
                .ok_or_else(|| "alert command was killed by a signal".to_string()),
            Err(_) => {
                if let Some(group) = group {
                    // SAFETY: a plain kill(2) of the process group the
                    // command leads; no memory is touched.
                    unsafe {
                        libc::kill(-group, libc::SIGKILL);
                    }
                }
                // Reap the program so it leaves no zombie.
                let _ = child.kill().await;
                Err(format!(
                    "alert command timed out after {} s",
                    timeout_ms / 1000
                ))
            }
        }
    }
}

/// One alert event: the payload, minus `sent_at` (set at delivery).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AlertEvent {
    pub kind: String,
    pub lane: Option<String>,
    pub host: Option<String>,
    pub level: String,
    pub previous_level: Option<String>,
    pub detail: String,
    /// When the problem began (RFC 3339 UTC) and the same in clock ms.
    pub since: Option<String>,
    pub since_ms: Option<u64>,
}

/// What the capacity report shows about alert delivery.
#[derive(Debug, Clone, Serialize)]
pub struct AlertStatus {
    pub configured: bool,
    pub last_sent_at: Option<String>,
    pub last_error: Option<String>,
    pub pending: usize,
    /// Events dropped because more than 64 were pending.
    pub dropped: u64,
    pub next_try_at: Option<String>,
}

#[derive(Debug, Clone)]
struct Settings {
    argv: Option<Vec<String>>,
    after_ms: u64,
    repeat_ms: u64,
    timeout_ms: u64,
}

/// One lane's problem as the alerter tracks it.
#[derive(Debug, Default)]
struct Watch {
    /// The lane's level at the last check.
    level: Option<LaneLevel>,
    /// When the current problem began, and the level before it.
    since_ms: Option<u64>,
    level_before: Option<LaneLevel>,
    /// The kind last sent for this problem, and when.
    sent_kind: Option<String>,
    sent_ms: Option<u64>,
    /// The level of the problem that alerted, for `recovered`.
    alerted_level: Option<LaneLevel>,
}

#[derive(Debug, Default)]
struct State {
    watches: BTreeMap<String, Watch>,
    queue: VecDeque<AlertEvent>,
    delivering: bool,
    failures: u32,
    retry_at_ms: Option<u64>,
    last_sent_ms: Option<u64>,
    last_error: Option<String>,
    dropped: u64,
}

/// Watches lane reports and delivers alerts (see the module docs).
pub struct Alerter {
    settings: RwLock<Settings>,
    /// Secret values found in the config (inline keys and the values of the
    /// env vars it names) and every operator command line: never in a
    /// payload.
    secrets: RwLock<Vec<String>>,
    commands: RwLock<Vec<String>>,
    sink: Arc<dyn AlertSink>,
    clock: Arc<dyn Clock>,
    gateway: String,
    state: Mutex<State>,
}

/// Whether a config key names secret material.
fn secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "key",
        "token",
        "secret",
        "password",
        "credential",
        "authorization",
    ]
    .iter()
    .any(|word| key.contains(word))
}

/// Every secret value in `value`: strings under a secret-named key, the
/// key material of an account's `auth` block (`inline`, an OAuth `refresh`),
/// and the current value of every env var such a key names (`*_env`, and
/// `env` / `refresh_env` in an `auth` block). `in_auth`: inside an `auth`.
fn collect_secrets(
    value: &serde_json::Value,
    key: Option<&str>,
    in_auth: bool,
    out: &mut BTreeSet<String>,
) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                collect_secrets(value, Some(key), in_auth || key == "auth", out);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_secrets(item, key, in_auth, out);
            }
        }
        serde_json::Value::String(text) => {
            let Some(key) = key else {
                return;
            };
            let auth_secret =
                in_auth && matches!(key, "inline" | "refresh" | "env" | "refresh_env");
            if !secret_key(key) && !auth_secret {
                return;
            }
            if key.ends_with("_env") || key == "env" {
                if let Ok(value) = std::env::var(text) {
                    out.insert(value);
                }
            } else {
                out.insert(text.clone());
            }
        }
        _ => {}
    }
}

/// The machine's name, for the payload's `gateway` field.
fn gateway_name() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for its length; gethostname writes at
    // most that many bytes.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return "switchback".to_string();
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
    if name.is_empty() {
        "switchback".to_string()
    } else {
        name
    }
}

/// Text safe for an alert payload: every configured command line becomes
/// `<command>`, every known secret value `<redacted>`, a URL loses its
/// userinfo and query, and token-like words go through the gateway's own
/// inline-secret redaction.
pub(crate) fn redact_text(text: &str, secrets: &[String], commands: &[String]) -> String {
    let mut out = text.to_string();
    for command in commands {
        if !command.trim().is_empty() {
            out = out.replace(command.as_str(), "<command>");
        }
    }
    // URLs before secrets: a `<redacted>` inside a URL's userinfo would end
    // the URL early and leave the userinfo in place.
    let mut out = out
        .split(' ')
        .map(strip_url_credentials)
        .collect::<Vec<_>>()
        .join(" ");
    for secret in secrets {
        if secret.len() >= 6 {
            out = out.replace(secret.as_str(), "<redacted>");
        }
    }
    let mut redactions = 0;
    crate::setup_cli::sanitize_inline_secret_text(&out, &mut redactions)
}

/// `word` with the userinfo and the query (and fragment) taken out of
/// every `scheme://` URL in it, wherever the URL sits in the word: after
/// `url=`, in quotes, in brackets. The text around each URL is kept.
fn strip_url_credentials(word: &str) -> String {
    const ENDS: &[char] = &['"', '\'', '`', '<', '>', '(', ')', '[', ']', '{', '}', ','];
    let mut out = String::with_capacity(word.len());
    let mut rest = word;
    while let Some(at) = rest.find("://") {
        let scheme_start = rest[..at]
            .char_indices()
            .rev()
            .find(|(_, c)| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        let after = at + 3;
        let mut end = rest[after..]
            .find(ENDS)
            .map(|i| after + i)
            .unwrap_or(rest.len());
        while end > after && rest[..end].ends_with(['.', ':', ';']) {
            end -= 1;
        }
        out.push_str(&rest[..scheme_start]);
        let scheme = &rest[scheme_start..after];
        let tail = &rest[after..end];
        let authority_end = tail.find(['/', '?', '#']).unwrap_or(tail.len());
        let (authority, path) = tail.split_at(authority_end);
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        let path = path.split(['?', '#']).next().unwrap_or_default();
        out.push_str(scheme);
        out.push_str(host);
        out.push_str(path);
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// The kind of problem a lane report shows, and whether it alerts at once
/// (`true`) or after `alert_after_secs`. `None`: no problem to alert (an
/// `ok` lane, or a `warn` whose only cause is a lane with no wake command,
/// which is a choice in the config, not something that happened).
fn classify(report: &LocalLaneReport) -> Option<(&'static str, bool)> {
    if report.escalated || report.state == LaneState::Degraded {
        let kind = if report.poweroff_failures > 0 {
            "poweroff_failed"
        } else if report
            .last_wake_result
            .as_deref()
            .is_some_and(|result| result == "timeout" || result.starts_with("failed"))
        {
            "wake_failed"
        } else {
            "escalated"
        };
        return Some((kind, true));
    }
    if report.poweroff_guard_broken {
        return Some(("guard_broken", false));
    }
    match report.level {
        LaneLevel::Ok | LaneLevel::Skip => None,
        LaneLevel::Warn
            if !report.wake_configured
                && report.poweroff_failures == 0
                && report.state != LaneState::Waking =>
        {
            None
        }
        LaneLevel::Warn => Some(("lane_warn", false)),
        LaneLevel::Fail => Some(("lane_fail", false)),
    }
}

/// The problem a kind names, for repeats: `escalated` and `wake_failed`
/// are one escalation whose kind only says what the lane's last wake did,
/// and that changes with every wake attempt while the lane stays
/// escalated. Without this, a wake that keeps failing would alert on every
/// attempt instead of once per `alert_repeat_secs`.
fn problem_family(kind: &str) -> &str {
    match kind {
        "wake_failed" => "escalated",
        other => other,
    }
}

/// Kinds that belong to a machine, not one lane: one event per machine.
fn machine_kind(kind: &str) -> bool {
    matches!(kind, "poweroff_failed" | "guard_broken")
}

impl Alerter {
    pub fn new(clock: Arc<dyn Clock>, sink: Arc<dyn AlertSink>) -> Self {
        Self {
            settings: RwLock::new(Settings {
                argv: None,
                after_ms: 180_000,
                repeat_ms: 21_600_000,
                timeout_ms: 20_000,
            }),
            secrets: RwLock::new(Vec::new()),
            commands: RwLock::new(Vec::new()),
            sink,
            clock,
            gateway: gateway_name(),
            state: Mutex::new(State::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn settings(&self) -> Settings {
        self.settings
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Take the alert settings, the secrets and the command lines of a
    /// published config.
    pub fn configure(&self, cfg: &Config) {
        let server = &cfg.server;
        *self
            .settings
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Settings {
            argv: server.alert_command.clone().filter(|argv| !argv.is_empty()),
            after_ms: server.alert_after_secs.saturating_mul(1000),
            repeat_ms: server.alert_repeat_secs.saturating_mul(1000).max(1),
            timeout_ms: server.alert_timeout_secs.saturating_mul(1000).max(1),
        };
        let mut secrets = BTreeSet::new();
        if let Ok(value) = serde_json::to_value(cfg) {
            collect_secrets(&value, None, false, &mut secrets);
        }
        // Longest first, so a secret that contains another is replaced whole.
        let mut secrets: Vec<String> = secrets.into_iter().collect();
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        *self
            .secrets
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = secrets;
        let mut commands: Vec<String> = cfg
            .local_executors
            .iter()
            .flat_map(|lane| {
                [
                    &lane.wake_command,
                    &lane.poweroff_command,
                    &lane.restart_command,
                    &lane.release_command,
                    &lane.poweroff_guard_command,
                ]
                .into_iter()
                .flatten()
                .cloned()
            })
            .chain(server.alert_command.iter().map(|argv| argv.join(" ")))
            .collect();
        commands.sort_by_key(|command| std::cmp::Reverse(command.len()));
        commands.dedup();
        *self
            .commands
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = commands;
    }

    fn redact(&self, text: &str) -> String {
        let secrets = self
            .secrets
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let commands = self
            .commands
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        redact_text(text, &secrets, &commands)
    }

    /// Check the lane reports of one supervisor pass and queue the events
    /// due. Never waits; see [`Self::kick`] for delivery.
    pub fn check(&self, reports: &[LocalLaneReport]) {
        let settings = self.settings();
        if settings.argv.is_none() {
            return;
        }
        let now = self.clock.now_ms();
        let mut state = self.lock();
        state
            .watches
            .retain(|lane, _| reports.iter().any(|report| report.name == *lane));
        let mut events = Vec::new();
        let mut machine_sent: BTreeSet<(String, String)> = BTreeSet::new();
        for report in reports {
            let machine = report.host.clone().unwrap_or_else(|| report.name.clone());
            let watch = state.watches.entry(report.name.clone()).or_default();
            let previous = watch.level.replace(report.level);
            let Some((kind, at_once)) = classify(report) else {
                if report.level == LaneLevel::Ok {
                    if let Some(alerted) = watch.alerted_level.take() {
                        let event = AlertEvent {
                            kind: "recovered".to_string(),
                            lane: Some(report.name.clone()),
                            host: Some(machine.clone()),
                            level: LaneLevel::Ok.as_str().to_string(),
                            previous_level: Some(alerted.as_str().to_string()),
                            detail: self.redact(&report.detail),
                            since: watch.since_ms.map(rfc3339_ms),
                            since_ms: watch.since_ms,
                        };
                        let key = (format!("recovered/{:?}", watch.sent_kind), machine.clone());
                        let shared = watch.sent_kind.as_deref().is_some_and(machine_kind);
                        if !shared || machine_sent.insert(key) {
                            events.push(event);
                        }
                    }
                    *watch = Watch {
                        level: Some(report.level),
                        ..Watch::default()
                    };
                } else {
                    watch.since_ms = None;
                }
                continue;
            };
            let since = match watch.since_ms {
                Some(since) => since,
                None => {
                    watch.level_before = previous;
                    *watch.since_ms.insert(now)
                }
            };
            if !at_once && now.saturating_sub(since) < settings.after_ms {
                continue;
            }
            let repeat =
                watch.sent_kind.as_deref().map(problem_family) == Some(problem_family(kind));
            if repeat
                && watch
                    .sent_ms
                    .is_some_and(|sent| now.saturating_sub(sent) < settings.repeat_ms)
            {
                continue;
            }
            watch.sent_kind = Some(kind.to_string());
            watch.sent_ms = Some(now);
            watch.alerted_level = Some(report.level);
            if machine_kind(kind) && !machine_sent.insert((kind.to_string(), machine.clone())) {
                continue;
            }
            events.push(AlertEvent {
                kind: kind.to_string(),
                lane: Some(report.name.clone()),
                host: Some(machine),
                level: report.level.as_str().to_string(),
                previous_level: watch.level_before.map(|level| level.as_str().to_string()),
                detail: self.redact(&report.detail),
                since: Some(rfc3339_ms(since)),
                since_ms: Some(since),
            });
        }
        for event in events {
            tracing::warn!(kind = %event.kind, lane = ?event.lane, "local lane alert queued");
            state.queue.push_back(event);
            if state.queue.len() > MAX_PENDING {
                state.queue.pop_front();
                state.dropped += 1;
            }
        }
    }

    /// The payload and summary of `event` sent now.
    fn render(&self, event: &AlertEvent) -> (String, String) {
        let now = self.clock.now_ms();
        let mut payload = serde_json::to_value(event).unwrap_or_default();
        if let Some(map) = payload.as_object_mut() {
            map.insert("schema".to_string(), ALERT_SCHEMA.into());
            map.insert("sent_at".to_string(), rfc3339_ms(now).into());
            map.insert("gateway".to_string(), self.gateway.clone().into());
        }
        let summary = format!(
            "switchback {}: {}{} is {}: {}",
            event.kind,
            event.lane.as_deref().unwrap_or("-"),
            event
                .host
                .as_deref()
                .filter(|host| Some(*host) != event.lane.as_deref())
                .map(|host| format!(" on {host}"))
                .unwrap_or_default(),
            event.level,
            event.detail
        )
        .replace(['\n', '\r'], " ");
        (payload.to_string(), summary)
    }

    /// Start a background delivery of the pending events unless one runs or
    /// the retry gap after a failure has not passed. Returns at once.
    pub fn kick(self: &Arc<Self>) {
        let Some(argv) = self.settings().argv else {
            return;
        };
        {
            let mut state = self.lock();
            let now = self.clock.now_ms();
            if state.delivering
                || state.queue.is_empty()
                || state.retry_at_ms.is_some_and(|at| now < at)
            {
                return;
            }
            state.delivering = true;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.lock().delivering = false;
            return;
        };
        let done = DeliveryDone(self.clone());
        handle.spawn(async move {
            let alerter = done.0.clone();
            loop {
                let Some(event) = alerter.lock().queue.front().cloned() else {
                    break;
                };
                let (payload, summary) = alerter.render(&event);
                let timeout_ms = alerter.settings().timeout_ms;
                let result = alerter
                    .sink
                    .deliver(&argv, &payload, &summary, timeout_ms)
                    .await;
                let now = alerter.clock.now_ms();
                let mut state = alerter.lock();
                match result {
                    Ok(0) => {
                        if state.queue.front() == Some(&event) {
                            state.queue.pop_front();
                        }
                        state.failures = 0;
                        state.retry_at_ms = None;
                        state.last_error = None;
                        state.last_sent_ms = Some(now);
                    }
                    failed => {
                        let error = match failed {
                            Ok(code) => format!("alert command exited {code}"),
                            Err(error) => error,
                        };
                        state.failures += 1;
                        let gap = FIRST_RETRY_GAP_MS
                            .saturating_mul(1 << state.failures.saturating_sub(1).min(10))
                            .min(MAX_RETRY_GAP_MS);
                        state.retry_at_ms = Some(now.saturating_add(gap));
                        tracing::error!(%error, retry_in_ms = gap, "alert delivery failed; kept pending");
                        state.last_error = Some(error);
                        break;
                    }
                }
            }
            drop(done);
        });
    }

    /// Send one `test` event now and return the command's exit code. It
    /// does not touch the pending events. `Err` when no command is
    /// configured, or it could not run or timed out.
    pub async fn send_test(&self) -> Result<i32, String> {
        let settings = self.settings();
        let Some(argv) = settings.argv else {
            return Err("server.alert_command is not set".to_string());
        };
        let event = AlertEvent {
            kind: "test".to_string(),
            lane: None,
            host: None,
            level: LaneLevel::Ok.as_str().to_string(),
            previous_level: None,
            detail: "test alert sent by POST /v1/admin/alerts/test".to_string(),
            since: None,
            since_ms: None,
        };
        let (payload, summary) = self.render(&event);
        let result = self
            .sink
            .deliver(&argv, &payload, &summary, settings.timeout_ms)
            .await;
        let now = self.clock.now_ms();
        let mut state = self.lock();
        match &result {
            Ok(0) => state.last_sent_ms = Some(now),
            Ok(code) => state.last_error = Some(format!("test alert: command exited {code}")),
            Err(error) => state.last_error = Some(format!("test alert: {error}")),
        }
        result
    }

    /// What the capacity report shows.
    pub fn status(&self) -> AlertStatus {
        let configured = self.settings().argv.is_some();
        let state = self.lock();
        AlertStatus {
            configured,
            last_sent_at: state.last_sent_ms.map(rfc3339_ms),
            last_error: state.last_error.clone(),
            pending: state.queue.len(),
            dropped: state.dropped,
            next_try_at: state
                .retry_at_ms
                .filter(|_| !state.queue.is_empty())
                .map(rfc3339_ms),
        }
    }
}

/// Clears the "delivering" flag on every end of a delivery task (done,
/// failed, panicked, runtime gone), so a lost task never stops alerts.
struct DeliveryDone(Arc<Alerter>);

impl Drop for DeliveryDone {
    fn drop(&mut self) {
        self.0.lock().delivering = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_removes_commands_secrets_and_url_credentials() {
        // The user info is joined at run time: no credential-shaped URL in
        // the source.
        let userinfo = ["user", "pw"].join(":");
        let text = format!("wake `ssh gw wake-box` failed: GET https://{userinfo}@engine.example/health?token=abc123 refused, key local-key-7f3a9c, bearer zzzzzz");
        let out = redact_text(
            &text,
            &["local-key-7f3a9c".to_string()],
            &["ssh gw wake-box".to_string()],
        );
        for gone in ["ssh gw", "user:pw", "abc123", "local-key-7f3a9c", "zzzzzz"] {
            assert!(!out.contains(gone), "{gone} in {out}");
        }
        assert!(
            out.contains("<command>") && out.contains("engine.example"),
            "{out}"
        );
    }

    // An account's key sits in its `auth` block under names that do not
    // say "key" (`inline`, `env`, an OAuth `refresh`): read from a real
    // config, they are found too.
    #[test]
    fn account_auth_secrets_are_found() {
        let cfg = Config::from_yaml(
            r#"
providers:
  - id: upstream
    type: openai_compatible
    base_url: "http://upstream.example/v1"
    accounts:
      - id: inline
        auth: { kind: api_key, inline: "account-inline-secret-3" }
      - id: from-env
        auth: { kind: api_key, env: PATH }
      - id: oauth
        auth: { kind: oauth, token: "oauth-token-secret-4", refresh: "oauth-refresh-secret-5" }
"#,
        )
        .unwrap();
        let mut out = BTreeSet::new();
        collect_secrets(&serde_json::to_value(&cfg).unwrap(), None, false, &mut out);
        for secret in [
            "account-inline-secret-3",
            "oauth-token-secret-4",
            "oauth-refresh-secret-5",
        ] {
            assert!(out.contains(secret), "{secret} missing from {out:?}");
        }
        let path = std::env::var("PATH").unwrap();
        assert!(
            out.contains(&path),
            "the value of the env var an account names"
        );
        assert!(!out.contains("http://upstream.example/v1"));
    }

    // A credentialed URL loses its userinfo and query wherever it sits in
    // a word, and the text around it stays.
    #[test]
    fn url_credentials_are_removed_in_any_position() {
        let userinfo = ["user", "pw4u"].join(":");
        for (text, want) in [
            (
                format!("url=http://{userinfo}@engine.example/health"),
                "url=http://engine.example/health",
            ),
            (
                format!("\"https://{userinfo}@engine.example:8188/view?key=abc123\""),
                "\"https://engine.example:8188/view\"",
            ),
            (
                format!("<http://{userinfo}@engine.example/health>"),
                "<http://engine.example/health>",
            ),
            (
                format!("[http://{userinfo}@engine.example]"),
                "[http://engine.example]",
            ),
            (
                "(http://engine.example/health?token=abc123).".to_string(),
                "(http://engine.example/health).",
            ),
            (
                "see http://engine.example/health now".to_string(),
                "see http://engine.example/health now",
            ),
        ] {
            assert_eq!(redact_text(&text, &[], &[]), want, "{text}");
        }
    }

    // An alert command that hangs is stopped at its timeout together with
    // everything it started (a script's `curl`, `sleep`, ...), so a month
    // of hanging deliveries leaves no processes behind.
    #[tokio::test]
    async fn a_hanging_alert_command_is_killed_with_its_children_at_the_timeout() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let pid_file =
            std::env::temp_dir().join(format!("sb-alert-child-{}-{nanos}", std::process::id()));
        let script = format!("sleep 30 & echo $! > '{}'; wait", pid_file.display());
        let argv = vec!["/bin/sh".to_string(), "-c".to_string(), script];
        let started = std::time::Instant::now();
        let result = CommandAlertSink.deliver(&argv, "{}", "summary", 500).await;
        assert!(result.unwrap_err().contains("timed out"));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        let child: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut gone = false;
        for _ in 0..50 {
            // SAFETY: signal 0 only asks whether the process exists.
            if unsafe { libc::kill(child, 0) } != 0 {
                gone = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            gone,
            "the command's child {child} still runs after the timeout"
        );
    }

    #[test]
    fn secrets_are_found_under_secret_keys_and_named_env_vars() {
        let value = serde_json::json!({
            "providers": [{"id": "x", "api_key": "inline-secret-1", "base_url": "http://a"}],
            "api_keys": [{"key": "inbound-secret-2", "tenant": "t"}],
            "name": "not-a-secret"
        });
        let mut out = BTreeSet::new();
        collect_secrets(&value, None, false, &mut out);
        assert!(out.contains("inline-secret-1") && out.contains("inbound-secret-2"));
        assert!(!out.contains("not-a-secret") && !out.contains("http://a"));
    }
}
