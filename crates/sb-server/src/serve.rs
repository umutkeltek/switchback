use std::path::{Path, PathBuf};
use std::sync::Arc;

use sb_core::Config;
use sb_runtime::Engine;

use crate::{build_app, AppState};

pub(crate) fn engine_from_config(cfg: Config) -> anyhow::Result<Engine> {
    if let Err(e) = Engine::validate_config(&cfg) {
        anyhow::bail!("config validation failed: {e}");
    }
    let registry =
        sb_adapters::AdapterRegistry::from_config(&cfg).map_err(|e| anyhow::anyhow!(e))?;
    let resolver =
        sb_credentials::CredentialResolver::from_config(&cfg).map_err(|e| anyhow::anyhow!(e))?;
    Engine::try_new(
        Arc::new(cfg),
        Arc::new(registry),
        Arc::new(resolver),
        Arc::new(sb_ledger::UsageLedger::in_memory()),
    )
    .map_err(|e| anyhow::anyhow!(e))
}

pub(crate) fn route_preview_json(
    path: &Path,
    model: &str,
    stream: bool,
) -> anyhow::Result<serde_json::Value> {
    let cfg = Config::from_path(path)?;
    let engine = engine_from_config(cfg)?;
    let mut req =
        sb_core::AiRequest::new(model.to_string(), vec![sb_core::Message::user("preview")]);
    req.stream = stream;
    let (revision, plan) = engine
        .preview_route(&req)
        .map_err(|e| anyhow::anyhow!(e.message))?;
    let harness_candidates = engine.harness_candidates_for_plan(&plan);
    Ok(serde_json::json!({
        "revision": revision,
        "decision": plan.decision,
        "candidates": plan.candidates.iter().map(|c| &c.id).collect::<Vec<_>>(),
        "harness_candidates": harness_candidates,
    }))
}
pub(crate) async fn serve_gateway(
    config_path: PathBuf,
    bind: Option<String>,
    cfg: Config,
) -> anyhow::Result<()> {
    if let Err(e) = Engine::validate_config(&cfg) {
        anyhow::bail!("config validation failed: {e}");
    }
    let registry =
        sb_adapters::AdapterRegistry::from_config(&cfg).map_err(|e| anyhow::anyhow!(e))?;
    let resolver =
        sb_credentials::CredentialResolver::from_config(&cfg).map_err(|e| anyhow::anyhow!(e))?;
    // Durable control-plane + usage state (opt-in via `server.state_store`).
    // Opened once and shared by the ledger (usage events) and the engine
    // (config revisions + audit). Optional stores degrade to memory on
    // open failure; `required: true` fails startup.
    let store = open_state_store(&cfg)?;
    let eval_evidence = open_eval_evidence_snapshot(&cfg)?;
    let store_required = cfg
        .server
        .state_store
        .as_ref()
        .map(|s| s.required())
        .unwrap_or(false);
    let mut ledger = match &cfg.server.usage_log {
        Some(path) => sb_ledger::UsageLedger::with_sink(path),
        None => sb_ledger::UsageLedger::in_memory(),
    };
    if let Some(s) = &store {
        ledger = ledger.with_store(s.clone());
    }
    let traces = Arc::new(sb_trace::TraceLog::new(
        cfg.server.trace_ring_size,
        cfg.server.trace_log.clone().map(Into::into),
        cfg.server.trace_sample,
    ));
    // Transparent tap (Mode B) listeners + their opt-in body-capture sink,
    // captured before `cfg` is moved into the engine. Sink lives next to the
    // trace log (or cwd if none).
    let taps = cfg.server.taps.clone();
    let forward_proxies = cfg.server.forward_proxies.clone();
    let tap_capture_sink: PathBuf = cfg
        .server
        .trace_log
        .as_ref()
        .and_then(|p| std::path::Path::new(p).parent().map(|d| d.to_path_buf()))
        .unwrap_or_default()
        .join("tap-bodies.jsonl");
    let bind = bind.unwrap_or_else(|| cfg.server.bind.clone());
    validate_open_admin_bind(&cfg, &bind)?;
    if !is_loopback_bind(&bind) && !cfg.server.block_private_networks {
        // The SSRF guard ships off so local-first setups (ollama/vLLM on a
        // private IP) work out of the box; warn — don't flip the default — when
        // exposed on a non-loopback bind, where upstream/proxy/token URLs could
        // reach private or link-local addresses (e.g. 169.254.169.254).
        tracing::warn!(
            %bind,
            "non-loopback bind with server.block_private_networks=false: upstream/proxy/token URLs can reach private and link-local addresses; set server.block_private_networks: true to enable the SSRF guard"
        );
    }
    let mut engine = Engine::try_new(
        Arc::new(cfg),
        Arc::new(registry),
        Arc::new(resolver),
        Arc::new(ledger),
    )
    .map_err(|e| anyhow::anyhow!(e))?
    .with_traces(traces.clone());
    if let Some(s) = store {
        engine = engine
            .with_store_policy(s, store_required)
            .map_err(|e| anyhow::anyhow!(e))?;
    }
    engine.set_config_path(config_path);
    let mut state = AppState::from_engine(engine);
    if let Some(eval_evidence) = eval_evidence {
        state = state.with_eval_evidence(eval_evidence);
    }
    // outcome-routing-v1 §4: periodic scorecard flush. No-ops on every tick
    // when no state store is configured or the scorecard is disabled; a
    // store failure is logged and retried next tick (never affects
    // requests). Detached like the tap/proxy listeners below — this process
    // has no graceful-shutdown signal for any background task yet.
    state.engine.clone().spawn_scorecard_flusher();
    state.engine.clone().spawn_quality_eval_worker();
    let app = build_app(state);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "switchback listening");

    // Run the canonical gateway alongside every transparent-tap listener. Each
    // tap binds its own loopback port and forwards verbatim to its upstream.
    let mut servers = Vec::new();
    servers.push(tokio::spawn(async move {
        // Connect info feeds the loopback-only guard on `/admin/*`.
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
    }));
    for tap in &taps {
        if !is_loopback_bind(&tap.bind) {
            anyhow::bail!("tap `{}` bind `{}` must be loopback", tap.id, tap.bind);
        }
        let tap_listener = tokio::net::TcpListener::bind(&tap.bind).await?;
        let tap_app =
            crate::tap::build_tap_app(tap, traces.clone(), Some(tap_capture_sink.clone()));
        tracing::info!(
            tap = %tap.id, bind = %tap.bind, upstream = %tap.upstream,
            capture_bodies = tap.capture_bodies, "switchback tap listening"
        );
        servers.push(tokio::spawn(async move {
            axum::serve(tap_listener, tap_app).await
        }));
    }
    for proxy in &forward_proxies {
        if !is_loopback_bind(&proxy.bind) {
            anyhow::bail!(
                "forward proxy `{}` bind `{}` must be loopback",
                proxy.id,
                proxy.bind
            );
        }
        let proxy_listener = tokio::net::TcpListener::bind(&proxy.bind).await?;
        tracing::info!(
            proxy = %proxy.id,
            bind = %proxy.bind,
            capture_bodies = proxy.capture_bodies,
            intercept_hosts = ?proxy.intercept_hosts,
            "switchback forward proxy listening"
        );
        let proxy_server = crate::forward_proxy::spawn_forward_proxy_listener(
            proxy.clone(),
            proxy_listener,
            traces.clone(),
            Some(tap_capture_sink.clone()),
        )
        .await?;
        servers.push(tokio::spawn(async move {
            proxy_server
                .await
                .map_err(std::io::Error::other)?
                .map_err(std::io::Error::other)
        }));
    }
    for server in servers {
        server.await??;
    }
    Ok(())
}

pub(crate) fn open_state_store(
    config: &Config,
) -> anyhow::Result<Option<Arc<dyn sb_store::StateStore>>> {
    let Some(state_store) = config.server.state_store.as_ref() else {
        return Ok(None);
    };
    let path = state_store.path();
    match sb_store::SqliteStore::open(path) {
        Ok(store) => {
            tracing::info!(%path, "state store enabled (revisions + audit + usage)");
            Ok(Some(Arc::new(store)))
        }
        Err(error) if state_store.required() => Err(anyhow::anyhow!(
            "state store `{path}` is required but could not be opened: {error}"
        )),
        Err(error) => {
            tracing::warn!(error = %error, %path, "state store disabled: open failed");
            Ok(None)
        }
    }
}

pub(crate) fn open_eval_evidence_snapshot(
    config: &Config,
) -> anyhow::Result<Option<Arc<sb_eval::EvalEvidenceSnapshot>>> {
    let Some(state_store) = config.server.state_store.as_ref() else {
        return Ok(None);
    };
    let path = state_store.path();
    match sb_store::SqliteStore::open(path) {
        Ok(store) => match store.get_eval_evidence_snapshot("current") {
            Ok(Some(snapshot)) => {
                tracing::info!(
                    %path,
                    rows = snapshot.rows.len(),
                    snapshot_id = %snapshot.snapshot_id,
                    "published eval evidence snapshot enabled for route-preview"
                );
                Ok(Some(Arc::new(snapshot)))
            }
            Ok(None) => {
                tracing::info!(
                    %path,
                    "eval evidence disabled: no published `current` snapshot"
                );
                Ok(None)
            }
            Err(error) if state_store.required() => Err(anyhow::anyhow!(
                "eval evidence snapshot `{path}` required but could not be loaded: {error}"
            )),
            Err(error) => {
                tracing::warn!(error = %error, %path, "eval evidence disabled: load failed");
                Ok(None)
            }
        },
        Err(error) if state_store.required() => Err(anyhow::anyhow!(
            "eval evidence store `{path}` required but could not be opened: {error}"
        )),
        Err(error) => {
            tracing::warn!(error = %error, %path, "eval evidence disabled: open failed");
            Ok(None)
        }
    }
}

pub(crate) fn validate_open_admin_bind(config: &Config, bind: &str) -> anyhow::Result<()> {
    if config_requires_auth(config) || config.server.allow_open_admin || is_loopback_bind(bind) {
        return Ok(());
    }
    anyhow::bail!(
        "refusing unauthenticated admin gateway on non-loopback bind `{bind}`; configure server.api_key/api_keys or set server.allow_open_admin=true"
    )
}

fn config_requires_auth(config: &Config) -> bool {
    config
        .server
        .api_key
        .as_deref()
        .is_some_and(|key| !key.trim().is_empty())
        || !config.api_keys.is_empty()
}

fn is_loopback_bind(bind: &str) -> bool {
    let host = bind
        .rsplit_once(':')
        .map(|(host, _)| host)
        .unwrap_or(bind)
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']');
    matches!(host, "localhost" | "::1") || host.starts_with("127.")
}
