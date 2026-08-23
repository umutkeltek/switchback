use std::path::{Path, PathBuf};

use clap::Subcommand;
use sb_core::Config;
use sb_runtime::Engine;

use crate::controlplane;

pub(crate) const STARTER_CONFIG: &str = include_str!("../../../config/quickstart.yaml");
pub(crate) const NATIVE_CLIENTS_CONFIG: &str = include_str!("../../../config/native-clients.yaml");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InitTemplate {
    Quickstart,
    NativeClients,
}

impl InitTemplate {
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Quickstart => "quickstart",
            Self::NativeClients => "native_clients",
        }
    }

    pub(crate) fn contents(self) -> &'static str {
        match self {
            Self::Quickstart => STARTER_CONFIG,
            Self::NativeClients => NATIVE_CLIENTS_CONFIG,
        }
    }

    pub(crate) fn next_commands(self, config: &Path) -> Vec<String> {
        let serve = format!("switchback serve --config {}", config.display());
        match self {
            Self::Quickstart => vec![serve],
            Self::NativeClients => vec![
                serve,
                "open \"${SWITCHBACK_BASE_URL%/}/\"".to_string(),
                "OPENAI_BASE_URL=\"${SWITCHBACK_BASE_URL%/}/v1\" OPENAI_API_KEY=$SWITCHBACK_API_KEY codex exec --model coding \"ping through Switchback\"".to_string(),
                "ANTHROPIC_BASE_URL=\"${SWITCHBACK_BASE_URL%/}\" ANTHROPIC_AUTH_TOKEN=$SWITCHBACK_API_KEY claude -p \"ping through Switchback\"".to_string(),
            ],
        }
    }
}

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

pub(crate) fn init_config_file(
    path: &Path,
    force: bool,
    template: InitTemplate,
) -> anyhow::Result<()> {
    let contents = template.contents();
    let cfg = Config::from_yaml(contents)?;
    if let Err(e) = Engine::validate_config(&cfg) {
        anyhow::bail!("bundled {} starter config is invalid: {e}", template.id());
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
    write_file_atomic(path, contents)?;
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
            {"path": "server.admission_slot_ttl_ms", "type": "integer"},
            {"path": "server.tenant_concurrency_ttl_ms", "type": "integer"},
            {"path": "server.max_request_bytes", "type": "integer|null"},
            {"path": "server.max_response_bytes", "type": "integer|null"},
            {"path": "server.idempotency.inflight_ttl_ms", "type": "integer"},
            {"path": "server.idempotency.persist_response_bodies", "type": "boolean"},
            {"path": "server.strict_schema_downlevel", "type": "boolean"},
            {"path": "server.egress_enabled", "type": "boolean"},
            {"path": "providers.N.id", "type": "string"},
            {"path": "providers.N.type", "type": "mock|openai_compatible|anthropic|gemini|vertex|bedrock|comfyui|codex_native_relay|claude_code_native_relay"},
            {"path": "providers.N.base_url", "type": "string"},
            {"path": "providers.N.api_key_env", "type": "string|null"},
            {"path": "providers.N.model_hint", "type": "string|null"},
            {"path": "providers.N.workflows.N.id", "type": "string"},
            {"path": "providers.N.workflows.N.kind", "type": "image_generation|video_generation|workflow_execution"},
            {"path": "providers.N.workflows.N.graph", "type": "object"},
            {"path": "providers.N.workflows.N.bindings.NAME.path", "type": "array<string>"},
            {"path": "providers.N.workflows.N.output_node_ids", "type": "array<string>"},
            {"path": "providers.N.accounts.N.id", "type": "string"},
            {"path": "providers.N.accounts.N.auth.kind", "type": "none|api_key|json_token|oauth|codex_oauth|claude_code_oauth|service_account|aws_sig_v4"},
            {"path": "providers.N.accounts.N.auth.token_env", "type": "string|null"},
            {"path": "providers.N.accounts.N.auth.token_file", "type": "string|null"},
            {"path": "providers.N.accounts.N.auth.access_token_pointer", "type": "json-pointer"},
            {"path": "client_profiles.N.id", "type": "string"},
            {"path": "client_profiles.N.kind", "type": "codex|claude_code"},
            {"path": "client_profiles.N.mode", "type": "switchback_ingress|native_relay|tap|scout_api"},
            {"path": "client_profiles.N.models", "type": "array<string>"},
            {"path": "client_profiles.N.accounts", "type": "array<string provider/account>"},
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

fn is_existing_route_targets_path(config: &Config, segments: &[&str]) -> bool {
    let ["routes", index, "targets"] = segments else {
        return false;
    };
    index
        .parse::<usize>()
        .is_ok_and(|index| index < config.routes.len())
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
    // Deserialize the already-parsed YAML tree directly. Rendering it and then
    // parsing the rendered text made a dotted update pay for a second full YAML
    // tokenize/parse pass, which dominates large generated route registries.
    let cfg: Config = serde_yaml::from_value(value.clone())
        .map_err(|e| anyhow::anyhow!("config would be invalid: {e}"))?;
    Engine::validate_config(&cfg).map_err(|e| anyhow::anyhow!("config would be invalid: {e}"))?;
    let rendered = serde_yaml::to_string(value)?;
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
    if !is_existing_route_targets_path(&cfg, &segments)
        && controlplane::pointer_get(&controlplane::redact_config(&cfg), pointer).is_none()
    {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn test_config_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "switchback-{label}-{}-{}.yaml",
            std::process::id(),
            sb_core::new_id("config")
        ))
    }

    fn large_route_config(route_count: usize) -> String {
        let mut config = String::from(
            "server:\n  bind: \"127.0.0.1:0\"\nproviders:\n  - id: mac\n    type: mock\nroutes:\n",
        );
        for index in 0..route_count {
            config.push_str(&format!(
                "  - name: route-{index}\n    match:\n      model: route-{index}\n    targets:\n      - mac/model-{index}\n"
            ));
        }
        config.push_str("client_profiles:\n");
        for index in (0..route_count).rev() {
            config.push_str(&format!(
                "  - id: profile-{index}\n    kind: codex\n    models:\n      - route-{index}\n"
            ));
        }
        config
    }

    #[test]
    fn targeted_set_is_bounded_for_four_thousand_routes() {
        let path = test_config_path("large-targeted-set");
        std::fs::write(&path, large_route_config(4_000)).unwrap();

        // Warm the YAML parser, HTTP client builder, and allocator before
        // enforcing the operator-facing bound.
        config_set_file(&path, "routes.2000.targets", r#"["mac/warm"]"#).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let started = Instant::now();
        config_set_file(&path, "routes.2000.targets", r#"["mac/new"]"#).unwrap();
        let elapsed = started.elapsed();
        let after = std::fs::read_to_string(&path).unwrap();

        assert!(
            elapsed < Duration::from_secs(2),
            "warm targeted update took {elapsed:?}"
        );
        assert_eq!(
            after,
            before.replacen("mac/warm", "mac/new", 1),
            "only the selected target bytes may change"
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn targeted_set_refuses_invalid_delta_without_writing() {
        let path = test_config_path("invalid-targeted-set");
        std::fs::write(&path, large_route_config(4)).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let unknown = config_set_file(&path, "routes.2.targets", r#"["missing/model"]"#)
            .expect_err("unknown provider must fail");
        assert!(unknown.to_string().contains("unknown provider"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        let malformed_target = config_set_file(&path, "routes.2.targets", r#"["not-a-target"]"#)
            .expect_err("target without provider/model must fail");
        assert!(malformed_target
            .to_string()
            .contains("must be `provider/model`"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        let malformed = config_set_file(&path, "routes.2.targets", "[7]")
            .expect_err("non-string target must fail schema validation");
        assert!(malformed.to_string().contains("config would be invalid"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn targeted_set_refuses_preexisting_route_order_conflicts() {
        let path = test_config_path("conflicting-targeted-set");
        let config = large_route_config(3).replacen("model: route-1", "model: route-0", 1);
        std::fs::write(&path, config).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let conflict = config_set_file(&path, "routes.2.targets", r#"["mac/new"]"#)
            .expect_err("ambiguous route ordering must fail");
        assert!(conflict.to_string().contains("route order"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let duplicate_name = large_route_config(3).replacen("name: route-1", "name: route-0", 1);
        std::fs::write(&path, duplicate_name).unwrap();
        let duplicate_before = std::fs::read_to_string(&path).unwrap();
        let duplicate = config_set_file(&path, "routes.2.targets", r#"["mac/new"]"#)
            .expect_err("duplicate route names must fail");
        assert!(duplicate.to_string().contains("duplicates"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), duplicate_before);

        let duplicate_wildcard = large_route_config(3)
            .replacen("model: route-0", "model: \"*\"", 1)
            .replacen("model: route-1", "model: \"*\"", 1);
        std::fs::write(&path, duplicate_wildcard).unwrap();
        let wildcard_before = std::fs::read_to_string(&path).unwrap();
        let wildcard_conflict = config_set_file(&path, "routes.2.targets", r#"["mac/new"]"#)
            .expect_err("multiple wildcard routes must fail");
        assert!(wildcard_conflict.to_string().contains("route order"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), wildcard_before);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn native_client_next_commands_use_switchback_base_url() {
        let commands = InitTemplate::NativeClients.next_commands(Path::new("switchback.yaml"));

        assert_eq!(commands[1], "open \"${SWITCHBACK_BASE_URL%/}/\"");
        assert_eq!(
            commands[2],
            "OPENAI_BASE_URL=\"${SWITCHBACK_BASE_URL%/}/v1\" OPENAI_API_KEY=$SWITCHBACK_API_KEY codex exec --model coding \"ping through Switchback\""
        );
        assert_eq!(
            commands[3],
            "ANTHROPIC_BASE_URL=\"${SWITCHBACK_BASE_URL%/}\" ANTHROPIC_AUTH_TOKEN=$SWITCHBACK_API_KEY claude -p \"ping through Switchback\""
        );
    }

    #[test]
    fn config_schema_advertises_json_token_without_secret_fields() {
        let schema = config_schema_json().to_string();
        assert!(schema.contains("json_token"));
        assert!(schema.contains("token_file"));
        assert!(schema.contains("access_token_pointer"));
        assert!(!schema.contains("access_token_value"));
        assert!(!schema.contains("token_body"));
    }
}
