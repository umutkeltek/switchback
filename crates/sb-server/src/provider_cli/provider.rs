use std::collections::HashSet;
use std::path::Path;
use std::time::Instant;

use futures::StreamExt;
use sb_core::{AiStreamEvent, Config};
use sb_runtime::ExecOutcome;

use crate::config_cli::{
    ensure_sequence, exact_route_mapping, mapping_str, write_file_atomic, yaml_key,
};
use crate::engine_from_config;

use super::{
    ProviderModelSummary, ProviderModelsSummary, ProviderSyncRoutesSummary, ProviderTestSummary,
};

pub(crate) async fn provider_sync_routes_config_file(
    path: &Path,
    provider_id: &str,
    prefix: Option<&str>,
    force: bool,
) -> anyhow::Result<ProviderSyncRoutesSummary> {
    let discovered = provider_models_config_file(path, provider_id).await?;
    let prefix = prefix
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(provider_id)
        .trim_end_matches('/')
        .to_string();
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
    let mut value: serde_yaml::Value = serde_yaml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("parse {} as YAML: {e}", path.display()))?;
    let root = value
        .as_mapping_mut()
        .ok_or_else(|| anyhow::anyhow!("{} must contain a YAML mapping", path.display()))?;
    let routes = ensure_sequence(root, "routes")?;

    let mut added = 0usize;
    let mut skipped = 0usize;
    let mut replaced = 0usize;
    for model in &discovered.models {
        let route_model = format!("{prefix}/{}", model.id);
        let route_entry = exact_route_mapping(&route_model, &model.switchback_model);
        match routes.iter().position(|entry| {
            entry
                .as_mapping()
                .and_then(|mapping| mapping.get(yaml_key("match")))
                .and_then(serde_yaml::Value::as_mapping)
                .and_then(|mapping| mapping_str(mapping, "model"))
                == Some(route_model.as_str())
        }) {
            Some(index) if force => {
                routes[index] = route_entry;
                replaced += 1;
            }
            Some(_) => skipped += 1,
            None => {
                routes.push(route_entry);
                added += 1;
            }
        }
    }

    let rendered = serde_yaml::to_string(&value)?;
    let cfg = Config::from_yaml(&rendered)?;
    let problems = cfg.semantic_problems();
    if !problems.is_empty() {
        anyhow::bail!("config would be invalid: {}", problems.join("; "));
    }
    write_file_atomic(path, &rendered)?;

    Ok(ProviderSyncRoutesSummary {
        ok: true,
        provider_id: provider_id.to_string(),
        prefix,
        discovered: discovered.models.len(),
        added,
        skipped,
        replaced,
    })
}

pub(crate) fn provider_scoped_config(cfg: &Config, provider_id: &str) -> anyhow::Result<Config> {
    let provider = cfg
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("provider `{provider_id}` is not configured"))?;
    let mut scoped = cfg.clone();
    scoped.providers = vec![provider];
    scoped.routes.clear();
    scoped.combos.clear();
    if scoped.server.default_provider.as_deref() != Some(provider_id) {
        scoped.server.default_provider = None;
    }
    scoped
        .server
        .budget
        .per_provider_usd
        .retain(|provider, _| provider == provider_id);
    Ok(scoped)
}

pub(crate) fn provider_model_hint(cfg: &Config, provider_id: &str) -> Option<String> {
    cfg.providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .and_then(|provider| provider.model_hint.as_deref())
        .map(str::trim)
        .filter(|hint| !hint.is_empty())
        .map(ToString::to_string)
}

pub(crate) async fn provider_test_config(
    cfg: Config,
    provider_id: &str,
    model: Option<&str>,
    stream: bool,
) -> anyhow::Result<ProviderTestSummary> {
    let resolved_model = match model.map(str::trim).filter(|value| !value.is_empty()) {
        Some(model) => model.to_string(),
        None => match provider_model_hint(&cfg, provider_id) {
            Some(model) => model,
            None => {
                let discovered = provider_models_config(cfg.clone(), provider_id).await?;
                discovered
                    .models
                    .first()
                    .map(|model| model.id.clone())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "provider `{provider_id}` did not report any models; pass --model"
                        )
                    })?
            }
        },
    };
    let engine = engine_from_config(cfg)?;
    let target_model = format!("{provider_id}/{resolved_model}");
    let mut req = sb_core::AiRequest::new(
        target_model.clone(),
        vec![sb_core::Message::user(
            "Switchback provider test. Reply briefly.",
        )],
    );
    req.max_output_tokens = Some(32);
    req.temperature = Some(0.0);
    req.stream = stream;

    let (_preview_revision, plan) = engine
        .preview_route(&req)
        .map_err(|e| anyhow::anyhow!(e.message))?;
    let selected = plan
        .decision
        .selected
        .as_ref()
        .map(|target| target.target_id.clone())
        .ok_or_else(|| anyhow::anyhow!("no selected target for `{target_model}`"))?;
    if selected != target_model {
        anyhow::bail!(
            "provider test selected `{selected}`, not requested `{target_model}`; check routes"
        );
    }

    let (revision, outcome) = engine.execute(req, Instant::now()).await;
    match outcome {
        ExecOutcome::Collected { response, summary } => {
            let response = *response;
            Ok(ProviderTestSummary {
                ok: true,
                revision,
                provider_id: provider_id.to_string(),
                model: resolved_model.clone(),
                target: selected,
                stream: false,
                summary,
                output_chars: response.message.text().chars().count(),
                event_count: 0,
                response_id: Some(response.id),
                finish_reason: Some(response.finish_reason),
                usage: Some(response.usage),
            })
        }
        ExecOutcome::Stream {
            mut stream,
            summary,
        } => {
            let mut event_count = 0usize;
            let mut output_chars = 0usize;
            let mut response_id = None;
            let mut finish_reason = None;
            let mut usage = None;
            while let Some(item) = stream.next().await {
                let event = item.map_err(|e| anyhow::anyhow!(e.message))?;
                event_count += 1;
                match event {
                    AiStreamEvent::MessageStart { id, .. } => {
                        response_id.get_or_insert(id);
                    }
                    AiStreamEvent::TextDelta { text } | AiStreamEvent::ReasoningDelta { text } => {
                        output_chars += text.chars().count();
                    }
                    AiStreamEvent::UsageDelta { usage: u } => usage = Some(u),
                    AiStreamEvent::MessageEnd { finish_reason: f } => finish_reason = Some(f),
                    AiStreamEvent::Error { message, .. } => anyhow::bail!(message),
                    AiStreamEvent::ToolCallStart(_)
                    | AiStreamEvent::ToolCallArgsDelta { .. }
                    | AiStreamEvent::ToolCallEnd { .. }
                    | AiStreamEvent::OutputImage { .. }
                    | AiStreamEvent::Citation { .. }
                    | AiStreamEvent::ServerToolCall { .. } => {}
                }
            }
            Ok(ProviderTestSummary {
                ok: true,
                revision,
                provider_id: provider_id.to_string(),
                model: resolved_model,
                target: selected,
                stream: true,
                summary,
                output_chars,
                event_count,
                response_id,
                finish_reason,
                usage,
            })
        }
        ExecOutcome::Error(e) => Err(anyhow::anyhow!(e.message)),
    }
}

pub(crate) async fn provider_test_config_file(
    path: &Path,
    provider_id: &str,
    model: Option<&str>,
    stream: bool,
) -> anyhow::Result<ProviderTestSummary> {
    let cfg = Config::from_path(path)?;
    let cfg = provider_scoped_config(&cfg, provider_id)?;
    provider_test_config(cfg, provider_id, model, stream).await
}

pub(crate) async fn provider_models_config(
    cfg: Config,
    provider_id: &str,
) -> anyhow::Result<ProviderModelsSummary> {
    let engine = engine_from_config(cfg)?;
    let snap = engine.snapshot();
    let adapter = snap
        .registry
        .adapter(provider_id)
        .ok_or_else(|| anyhow::anyhow!("provider `{provider_id}` is not configured"))?;

    let (account_id, lease) = match snap.resolver.resolve(provider_id, "", &HashSet::new()) {
        sb_credentials::ResolveOutcome::Selected { account_id, lease } => (account_id, lease),
        sb_credentials::ResolveOutcome::AllUnavailable { retry_after } => {
            let suffix = retry_after
                .map(|duration| format!("; retry after {}ms", duration.as_millis()))
                .unwrap_or_default();
            anyhow::bail!("provider `{provider_id}` has no available accounts{suffix}");
        }
        sb_credentials::ResolveOutcome::NoAccounts => {
            anyhow::bail!("provider `{provider_id}` has no accounts");
        }
    };
    let lease = snap
        .resolver
        .fresh_lease(provider_id, &account_id, lease)
        .await
        .map_err(|e| anyhow::anyhow!("refresh credential for `{provider_id}`: {e}"))?;
    let upstream_models = adapter
        .list_models(Some(lease), None)
        .await
        .map_err(|e| anyhow::anyhow!(e.message))?;

    let mut seen = HashSet::new();
    let models = upstream_models
        .into_iter()
        .filter(|id| seen.insert(id.clone()))
        .map(|id| ProviderModelSummary {
            switchback_model: format!("{provider_id}/{id}"),
            id,
        })
        .collect();

    Ok(ProviderModelsSummary {
        ok: true,
        revision: snap.revision,
        provider_id: provider_id.to_string(),
        models,
    })
}

pub(crate) async fn provider_models_config_file(
    path: &Path,
    provider_id: &str,
) -> anyhow::Result<ProviderModelsSummary> {
    let cfg = Config::from_path(path)?;
    let cfg = provider_scoped_config(&cfg, provider_id)?;
    provider_models_config(cfg, provider_id).await
}
