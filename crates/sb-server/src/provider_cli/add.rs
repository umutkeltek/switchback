use std::path::Path;

use sb_core::Config;

use crate::config_cli::{
    clean_optional, ensure_sequence, exact_route_mapping, mapping_str, write_file_atomic, yaml_key,
    yaml_string,
};
use crate::provider_preset::{preset_defaults, ProviderPreset};

use super::{ProviderAddRequest, ProviderAddSummary};

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
