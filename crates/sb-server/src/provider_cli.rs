use std::path::Path;

use clap::Subcommand;
use sb_core::{Config, FinishReason, Usage};
use serde::Serialize;

use crate::provider_preset::{preset_defaults, ProviderPreset};
use crate::{
    clean_optional, ensure_sequence, exact_route_mapping, mapping_str, provider_models_config_file,
    write_file_atomic, yaml_key, yaml_string,
};

#[derive(Subcommand)]
pub(crate) enum ProviderCmd {
    /// List provider presets and their default onboarding settings.
    Presets,
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
