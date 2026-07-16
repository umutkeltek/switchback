//! `sb doctor local` — a stateless local-executor capacity probe, modelled on
//! the fal balance probe. For each configured `local_executors` lane it does a
//! one-shot health GET and reports reachability plus which command legs are
//! wired. Local executors are intentionally on the LAN/loopback, so the
//! private-network guard is deliberately NOT applied here.
//!
//! This is the config + live-health snapshot available without a running
//! gateway. The running server's own live lane state (queue depth, retries,
//! last wake result) is served at `GET /v1/workloads/capacity`.

use std::time::Duration;

use sb_core::{Config, LocalExecutorConfig};
use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LocalProbeLevel {
    Ok,
    Warn,
    Fail,
    Skip,
}

impl LocalProbeLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
            Self::Skip => "skip",
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct LocalLaneCheck {
    pub name: String,
    pub level: LocalProbeLevel,
    pub health_endpoint: String,
    pub reachable: bool,
    pub wake_configured: bool,
    pub poweroff_configured: bool,
    pub restart_configured: bool,
    pub detail: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct LocalCapacityReport {
    pub schema: &'static str,
    pub level: LocalProbeLevel,
    pub checks: Vec<LocalLaneCheck>,
}

impl LocalCapacityReport {
    fn new(checks: Vec<LocalLaneCheck>) -> Self {
        Self {
            schema: "switchback/local-capacity-doctor@1",
            level: aggregate_level(&checks),
            checks,
        }
    }

    fn skipped() -> Self {
        Self {
            schema: "switchback/local-capacity-doctor@1",
            level: LocalProbeLevel::Skip,
            checks: Vec::new(),
        }
    }
}

pub(crate) async fn local_capacity_report(cfg: &Config, timeout_ms: u64) -> LocalCapacityReport {
    if cfg.local_executors.is_empty() {
        return LocalCapacityReport::skipped();
    }
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms.max(1)))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return LocalCapacityReport::new(
                cfg.local_executors
                    .iter()
                    .map(|ex| failed_check(ex, format!("build probe client: {error}")))
                    .collect(),
            )
        }
    };
    let mut checks = Vec::with_capacity(cfg.local_executors.len());
    for executor in &cfg.local_executors {
        checks.push(probe_executor(&client, executor).await);
    }
    LocalCapacityReport::new(checks)
}

pub(crate) fn print_local_capacity_text(report: &LocalCapacityReport) {
    println!("local capacity doctor: {}", report.level.as_str());
    for check in &report.checks {
        println!("{} {}: {}", check.level.as_str(), check.name, check.detail);
    }
}

async fn probe_executor(
    client: &reqwest::Client,
    executor: &LocalExecutorConfig,
) -> LocalLaneCheck {
    let reachable = matches!(
        client.get(&executor.health_endpoint).send().await,
        Ok(response) if response.status().is_success()
    );
    let wake_configured = executor.wake_command.is_some();
    let poweroff_configured = executor.poweroff_command.is_some();
    let restart_configured = executor.restart_command.is_some();

    let (level, detail) = if reachable {
        (
            LocalProbeLevel::Ok,
            format!("{} healthy at {}", executor.name, executor.health_endpoint),
        )
    } else if !wake_configured {
        (
            LocalProbeLevel::Fail,
            format!(
                "{} offline and wake command is unconfigured; jobs will queue indefinitely",
                executor.name
            ),
        )
    } else {
        (
            LocalProbeLevel::Warn,
            format!(
                "{} offline; will wake on demand within {}s",
                executor.name, executor.boot_timeout_secs
            ),
        )
    };

    LocalLaneCheck {
        name: executor.name.clone(),
        level,
        health_endpoint: executor.health_endpoint.clone(),
        reachable,
        wake_configured,
        poweroff_configured,
        restart_configured,
        detail,
    }
}

fn failed_check(executor: &LocalExecutorConfig, detail: String) -> LocalLaneCheck {
    LocalLaneCheck {
        name: executor.name.clone(),
        level: LocalProbeLevel::Fail,
        health_endpoint: executor.health_endpoint.clone(),
        reachable: false,
        wake_configured: executor.wake_command.is_some(),
        poweroff_configured: executor.poweroff_command.is_some(),
        restart_configured: executor.restart_command.is_some(),
        detail,
    }
}

fn aggregate_level(checks: &[LocalLaneCheck]) -> LocalProbeLevel {
    if checks.iter().any(|c| c.level == LocalProbeLevel::Fail) {
        LocalProbeLevel::Fail
    } else if checks.iter().any(|c| c.level == LocalProbeLevel::Warn) {
        LocalProbeLevel::Warn
    } else if checks.iter().any(|c| c.level == LocalProbeLevel::Ok) {
        LocalProbeLevel::Ok
    } else {
        LocalProbeLevel::Skip
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use axum::Router;

    fn cfg_with_executor(health_endpoint: &str, wake: &str) -> Config {
        Config::from_yaml(&format!(
            r#"
server:
  bind: "127.0.0.1:8765"
providers:
  - id: comfy-local
    type: comfyui
    base_url: "http://executor-1.local:8188"
local_executors:
  - name: comfy-local
    base_url: "http://executor-1.local:8188"
    health_endpoint: "{health_endpoint}"
    wake_command: "{wake}"
"#
        ))
        .unwrap()
    }

    async fn fake_healthy() -> String {
        let app = Router::new().route("/system_stats", get(|| async { "ok" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}/system_stats")
    }

    #[tokio::test]
    async fn skips_cleanly_without_local_executors() {
        let cfg = Config::from_yaml("server:\n  bind: \"127.0.0.1:8765\"\n").unwrap();
        let report = local_capacity_report(&cfg, 100).await;
        assert_eq!(report.level, LocalProbeLevel::Skip);
        assert!(report.checks.is_empty());
    }

    #[tokio::test]
    async fn reports_ok_when_health_endpoint_is_reachable() {
        let endpoint = fake_healthy().await;
        let cfg = cfg_with_executor(&endpoint, "ssh gateway wake-host executor-1");
        let report = local_capacity_report(&cfg, 1_000).await;
        assert_eq!(report.level, LocalProbeLevel::Ok);
        assert!(report.checks[0].reachable);
    }

    #[tokio::test]
    async fn warns_when_offline_but_wakeable() {
        // Loopback port 1 is never listening: refused instantly, no timeout wait.
        let cfg = cfg_with_executor(
            "http://127.0.0.1:1/system_stats",
            "ssh gateway wake-host executor-1",
        );
        let report = local_capacity_report(&cfg, 200).await;
        assert_eq!(report.level, LocalProbeLevel::Warn);
        assert!(!report.checks[0].reachable);
    }
}
