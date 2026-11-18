//! `POST /v1/admin/alerts/test` over real HTTP with a real alert program:
//! the event reaches the program's stdin, the summary its env, the exit
//! code comes back, and only an admin key may send it.

use std::sync::Arc;

use serde_json::Value;

const ADMIN: &str = "sk-test-admin";
const OPERATOR: &str = "sk-test-operator";
const CLIENT: &str = "sk-test-client";

async fn serve(out: &std::path::Path, exit: u8) -> String {
    let script = format!(
        "cat > \"$0\" && printf '%s' \"$SWITCHBACK_ALERT_SUMMARY\" > \"$0.summary\" && exit {exit}"
    );
    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
  alert_command: ["/bin/sh", "-c", {script:?}, "{}"]
api_keys:
  - {{ key: "{ADMIN}", tenant: ops, role: admin }}
  - {{ key: "{OPERATOR}", tenant: ops, role: operator }}
  - {{ key: "{CLIENT}", tenant: team }}
tenants:
  - id: ops
  - id: team
providers:
  - id: mock
    type: mock
"#,
        out.display()
    );
    let cfg = sb_core::Config::from_yaml(&cfg).unwrap();
    let registry = sb_adapters::AdapterRegistry::from_config(&cfg).unwrap();
    let resolver = sb_credentials::CredentialResolver::from_config(&cfg).unwrap();
    let state = sb_server::AppState::new(
        Arc::new(cfg),
        Arc::new(registry),
        Arc::new(resolver),
        Arc::new(sb_ledger::UsageLedger::in_memory()),
    );
    let app = sb_server::build_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

async fn send(sb: &str, key: &str) -> (u16, Value) {
    let response = reqwest::Client::new()
        .post(format!("{sb}/v1/admin/alerts/test"))
        .bearer_auth(key)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

fn scratch(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("sb-alert-{name}-{}-{nanos}", std::process::id()))
}

#[tokio::test]
async fn an_admin_sends_a_test_alert_through_the_alert_command() {
    let out = scratch("ok");
    let sb = serve(&out, 0).await;
    assert_eq!(send(&sb, OPERATOR).await.0, 403, "admin keys only");
    assert_eq!(send(&sb, CLIENT).await.0, 403);

    let (status, body) = send(&sb, ADMIN).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["exit_code"], 0);
    assert_eq!(body["sent"], true);
    let payload: Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!(payload["kind"], "test");
    assert_eq!(payload["schema"], "switchback/lane-alert@1");
    assert!(payload["sent_at"].as_str().unwrap().ends_with('Z'));
    assert!(payload["gateway"]
        .as_str()
        .is_some_and(|name| !name.is_empty()));
    let summary = std::fs::read_to_string(out.with_extension("summary")).unwrap();
    assert!(summary.starts_with("switchback test:"), "{summary}");
}

#[tokio::test]
async fn a_failing_alert_command_reports_its_exit_code() {
    let out = scratch("fail");
    let sb = serve(&out, 3).await;
    let (status, body) = send(&sb, ADMIN).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["exit_code"], 3);
    assert_eq!(body["sent"], false);
    let report: Value = reqwest::Client::new()
        .get(format!("{sb}/v1/workloads/capacity"))
        .bearer_auth(ADMIN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(report["alerts"]["configured"], true);
    assert!(
        report["alerts"]["last_error"]
            .as_str()
            .unwrap()
            .contains("exited 3"),
        "{report}"
    );
}
