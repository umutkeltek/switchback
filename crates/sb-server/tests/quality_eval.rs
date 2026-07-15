//! Mandatory live-traffic quality-evaluation smoke: real TCP HTTP, SQLite WAL,
//! stream + collect capture, deferred-worker barrier, recursion guards,
//! metadata-only persistence, restart replay, scoped judge failure isolation,
//! and dormant routing influence constrained by outcome tiers.

use std::sync::Arc;
use std::time::Duration;

use reqwest::StatusCode;
use sb_store::StateStore as _;
use serde_json::{json, Value};

struct Harness {
    base: String,
    engine: Arc<sb_runtime::Engine>,
    server: tokio::task::JoinHandle<()>,
}

fn unique_path(name: &str, extension: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "{name}-{}-{}.{}",
        std::process::id(),
        sb_core::new_id("qe"),
        extension
    ))
}

fn write_cost_map(path: &std::path::Path) {
    let models = [
        ("served-low", "echo"),
        ("served-high", "echo"),
        ("judge", "quality-judge"),
        ("judge", "always-error"),
        ("fallback", "quality-judge"),
    ]
    .into_iter()
    .map(|(provider_id, model_id)| {
        json!({
            "provider_id": provider_id,
            "model_id": model_id,
            "input_micros_per_mtok": 1_000_000,
            "output_micros_per_mtok": 1_000_000,
        })
    })
    .collect::<Vec<_>>();
    std::fs::write(
        path,
        serde_json::to_vec(&json!({"models": models})).unwrap(),
    )
    .unwrap();
}

fn config_yaml(
    db: &std::path::Path,
    cost_map: &std::path::Path,
    routing_weight: f64,
    judge_targets: &[&str],
    allowed_target: &str,
    high_fails: bool,
) -> String {
    let judge_targets = judge_targets
        .iter()
        .map(|target| format!("      - \"{target}\""))
        .collect::<Vec<_>>()
        .join("\n");
    let fast_demote_streak = if high_fails { 1 } else { 3 };
    let high_accounts = if high_fails {
        "\n    accounts:\n      - { id: fail-account, auth: { kind: none } }"
    } else {
        ""
    };
    format!(
        r#"
server:
  bind: "127.0.0.1:0"
  state_store: "{db}"
  cost_map: "{cost_map}"
  scorecard:
    enabled: true
    demotion:
      min_samples: 8
      fast_demote_streak: {fast_demote_streak}
  quality_eval:
    enabled: true
    judge_route: auto/judge
    body_allowed_targets: ["{allowed_target}"]
    max_judgments_per_24h: 60
    max_cost_micros_per_24h: 500000
    min_input_chars: 32
    min_output_chars: 64
    max_input_bytes: 32768
    max_output_bytes: 32768
    capture_slots: 8
    queue_capacity: 8
    judge_timeout_ms: 2000
    judge_max_output_tokens: 128
    failure_backoff_after: 3
    failure_backoff_secs: 60
    ewma_alpha: 0.20
    routing_min_samples: 1
    routing_full_confidence_samples: 1
    routing_weight: {routing_weight}
providers:
  - id: served-low
    type: mock
  - id: served-high
    type: mock{high_accounts}
  - id: judge
    type: mock
  - id: fallback
    type: mock
routes:
  - name: quality-judge
    match: {{ model: "auto/judge" }}
    targets:
{judge_targets}
  - name: default
    match: {{ model: "*" }}
    targets:
      - "served-low/echo"
      - "served-high/echo"
"#,
        db = db.display(),
        cost_map = cost_map.display(),
    )
}

async fn spawn(cfg_yaml: &str, store: Arc<sb_store::SqliteStore>) -> Harness {
    let cfg = sb_core::Config::from_yaml(cfg_yaml).unwrap();
    sb_runtime::Engine::validate_config(&cfg).unwrap();
    let registry = sb_adapters::AdapterRegistry::from_config(&cfg).unwrap();
    let resolver = sb_credentials::CredentialResolver::from_config(&cfg).unwrap();
    let ledger = sb_ledger::UsageLedger::in_memory().with_store(store.clone());
    let engine = sb_runtime::Engine::new(
        Arc::new(cfg),
        Arc::new(registry),
        Arc::new(resolver),
        Arc::new(ledger),
    )
    .with_store(store);
    let state = sb_server::AppState::from_engine(engine);
    let engine = state.engine.clone();
    let app = sb_server::build_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Harness {
        base: format!("http://{addr}"),
        engine,
        server,
    }
}

async fn chat(base: &str, model: &str, content: &str, stream: bool) -> (StatusCode, String) {
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": model,
            "stream": stream,
            "messages": [{"role": "user", "content": content}],
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    (status, body)
}

async fn usage(base: &str) -> Value {
    reqwest::Client::new()
        .get(format!("{base}/v1/usage"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn preview(base: &str, model: &str) -> Value {
    reqwest::Client::new()
        .post(format!("{base}/cp/v1/route-preview"))
        .json(&json!({
            "model": model,
            "messages": [{"role": "user", "content": "preview only"}],
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn quality_reason(preview: &Value) -> Option<&str> {
    preview["decision"]["reason"]
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .find(|line| line.starts_with("outcome/quality "))
}

async fn wait_for_rows(
    store: &sb_store::SqliteStore,
    count: usize,
) -> Vec<sb_store::QualityJudgmentRecord> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let rows = store.recent_quality_judgments(20).unwrap();
            if rows.len() >= count && rows.iter().all(|row| row.status != "started") {
                break rows;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("quality worker completed")
}

#[tokio::test]
async fn live_quality_eval_serves_before_judging_excludes_recursion_and_replays() {
    let db = unique_path("switchback-quality-live", "sqlite");
    let cost_map = unique_path("switchback-quality-costs", "json");
    write_cost_map(&cost_map);
    let cfg = config_yaml(
        &db,
        &cost_map,
        0.0,
        &["judge/quality-judge"],
        "judge/quality-judge",
        false,
    );
    let store = Arc::new(sb_store::SqliteStore::open(db.to_str().unwrap()).unwrap());
    let first = spawn(&cfg, store.clone()).await;

    let fail_canary = format!("QUALITY_FAIL {}", "collected-response-canary ".repeat(4));
    let pass_canary = format!("QUALITY_PASS {}", "stream-response-canary ".repeat(4));
    let (collected_status, collected_body) =
        chat(&first.base, "served-low/echo", &fail_canary, false).await;
    assert_eq!(collected_status, StatusCode::OK);
    assert!(collected_body.contains("QUALITY_FAIL"));
    let (stream_status, stream_body) =
        chat(&first.base, "served-high/echo", &pass_canary, true).await;
    assert_eq!(stream_status, StatusCode::OK);
    assert!(stream_body.contains("[DONE]"));
    assert!(stream_body.contains("QUALITY_PASS"));

    // Extra ineligible high-target success gives routing a tiny independent
    // outcome advantage later, without adding a third quality sample.
    assert_eq!(
        chat(&first.base, "served-high/echo", "x", false).await.0,
        StatusCode::OK
    );

    let before = usage(&first.base).await;
    assert_eq!(before["quality_eval"]["queue_depth"], 2);
    assert_eq!(before["quality_eval"]["rolling_24h"]["attempted"], 0);
    assert!(store.recent_quality_judgments(20).unwrap().is_empty());
    assert!(store
        .recent_usage(20)
        .unwrap()
        .iter()
        .all(|event| event.tenant.as_deref() != Some("sb-internal")));
    println!("EVIDENCE barrier served_complete=2 judge_before_release=0 queue_depth=2");

    let worker = first
        .engine
        .clone()
        .spawn_quality_eval_worker()
        .expect("enabled worker");
    let rows = wait_for_rows(&store, 2).await;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row.status == "scored"));
    let low_row = rows
        .iter()
        .find(|row| row.served_target_id == "served-low/echo")
        .unwrap();
    let high_row = rows
        .iter()
        .find(|row| row.served_target_id == "served-high/echo")
        .unwrap();
    assert_eq!(low_row.score_norm, Some(0.0));
    assert_eq!(high_row.score_norm, Some(1.0));

    let judge_usage = store
        .recent_usage(20)
        .unwrap()
        .into_iter()
        .filter(|event| event.tenant.as_deref() == Some("sb-internal"))
        .collect::<Vec<_>>();
    assert_eq!(judge_usage.len(), 2);
    assert!(judge_usage.iter().all(|event| {
        event.project.as_deref() == Some("quality-eval")
            && event.cost_micros.unwrap_or_default() > 0
    }));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(store.recent_quality_judgments(20).unwrap().len(), 2);

    let persisted = serde_json::to_string(&rows).unwrap();
    let traces = serde_json::to_string(&first.engine.traces().recent(20)).unwrap();
    for canary in ["collected-response-canary", "stream-response-canary"] {
        assert!(!persisted.contains(canary));
        assert!(!traces.contains(canary));
    }
    for row in &rows {
        let judge_trace = first
            .engine
            .traces()
            .get(&row.judge_request_id)
            .expect("judge trace");
        let receipt = judge_trace
            .decision
            .receipt
            .as_ref()
            .expect("judge execution receipt");
        assert_eq!(receipt.cache.status, sb_core::CacheStatus::Bypass);
        assert!(receipt.cache.key.is_none());
        assert_eq!(receipt.job.context_fingerprint, "redacted:quality_eval");
    }
    let low_preview = preview(&first.base, "served-low/echo").await;
    let high_preview = preview(&first.base, "served-high/echo").await;
    assert!(quality_reason(&low_preview)
        .unwrap()
        .contains("q=0.000 n=1 age=0s rubric=quality-v1 mode=observe"));
    assert!(quality_reason(&high_preview)
        .unwrap()
        .contains("q=1.000 n=1 age=0s rubric=quality-v1 mode=observe"));
    println!(
        "EVIDENCE recursion judge_calls=2 rows=2 markers=task_type+internal_origin audit_body_free=true"
    );

    worker.abort();
    first.server.abort();
    drop(first.engine);
    drop(store);
    tokio::task::yield_now().await;

    let restarted_store = Arc::new(sb_store::SqliteStore::open(db.to_str().unwrap()).unwrap());
    let restarted = spawn(&cfg, restarted_store.clone()).await;
    let replay_low = preview(&restarted.base, "served-low/echo").await;
    let replay_high = preview(&restarted.base, "served-high/echo").await;
    let replay_low_reason = quality_reason(&replay_low).unwrap();
    let replay_high_reason = quality_reason(&replay_high).unwrap();
    assert!(replay_low_reason.contains("q=0.000 n=1"));
    assert!(replay_low_reason.contains("rubric=quality-v1 mode=observe"));
    assert!(replay_high_reason.contains("q=1.000 n=1"));
    assert!(replay_high_reason.contains("rubric=quality-v1 mode=observe"));
    println!("EVIDENCE restart low_q=0.000 high_q=1.000 samples=1 replay=true");

    // Rebuild fresh outcome evidence with capture-ineligible short requests,
    // clear latency state via reload, then show weight 0 keeps declared order
    // while 0.05 lets the two qualified live-quality peers reorder.
    assert_eq!(
        chat(&restarted.base, "served-low/echo", "x", false).await.0,
        StatusCode::OK
    );
    for _ in 0..2 {
        assert_eq!(
            chat(&restarted.base, "served-high/echo", "x", false)
                .await
                .0,
            StatusCode::OK
        );
    }
    restarted
        .engine
        .reload(sb_core::Config::from_yaml(&cfg).unwrap())
        .unwrap();
    let observe = preview(&restarted.base, "auto/cheap").await;
    assert_eq!(
        observe["decision"]["selected"]["target_id"],
        "served-low/echo"
    );

    let scoring_cfg = config_yaml(
        &db,
        &cost_map,
        0.05,
        &["judge/quality-judge"],
        "judge/quality-judge",
        true,
    );
    restarted
        .engine
        .reload(sb_core::Config::from_yaml(&scoring_cfg).unwrap())
        .unwrap();
    let scored = preview(&restarted.base, "auto/cheap").await;
    assert_eq!(
        scored["decision"]["selected"]["target_id"],
        "served-high/echo"
    );
    assert_eq!(
        scored["decision"]["scores"][0]["factors"]["response_quality"],
        1.0
    );

    // Trigger the unhealthy tier over real HTTP. The reloaded high provider's
    // sole `fail-account` fixture returns ProviderOverloaded, then its 2s
    // account cooldown expires so account health cannot mask the scorecard's
    // one-strike demotion reason.
    let _ = chat(&restarted.base, "served-high/echo", "x", false).await;
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let demoted = preview(&restarted.base, "auto/cheap").await;
    assert_eq!(
        demoted["decision"]["selected"]["target_id"],
        "served-low/echo"
    );
    assert!(demoted["decision"]["reason"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .any(|line| line.starts_with("outcome/demote target=served-high/echo")));
    println!("EVIDENCE routing weight=0.05 reordered=served-high tier_guard=served-low");

    restarted.server.abort();
    let _ = std::fs::remove_file(db);
    let _ = std::fs::remove_file(cost_map);
}

#[tokio::test]
async fn failing_allowlisted_judge_cannot_reach_a_healthy_nonallowlisted_fallback() {
    let db = unique_path("switchback-quality-failing", "sqlite");
    let cost_map = unique_path("switchback-quality-failing-costs", "json");
    write_cost_map(&cost_map);
    let cfg = config_yaml(
        &db,
        &cost_map,
        0.0,
        &["judge/always-error", "fallback/quality-judge"],
        "judge/always-error",
        false,
    );
    let store = Arc::new(sb_store::SqliteStore::open(db.to_str().unwrap()).unwrap());
    let harness = spawn(&cfg, store.clone()).await;
    let worker = harness
        .engine
        .clone()
        .spawn_quality_eval_worker()
        .expect("enabled worker");

    let content = format!("QUALITY_PASS {}", "served-despite-judge-failure ".repeat(4));
    let (status, body) = chat(&harness.base, "served-low/echo", &content, false).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("served-despite-judge-failure"));

    let rows = wait_for_rows(&store, 1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "failed");
    assert!(rows[0].score_norm.is_none());
    let judge_trace = harness
        .engine
        .traces()
        .get(&rows[0].judge_request_id)
        .expect("judge trace");
    assert_eq!(judge_trace.attempts.len(), 1);
    assert_eq!(judge_trace.attempts[0].target_id, "judge/always-error");
    assert!(store
        .recent_usage(20)
        .unwrap()
        .iter()
        .all(|event| event.tenant.as_deref() != Some("sb-internal")));
    let served_preview = preview(&harness.base, "served-low/echo").await;
    assert!(quality_reason(&served_preview).is_none());
    println!("EVIDENCE failing_judge served_status=200 terminal=failed scored=0 fallback_used=0");

    worker.abort();
    harness.server.abort();
    let _ = std::fs::remove_file(db);
    let _ = std::fs::remove_file(cost_map);
}
