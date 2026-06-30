use crate::config_cli;
use crate::config_cli::{init_config_file, InitTemplate};
use crate::provider_cli::{
    provider_add_config_file, provider_doctor_config_file, provider_mapping,
    provider_matrix_config_file, provider_missing_envs, provider_models_config_file,
    provider_sync_routes_config_file, provider_test_config_file, ProviderAddRequest,
};
use crate::provider_preset::{
    preset_defaults, preset_is_workload_executor, preset_model_hint, preset_name,
    provider_readiness_manifest_json, ProviderPreset, PROVIDER_PRESETS,
};
use crate::serve::{open_state_store, validate_open_admin_bind};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::ValueEnum;
use sb_core::Config;
use sb_runtime::Engine;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_name(prefix: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn config_with_state_store(state_store: &str) -> Config {
    Config::from_yaml(&format!(
        r#"
server:
  bind: "127.0.0.1:0"
{state_store}
providers:
  - id: mock
    type: mock
routes:
  - name: default
    match:
      model: "*"
    targets:
      - "mock/echo"
"#
    ))
    .unwrap()
}

#[test]
fn starter_config_is_valid() {
    let cfg = Config::from_yaml(config_cli::STARTER_CONFIG).unwrap();
    Engine::validate_config(&cfg).unwrap();
    assert_eq!(cfg.providers[0].id, "mock");
}

#[test]
fn state_store_open_failure_degrades_when_optional() {
    let missing_parent = temp_name("switchback-optional-state-store").join("missing");
    let db_path = missing_parent.join("state.sqlite");
    let cfg = config_with_state_store(&format!("  state_store: \"{}\"", db_path.display()));

    let store = open_state_store(&cfg).unwrap();

    assert!(store.is_none());
    assert!(!missing_parent.exists());
}

#[test]
fn state_store_open_failure_fails_when_required() {
    let missing_parent = temp_name("switchback-required-state-store").join("missing");
    let db_path = missing_parent.join("state.sqlite");
    let cfg = config_with_state_store(&format!(
        "  state_store:\n    path: \"{}\"\n    required: true",
        db_path.display()
    ));

    let error = match open_state_store(&cfg) {
        Ok(_) => panic!("required state store should fail when its path cannot be opened"),
        Err(error) => error.to_string(),
    };

    assert!(error.contains("state store"));
    assert!(error.contains("required"));
    assert!(error.contains("could not be opened"));
    assert!(!missing_parent.exists());
}

#[test]
fn open_non_loopback_bind_requires_auth_or_explicit_allow() {
    let cfg = Config::from_yaml(
        r#"
server:
  bind: "0.0.0.0:8765"
providers:
  - id: mock
    type: mock
routes:
  - name: default
    match:
      model: "*"
    targets:
      - "mock/echo"
"#,
    )
    .unwrap();

    let error = validate_open_admin_bind(&cfg, "0.0.0.0:8765")
        .unwrap_err()
        .to_string();
    assert!(error.contains("allow_open_admin"));

    let mut explicitly_open = cfg.clone();
    explicitly_open.server.allow_open_admin = true;
    validate_open_admin_bind(&explicitly_open, "0.0.0.0:8765").unwrap();

    let mut authenticated = cfg.clone();
    authenticated.server.api_key = Some("local-admin".to_string());
    validate_open_admin_bind(&authenticated, "0.0.0.0:8765").unwrap();

    validate_open_admin_bind(&cfg, "127.0.0.1:8765").unwrap();
    validate_open_admin_bind(&cfg, "localhost:8765").unwrap();
    validate_open_admin_bind(&cfg, "[::1]:8765").unwrap();
}

#[test]
fn init_config_writes_parent_dirs_and_refuses_overwrite() {
    let root = temp_name("switchback-init-test");
    let path = root.join("nested").join("switchback.yaml");

    init_config_file(&path, false, InitTemplate::Quickstart).unwrap();
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(written.contains("mock/echo"));

    let err = init_config_file(&path, false, InitTemplate::Quickstart)
        .unwrap_err()
        .to_string();
    assert!(err.contains("already exists"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), written);

    init_config_file(&path, true, InitTemplate::Quickstart).unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn provider_add_appends_env_key_provider_and_optional_route() {
    let root = std::env::temp_dir().join(format!(
        "switchback-provider-add-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("switchback.yaml");
    std::fs::write(&path, config_cli::STARTER_CONFIG).unwrap();

    let summary = provider_add_config_file(
        &path,
        ProviderAddRequest {
            preset: ProviderPreset::Openai,
            id: None,
            base_url: None,
            api_key_env: None,
            model: Some("gpt-test".to_string()),
            route: Some("openai/test".to_string()),
            force: false,
        },
    )
    .unwrap();
    assert_eq!(summary.provider_id, "openai");
    assert_eq!(summary.api_key_env.as_deref(), Some("OPENAI_API_KEY"));
    assert_eq!(summary.route_model.as_deref(), Some("openai/test"));
    assert_eq!(summary.target.as_deref(), Some("openai/gpt-test"));

    let written = std::fs::read_to_string(&path).unwrap();
    assert!(written.contains("OPENAI_API_KEY"));
    assert!(!written.contains("api_key:"));

    let cfg = Config::from_yaml(&written).unwrap();
    let provider = cfg.providers.iter().find(|p| p.id == "openai").unwrap();
    match &provider.kind {
        sb_core::ProviderKind::OpenaiCompatible {
            base_url,
            api_key_env,
            api_key,
            ..
        } => {
            assert_eq!(base_url, "https://api.openai.com/v1");
            assert_eq!(api_key_env.as_deref(), Some("OPENAI_API_KEY"));
            assert!(api_key.is_none());
        }
        _ => panic!("expected openai-compatible provider"),
    }
    let route = cfg.exact_route_for("openai/test").unwrap();
    assert_eq!(route.targets, vec!["openai/gpt-test"]);

    let err = provider_add_config_file(
        &path,
        ProviderAddRequest {
            preset: ProviderPreset::Openai,
            id: None,
            base_url: None,
            api_key_env: None,
            model: None,
            route: None,
            force: false,
        },
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("already exists"));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn provider_presets_cover_common_official_api_hosts() {
    let expected = [
        (
            "deepseek",
            "deepseek",
            "https://api.deepseek.com",
            "DEEPSEEK_API_KEY",
        ),
        (
            "groq",
            "groq",
            "https://api.groq.com/openai/v1",
            "GROQ_API_KEY",
        ),
        (
            "mistral",
            "mistral",
            "https://api.mistral.ai/v1",
            "MISTRAL_API_KEY",
        ),
        (
            "together",
            "together",
            "https://api.together.ai/v1",
            "TOGETHER_API_KEY",
        ),
        (
            "fireworks",
            "fireworks",
            "https://api.fireworks.ai/inference/v1",
            "FIREWORKS_API_KEY",
        ),
        (
            "cerebras",
            "cerebras",
            "https://api.cerebras.ai/v1",
            "CEREBRAS_API_KEY",
        ),
        ("xai", "xai", "https://api.x.ai/v1", "XAI_API_KEY"),
        (
            "nvidia",
            "nvidia",
            "https://integrate.api.nvidia.com/v1",
            "NVIDIA_API_KEY",
        ),
    ];

    for (cli, id, base_url, env) in expected {
        let preset = ProviderPreset::from_str(cli, true).unwrap();
        let (_default_id, _kind, default_base_url, default_api_key_env) = preset_defaults(preset);
        let value = provider_mapping(
            preset,
            id,
            default_base_url.map(ToString::to_string),
            default_api_key_env.map(ToString::to_string),
        );
        let mapping = value.as_mapping().unwrap();
        assert_eq!(config_cli::mapping_str(mapping, "id"), Some(id));
        assert_eq!(config_cli::mapping_str(mapping, "base_url"), Some(base_url));
        assert_eq!(config_cli::mapping_str(mapping, "api_key_env"), Some(env));
    }
}

#[test]
fn provider_presets_add_validate_and_expose_readiness_contracts() {
    let root = temp_name("switchback-provider-preset-e2e");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("switchback.yaml");
    std::fs::write(&path, config_cli::STARTER_CONFIG).unwrap();

    for preset in PROVIDER_PRESETS {
        let model = preset_model_hint(preset).map(ToString::to_string);
        provider_add_config_file(
            &path,
            ProviderAddRequest {
                preset,
                id: None,
                base_url: None,
                api_key_env: None,
                model: model.clone(),
                route: None,
                force: false,
            },
        )
        .unwrap_or_else(|error| {
            panic!("preset {} should add cleanly: {error}", preset_name(preset))
        });

        let manifest = provider_readiness_manifest_json(preset);
        assert_eq!(manifest["schema"], "switchback/provider-readiness@1");
        assert_eq!(manifest["preset"], preset_name(preset));
        let required_checks = manifest["required_checks"].as_array().unwrap();
        if preset_is_workload_executor(preset) {
            assert_eq!(manifest["provider_role"], "workload_executor");
            assert!(required_checks
                .iter()
                .any(|check| check == "image_generation"));
            assert!(!required_checks.iter().any(|check| check == "chat_stream"));
            assert_eq!(
                manifest["capability_contract"]["chat_stream"],
                "unsupported"
            );
        } else {
            assert_eq!(manifest["provider_role"], "model_api");
            assert!(required_checks.iter().any(|check| check == "chat_stream"));
        }
    }

    let written = std::fs::read_to_string(&path).unwrap();
    let cfg = Config::from_yaml(&written).unwrap();
    let semantic_problems = cfg.semantic_problems();
    assert!(
        semantic_problems.is_empty(),
        "preset config should be semantically valid: {semantic_problems:?}"
    );

    for preset in PROVIDER_PRESETS {
        let id = preset_name(preset);
        assert!(
            cfg.providers.iter().any(|provider| provider.id == id),
            "missing provider {id}"
        );
        if let Some(model) = preset_model_hint(preset) {
            assert!(
                cfg.routes.iter().any(|route| {
                    route.match_.model.as_deref() == Some(&format!("{id}/{model}"))
                        && route.targets == vec![format!("{id}/{model}")]
                }),
                "missing exact route for {id}/{model}"
            );
        } else {
            assert!(
                cfg.routes
                    .iter()
                    .all(|route| route.targets.iter().all(|target| !target.starts_with(id))),
                "workload provider {id} should not get a text route"
            );
        }
    }

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn provider_add_empty_api_key_env_disables_auth_default() {
    let root = std::env::temp_dir().join(format!(
        "switchback-provider-no-auth-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("switchback.yaml");
    std::fs::write(&path, config_cli::STARTER_CONFIG).unwrap();

    provider_add_config_file(
        &path,
        ProviderAddRequest {
            preset: ProviderPreset::Openai,
            id: Some("local-openai".to_string()),
            base_url: Some(format!("{}://{}:{}/v1", "http", "localhost", 9999)),
            api_key_env: Some(String::new()),
            model: None,
            route: None,
            force: false,
        },
    )
    .unwrap();

    let written = std::fs::read_to_string(&path).unwrap();
    let cfg = Config::from_yaml(&written).unwrap();
    let provider = cfg
        .providers
        .iter()
        .find(|p| p.id == "local-openai")
        .unwrap();
    match &provider.kind {
        sb_core::ProviderKind::OpenaiCompatible { api_key_env, .. } => {
            assert!(api_key_env.is_none());
        }
        _ => panic!("expected openai-compatible provider"),
    }
    Engine::validate_config(&cfg).unwrap();

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn provider_test_executes_the_selected_direct_target() {
    let root = std::env::temp_dir().join(format!(
        "switchback-provider-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("switchback.yaml");
    std::fs::write(
        &path,
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
    accounts:
      - id: a
        auth: { kind: api_key, inline: "k" }
  - id: alt
    type: mock
    accounts:
      - id: a
        auth: { kind: api_key, inline: "k" }
routes:
  - name: default
    match: { model: "*" }
    targets:
      - "mock/echo"
"#,
    )
    .unwrap();

    let summary = provider_test_config_file(&path, "alt", Some("echo"), false)
        .await
        .unwrap();

    assert_eq!(summary.target, "alt/echo");
    assert!(!summary.stream);
    assert!(summary.output_chars > 0);

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn provider_test_uses_first_discovered_model_when_model_is_omitted() {
    let root = std::env::temp_dir().join(format!(
        "switchback-provider-test-discovery-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("switchback.yaml");
    std::fs::write(
        &path,
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
"#,
    )
    .unwrap();

    let summary = provider_test_config_file(&path, "mock", None, false)
        .await
        .unwrap();

    assert_eq!(summary.model, "echo");
    assert_eq!(summary.target, "mock/echo");

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn provider_doctor_reports_core_provider_checks() {
    let root = std::env::temp_dir().join(format!(
        "switchback-provider-doctor-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("switchback.yaml");
    std::fs::write(
        &path,
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
"#,
    )
    .unwrap();

    let summary = provider_doctor_config_file(&path, "mock", None)
        .await
        .unwrap();

    assert!(summary.ok);
    assert_eq!(summary.provider_id, "mock");
    assert_eq!(summary.model, "echo");
    assert_eq!(summary.target, "mock/echo");
    for name in [
        "config",
        "auth",
        "models",
        "route_preview",
        "chat_non_stream",
        "chat_stream",
        "embeddings",
    ] {
        assert!(
            summary
                .checks
                .iter()
                .any(|check| check.name == name && check.status == "ok"),
            "missing ok check {name}: {:?}",
            summary.checks
        );
    }

    std::fs::remove_dir_all(root).unwrap();
}

async fn fake_comfy_system_stats() -> Json<serde_json::Value> {
    Json(serde_json::json!({"system": {"os": "test"}, "devices": []}))
}

async fn spawn_fake_comfy_system_stats() -> String {
    let app = Router::new().route("/system_stats", get(fake_comfy_system_stats));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn provider_doctor_probes_comfyui_workload_executor() {
    let upstream = spawn_fake_comfy_system_stats().await;
    let root = temp_name("switchback-comfyui-provider-doctor-test");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("switchback.yaml");
    std::fs::write(
        &path,
        format!(
            r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: comfy
    type: comfyui
    base_url: "{upstream}"
    workflows:
      - id: txt2img
        kind: image_generation
        version: test
        graph: {{"6": {{"class_type": "CLIPTextEncode", "inputs": {{"text": ""}}}}, "9": {{"class_type": "SaveImage", "inputs": {{"filename_prefix": "switchback"}}}}}}
        bindings:
          prompt: {{ path: ["6", "inputs", "text"] }}
        output_node_ids: ["9"]
"#
        ),
    )
    .unwrap();

    let summary = provider_doctor_config_file(&path, "comfy", None)
        .await
        .unwrap();
    assert!(summary.ok, "{:?}", summary.checks);
    assert_eq!(summary.provider_id, "comfy");
    assert_eq!(summary.model, "workflows");
    assert_eq!(summary.target, "comfy/workflows");
    for name in [
        "config",
        "auth",
        "comfyui_system_stats",
        "workflow_templates",
    ] {
        assert!(
            summary
                .checks
                .iter()
                .any(|check| check.name == name && check.status == "ok"),
            "missing ok check {name}: {:?}",
            summary.checks
        );
    }
    assert!(!summary
        .checks
        .iter()
        .any(|check| check.name == "chat_stream"));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn provider_missing_envs_reports_oauth_account_sources() {
    let missing_refresh = format!(
        "SB_CODEX_REFRESH_{}_{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let missing_secret = format!(
        "SB_CLAUDE_SECRET_{}_{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let cfg = Config::from_yaml(&format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: multi
    type: openai_compatible
    base_url: "https://example.invalid/v1"
    accounts:
      - id: codex
        auth:
          kind: oauth
          refresh_env: "{missing_refresh}"
          token_url: "https://oauth.example/token"
      - id: claude-code
        auth:
          kind: oauth
          refresh: "inline-refresh-token"
          token_url: "https://oauth.example/token"
          client_secret_env: "{missing_secret}"
"#
    ))
    .unwrap();
    let provider = cfg
        .providers
        .iter()
        .find(|provider| provider.id == "multi")
        .unwrap();

    let missing = provider_missing_envs(provider);

    assert_eq!(missing.len(), 2);
    assert!(missing.contains(&missing_refresh));
    assert!(missing.contains(&missing_secret));
}

#[tokio::test]
async fn provider_matrix_skips_missing_env_and_checks_available_providers() {
    let missing_env = format!(
        "SB_MATRIX_MISSING_{}_{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let root = std::env::temp_dir().join(format!(
        "switchback-provider-matrix-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("switchback.yaml");
    std::fs::write(
        &path,
        format!(
            r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
  - id: remote
    type: openai_compatible
    base_url: "https://example.invalid/v1"
    api_key_env: "{missing_env}"
"#
        ),
    )
    .unwrap();

    let summary = provider_matrix_config_file(&path).await.unwrap();

    assert_eq!(summary.schema, "switchback/provider-matrix@1");
    assert!(summary.ok);
    assert_eq!(summary.total, 2);
    assert_eq!(summary.checked, 1);
    assert_eq!(summary.skipped, 1);
    assert_eq!(summary.failed, 0);
    let mock = summary
        .providers
        .iter()
        .find(|provider| provider.provider_id == "mock")
        .unwrap();
    assert_eq!(mock.status, "ok");
    assert_eq!(mock.doctor.as_ref().unwrap().target, "mock/echo");
    let remote = summary
        .providers
        .iter()
        .find(|provider| provider.provider_id == "remote")
        .unwrap();
    assert_eq!(remote.status, "skipped");
    assert_eq!(remote.missing_env, vec![missing_env]);
    assert!(remote.doctor.is_none());

    std::fs::remove_dir_all(root).unwrap();
}

async fn fake_openai_chat_without_models(Json(body): Json<serde_json::Value>) -> Response {
    let model = body
        .get("model")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("missing-model");
    if body
        .get("stream")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        let sse = format!(
                "data: {{\"id\":\"chatcmpl-hint\",\"object\":\"chat.completion.chunk\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\"}},\"finish_reason\":null}}]}}\n\n\
data: {{\"id\":\"chatcmpl-hint\",\"object\":\"chat.completion.chunk\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"model={model}\"}},\"finish_reason\":null}}]}}\n\n\
data: {{\"id\":\"chatcmpl-hint\",\"object\":\"chat.completion.chunk\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}}}\n\n\
data: [DONE]\n\n"
            );
        return ([("content-type", "text/event-stream")], sse).into_response();
    }

    Json(serde_json::json!({
        "id": "chatcmpl-hint",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": {
                "role": "assistant",
                "content": format!("model={model}")
            }
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
    }))
    .into_response()
}

async fn spawn_fake_openai_without_models() -> String {
    let app = Router::new().route("/chat/completions", post(fake_openai_chat_without_models));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn provider_doctor_uses_model_hint_when_models_endpoint_is_unavailable() {
    let upstream = spawn_fake_openai_without_models().await;
    let root = std::env::temp_dir().join(format!(
        "switchback-provider-hint-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("switchback.yaml");
    std::fs::write(
        &path,
        format!(
            r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: hinted
    type: openai_compatible
    base_url: "{upstream}"
    model_hint: "hint-model"
"#
        ),
    )
    .unwrap();

    let summary = provider_doctor_config_file(&path, "hinted", None)
        .await
        .unwrap();

    assert!(summary.ok);
    assert_eq!(summary.model, "hint-model");
    assert_eq!(summary.target, "hinted/hint-model");
    assert!(
        summary
            .checks
            .iter()
            .any(|check| check.name == "model_hint" && check.status == "ok"),
        "missing model hint check: {:?}",
        summary.checks
    );
    assert!(
        summary
            .checks
            .iter()
            .any(|check| check.name == "models" && !check.required),
        "model discovery should be optional when a hint is configured: {:?}",
        summary.checks
    );

    let test_summary = provider_test_config_file(&path, "hinted", None, false)
        .await
        .unwrap();
    assert_eq!(test_summary.model, "hint-model");
    assert_eq!(test_summary.target, "hinted/hint-model");

    let matrix = provider_matrix_config_file(&path).await.unwrap();
    assert_eq!(matrix.checked, 1);
    assert_eq!(matrix.failed, 0);
    assert_eq!(matrix.providers[0].status, "ok");
    assert_eq!(
        matrix.providers[0].doctor.as_ref().unwrap().model,
        "hint-model"
    );

    std::fs::remove_dir_all(root).unwrap();
}

async fn fake_openai_models(headers: HeaderMap) -> Json<serde_json::Value> {
    let auth = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("absent");
    Json(serde_json::json!({
        "object": "list",
        "data": [
            {
                "id": "model-a",
                "object": "model",
                "owned_by": auth
            },
            {
                "id": "owner/model-b",
                "object": "model",
                "owned_by": "test"
            }
        ]
    }))
}

async fn spawn_fake_openai_models() -> String {
    let app = Router::new().route("/models", get(fake_openai_models));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn provider_models_lists_upstream_models_with_switchback_ids() {
    let upstream = spawn_fake_openai_models().await;
    let root = std::env::temp_dir().join(format!(
        "switchback-provider-models-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("switchback.yaml");
    std::fs::write(
        &path,
        format!(
            r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: upstream
    type: openai_compatible
    base_url: "{upstream}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "secret-xyz" }}
"#
        ),
    )
    .unwrap();

    let summary = provider_models_config_file(&path, "upstream")
        .await
        .unwrap();

    assert_eq!(summary.provider_id, "upstream");
    assert_eq!(summary.models.len(), 2);
    assert_eq!(summary.models[0].id, "model-a");
    assert_eq!(summary.models[0].switchback_model, "upstream/model-a");
    assert_eq!(summary.models[1].id, "owner/model-b");
    assert_eq!(summary.models[1].switchback_model, "upstream/owner/model-b");

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn provider_sync_routes_imports_discovered_models() {
    let upstream = spawn_fake_openai_models().await;
    let root = std::env::temp_dir().join(format!(
        "switchback-provider-sync-routes-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("switchback.yaml");
    std::fs::write(
        &path,
        format!(
            r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: upstream
    type: openai_compatible
    base_url: "{upstream}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "secret-xyz" }}
routes:
  - name: existing
    match: {{ model: "upstream/model-a" }}
    targets:
      - "upstream/old-model"
"#
        ),
    )
    .unwrap();

    let skipped = provider_sync_routes_config_file(&path, "upstream", None, false)
        .await
        .unwrap();
    assert_eq!(skipped.added, 1);
    assert_eq!(skipped.skipped, 1);
    assert_eq!(skipped.replaced, 0);

    let cfg = Config::from_path(&path).unwrap();
    assert_eq!(
        cfg.exact_route_for("upstream/model-a").unwrap().targets,
        vec!["upstream/old-model"]
    );
    assert_eq!(
        cfg.exact_route_for("upstream/owner/model-b")
            .unwrap()
            .targets,
        vec!["upstream/owner/model-b"]
    );

    let forced = provider_sync_routes_config_file(&path, "upstream", Some("local"), true)
        .await
        .unwrap();
    assert_eq!(forced.added, 2);
    assert_eq!(forced.skipped, 0);
    assert_eq!(forced.replaced, 0);

    let cfg = Config::from_path(&path).unwrap();
    assert_eq!(
        cfg.exact_route_for("local/model-a").unwrap().targets,
        vec!["upstream/model-a"]
    );
    assert_eq!(
        cfg.exact_route_for("local/owner/model-b").unwrap().targets,
        vec!["upstream/owner/model-b"]
    );

    std::fs::remove_dir_all(root).unwrap();
}
