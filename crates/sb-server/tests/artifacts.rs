//! Generated artifacts: kept on disk with a bounded footprint, survive a
//! restart, and are readable by the principal whose job made them.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "sb-artifacts-it-{name}-{}-{nanos}",
        std::process::id()
    ))
}

fn config(dir: &Path, extra_server: &str) -> String {
    format!(
        r#"
server:
  bind: "127.0.0.1:0"
  artifacts:
    dir: "{dir}"
{extra_server}
api_keys:
  - key: "sk-test-admin"
    tenant: ops
    role: admin
  - key: "sk-test-operator"
    tenant: ops
    role: operator
  - key: "sk-test-client-a"
    tenant: team-a
  - key: "sk-test-client-a-other-project"
    tenant: team-a
    project: other
  - key: "sk-test-client-b"
    tenant: team-b
tenants:
  - id: ops
  - id: team-a
  - id: team-b
providers:
  - id: mock
    type: mock
routes:
  - name: default
    match: {{ model: "*" }}
    targets: ["mock/echo"]
"#,
        dir = dir.display()
    )
}

async fn serve(config: &str) -> String {
    let cfg = sb_core::Config::from_yaml(config).unwrap();
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

async fn generate(base: &str, key: &str, prompt: &str) -> Value {
    reqwest::Client::new()
        .post(format!("{base}/v1/images/generations"))
        .bearer_auth(key)
        .json(&json!({"model": "mock/image", "prompt": prompt, "size": "1x1"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn get(base: &str, key: &str, path: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("{base}{path}"))
        .bearer_auth(key)
        .send()
        .await
        .unwrap()
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files
}

#[tokio::test]
async fn an_artifact_survives_a_gateway_restart_and_nothing_but_metadata_is_written() {
    let dir = temp_dir("restart");
    let cfg = config(&dir, "");
    let first = serve(&cfg).await;
    let prompt = "a prompt that must never reach the artifact directory";
    let created = generate(&first, "sk-test-client-a", prompt).await;
    let artifact_id = created["data"][0]["artifact_id"]
        .as_str()
        .unwrap()
        .to_string();
    let job_id = created["job"]["id"].as_str().unwrap().to_string();
    let original = get(
        &first,
        "sk-test-admin",
        &format!("/v1/artifacts/{artifact_id}"),
    )
    .await
    .bytes()
    .await
    .unwrap();

    let job: Value = get(&first, "sk-test-admin", &format!("/v1/jobs/{job_id}"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(job["artifacts"][0]["retention"], "disk", "{job}");
    let storage_ref = job["artifacts"][0]["storage_ref"].as_str().unwrap();
    assert!(storage_ref.starts_with("blob:sha256/"), "{storage_ref}");

    // A second gateway on the same directory: a restart.
    let second = serve(&cfg).await;
    let response = get(
        &second,
        "sk-test-admin",
        &format!("/v1/artifacts/{artifact_id}"),
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "image/png");
    assert_eq!(
        response.headers()["content-length"],
        original.len().to_string().as_str()
    );
    assert_eq!(response.bytes().await.unwrap(), original);

    for file in files_under(&dir) {
        let text = String::from_utf8_lossy(&std::fs::read(&file).unwrap()).to_string();
        assert!(
            !text.contains(prompt),
            "{} holds the prompt",
            file.display()
        );
    }
}

#[tokio::test]
async fn an_artifact_removed_by_retention_answers_404_artifact_evicted() {
    let dir = temp_dir("evicted");
    let base = serve(&config(&dir, "    max_age_secs: 1")).await;
    let old = generate(&base, "sk-test-client-a", "first").await;
    let old_id = old["data"][0]["artifact_id"].as_str().unwrap().to_string();
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    // Writing the next artifact applies retention: the first is past the
    // age cap.
    let new = generate(&base, "sk-test-client-a", "second").await;
    let new_id = new["data"][0]["artifact_id"].as_str().unwrap().to_string();

    let response = get(&base, "sk-test-admin", &format!("/v1/artifacts/{old_id}")).await;
    assert_eq!(response.status(), 404);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], "artifact_evicted", "{body}");

    let response = get(&base, "sk-test-admin", &format!("/v1/artifacts/{new_id}")).await;
    assert_eq!(response.status(), 200);

    let response = get(&base, "sk-test-admin", "/v1/artifacts/art_never_made").await;
    assert_eq!(response.status(), 404);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], "not_found", "{body}");
}
