//! Per-request speed end to end: a fake OpenAI-compatible engine, with and
//! without llama.cpp `timings`, served streamed and collected through the
//! gateway; the usage records carry the numbers with their source, and
//! `GET /v1/targets/stats` summarizes them.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use futures::StreamExt;
use serde_json::{json, Value};

#[derive(Clone)]
struct Engine {
    /// Send llama.cpp `timings` (prompt 100 tokens in 50 ms, 4 tokens in
    /// 80 ms: 2000 and 50 tokens per second).
    timings: bool,
}

fn timings() -> Value {
    json!({"prompt_n": 100, "prompt_ms": 50.0, "predicted_n": 4, "predicted_ms": 80.0})
}

async fn chat(State(engine): State<Engine>, Json(body): Json<Value>) -> Response {
    if body["stream"].as_bool() == Some(true) {
        let mut last = json!({"id": "x", "model": "local",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 4, "total_tokens": 104}});
        if engine.timings {
            last["timings"] = timings();
        }
        // A 30 ms prompt phase, then tokens one by one about 30 ms apart,
        // so the gateway sees a real time to first token and decode window.
        let frames = futures::stream::iter(0..6).then(move |i| {
            let last = last.clone();
            async move {
                if i < 4 {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                }
                let frame = match i {
                    0..=3 => {
                        let word = ["a", "b", "c", "d"][i];
                        let chunk = json!({"id": "x", "model": "local", "choices": [{"index": 0,
                            "delta": {"content": word}, "finish_reason": null}]});
                        format!("data: {chunk}\n\n")
                    }
                    4 => format!("data: {last}\n\n"),
                    _ => "data: [DONE]\n\n".to_string(),
                };
                Ok::<_, std::convert::Infallible>(frame)
            }
        });
        return (
            [("content-type", "text/event-stream")],
            axum::body::Body::from_stream(frames),
        )
            .into_response();
    }
    let mut answer = json!({
        "id": "x", "object": "chat.completion", "model": "local",
        "choices": [{"index": 0, "finish_reason": "stop",
            "message": {"role": "assistant", "content": "abcd"}}],
        "usage": {"prompt_tokens": 100, "completion_tokens": 4, "total_tokens": 104}
    });
    if engine.timings {
        answer["timings"] = timings();
    }
    Json(answer).into_response()
}

async fn spawn_engine(timings: bool) -> String {
    let app = Router::new()
        .route("/chat/completions", post(chat))
        .with_state(Engine { timings });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

async fn spawn_gateway(cfg_yaml: &str) -> (String, Arc<sb_ledger::UsageLedger>) {
    let cfg = sb_core::Config::from_yaml(cfg_yaml).unwrap();
    let registry = sb_adapters::AdapterRegistry::from_config(&cfg).unwrap();
    let resolver = sb_credentials::CredentialResolver::from_config(&cfg).unwrap();
    let ledger = Arc::new(sb_ledger::UsageLedger::in_memory());
    let state = sb_server::AppState::new(
        Arc::new(cfg),
        Arc::new(registry),
        Arc::new(resolver),
        ledger.clone(),
    );
    let app = sb_server::build_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), ledger)
}

async fn ask(base: &str, model: &str, stream: bool) {
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model": model, "stream": stream,
            "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    let body = response.text().await.unwrap();
    assert!(
        !body.contains("predicted_n") && !body.contains("timings"),
        "engine timings never reach the client: {body}"
    );
}

fn speed_of(
    ledger: &sb_ledger::UsageLedger,
    provider: &str,
    streamed: bool,
) -> sb_ledger::RequestSpeed {
    ledger
        .recent(50)
        .into_iter()
        .find(|record| record.provider_id == provider && record.streamed == streamed)
        .unwrap_or_else(|| panic!("a usage record for {provider} streamed={streamed}"))
        .speed
}

#[tokio::test]
async fn usage_records_carry_engine_or_gateway_speed_and_stats_summarize_them() {
    use sb_ledger::SpeedSource::{Engine, Gateway};
    let llama = spawn_engine(true).await;
    let plain = spawn_engine(false).await;
    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: gpu-box
    type: openai_compatible
    base_url: "{llama}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
  - id: plain
    type: openai_compatible
    base_url: "{plain}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
routes:
  - name: local
    match: {{ model: "local" }}
    targets: ["gpu-box/local"]
  - name: other
    match: {{ model: "other" }}
    targets: ["plain/local"]
"#
    );
    let (gateway, ledger) = spawn_gateway(&cfg).await;

    // llama.cpp timings, streamed (timings on the last chunk) and collected.
    for stream in [true, false] {
        ask(&gateway, "local", stream).await;
        let speed = speed_of(&ledger, "gpu-box", stream);
        assert_eq!(speed.decode_tps, Some(50.0), "stream={stream}");
        assert_eq!(speed.decode_tps_source, Some(Engine));
        assert_eq!(speed.prompt_tps, Some(2000.0));
        assert_eq!(speed.prompt_tps_source, Some(Engine));
        assert!(speed.ttft_ms.is_some());
        assert_eq!(speed.ttft_source, Some(Gateway));
    }

    // No timings: the gateway's clocks. Streamed, four tokens about 30 ms
    // apart: decode = 4 tokens / (end - first token), well under 4/0.09 s.
    ask(&gateway, "other", true).await;
    let speed = speed_of(&ledger, "plain", true);
    assert_eq!(speed.decode_tps_source, Some(Gateway));
    let decode = speed.decode_tps.expect("computed from the gateway clock");
    assert!(decode > 0.0 && decode < 4.0 / 0.085, "{decode}");
    assert_eq!(speed.prompt_tps_source, Some(Gateway));
    let prompt = speed
        .prompt_tps
        .expect("100 prompt tokens over the time to first token");
    assert!(prompt > 0.0 && prompt < 100.0 / 0.025, "{prompt}");
    // Collected in one piece: no decode window to measure, so no decode
    // speed is made up; time to first token and prompt speed still are.
    ask(&gateway, "other", false).await;
    let speed = speed_of(&ledger, "plain", false);
    assert_eq!((speed.decode_tps, speed.decode_tps_source), (None, None));
    assert!(speed.ttft_ms.is_some());

    let stats: Value = reqwest::get(format!("{gateway}/v1/targets/stats"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let data = stats["data"].as_array().unwrap();
    let gpu = data
        .iter()
        .find(|row| row["target"] == "gpu-box/local")
        .unwrap();
    assert_eq!(gpu["count"], 2);
    assert_eq!(gpu["p50_decode_tps"], 50.0);
    assert_eq!(gpu["p50_prompt_tps"], 2000.0);
    assert_eq!(gpu["decode_sources"]["engine"], 2);
    assert!(gpu["p50_ttft_ms"].is_u64());
    assert!(gpu["last_seen_unix"].as_u64().unwrap() > 0);
    let plain = data
        .iter()
        .find(|row| row["target"] == "plain/local")
        .unwrap();
    assert_eq!(plain["count"], 2);
    assert_eq!(plain["decode_sources"]["gateway"], 1);
    assert_eq!(stats["window"]["records"], 4);
}

// Client keys may read the summary (read-only), like the capacity report.
#[tokio::test]
async fn a_client_key_reads_target_stats() {
    let (gateway, _ledger) = spawn_gateway(
        r#"
server:
  bind: "127.0.0.1:0"
tenants:
  - id: team
api_keys:
  - key: "client-key"
    tenant: team
    role: client
providers:
  - id: mock
    type: mock
routes:
  - name: default
    match: { model: "*" }
    targets: ["mock/echo"]
"#,
    )
    .await;
    let client = reqwest::Client::new();
    let ok = client
        .get(format!("{gateway}/v1/targets/stats"))
        .bearer_auth("client-key")
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    let refused = client
        .get(format!("{gateway}/v1/config"))
        .bearer_auth("client-key")
        .send()
        .await
        .unwrap();
    assert_eq!(
        refused.status(),
        403,
        "client keys stay off operator surfaces"
    );
    let anonymous = client
        .get(format!("{gateway}/v1/targets/stats"))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), 401);
}

/// One usage record with a speed, as tenant `tenant` on `provider/echo`.
fn record_as(ledger: &sb_ledger::UsageLedger, tenant: &str, provider: &str, ttft_ms: u64) {
    ledger.record(
        sb_ledger::UsageRecord::new(
            format!("{tenant}-{provider}-{ttft_ms}"),
            provider,
            "echo",
            None,
            sb_core::Usage::default(),
            ttft_ms,
            false,
            &sb_core::Catalog::default(),
        )
        .with_tenant(Some(tenant.to_string()))
        .with_speed(sb_ledger::RequestSpeed {
            ttft_ms: Some(ttft_ms),
            ttft_source: Some(sb_ledger::SpeedSource::Gateway),
            ..Default::default()
        }),
    );
}

/// The `target -> count` rows `key` sees on the summary.
async fn stats_seen_by(gateway: &str, key: &str) -> Vec<(String, u64)> {
    let stats: Value = reqwest::Client::new()
        .get(format!("{gateway}/v1/targets/stats"))
        .bearer_auth(key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    stats["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["target"].as_str().unwrap().to_string(),
                row["count"].as_u64().unwrap(),
            )
        })
        .collect()
}

// A client key sees only what its own tenant made, and only on targets its
// scoped config can reach: nothing of another tenant. Admin keys see all.
#[tokio::test]
async fn a_client_key_sees_only_its_own_tenants_targets() {
    let (gateway, ledger) = spawn_gateway(
        r#"
server:
  bind: "127.0.0.1:0"
tenants:
  - id: team-a
    allowed_providers: ["mock"]
  - id: team-b
api_keys:
  - key: "client-a"
    tenant: team-a
    role: client
  - key: "client-b"
    tenant: team-b
    role: client
  - key: "admin-key"
    tenant: team-a
    role: admin
providers:
  - id: mock
    type: mock
  - id: secret
    type: mock
routes:
  - name: default
    match: { model: "*" }
    targets: ["mock/echo", "secret/echo"]
"#,
    )
    .await;
    record_as(&ledger, "team-a", "mock", 10);
    record_as(&ledger, "team-b", "mock", 20);
    // Used only by tenant B.
    record_as(&ledger, "team-b", "secret", 30);
    // Tenant A on a target outside its scope (say, before a config change).
    record_as(&ledger, "team-a", "secret", 40);

    assert_eq!(
        stats_seen_by(&gateway, "client-a").await,
        vec![("mock/echo".to_string(), 1)],
        "tenant A sees neither B's records nor a target it cannot reach"
    );
    assert_eq!(
        stats_seen_by(&gateway, "client-b").await,
        vec![("mock/echo".to_string(), 1), ("secret/echo".to_string(), 1)]
    );
    assert_eq!(
        stats_seen_by(&gateway, "admin-key").await,
        vec![("mock/echo".to_string(), 2), ("secret/echo".to_string(), 2)]
    );
}
