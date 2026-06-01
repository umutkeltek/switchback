use std::path::{Path, PathBuf};

use clap::Subcommand;
use sb_core::Config;
use sb_runtime::Engine;

use crate::controlplane;

pub(crate) const STARTER_CONFIG: &str = include_str!("../../../config/quickstart.yaml");

#[derive(Subcommand)]
pub(crate) enum ConfigCmd {
    /// Print the full effective config as redacted JSON.
    Show,
    /// Print one value by dotted path (e.g. `server.cost_aware`, `providers.0.id`).
    Get { pointer: String },
    /// Set one YAML value by dotted path. The value must be valid JSON.
    Set { pointer: String, value: String },
    /// Remove one YAML value by dotted path.
    Unset { pointer: String },
    /// Deep-merge a YAML/JSON patch file into the config.
    Patch {
        #[arg(long)]
        from_file: PathBuf,
    },
    /// Rewrite the config in Switchback's canonical YAML format.
    Format,
    /// Load + validate the config; exit non-zero on problems.
    Validate,
    /// List providers (id, type, egress, account ids).
    Providers,
    /// List routes and combo profiles (name + targets).
    Routes,
}

pub(crate) fn init_config_file(path: &Path, force: bool) -> anyhow::Result<()> {
    let cfg = Config::from_yaml(STARTER_CONFIG)?;
    if let Err(e) = Engine::validate_config(&cfg) {
        anyhow::bail!("bundled starter config is invalid: {e}");
    }
    if path.exists() && !force {
        anyhow::bail!(
            "{} already exists; pass --force to replace it",
            path.display()
        );
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    write_file_atomic(path, STARTER_CONFIG)?;
    Ok(())
}

pub(crate) fn write_file_atomic(path: &Path, contents: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("switchback.yaml");
    let tmp_name = format!(".{file_name}.{}.tmp", std::process::id());
    let tmp_path = parent
        .map(|parent| parent.join(&tmp_name))
        .unwrap_or_else(|| PathBuf::from(&tmp_name));
    std::fs::write(&tmp_path, contents)
        .map_err(|e| anyhow::anyhow!("write {}: {e}", tmp_path.display()))?;
    if let Err(e) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        anyhow::bail!("replace {}: {e}", path.display());
    }
    Ok(())
}

pub(crate) fn config_schema_json() -> serde_json::Value {
    serde_json::json!({
        "schema": "switchback/config-paths@1",
        "path_format": "dotted path; use N as a placeholder for array indexes",
        "value_format": "config set values are JSON literals",
        "paths": [
            {"path": "server.bind", "type": "string", "example_json": "\"127.0.0.1:8765\""},
            {"path": "server.api_key", "type": "string|null", "secret": true},
            {"path": "server.cost_aware", "type": "boolean"},
            {"path": "server.latency_aware", "type": "boolean"},
            {"path": "server.default_provider", "type": "string|null"},
            {"path": "server.max_concurrency", "type": "integer|null"},
            {"path": "server.admission_timeout_ms", "type": "integer"},
            {"path": "server.strict_schema_downlevel", "type": "boolean"},
            {"path": "server.egress_enabled", "type": "boolean"},
            {"path": "providers.N.id", "type": "string"},
            {"path": "providers.N.type", "type": "provider kind"},
            {"path": "providers.N.base_url", "type": "string"},
            {"path": "providers.N.api_key_env", "type": "string|null"},
            {"path": "providers.N.model_hint", "type": "string|null"},
            {"path": "providers.N.accounts.N.id", "type": "string"},
            {"path": "routes.N.name", "type": "string"},
            {"path": "routes.N.match.model", "type": "string"},
            {"path": "routes.N.targets", "type": "array<string>"},
            {"path": "combos.NAME.models", "type": "array<string>"},
            {"path": "combos.NAME.strategy", "type": "fallback|round_robin"},
            {"path": "tenants.N.id", "type": "string"},
            {"path": "tenants.N.allowed_routes", "type": "array<string>"},
            {"path": "tenants.N.allowed_providers", "type": "array<string>"},
            {"path": "tenants.N.allowed_accounts", "type": "array<string>"},
            {"path": "tenants.N.budget_usd", "type": "number|null"},
            {"path": "egress.N.id", "type": "string"},
            {"path": "plugins.N.type", "type": "plugin kind"}
        ],
        "examples": [
            "switchback config set server.cost_aware true --config switchback.yaml",
            "switchback config set providers.0.model_hint '\"gpt-4.1-mini\"' --config switchback.yaml",
            "switchback config patch --from-file patch.yaml --config switchback.yaml"
        ]
    })
}

pub(crate) fn yaml_key(key: &str) -> serde_yaml::Value {
    serde_yaml::Value::String(key.to_string())
}

pub(crate) fn yaml_string(value: impl Into<String>) -> serde_yaml::Value {
    serde_yaml::Value::String(value.into())
}

pub(crate) fn mapping_str<'a>(mapping: &'a serde_yaml::Mapping, key: &str) -> Option<&'a str> {
    mapping
        .get(yaml_key(key))
        .and_then(serde_yaml::Value::as_str)
}

pub(crate) fn clean_optional(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

pub(crate) fn exact_route_mapping(route_model: &str, target: &str) -> serde_yaml::Value {
    let mut match_mapping = serde_yaml::Mapping::new();
    match_mapping.insert(yaml_key("model"), yaml_string(route_model));

    let mut route = serde_yaml::Mapping::new();
    route.insert(yaml_key("name"), yaml_string(route_model));
    route.insert(yaml_key("match"), serde_yaml::Value::Mapping(match_mapping));
    route.insert(
        yaml_key("targets"),
        serde_yaml::Value::Sequence(vec![yaml_string(target)]),
    );
    serde_yaml::Value::Mapping(route)
}

pub(crate) fn ensure_sequence<'a>(
    root: &'a mut serde_yaml::Mapping,
    key: &str,
) -> anyhow::Result<&'a mut Vec<serde_yaml::Value>> {
    let yaml_key = yaml_key(key);
    if !root.contains_key(&yaml_key) {
        root.insert(yaml_key.clone(), serde_yaml::Value::Sequence(Vec::new()));
    }
    root.get_mut(&yaml_key)
        .and_then(serde_yaml::Value::as_sequence_mut)
        .ok_or_else(|| anyhow::anyhow!("top-level `{key}` must be a YAML sequence"))
}

fn config_path_segments(pointer: &str) -> anyhow::Result<Vec<&str>> {
    let segments = pointer
        .split('.')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.is_empty() {
        anyhow::bail!("config path must not be empty");
    }
    Ok(segments)
}

fn yaml_set_path(
    value: &mut serde_yaml::Value,
    segments: &[&str],
    replacement: serde_yaml::Value,
) -> anyhow::Result<()> {
    let Some((segment, rest)) = segments.split_first() else {
        anyhow::bail!("config path must not be empty");
    };
    if rest.is_empty() {
        match value {
            serde_yaml::Value::Mapping(mapping) => {
                mapping.insert(yaml_key(segment), replacement);
                Ok(())
            }
            serde_yaml::Value::Sequence(items) => {
                let index = segment.parse::<usize>().map_err(|_| {
                    anyhow::anyhow!("path segment `{segment}` must be an array index")
                })?;
                let slot = items
                    .get_mut(index)
                    .ok_or_else(|| anyhow::anyhow!("array index `{segment}` is out of range"))?;
                *slot = replacement;
                Ok(())
            }
            _ => anyhow::bail!("path segment `{segment}` does not point into a map or array"),
        }
    } else {
        match value {
            serde_yaml::Value::Mapping(mapping) => {
                let key = yaml_key(segment);
                if !mapping.contains_key(&key) {
                    mapping.insert(key.clone(), serde_yaml::Value::Mapping(Default::default()));
                }
                let child = mapping.get_mut(&key).expect("inserted key is present");
                yaml_set_path(child, rest, replacement)
            }
            serde_yaml::Value::Sequence(items) => {
                let index = segment.parse::<usize>().map_err(|_| {
                    anyhow::anyhow!("path segment `{segment}` must be an array index")
                })?;
                let child = items
                    .get_mut(index)
                    .ok_or_else(|| anyhow::anyhow!("array index `{segment}` is out of range"))?;
                yaml_set_path(child, rest, replacement)
            }
            _ => anyhow::bail!("path segment `{segment}` does not point into a map or array"),
        }
    }
}

fn yaml_unset_path(value: &mut serde_yaml::Value, segments: &[&str]) -> anyhow::Result<bool> {
    let Some((segment, rest)) = segments.split_first() else {
        anyhow::bail!("config path must not be empty");
    };
    if rest.is_empty() {
        match value {
            serde_yaml::Value::Mapping(mapping) => Ok(mapping.remove(yaml_key(segment)).is_some()),
            serde_yaml::Value::Sequence(items) => {
                let index = segment.parse::<usize>().map_err(|_| {
                    anyhow::anyhow!("path segment `{segment}` must be an array index")
                })?;
                if index < items.len() {
                    items.remove(index);
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            _ => Ok(false),
        }
    } else {
        match value {
            serde_yaml::Value::Mapping(mapping) => match mapping.get_mut(yaml_key(segment)) {
                Some(child) => yaml_unset_path(child, rest),
                None => Ok(false),
            },
            serde_yaml::Value::Sequence(items) => {
                let index = segment.parse::<usize>().map_err(|_| {
                    anyhow::anyhow!("path segment `{segment}` must be an array index")
                })?;
                match items.get_mut(index) {
                    Some(child) => yaml_unset_path(child, rest),
                    None => Ok(false),
                }
            }
            _ => Ok(false),
        }
    }
}

fn merge_yaml_value(target: &mut serde_yaml::Value, patch: serde_yaml::Value) {
    match (target, patch) {
        (serde_yaml::Value::Mapping(target), serde_yaml::Value::Mapping(patch)) => {
            for (key, value) in patch {
                match target.get_mut(&key) {
                    Some(existing) => merge_yaml_value(existing, value),
                    None => {
                        target.insert(key, value);
                    }
                }
            }
        }
        (target, patch) => *target = patch,
    }
}

fn read_yaml_value(path: &Path) -> anyhow::Result<serde_yaml::Value> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
    serde_yaml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("parse {} as YAML: {e}", path.display()))
}

fn render_and_validate_config_value(value: &serde_yaml::Value) -> anyhow::Result<(String, Config)> {
    let rendered = serde_yaml::to_string(value)?;
    let cfg = Config::from_yaml(&rendered)
        .map_err(|e| anyhow::anyhow!("config would be invalid: {e}"))?;
    Engine::validate_config(&cfg).map_err(|e| anyhow::anyhow!("config would be invalid: {e}"))?;
    Ok((rendered, cfg))
}

fn validate_and_write_config_value(path: &Path, value: &serde_yaml::Value) -> anyhow::Result<()> {
    let (rendered, _cfg) = render_and_validate_config_value(value)?;
    write_file_atomic(path, &rendered)
}

pub(crate) fn config_set_file(
    path: &Path,
    pointer: &str,
    json_value: &str,
) -> anyhow::Result<serde_json::Value> {
    let parsed: serde_json::Value = serde_json::from_str(json_value)
        .map_err(|e| anyhow::anyhow!("value must be valid JSON: {e}"))?;
    let yaml_value = serde_yaml::to_value(&parsed)?;
    let mut config = read_yaml_value(path)?;
    let segments = config_path_segments(pointer)?;
    yaml_set_path(&mut config, &segments, yaml_value)?;
    let (rendered, cfg) = render_and_validate_config_value(&config)?;
    if controlplane::pointer_get(&controlplane::redact_config(&cfg), pointer).is_none() {
        anyhow::bail!("path `{pointer}` is not recognized by the effective config");
    }
    write_file_atomic(path, &rendered)?;
    Ok(parsed)
}

pub(crate) fn config_unset_file(path: &Path, pointer: &str) -> anyhow::Result<bool> {
    let mut config = read_yaml_value(path)?;
    let segments = config_path_segments(pointer)?;
    let removed = yaml_unset_path(&mut config, &segments)?;
    validate_and_write_config_value(path, &config)?;
    Ok(removed)
}

pub(crate) fn config_patch_file(path: &Path, from_file: &Path) -> anyhow::Result<()> {
    let mut config = read_yaml_value(path)?;
    let patch = read_yaml_value(from_file)?;
    merge_yaml_value(&mut config, patch);
    validate_and_write_config_value(path, &config)
}

pub(crate) fn config_format_file(path: &Path) -> anyhow::Result<()> {
    let config = read_yaml_value(path)?;
    validate_and_write_config_value(path, &config)
}

pub(crate) fn config_validate_json(path: &Path) -> anyhow::Result<serde_json::Value> {
    let cfg = Config::from_path(path)?;
    if let Err(e) = Engine::validate_config(&cfg) {
        let problems: Vec<&str> = e.split("; ").collect();
        Ok(serde_json::json!({"ok": false, "problems": problems}))
    } else {
        Ok(serde_json::json!({"ok": true}))
    }
}
