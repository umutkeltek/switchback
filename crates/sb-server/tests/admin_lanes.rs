//! `GET /admin/lanes` — the loopback-only lane-health API: per-lane targets,
//! read-only circuit state, rolling p50/p95 latency, last error, and request
//! counts, consumed by local orchestrators to prefer healthy lanes at runtime.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ConnectInfo;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt;

/// Upstream that always fails with 503 (to trip the breaker).
async fn always_503() -> axum::response::Response {
    (StatusCode::SERVICE_UNAVAILABLE, "down").into_response()
}

async fn spawn_dead_upstream() -> String {
    let app = Router::new().route("/chat/completions", post(always_503));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn app_for(cfg_yaml: &str) -> Router {
    let cfg = sb_core::Config::from_yaml(cfg_yaml).unwrap();
    let registry = sb_adapters::AdapterRegistry::from_config(&cfg).unwrap();
    let resolver = sb_credentials::CredentialResolver::from_config(&cfg).unwrap();
    let state = sb_server::AppState::new(
        Arc::new(cfg),
        Arc::new(registry),
        Arc::new(resolver),
        Arc::new(sb_ledger::UsageLedger::in_memory()),
    );
    sb_server::build_app(state)
}

async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    format!("http://{addr}")
}

async fn get_lanes(base: &str, query: &str) -> Value {
    let resp = reqwest::get(format!("{base}/admin/lanes{query}"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "/admin/lanes should serve on loopback");
    resp.json().await.unwrap()
}

fn lane<'a>(body: &'a Value, route: &str) -> &'a Value {
    body["lanes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["route"] == route)
        .unwrap_or_else(|| panic!("no lane for route `{route}`"))
}

const MOCK_CFG: &str = r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
routes:
  - name: scout-code
    match: { model: "scout/code" }
    targets:
      - "mock/echo"
  - name: default
    match: { model: "*" }
    targets:
      - "mock/echo"
"#;

const OPTIONAL_FALLBACK_CFG: &str = r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
  - id: missing
    type: openai_compatible
    base_url: "https://example.invalid/v1"
    accounts:
      - id: env
        auth: { kind: api_key, env: "SWITCHBACK_TEST_MISSING_ADMIN_LANES_KEY" }
routes:
  - name: scout-code
    match: { model: "scout/code" }
    targets:
      - "mock/echo"
      - "missing/optional"
"#;

#[tokio::test]
async fn healthy_lane_reports_targets_stats_and_percentiles() {
    let base = spawn(app_for(MOCK_CFG)).await;
    let client = reqwest::Client::new();
    for _ in 0..3 {
        let resp = client
            .post(format!("{base}/v1/chat/completions"))
            .json(&json!({"model":"scout/code","messages":[{"role":"user","content":"hi"}]}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    let body = get_lanes(&base, "").await;
    assert_eq!(body["schema"], "switchback/admin-lanes@1");
    assert_eq!(body["source"]["kind"], "recent_trace_ring");

    let lane = lane(&body, "scout-code");
    assert_eq!(lane["lane"], "scout/code", "lane = the route's match model");
    assert_eq!(lane["status"], "healthy");
    assert_eq!(lane["targets"][0]["id"], "mock/echo");
    assert_eq!(lane["targets"][0]["provider_id"], "mock");
    assert_eq!(lane["targets"][0]["circuit"]["state"], "closed");
    assert_eq!(lane["requests"]["window"], 3);
    assert_eq!(lane["requests"]["today"], 3);
    assert_eq!(lane["requests"]["errors_window"], 0);
    assert_eq!(lane["latency_ms"]["samples"], 3);
    assert!(lane["latency_ms"]["p50"].is_u64());
    assert!(lane["latency_ms"]["p95"].is_u64());
    assert!(lane["last_error"].is_null());
    assert!(lane["last_request_unix"].is_u64());

    // The wildcard lane saw no traffic.
    let default = self::lane(&body, "default");
    assert_eq!(default["lane"], "*");
    assert_eq!(default["requests"]["window"], 0);
    assert!(default["latency_ms"]["p50"].is_null());
}

#[tokio::test]
async fn since_param_moves_the_today_boundary() {
    let base = spawn(app_for(MOCK_CFG)).await;
    let client = reqwest::Client::new();
    client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model":"scout/code","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap();

    // A "today" boundary in the far future counts nothing as today.
    let body = get_lanes(&base, "?since=99999999999").await;
    let lane = lane(&body, "scout-code");
    assert_eq!(lane["requests"]["window"], 1);
    assert_eq!(lane["requests"]["today"], 0);
    assert_eq!(body["today_start_unix"], 99999999999u64);
}

#[tokio::test]
async fn unavailable_optional_fallback_does_not_degrade_a_usable_lane() {
    std::env::remove_var("SWITCHBACK_TEST_MISSING_ADMIN_LANES_KEY");
    let base = spawn(app_for(OPTIONAL_FALLBACK_CFG)).await;
    let body = get_lanes(&base, "").await;
    let lane = lane(&body, "scout-code");
    assert_eq!(lane["status"], "healthy");
    assert_eq!(lane["availability"]["known_targets"], 2);
    assert_eq!(lane["availability"]["unavailable_targets"], 1);
    assert_eq!(lane["availability"]["primary_unavailable"], false);
    assert_eq!(lane["targets"][0]["accounts"]["healthy"], 1);
    assert_eq!(lane["targets"][1]["accounts"]["healthy"], 0);
}

#[tokio::test]
async fn tripped_breaker_and_last_error_are_visible_without_consuming_the_probe() {
    let upstream = spawn_dead_upstream().await;
    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
  circuit_breaker: {{ enabled: true, failure_threshold: 1, open_secs: 60 }}
providers:
  - id: up
    type: openai_compatible
    base_url: "{upstream}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
routes:
  - name: local-fast
    match: {{ model: "local/fast" }}
    targets:
      - "up/m"
"#
    );
    let base = spawn(app_for(&cfg)).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model":"local/fast","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_server_error(), "dead upstream fails");

    // Poll the lane API repeatedly: the circuit must read open every time —
    // observation never consumes the breaker's half-open probe.
    for _ in 0..3 {
        let body = get_lanes(&base, "").await;
        let lane = lane(&body, "local-fast");
        assert_eq!(lane["status"], "down", "single blocked target = down");
        assert_eq!(lane["targets"][0]["circuit"]["state"], "open");
        assert!(lane["targets"][0]["circuit"]["open_remaining_ms"].is_u64());
        assert_eq!(lane["requests"]["errors_window"], 1);
        let err = &lane["last_error"];
        assert_eq!(err["target_id"], "up/m");
        assert!(err["class"].is_string());
        assert!(err["timestamp_unix"].is_u64());
    }
}

#[tokio::test]
async fn admin_lanes_refuses_non_loopback_peers() {
    let app = app_for(MOCK_CFG);

    let mut req = axum::http::Request::builder()
        .uri("/admin/lanes")
        .body(axum::body::Body::empty())
        .unwrap();
    req.extensions_mut()
        .insert(ConnectInfo::<SocketAddr>("192.0.2.7:9999".parse().unwrap()));
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "non-loopback refused");

    let mut req = axum::http::Request::builder()
        .uri("/admin/lanes")
        .body(axum::body::Body::empty())
        .unwrap();
    req.extensions_mut()
        .insert(ConnectInfo::<SocketAddr>("127.0.0.1:9999".parse().unwrap()));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "loopback served");
}
