use std::collections::HashSet;
use std::path::Path;
use std::time::Instant;

use clap::Subcommand;
use futures::StreamExt;
use sb_core::{
    AiStreamEvent, AuthConfig, Config, FinishReason, ProviderConfig, ProviderKind, Usage,
};
use sb_runtime::{EmbeddingsOutcome, Engine, ExecOutcome};
use serde::Serialize;

use crate::config_cli::{
    clean_optional, ensure_sequence, exact_route_mapping, mapping_str, write_file_atomic, yaml_key,
    yaml_string,
};
use crate::engine_from_config;
use crate::provider_preset::{preset_defaults, ProviderPreset};

#[derive(Subcommand)]
pub(crate) enum ProviderCmd {
    /// List provider presets and their default onboarding settings.
    Presets,
    /// Print provider readiness manifests for agents and operators.
    Readiness {
        /// Optional preset to print. Omit to list every manifest.
        preset: Option<ProviderPreset>,
    },
    /// Append or replace a provider entry. Secrets are referenced by env var only.
    Add {
        preset: ProviderPreset,
        /// Override the provider id written to config.
        #[arg(long)]
        id: Option<String>,
        /// Override the upstream base URL.
        #[arg(long)]
        base_url: Option<String>,
        /// Override the API-key env var name. Empty value is treated as no auth.
        #[arg(long)]
        api_key_env: Option<String>,
        /// Optional upstream model id to add as an exact route target.
        #[arg(long)]
        model: Option<String>,
        /// Optional inbound route/alias for --model. Defaults to provider/model.
        #[arg(long)]
        route: Option<String>,
        /// Replace an existing provider or exact route with the same id/alias.
        #[arg(long)]
        force: bool,
    },
    /// Execute a tiny request against one configured provider/model.
    Test {
        provider: String,
        /// Upstream model id to test. Defaults to the first discoverable model.
        #[arg(long)]
        model: Option<String>,
        /// Exercise the provider's streaming path.
        #[arg(long)]
        stream: bool,
    },
    /// List upstream models visible to one configured provider/account.
    Models { provider: String },
    /// Discover upstream models and add exact provider/model routes.
    SyncRoutes {
        provider: String,
        /// Optional local route prefix. Defaults to the provider id.
        #[arg(long)]
        prefix: Option<String>,
        /// Replace existing routes with the same local model id.
        #[arg(long)]
        force: bool,
    },
    /// Run model discovery, route preview, chat, stream, and embeddings checks.
    Doctor {
        provider: String,
        /// Upstream model id to test. Defaults to the first discoverable model.
        #[arg(long)]
        model: Option<String>,
    },
    /// Produce a stable end-to-end readiness report for one provider.
    Certify {
        provider: String,
        /// Upstream model id to certify. Defaults to model_hint or discovery.
        #[arg(long)]
        model: Option<String>,
    },
    /// Produce readiness certifications for every configured provider.
    CertifyAll,
    /// Run provider doctor across every configured provider.
    Matrix,
}

#[derive(Debug)]
pub(crate) struct ProviderAddSummary {
    pub(crate) provider_id: String,
    pub(crate) api_key_env: Option<String>,
    pub(crate) route_model: Option<String>,
    pub(crate) target: Option<String>,
}

pub(crate) struct ProviderAddRequest {
    pub(crate) preset: ProviderPreset,
    pub(crate) id: Option<String>,
    pub(crate) base_url: Option<String>,
    pub(crate) api_key_env: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) route: Option<String>,
    pub(crate) force: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct ProviderTestSummary {
    pub(crate) ok: bool,
    pub(crate) revision: u64,
    pub(crate) provider_id: String,
    pub(crate) model: String,
    pub(crate) target: String,
    pub(crate) stream: bool,
    pub(crate) summary: String,
    pub(crate) output_chars: usize,
    pub(crate) event_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) response_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) finish_reason: Option<FinishReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) usage: Option<Usage>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ProviderModelSummary {
    pub(crate) id: String,
    pub(crate) switchback_model: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct ProviderModelsSummary {
    pub(crate) ok: bool,
    pub(crate) revision: u64,
    pub(crate) provider_id: String,
    pub(crate) models: Vec<ProviderModelSummary>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ProviderSyncRoutesSummary {
    pub(crate) ok: bool,
    pub(crate) provider_id: String,
    pub(crate) prefix: String,
    pub(crate) discovered: usize,
    pub(crate) added: usize,
    pub(crate) skipped: usize,
    pub(crate) replaced: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct ProviderDoctorCheck {
    pub(crate) name: String,
    pub(crate) ok: bool,
    pub(crate) required: bool,
    pub(crate) status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) detail: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ProviderDoctorSummary {
    pub(crate) ok: bool,
    pub(crate) revision: u64,
    pub(crate) provider_id: String,
    pub(crate) model: String,
    pub(crate) target: String,
    pub(crate) checks: Vec<ProviderDoctorCheck>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ProviderCertificationCounts {
    pub(crate) required_passed: usize,
    pub(crate) required_failed: usize,
    pub(crate) optional_passed: usize,
    pub(crate) optional_failed: usize,
    pub(crate) optional_unsupported: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct ProviderCertificationSummary {
    pub(crate) schema: &'static str,
    pub(crate) ok: bool,
    pub(crate) status: String,
    pub(crate) provider_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) target: Option<String>,
    pub(crate) summary: ProviderCertificationCounts,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) verified_capabilities: Vec<String>,
    pub(crate) checks: Vec<ProviderDoctorCheck>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) missing_env: Vec<String>,
    pub(crate) recommendations: Vec<String>,
    pub(crate) next_commands: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ProviderCertifyAllSummary {
    pub(crate) schema: &'static str,
    pub(crate) ok: bool,
    pub(crate) total: usize,
    pub(crate) certified: usize,
    pub(crate) blocked: usize,
    pub(crate) failed: usize,
    pub(crate) providers: Vec<ProviderCertificationSummary>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ProviderMatrixProviderSummary {
    pub(crate) provider_id: String,
    pub(crate) status: String,
    pub(crate) ok: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) missing_env: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) doctor: Option<ProviderDoctorSummary>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ProviderMatrixSummary {
    pub(crate) schema: &'static str,
    pub(crate) ok: bool,
    pub(crate) total: usize,
    pub(crate) checked: usize,
    pub(crate) skipped: usize,
    pub(crate) failed: usize,
    pub(crate) providers: Vec<ProviderMatrixProviderSummary>,
}

pub(crate) fn provider_mapping(
    preset: ProviderPreset,
    id: &str,
    base_url: Option<String>,
    api_key_env: Option<String>,
) -> serde_yaml::Value {
    let (_default_id, kind, _default_base_url, _default_api_key_env) = preset_defaults(preset);
    let mut provider = serde_yaml::Mapping::new();
    provider.insert(yaml_key("id"), yaml_string(id));
    provider.insert(yaml_key("type"), yaml_string(kind));
    if let Some(base_url) = base_url {
        provider.insert(yaml_key("base_url"), yaml_string(base_url));
    }
    if let Some(api_key_env) = api_key_env {
        provider.insert(yaml_key("api_key_env"), yaml_string(api_key_env));
    }
    serde_yaml::Value::Mapping(provider)
}

pub(crate) fn provider_add_config_file(
    path: &Path,
    request: ProviderAddRequest,
) -> anyhow::Result<ProviderAddSummary> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
    let mut value: serde_yaml::Value = serde_yaml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("parse {} as YAML: {e}", path.display()))?;
    let root = value
        .as_mapping_mut()
        .ok_or_else(|| anyhow::anyhow!("{} must contain a YAML mapping", path.display()))?;

    let (default_id, _kind, default_base_url, default_api_key_env) =
        preset_defaults(request.preset);
    let provider_id = clean_optional(request.id).unwrap_or_else(|| default_id.to_string());
    let base_url = clean_optional(request.base_url)
        .or_else(|| default_base_url.map(ToString::to_string))
        .or_else(|| {
            (request.preset == ProviderPreset::Ollama)
                .then(|| format!("{}://{}:{}/v1", "http", "localhost", 11434))
        })
        .or_else(|| {
            (request.preset == ProviderPreset::Vllm)
                .then(|| format!("{}://{}:{}/v1", "http", "localhost", 8000))
        });
    let api_key_env = match request.api_key_env {
        Some(value) => clean_optional(Some(value)),
        None => default_api_key_env.map(ToString::to_string),
    };
    let provider = provider_mapping(request.preset, &provider_id, base_url, api_key_env.clone());
    let providers = ensure_sequence(root, "providers")?;
    match providers.iter().position(|entry| {
        entry
            .as_mapping()
            .and_then(|mapping| mapping_str(mapping, "id"))
            == Some(provider_id.as_str())
    }) {
        Some(index) if request.force => providers[index] = provider,
        Some(_) => {
            anyhow::bail!(
                "provider `{provider_id}` already exists in {}; pass --force to replace it",
                path.display()
            );
        }
        None => providers.push(provider),
    }

    let model = clean_optional(request.model);
    let mut route_model = None;
    let mut target = None;
    if let Some(model) = model {
        let target_id = format!("{provider_id}/{model}");
        let inbound = clean_optional(request.route).unwrap_or_else(|| target_id.clone());
        let routes = ensure_sequence(root, "routes")?;
        let route_entry = exact_route_mapping(&inbound, &target_id);
        match routes.iter().position(|entry| {
            entry
                .as_mapping()
                .and_then(|mapping| mapping.get(yaml_key("match")))
                .and_then(serde_yaml::Value::as_mapping)
                .and_then(|mapping| mapping_str(mapping, "model"))
                == Some(inbound.as_str())
        }) {
            Some(index) if request.force => routes[index] = route_entry,
            Some(_) => {
                anyhow::bail!(
                    "route `{inbound}` already exists in {}; pass --force to replace it",
                    path.display()
                );
            }
            None => routes.push(route_entry),
        }
        route_model = Some(inbound);
        target = Some(target_id);
    }

    let rendered = serde_yaml::to_string(&value)?;
    let cfg = Config::from_yaml(&rendered)?;
    let problems = cfg.semantic_problems();
    if !problems.is_empty() {
        anyhow::bail!("config would be invalid: {}", problems.join("; "));
    }
    write_file_atomic(path, &rendered)?;
    Ok(ProviderAddSummary {
        provider_id,
        api_key_env,
        route_model,
        target,
    })
}

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
        ExecOutcome::Collected { response, summary } => Ok(ProviderTestSummary {
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
        }),
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
                    | AiStreamEvent::ToolCallEnd { .. } => {}
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

fn env_missing(name: &str) -> bool {
    std::env::var(name)
        .map(|value| value.trim().is_empty())
        .unwrap_or(true)
}

fn non_empty(value: Option<&String>) -> bool {
    value.is_some_and(|value| !value.trim().is_empty())
}

fn auth_missing_envs(auth: &AuthConfig) -> Vec<String> {
    match auth {
        AuthConfig::None => Vec::new(),
        AuthConfig::ApiKey { env, inline, vault } => {
            if non_empty(vault.as_ref()) || non_empty(inline.as_ref()) {
                Vec::new()
            } else {
                env.iter()
                    .filter(|name| env_missing(name))
                    .cloned()
                    .collect()
            }
        }
        AuthConfig::Oauth { .. } => Vec::new(),
        AuthConfig::ServiceAccount {
            key_file, key_env, ..
        } => {
            if non_empty(key_file.as_ref()) {
                Vec::new()
            } else {
                key_env
                    .iter()
                    .filter(|name| env_missing(name))
                    .cloned()
                    .collect()
            }
        }
    }
}

pub(crate) fn provider_missing_envs(provider: &ProviderConfig) -> Vec<String> {
    let mut missing = Vec::new();
    if provider.accounts.is_empty() {
        match &provider.kind {
            ProviderKind::OpenaiCompatible {
                api_key_env,
                api_key,
                ..
            }
            | ProviderKind::Anthropic {
                api_key_env,
                api_key,
                ..
            }
            | ProviderKind::Gemini {
                api_key_env,
                api_key,
                ..
            }
            | ProviderKind::Vertex {
                api_key_env,
                api_key,
                ..
            } => {
                if !non_empty(api_key.as_ref()) {
                    missing.extend(api_key_env.iter().filter(|name| env_missing(name)).cloned());
                }
            }
            ProviderKind::Bedrock {
                access_key_env,
                secret_key_env,
                ..
            } => {
                if env_missing(access_key_env) {
                    missing.push(access_key_env.clone());
                }
                if env_missing(secret_key_env) {
                    missing.push(secret_key_env.clone());
                }
            }
            ProviderKind::Mock => {}
        }
    } else {
        for account in &provider.accounts {
            missing.extend(auth_missing_envs(&account.auth));
        }
    }
    missing.sort();
    missing.dedup();
    missing
}

fn auth_env_names(auth: &AuthConfig) -> Vec<String> {
    match auth {
        AuthConfig::None => Vec::new(),
        AuthConfig::ApiKey { env, .. } => env.iter().cloned().collect(),
        AuthConfig::Oauth {
            token_env,
            refresh_env,
            client_secret_env,
            ..
        } => [token_env, refresh_env, client_secret_env]
            .into_iter()
            .filter_map(|value| value.clone())
            .collect(),
        AuthConfig::ServiceAccount { key_env, .. } => key_env.iter().cloned().collect(),
    }
}

pub(crate) fn provider_auth_env_names(provider: &ProviderConfig) -> Vec<String> {
    let mut names = Vec::new();
    if provider.accounts.is_empty() {
        match &provider.kind {
            ProviderKind::OpenaiCompatible { api_key_env, .. }
            | ProviderKind::Anthropic { api_key_env, .. }
            | ProviderKind::Gemini { api_key_env, .. }
            | ProviderKind::Vertex { api_key_env, .. } => {
                names.extend(api_key_env.iter().cloned());
            }
            ProviderKind::Bedrock {
                access_key_env,
                secret_key_env,
                ..
            } => {
                names.push(access_key_env.clone());
                names.push(secret_key_env.clone());
            }
            ProviderKind::Mock => {}
        }
    } else {
        for account in &provider.accounts {
            names.extend(auth_env_names(&account.auth));
        }
    }
    names.sort();
    names.dedup();
    names
}

fn provider_doctor_check(
    name: &str,
    ok: bool,
    required: bool,
    status: &str,
    detail: Option<String>,
) -> ProviderDoctorCheck {
    ProviderDoctorCheck {
        name: name.to_string(),
        ok,
        required,
        status: status.to_string(),
        detail,
    }
}

fn provider_doctor_ok(
    name: &str,
    required: bool,
    detail: impl Into<Option<String>>,
) -> ProviderDoctorCheck {
    provider_doctor_check(name, true, required, "ok", detail.into())
}

fn provider_doctor_failed(
    name: &str,
    required: bool,
    detail: impl Into<String>,
) -> ProviderDoctorCheck {
    provider_doctor_check(name, false, required, "failed", Some(detail.into()))
}

fn provider_doctor_unsupported(name: &str, detail: impl Into<String>) -> ProviderDoctorCheck {
    provider_doctor_check(name, false, false, "unsupported", Some(detail.into()))
}

async fn provider_doctor_embeddings_check(
    engine: &Engine,
    target_model: &str,
) -> ProviderDoctorCheck {
    let body = serde_json::json!({
        "model": target_model,
        "input": "Switchback provider doctor"
    });
    let (_revision, outcome) = engine
        .execute_embeddings(body, None, None, None, Instant::now())
        .await;
    match outcome {
        EmbeddingsOutcome::Json { value, summary, .. } => {
            let rows = value
                .get("data")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len)
                .unwrap_or_default();
            provider_doctor_ok(
                "embeddings",
                false,
                Some(format!("{summary}; embeddings={rows}")),
            )
        }
        EmbeddingsOutcome::Error { error, .. }
            if error.status == 422
                || error
                    .message
                    .to_ascii_lowercase()
                    .contains("embeddings not supported") =>
        {
            provider_doctor_unsupported("embeddings", error.message)
        }
        EmbeddingsOutcome::Error { error, .. } => provider_doctor_failed(
            "embeddings",
            false,
            format!("{}: {}", error.error_type, error.message),
        ),
    }
}

pub(crate) async fn provider_doctor_config_file(
    path: &Path,
    provider_id: &str,
    model: Option<&str>,
) -> anyhow::Result<ProviderDoctorSummary> {
    let cfg = Config::from_path(path)?;
    let cfg = provider_scoped_config(&cfg, provider_id)?;
    provider_doctor_config(cfg, provider_id, model).await
}

async fn provider_doctor_config(
    cfg: Config,
    provider_id: &str,
    model: Option<&str>,
) -> anyhow::Result<ProviderDoctorSummary> {
    let engine = engine_from_config(cfg)?;
    let revision = engine.revision();
    let mut checks = Vec::new();
    checks.push(provider_doctor_ok(
        "config",
        true,
        Some(format!("revision {revision}")),
    ));

    let explicit_model = model
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);
    let model_hint = provider_model_hint(engine.snapshot().config.as_ref(), provider_id);
    let models_required = explicit_model.is_none() && model_hint.is_none();
    let discovered =
        provider_models_config(engine.snapshot().config.as_ref().clone(), provider_id).await;
    if explicit_model.is_none() {
        if let Some(model) = model_hint.as_deref() {
            checks.push(provider_doctor_ok(
                "model_hint",
                true,
                Some(format!("using configured model hint `{model}`")),
            ));
        }
    }
    match &discovered {
        Ok(summary) => {
            checks.push(provider_doctor_ok(
                "models",
                models_required,
                Some(format!("{} model(s) discoverable", summary.models.len())),
            ));
        }
        Err(e) => {
            checks.push(provider_doctor_failed(
                "models",
                models_required,
                e.to_string(),
            ));
            if models_required {
                anyhow::bail!("model discovery failed for `{provider_id}`; pass --model: {e}");
            }
        }
    };
    let resolved_model = if let Some(model) = explicit_model {
        model
    } else if let Some(model) = model_hint {
        model
    } else {
        match &discovered {
            Ok(summary) => summary
                .models
                .first()
                .map(|model| model.id.clone())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "provider `{provider_id}` did not report any models; pass --model"
                    )
                })?,
            Err(e) => {
                anyhow::bail!("model discovery failed for `{provider_id}`; pass --model: {e}")
            }
        }
    };
    let target_model = format!("{provider_id}/{resolved_model}");

    let mut req = sb_core::AiRequest::new(
        target_model.clone(),
        vec![sb_core::Message::user("Switchback provider doctor")],
    );
    req.max_output_tokens = Some(32);
    req.temperature = Some(0.0);
    match engine.preview_route(&req) {
        Ok((_preview_revision, plan)) => {
            let selected = plan
                .decision
                .selected
                .as_ref()
                .map(|target| target.target_id.as_str());
            if selected == Some(target_model.as_str()) {
                checks.push(provider_doctor_ok(
                    "route_preview",
                    true,
                    Some(plan.decision.summary()),
                ));
            } else {
                checks.push(provider_doctor_failed(
                    "route_preview",
                    true,
                    format!(
                        "selected `{}`, expected `{target_model}`",
                        selected.unwrap_or("<none>")
                    ),
                ));
            }
        }
        Err(e) => checks.push(provider_doctor_failed("route_preview", true, e.message)),
    }

    match provider_test_config(
        engine.snapshot().config.as_ref().clone(),
        provider_id,
        Some(&resolved_model),
        false,
    )
    .await
    {
        Ok(summary) => checks.push(provider_doctor_ok(
            "chat_non_stream",
            true,
            Some(format!(
                "{}; output_chars={}",
                summary.summary, summary.output_chars
            )),
        )),
        Err(e) => checks.push(provider_doctor_failed(
            "chat_non_stream",
            true,
            e.to_string(),
        )),
    }

    match provider_test_config(
        engine.snapshot().config.as_ref().clone(),
        provider_id,
        Some(&resolved_model),
        true,
    )
    .await
    {
        Ok(summary) => checks.push(provider_doctor_ok(
            "chat_stream",
            true,
            Some(format!(
                "{}; events={}; output_chars={}",
                summary.summary, summary.event_count, summary.output_chars
            )),
        )),
        Err(e) => checks.push(provider_doctor_failed("chat_stream", true, e.to_string())),
    }

    checks.push(provider_doctor_embeddings_check(&engine, &target_model).await);
    let ok = checks
        .iter()
        .filter(|check| check.required)
        .all(|check| check.ok);

    Ok(ProviderDoctorSummary {
        ok,
        revision,
        provider_id: provider_id.to_string(),
        model: resolved_model,
        target: target_model,
        checks,
    })
}

fn provider_certification_counts(checks: &[ProviderDoctorCheck]) -> ProviderCertificationCounts {
    ProviderCertificationCounts {
        required_passed: checks
            .iter()
            .filter(|check| check.required && check.ok)
            .count(),
        required_failed: checks
            .iter()
            .filter(|check| check.required && !check.ok)
            .count(),
        optional_passed: checks
            .iter()
            .filter(|check| !check.required && check.ok)
            .count(),
        optional_failed: checks
            .iter()
            .filter(|check| !check.required && !check.ok && check.status != "unsupported")
            .count(),
        optional_unsupported: checks
            .iter()
            .filter(|check| !check.required && check.status == "unsupported")
            .count(),
    }
}

fn provider_verified_capabilities(checks: &[ProviderDoctorCheck]) -> Vec<String> {
    let check_ok = |name: &str| checks.iter().any(|check| check.name == name && check.ok);
    let mut capabilities = Vec::new();
    if check_ok("models") {
        capabilities.push("model_discovery".to_string());
    }
    if check_ok("route_preview") {
        capabilities.push("route_preview".to_string());
    }
    if check_ok("chat_non_stream") {
        capabilities.push("chat_non_stream".to_string());
    }
    if check_ok("chat_stream") {
        capabilities.push("chat_stream".to_string());
    }
    if check_ok("embeddings") {
        capabilities.push("embeddings".to_string());
    }
    capabilities
}

fn provider_certification_next_commands(provider_id: &str, model: Option<&str>) -> Vec<String> {
    let routed_model = model
        .map(|model| format!("{provider_id}/{model}"))
        .unwrap_or_else(|| format!("{provider_id}/<model>"));
    vec![
        format!("switchback provider doctor {provider_id} --config switchback.yaml"),
        format!("switchback route-preview --config switchback.yaml --model {routed_model}"),
        "switchback serve --config switchback.yaml".to_string(),
    ]
}

fn provider_certification_from_doctor(
    doctor: ProviderDoctorSummary,
) -> ProviderCertificationSummary {
    let counts = provider_certification_counts(&doctor.checks);
    let status = if counts.required_failed == 0 {
        "certified"
    } else {
        "failed"
    };
    let mut recommendations = Vec::new();
    if status == "certified" {
        recommendations.push("Provider is ready for chat and streaming traffic.".to_string());
    } else {
        recommendations
            .push("Fix failed required checks, then rerun provider certification.".to_string());
    }
    if counts.optional_unsupported > 0 {
        recommendations.push(
            "Optional embeddings are unsupported; chat/stream certification is still valid."
                .to_string(),
        );
    }
    if counts.optional_failed > 0 {
        recommendations
            .push("Review failed optional checks before enabling those features.".to_string());
    }

    ProviderCertificationSummary {
        schema: "switchback/provider-certification@1",
        ok: status == "certified",
        status: status.to_string(),
        provider_id: doctor.provider_id.clone(),
        revision: Some(doctor.revision),
        model: Some(doctor.model.clone()),
        target: Some(doctor.target.clone()),
        summary: counts,
        verified_capabilities: provider_verified_capabilities(&doctor.checks),
        checks: doctor.checks,
        missing_env: Vec::new(),
        recommendations,
        next_commands: provider_certification_next_commands(
            &doctor.provider_id,
            Some(&doctor.model),
        ),
    }
}

pub(crate) async fn provider_certify_config_file(
    path: &Path,
    provider_id: &str,
    model: Option<&str>,
) -> anyhow::Result<ProviderCertificationSummary> {
    let cfg = Config::from_path(path)?;
    provider_certify_config(&cfg, provider_id, model).await
}

async fn provider_certify_config(
    cfg: &Config,
    provider_id: &str,
    model: Option<&str>,
) -> anyhow::Result<ProviderCertificationSummary> {
    let provider = cfg
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .ok_or_else(|| anyhow::anyhow!("provider `{provider_id}` is not configured"))?;
    let missing_env = provider_missing_envs(provider);
    if !missing_env.is_empty() {
        return Ok(ProviderCertificationSummary {
            schema: "switchback/provider-certification@1",
            ok: false,
            status: "blocked".to_string(),
            provider_id: provider_id.to_string(),
            revision: None,
            model: model
                .map(ToString::to_string)
                .or_else(|| provider.model_hint.clone()),
            target: None,
            summary: ProviderCertificationCounts {
                required_passed: 0,
                required_failed: 1,
                optional_passed: 0,
                optional_failed: 0,
                optional_unsupported: 0,
            },
            verified_capabilities: Vec::new(),
            checks: vec![provider_doctor_failed(
                "credentials",
                true,
                format!("missing env: {}", missing_env.join(", ")),
            )],
            missing_env: missing_env.clone(),
            recommendations: vec![format!(
                "Set missing environment variables, then rerun: {}",
                missing_env.join(", ")
            )],
            next_commands: provider_certification_next_commands(provider_id, model),
        });
    }

    match provider_doctor_config(
        provider_scoped_config(cfg, provider_id)?,
        provider_id,
        model,
    )
    .await
    {
        Ok(doctor) => Ok(provider_certification_from_doctor(doctor)),
        Err(e) => Ok(ProviderCertificationSummary {
            schema: "switchback/provider-certification@1",
            ok: false,
            status: "blocked".to_string(),
            provider_id: provider_id.to_string(),
            revision: None,
            model: model
                .map(ToString::to_string)
                .or_else(|| provider.model_hint.clone()),
            target: None,
            summary: ProviderCertificationCounts {
                required_passed: 0,
                required_failed: 1,
                optional_passed: 0,
                optional_failed: 0,
                optional_unsupported: 0,
            },
            verified_capabilities: Vec::new(),
            checks: vec![provider_doctor_failed("certification", true, e.to_string())],
            missing_env: Vec::new(),
            recommendations: vec![
                "Resolve the certification blocker and rerun provider certify.".to_string(),
            ],
            next_commands: provider_certification_next_commands(provider_id, model),
        }),
    }
}

pub(crate) async fn provider_certify_all_config_file(
    path: &Path,
) -> anyhow::Result<ProviderCertifyAllSummary> {
    let cfg = Config::from_path(path)?;
    let mut providers = Vec::new();
    for provider in &cfg.providers {
        providers.push(provider_certify_config(&cfg, &provider.id, None).await?);
    }

    let certified = providers
        .iter()
        .filter(|provider| provider.status == "certified")
        .count();
    let blocked = providers
        .iter()
        .filter(|provider| provider.status == "blocked")
        .count();
    let failed = providers
        .iter()
        .filter(|provider| provider.status == "failed")
        .count();

    Ok(ProviderCertifyAllSummary {
        schema: "switchback/provider-certifications@1",
        ok: blocked == 0 && failed == 0,
        total: providers.len(),
        certified,
        blocked,
        failed,
        providers,
    })
}

pub(crate) async fn provider_matrix_config_file(
    path: &Path,
) -> anyhow::Result<ProviderMatrixSummary> {
    let cfg = Config::from_path(path)?;
    let mut providers = Vec::new();
    let mut checked = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;

    for provider in &cfg.providers {
        let missing_env = provider_missing_envs(provider);
        if !missing_env.is_empty() {
            skipped += 1;
            providers.push(ProviderMatrixProviderSummary {
                provider_id: provider.id.clone(),
                status: "skipped".to_string(),
                ok: false,
                missing_env,
                reason: Some("required credential environment variable is not set".to_string()),
                doctor: None,
            });
            continue;
        }

        checked += 1;
        let scoped = provider_scoped_config(&cfg, &provider.id)?;
        match provider_doctor_config(scoped, &provider.id, None).await {
            Ok(doctor) if doctor.ok => providers.push(ProviderMatrixProviderSummary {
                provider_id: provider.id.clone(),
                status: "ok".to_string(),
                ok: true,
                missing_env: Vec::new(),
                reason: None,
                doctor: Some(doctor),
            }),
            Ok(doctor) => {
                failed += 1;
                providers.push(ProviderMatrixProviderSummary {
                    provider_id: provider.id.clone(),
                    status: "failed".to_string(),
                    ok: false,
                    missing_env: Vec::new(),
                    reason: Some("one or more required provider checks failed".to_string()),
                    doctor: Some(doctor),
                });
            }
            Err(e) => {
                failed += 1;
                providers.push(ProviderMatrixProviderSummary {
                    provider_id: provider.id.clone(),
                    status: "failed".to_string(),
                    ok: false,
                    missing_env: Vec::new(),
                    reason: Some(e.to_string()),
                    doctor: None,
                });
            }
        }
    }

    Ok(ProviderMatrixSummary {
        schema: "switchback/provider-matrix@1",
        ok: failed == 0,
        total: cfg.providers.len(),
        checked,
        skipped,
        failed,
        providers,
    })
}
