//! Live smoke (outcome-routing-v1 §8/§10 commit 6): the outcome scorecard
//! demotes a persistently-failing mock target below a healthy one within a
//! route group, evidences the demotion in `RouteDecision.reason`, and
//! survives a process restart as a SOFT-deprioritized prior (never a hard
//! demote — demotion is re-earned from live traffic, per spec §4).

use std::sync::Arc;

use serde_json::{json, Value};

/// `bad` gets exactly 3 accounts — one per expected failure, matching
/// `fast_demote_streak`. A single request against an always-failing target
/// exhausts EVERY configured account before falling over (the credential
/// layer's account-level fallover tries every account on a target before
/// moving to the next target), so after that request all 3 are briefly
/// locked and `healthy_accounts == Some(0)`. The mock's `always-error` model
/// uses `ErrorClass::ProviderOverloaded`, which the credential layer cools
/// down with a short exponential backoff (2s at its first offense) rather
/// than the flat 30s applied to `ServerError` — so this test only needs a
/// ~2.5s sleep for the pool to recover before asserting the scorecard (not
/// account-health) is what the router's demotion reason cites.
fn cfg_bad_good() -> &'static str {
    r#"
server:
  bind: "127.0.0.1:0"
  scorecard:
    demotion:
      min_samples: 8
      fast_demote_streak: 3
      fast_recover_streak: 3
providers:
  - id: bad
    type: mock
    accounts:
      - { id: a1, auth: { kind: none } }
      - { id: a2, auth: { kind: none } }
      - { id: a3, auth: { kind: none } }
  - id: good
    type: mock
routes:
  - name: default
    match: { model: "*" }
    targets:
      - "bad/always-error"
      - "good/echo"
"#
}

fn cfg_trunc_good() -> &'static str {
    r#"
server:
  bind: "127.0.0.1:0"
  scorecard:
    demotion:
      min_samples: 8
      fast_demote_streak: 3
      fast_recover_streak: 3
      trunc_demote_rate: 0.25
providers:
  - id: trunc
    type: mock
  - id: good
    type: mock
routes:
  - name: default
    match: { model: "*" }
    targets:
      - "trunc/always-truncated"
      - "good/echo"
"#
}

/// Spawn a switchback whose ledger AND engine share one state store, exactly
/// as `serve` wires it (mirrors `usage_durable.rs::spawn`) — returns the base
/// URL plus the `Engine` handle so the test can force a scorecard flush
/// without waiting on the background flusher (not spawned by this harness).
async fn spawn(
    cfg_yaml: &str,
    store: Arc<dyn sb_store::StateStore>,
) -> (String, Arc<sb_runtime::Engine>) {
    let cfg = sb_core::Config::from_yaml(cfg_yaml).unwrap();
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
    let engine_handle = state.engine.clone();
    let app = sb_server::build_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), engine_handle)
}

async fn chat_non_stream(base: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model":"m","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
}

async fn chat_stream(base: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
}

/// The explainable decision for the standard route WITHOUT executing it
/// (`POST /cp/v1/route-preview`) — the same surface `crates/sb-server/tests/cp.rs`
/// already uses to inspect `RouteDecision.reason` over HTTP.
async fn route_preview(base: &str) -> Value {
    reqwest::Client::new()
        .post(format!("{base}/cp/v1/route-preview"))
        .json(&json!({"model":"m","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn unique_db(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "{name}-{}-{}.sqlite",
        std::process::id(),
        sb_store::now_millis()
    ))
}

/// Find an `outcome/demote target=<id> ...` reason line whose target field
/// contains `needle` — the same shape as the spec §8 pattern
/// `outcome/demote target=.*bad`, matched on the target field directly since
/// this crate has no `regex` dependency to add for one test.
fn demote_line<'a>(reasons: &'a [Value], needle: &str) -> Option<&'a str> {
    reasons.iter().find_map(|r| {
        let line = r.as_str()?;
        let rest = line.strip_prefix("outcome/demote target=")?;
        let target = rest.split_whitespace().next().unwrap_or("");
        target.contains(needle).then_some(line)
    })
}

fn select_line(reasons: &[Value]) -> Option<&str> {
    reasons.iter().find_map(|r| {
        let line = r.as_str()?;
        line.starts_with("outcome/select target=").then_some(line)
    })
}

#[tokio::test]
async fn scorecard_demotes_a_failing_target_and_hydrates_soft_evidence_across_a_restart() {
    let db = unique_db("sb_scorecard_demotion");
    let _ = std::fs::remove_file(&db);
    let db_str = db.to_string_lossy().to_string();

    // --- process 1: trip the fast-demote streak on `bad` ---
    {
        let store: Arc<dyn sb_store::StateStore> =
            Arc::new(sb_store::SqliteStore::open(&db_str).unwrap());
        let (sb, engine) = spawn(cfg_bad_good(), store).await;

        // Before any traffic `bad` is declared first and carries no evidence.
        let preview = route_preview(&sb).await;
        assert_eq!(
            preview["decision"]["selected"]["target_id"], "bad/always-error",
            "declared order stands with no evidence yet: {preview}"
        );

        // One non-streaming request: `bad`'s account-level fallover tries all
        // 3 accounts (each fails, precommit — a legal fallover for both
        // streaming and non-streaming) before falling over to the next
        // TARGET, `good`, which serves it. That's 3 separately-recorded
        // TargetFailure samples on `bad/always-error` in one request —
        // exactly `fast_demote_streak`.
        let r1 = chat_non_stream(&sb).await;
        assert_eq!(r1.status(), 200, "fallover to good must succeed");
        let body1: Value = r1.json().await.unwrap();
        assert!(body1["choices"][0]["message"]["content"]
            .as_str()
            .unwrap()
            .contains("echo:"));

        // A second, streaming request — `bad`'s 3 accounts are already
        // locked (briefly) from the first request, so this one lands
        // directly on `good`'s streaming path. Together the two requests
        // exercise both the collect-finish and stream-finish scorecard write
        // paths (spec §8: "incl. 1 stream + 1 non-stream").
        let r2 = chat_stream(&sb).await;
        assert_eq!(r2.status(), 200, "fallover to good must succeed (stream)");
        let text2 = r2.text().await.unwrap();
        assert!(text2.contains("data:"));
        assert!(text2.contains("[DONE]"));

        // Let `bad`'s short `ProviderOverloaded` cooldown (2s) lapse so its
        // account pool is healthy again — otherwise the router's rank-2
        // ("no healthy accounts") demotion would mask the scorecard's rank-1
        // ("outcome/demote") reason we're about to assert on.
        tokio::time::sleep(std::time::Duration::from_millis(2_500)).await;

        // 3 consecutive TargetFailures on `bad` tripped fast_demote_streak(3);
        // its account pool has since recovered, so this is a scorecard
        // demotion (rank 1), not a no-healthy-accounts one (rank 2).
        let preview = route_preview(&sb).await;
        assert_eq!(
            preview["decision"]["selected"]["target_id"], "good/echo",
            "bad demoted to fallback, good selected first: {preview}"
        );
        let reasons = preview["decision"]["reason"].as_array().unwrap();
        let demote = demote_line(reasons, "bad")
            .unwrap_or_else(|| panic!("no outcome/demote line for bad in {reasons:?}"));
        assert!(demote.contains("reason=streak"), "line was: {demote}");

        // Force the flusher's one-shot equivalent (the background loop isn't
        // spawned by this harness) so the dirty aggregate is durable before
        // "the process exits".
        engine.flush_scorecard_once();
    }

    // --- process 2: brand-new engine/server against the SAME sqlite file ---
    {
        let store: Arc<dyn sb_store::StateStore> =
            Arc::new(sb_store::SqliteStore::open(&db_str).unwrap());
        let (sb, _engine) = spawn(cfg_bad_good(), store).await;

        // Restart-hydration proof, no live traffic yet. Spec §4/§8: a restart
        // yields SOFT deprioritization via the hydrated prior, not a hard
        // demote — re-earning demotion needs live evidence. So `bad` is
        // selected FIRST again (declared order, not reordered)...
        let preview = route_preview(&sb).await;
        assert_eq!(
            preview["decision"]["selected"]["target_id"], "bad/always-error",
            "a restart must not hard-demote — that's re-earned from live traffic: {preview}"
        );
        let reasons = preview["decision"]["reason"].as_array().unwrap();
        // ...but its `outcome/select` reason line cites the hydrated prior:
        // n=0 (no live samples yet this process) yet ok=0.0% (the flushed
        // aggregate's poor history), not the optimistic cold-start default.
        let select =
            select_line(reasons).unwrap_or_else(|| panic!("no outcome/select line in {reasons:?}"));
        assert!(select.contains("bad/always-error"), "line was: {select}");
        assert!(select.contains("ok=0.0%"), "line was: {select}");
        assert!(select.contains("n=0"), "line was: {select}");
    }
}

#[tokio::test]
async fn scorecard_demotes_a_truncating_target_with_reason_truncation() {
    let store: Arc<dyn sb_store::StateStore> =
        Arc::new(sb_store::SqliteStore::in_memory().unwrap());
    let (sb, _engine) = spawn(cfg_trunc_good(), store).await;

    // min_samples(8) truncated (FinishReason::Length) responses from `trunc`
    // accumulate the truncation-rate gate (1.0 >= 0.25); no ServerError is
    // ever returned, so the fast-demote streak (which only counts
    // TargetFailure) never fires — this isolates the truncation trigger.
    for _ in 0..8 {
        let r = chat_non_stream(&sb).await;
        assert_eq!(r.status(), 200, "a truncated response is still a 200");
    }

    let preview = route_preview(&sb).await;
    assert_eq!(
        preview["decision"]["selected"]["target_id"], "good/echo",
        "trunc demoted to fallback: {preview}"
    );
    let reasons = preview["decision"]["reason"].as_array().unwrap();
    let demote = demote_line(reasons, "trunc")
        .unwrap_or_else(|| panic!("no outcome/demote line for trunc in {reasons:?}"));
    assert!(demote.contains("reason=truncation"), "line was: {demote}");
}
