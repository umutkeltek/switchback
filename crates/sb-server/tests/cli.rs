use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const MINIMAL_CFG: &str = r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
routes:
  - name: default
    match:
      model: "*"
    targets:
      - "mock/echo"
"#;

fn switchback_bin() -> &'static str {
    env!("CARGO_BIN_EXE_switchback")
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("switchback-{tag}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_config(dir: &Path) -> PathBuf {
    let config = dir.join("switchback.yaml");
    fs::write(&config, MINIMAL_CFG).unwrap();
    config
}

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

#[test]
fn doctor_json_emits_parseable_report_on_stdout() {
    let dir = temp_dir("doctor-json");
    let config = write_config(&dir);

    let output = Command::new(switchback_bin())
        .arg("--json")
        .arg("doctor")
        .arg("--config")
        .arg(&config)
        .env("RUST_LOG", "info")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("doctor --json stdout should be parseable JSON");

    assert_eq!(value["ok"], serde_json::json!(true));
    assert_eq!(value["providers"][0]["id"], "mock");
    assert_eq!(value["routes"][0]["name"], "default");

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn provider_add_json_reports_written_provider_and_route() {
    let dir = temp_dir("provider-add-json");
    let config = write_config(&dir);

    let output = Command::new(switchback_bin())
        .arg("--json")
        .arg("provider")
        .arg("add")
        .arg("openai")
        .arg("--config")
        .arg(&config)
        .arg("--model")
        .arg("gpt-test")
        .env("RUST_LOG", "info")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("provider add --json stdout should be parseable JSON");
    assert_eq!(value["ok"], serde_json::json!(true));
    assert_eq!(value["provider_id"], "openai");
    assert_eq!(value["route_model"], "openai/gpt-test");
    assert_eq!(value["target"], "openai/gpt-test");

    let config_text = fs::read_to_string(&config).unwrap();
    assert!(config_text.contains("id: openai"), "{config_text}");
    assert!(config_text.contains("gpt-test"), "{config_text}");

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn init_native_clients_writes_explicit_codex_and_claude_profiles() {
    let dir = temp_dir("init-native-clients");
    let config = dir.join("switchback.yaml");

    let output = Command::new(switchback_bin())
        .arg("--json")
        .arg("init")
        .arg("--native-clients")
        .arg("--config")
        .arg(&config)
        .env("RUST_LOG", "info")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("init --native-clients --json stdout should be parseable JSON");
    assert_eq!(value["ok"], serde_json::json!(true));
    assert_eq!(value["template"], "native_clients");
    assert!(value["next_commands"]
        .as_array()
        .unwrap()
        .iter()
        .any(|cmd| cmd.as_str().unwrap().contains("codex exec")));
    assert!(value["next_commands"]
        .as_array()
        .unwrap()
        .iter()
        .any(|cmd| cmd.as_str().unwrap().contains("claude -p")));

    let config_text = fs::read_to_string(&config).unwrap();
    assert!(config_text.contains("client_profiles:"), "{config_text}");
    assert!(config_text.contains("kind: codex"), "{config_text}");
    assert!(config_text.contains("kind: claude_code"), "{config_text}");
    assert!(
        config_text.contains("accounts: [\"mock/local\"]"),
        "{config_text}"
    );
    assert!(
        config_text.contains("id: openai"),
        "real provider example should be discoverable"
    );
    assert!(
        config_text.contains("id: anthropic"),
        "real provider example should be discoverable"
    );

    let parsed = sb_core::Config::from_yaml(&config_text).unwrap();
    sb_runtime::Engine::validate_config(&parsed).unwrap();
    assert_eq!(parsed.client_profiles.len(), 2);
    assert_eq!(parsed.client_profiles[0].id, "codex");
    assert_eq!(parsed.client_profiles[1].id, "claude-code");

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn setup_native_creates_config_and_reports_native_sources_without_leaking_tokens() {
    let dir = temp_dir("setup-native");
    let config = dir.join("switchback.yaml");
    let home = dir.join("home");
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::write(
        home.join(".codex/auth.json"),
        r#"{"tokens":{"access_token":"codex-secret-token"}}"#,
    )
    .unwrap();
    fs::write(
        home.join(".claude/.credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"claude-secret-token"}}"#,
    )
    .unwrap();

    let output = Command::new(switchback_bin())
        .arg("--json")
        .arg("setup")
        .arg("native")
        .arg("--config")
        .arg(&config)
        .env("HOME", &home)
        .env_remove("CODEX_ACCESS_TOKEN")
        .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
        .env("RUST_LOG", "info")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("codex-secret-token"), "{stdout}");
    assert!(!stdout.contains("claude-secret-token"), "{stdout}");
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("setup native should emit JSON");

    assert_eq!(value["schema"], "switchback/setup-native@1");
    assert_eq!(value["ok"], serde_json::json!(true));
    assert_eq!(value["created_config"], serde_json::json!(true));
    assert_eq!(value["validation"]["ok"], serde_json::json!(true));
    assert!(value["next_commands"]
        .as_array()
        .unwrap()
        .iter()
        .any(|cmd| cmd
            .as_str()
            .unwrap()
            .contains("setup pack install native-token-adapter")));

    let clients = value["clients"].as_array().unwrap();
    assert_eq!(clients.len(), 2);
    assert!(clients
        .iter()
        .all(|client| client["token_available"] == serde_json::json!(true)));
    assert!(clients
        .iter()
        .all(|client| client["native_account_configured"] == serde_json::json!(false)));

    let config_text = fs::read_to_string(&config).unwrap();
    let parsed = sb_core::Config::from_yaml(&config_text).unwrap();
    sb_runtime::Engine::validate_config(&parsed).unwrap();

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn setup_pack_install_native_token_adapter_adds_profiles_without_removing_mock_smoke_path() {
    let dir = temp_dir("setup-pack-native-token-adapter");
    let config = dir.join("switchback.yaml");

    let output = Command::new(switchback_bin())
        .arg("--json")
        .arg("setup")
        .arg("pack")
        .arg("install")
        .arg("native-token-adapter")
        .arg("--config")
        .arg(&config)
        .env("RUST_LOG", "info")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("setup pack install should emit JSON");
    assert_eq!(value["schema"], "switchback/setup-pack-install@1");
    assert_eq!(value["ok"], serde_json::json!(true));
    assert_eq!(value["initialized_config"], serde_json::json!(true));
    assert!(value["next_commands"]
        .as_array()
        .unwrap()
        .iter()
        .any(|cmd| cmd.as_str().unwrap().contains("--model codex-native")));

    let config_text = fs::read_to_string(&config).unwrap();
    assert!(config_text.contains("mock/local"), "{config_text}");
    assert!(config_text.contains("id: openai-native"), "{config_text}");
    assert!(
        config_text.contains("id: anthropic-claude-code-native"),
        "{config_text}"
    );
    assert!(config_text.contains("kind: codex_oauth"), "{config_text}");
    assert!(
        config_text.contains("kind: claude_code_oauth"),
        "{config_text}"
    );
    assert!(config_text.contains("id: codex-native"), "{config_text}");
    assert!(
        config_text.contains("id: claude-code-native"),
        "{config_text}"
    );

    let parsed = sb_core::Config::from_yaml(&config_text).unwrap();
    sb_runtime::Engine::validate_config(&parsed).unwrap();
    assert!(parsed
        .client_profiles
        .iter()
        .any(|profile| profile.id == "codex-native"));
    assert!(parsed
        .client_profiles
        .iter()
        .any(|profile| profile.id == "claude-code-native"));

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn setup_native_relay_audit_reports_shape_without_enabling_or_leaking_tokens() {
    let dir = temp_dir("setup-native-relay-audit");
    let home = dir.join("home");
    let bin = dir.join("bin");
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::create_dir_all(&bin).unwrap();
    fs::write(
        home.join(".codex/auth.json"),
        r#"{"tokens":{"access_token":"codex-relay-secret"}}"#,
    )
    .unwrap();
    fs::write(
        home.join(".claude/.credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"claude-relay-secret"}}"#,
    )
    .unwrap();
    write_executable(&bin.join("codex"), "#!/bin/sh\necho codex 1.2.3\n");
    write_executable(&bin.join("claude"), "#!/bin/sh\necho claude 4.5.6\n");

    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::new(switchback_bin())
        .arg("--json")
        .arg("setup")
        .arg("native-relay")
        .arg("audit")
        .env("HOME", &home)
        .env("PATH", path)
        .env("RUST_LOG", "info")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("codex-relay-secret"), "{stdout}");
    assert!(!stdout.contains("claude-relay-secret"), "{stdout}");
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("native relay audit should emit JSON");

    assert_eq!(value["schema"], "switchback/native-relay-audit@1");
    assert_eq!(value["status"], "planned_not_implemented");
    assert_eq!(value["relay_implemented"], serde_json::json!(false));
    assert_eq!(
        value["fixture_manifest"],
        "crates/sb-protocols/tests/fixtures/native-relay/manifest.json"
    );
    assert!(value["adapter_gate"]
        .as_str()
        .unwrap()
        .contains("rejects codex_native_relay"));
    let clients = value["clients"].as_array().unwrap();
    assert_eq!(clients.len(), 2);
    assert!(clients
        .iter()
        .all(|client| client["installed"] == serde_json::json!(true)));
    assert!(clients.iter().all(|client| {
        client["auth_store"]["exists"] == serde_json::json!(true)
            && client["auth_store"]["access_token_present"] == serde_json::json!(true)
            && client["auth_store"]["inspected_shape_only"] == serde_json::json!(true)
    }));
    assert!(value["required_fixtures"]
        .as_array()
        .unwrap()
        .iter()
        .any(|fixture| fixture.as_str() == Some("stream_request_first_byte_and_finish")));

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn setup_native_relay_capture_writes_sanitized_fixture_without_leaking_tokens() {
    let dir = temp_dir("setup-native-relay-capture");
    let raw = dir.join("raw.json");
    let fixture = dir.join("fixture.json");
    fs::write(
        &raw,
        r#"{
  "request": {
    "url": "https://chatgpt.com/backend-api/codex",
    "headers": {
      "authorization": "Bearer codex-capture-secret",
      "cookie": "session=claude-cookie-secret",
      "x-api-key": "sk-capture-secret"
    },
    "body": {
      "prompt": "ping",
      "access_token": "access-capture-secret",
      "nested": { "safe": "keep" }
    }
  }
}"#,
    )
    .unwrap();

    let output = Command::new(switchback_bin())
        .arg("--json")
        .arg("setup")
        .arg("native-relay")
        .arg("capture")
        .arg("--client")
        .arg("codex")
        .arg("--fixture")
        .arg("non_stream_request_response")
        .arg("--from-file")
        .arg(&raw)
        .arg("--out-file")
        .arg(&fixture)
        .env("RUST_LOG", "info")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("codex-capture-secret"), "{stdout}");
    assert!(!stdout.contains("claude-cookie-secret"), "{stdout}");
    assert!(!stdout.contains("sk-capture-secret"), "{stdout}");
    assert!(!stdout.contains("access-capture-secret"), "{stdout}");

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("native relay capture should emit JSON");
    assert_eq!(value["schema"], "switchback/native-relay-capture@1");
    assert_eq!(value["ok"], serde_json::json!(true));
    assert_eq!(value["client"], "codex");
    assert_eq!(value["fixture"], "non_stream_request_response");
    assert!(value["redactions"].as_u64().unwrap() >= 4);

    let fixture_text = fs::read_to_string(&fixture).unwrap();
    assert!(
        !fixture_text.contains("codex-capture-secret"),
        "{fixture_text}"
    );
    assert!(
        !fixture_text.contains("claude-cookie-secret"),
        "{fixture_text}"
    );
    assert!(
        !fixture_text.contains("sk-capture-secret"),
        "{fixture_text}"
    );
    assert!(
        !fixture_text.contains("access-capture-secret"),
        "{fixture_text}"
    );
    let written: serde_json::Value =
        serde_json::from_str(&fixture_text).expect("sanitized fixture should be JSON");
    assert_eq!(
        written["schema"],
        "switchback/native-relay-sanitized-fixture@1"
    );
    assert_eq!(
        written["capture"]["json"]["request"]["body"]["nested"]["safe"],
        "keep"
    );
    assert_eq!(
        written["capture"]["json"]["request"]["headers"]["authorization"],
        "<redacted>"
    );

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn config_writer_commands_update_validate_and_report_json() {
    let dir = temp_dir("config-writer");
    let config = write_config(&dir);

    let set = Command::new(switchback_bin())
        .arg("config")
        .arg("set")
        .arg("server.bind")
        .arg(r#""127.0.0.1:9999""#)
        .arg("--config")
        .arg(&config)
        .output()
        .unwrap();
    assert!(
        set.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&set.stdout),
        String::from_utf8_lossy(&set.stderr)
    );
    let set_json: serde_json::Value =
        serde_json::from_slice(&set.stdout).expect("config set should emit JSON");
    assert_eq!(set_json["ok"], serde_json::json!(true));
    assert_eq!(set_json["path"], "server.bind");
    assert_eq!(set_json["value"], "127.0.0.1:9999");

    let patch = dir.join("patch.yaml");
    fs::write(&patch, "server:\n  cost_aware: true\n").unwrap();
    let patched = Command::new(switchback_bin())
        .arg("config")
        .arg("patch")
        .arg("--from-file")
        .arg(&patch)
        .arg("--config")
        .arg(&config)
        .output()
        .unwrap();
    assert!(
        patched.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&patched.stdout),
        String::from_utf8_lossy(&patched.stderr)
    );
    let patch_json: serde_json::Value =
        serde_json::from_slice(&patched.stdout).expect("config patch should emit JSON");
    assert_eq!(patch_json["ok"], serde_json::json!(true));
    assert_eq!(patch_json["patch"], patch.to_string_lossy().as_ref());

    let unset = Command::new(switchback_bin())
        .arg("config")
        .arg("unset")
        .arg("server.cost_aware")
        .arg("--config")
        .arg(&config)
        .output()
        .unwrap();
    assert!(
        unset.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&unset.stdout),
        String::from_utf8_lossy(&unset.stderr)
    );
    let unset_json: serde_json::Value =
        serde_json::from_slice(&unset.stdout).expect("config unset should emit JSON");
    assert_eq!(unset_json["ok"], serde_json::json!(true));
    assert_eq!(unset_json["removed"], serde_json::json!(true));

    let formatted = Command::new(switchback_bin())
        .arg("config")
        .arg("format")
        .arg("--config")
        .arg(&config)
        .output()
        .unwrap();
    assert!(
        formatted.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&formatted.stdout),
        String::from_utf8_lossy(&formatted.stderr)
    );
    let format_json: serde_json::Value =
        serde_json::from_slice(&formatted.stdout).expect("config format should emit JSON");
    assert_eq!(format_json["ok"], serde_json::json!(true));

    let bind = Command::new(switchback_bin())
        .arg("config")
        .arg("get")
        .arg("server.bind")
        .arg("--config")
        .arg(&config)
        .output()
        .unwrap();
    assert!(bind.status.success());
    let bind_json: serde_json::Value = serde_json::from_slice(&bind.stdout).unwrap();
    assert_eq!(bind_json, serde_json::json!("127.0.0.1:9999"));

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn schema_commands_describe_cli_and_config_for_agents() {
    let commands = Command::new(switchback_bin())
        .arg("schema")
        .arg("commands")
        .output()
        .unwrap();
    assert!(
        commands.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&commands.stdout),
        String::from_utf8_lossy(&commands.stderr)
    );
    let commands_json: serde_json::Value =
        serde_json::from_slice(&commands.stdout).expect("schema commands emits JSON");
    assert_eq!(commands_json["schema"], "switchback/commands@1");
    assert!(commands_json["commands"]
        .as_array()
        .unwrap()
        .iter()
        .any(|cmd| cmd["name"] == "config set"));

    let config = Command::new(switchback_bin())
        .arg("schema")
        .arg("config")
        .output()
        .unwrap();
    assert!(
        config.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&config.stdout),
        String::from_utf8_lossy(&config.stderr)
    );
    let config_json: serde_json::Value =
        serde_json::from_slice(&config.stdout).expect("schema config emits JSON");
    assert_eq!(config_json["schema"], "switchback/config-paths@1");
    assert!(config_json["paths"]
        .as_array()
        .unwrap()
        .iter()
        .any(|path| path["path"] == "providers.N.model_hint"));

    let docs = Command::new(switchback_bin())
        .arg("schema")
        .arg("docs")
        .output()
        .unwrap();
    assert!(
        docs.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&docs.stdout),
        String::from_utf8_lossy(&docs.stderr)
    );
    let docs_text = String::from_utf8_lossy(&docs.stdout);
    assert!(docs_text.contains("# Switchback Generated CLI Contract"));
    assert!(docs_text.contains("provider certify-all"));
    assert!(docs_text.contains("--skip-missing-env"));
    assert!(docs_text.contains("Provider Readiness"));
}

#[test]
fn provider_presets_list_onboarding_defaults() {
    let output = Command::new(switchback_bin())
        .arg("provider")
        .arg("presets")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("provider presets emits JSON");
    let presets = value["presets"].as_array().unwrap();
    assert!(presets
        .iter()
        .any(|preset| preset["id"] == "openai" && preset["api_key_env"] == "OPENAI_API_KEY"));
    assert!(presets
        .iter()
        .any(|preset| preset["id"] == "ollama" && preset["local"] == true));
    assert!(presets.iter().any(|preset| {
        preset["id"] == "openai"
            && preset["readiness_manifest"]["required_checks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|check| check == "chat_stream")
    }));
}

#[test]
fn provider_readiness_prints_all_or_one_manifest() {
    let all = Command::new(switchback_bin())
        .arg("provider")
        .arg("readiness")
        .output()
        .unwrap();
    assert!(
        all.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&all.stdout),
        String::from_utf8_lossy(&all.stderr)
    );
    let all_json: serde_json::Value =
        serde_json::from_slice(&all.stdout).expect("provider readiness emits JSON");
    assert_eq!(
        all_json["schema"],
        "switchback/provider-readiness-manifests@1"
    );
    assert!(all_json["manifests"]
        .as_array()
        .unwrap()
        .iter()
        .any(|manifest| manifest["preset"] == "gemini"));

    let one = Command::new(switchback_bin())
        .arg("provider")
        .arg("readiness")
        .arg("openai")
        .output()
        .unwrap();
    assert!(
        one.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&one.stdout),
        String::from_utf8_lossy(&one.stderr)
    );
    let one_json: serde_json::Value =
        serde_json::from_slice(&one.stdout).expect("provider readiness openai emits JSON");
    assert_eq!(one_json["schema"], "switchback/provider-readiness@1");
    assert_eq!(one_json["preset"], "openai");
    assert_eq!(
        one_json["credential_contract"]["api_key_env"],
        "OPENAI_API_KEY"
    );
    assert!(one_json["e2e_commands"]
        .as_array()
        .unwrap()
        .iter()
        .any(|cmd| cmd.as_str().unwrap().contains("provider certify openai")));
}

#[test]
fn provider_certify_reports_pass_fail_counts_and_next_commands() {
    let dir = temp_dir("provider-certify");
    let config = write_config(&dir);

    let output = Command::new(switchback_bin())
        .arg("provider")
        .arg("certify")
        .arg("mock")
        .arg("--config")
        .arg(&config)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("provider certify emits JSON");
    assert_eq!(value["schema"], "switchback/provider-certification@1");
    assert_eq!(value["ok"], serde_json::json!(true));
    assert_eq!(value["status"], "certified");
    assert_eq!(value["provider_id"], "mock");
    assert_eq!(value["model"], "echo");
    assert!(value["summary"]["required_passed"].as_u64().unwrap() >= 4);
    assert_eq!(value["summary"]["required_failed"], 0);
    assert!(value["verified_capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|capability| capability == "chat_stream"));
    assert!(value["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|check| check["name"] == "chat_stream" && check["ok"] == true));
    assert!(value["next_commands"]
        .as_array()
        .unwrap()
        .iter()
        .any(|cmd| cmd.as_str().unwrap().contains("route-preview")));

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn provider_certify_all_reports_each_configured_provider() {
    let dir = temp_dir("provider-certify-all");
    let config = write_config(&dir);

    let output = Command::new(switchback_bin())
        .arg("provider")
        .arg("certify-all")
        .arg("--config")
        .arg(&config)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("provider certify-all emits JSON");
    assert_eq!(value["schema"], "switchback/provider-certifications@1");
    assert_eq!(value["ok"], serde_json::json!(true));
    assert_eq!(value["total"], 1);
    assert_eq!(value["certified"], 1);
    assert_eq!(value["skipped"], 0);
    assert_eq!(value["blocked"], 0);
    assert_eq!(value["failed"], 0);
    assert_eq!(value["providers"][0]["provider_id"], "mock");
    assert_eq!(value["providers"][0]["status"], "certified");

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn provider_certify_all_can_skip_missing_env_for_partial_live_smoke() {
    let dir = temp_dir("provider-certify-all-skip-missing");
    let config = dir.join("switchback.yaml");
    fs::write(
        &config,
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
  - id: openai
    type: openai_compatible
    base_url: "https://api.openai.com/v1"
    api_key_env: SWITCHBACK_TEST_CERTIFY_ALL_MISSING_ENV
    model_hint: gpt-test
routes:
  - name: default
    match:
      model: "*"
    targets:
      - "mock/echo"
"#,
    )
    .unwrap();

    let output = Command::new(switchback_bin())
        .arg("provider")
        .arg("certify-all")
        .arg("--skip-missing-env")
        .arg("--config")
        .arg(&config)
        .env_remove("SWITCHBACK_TEST_CERTIFY_ALL_MISSING_ENV")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("provider certify-all emits JSON");
    assert_eq!(value["schema"], "switchback/provider-certifications@1");
    assert_eq!(value["ok"], serde_json::json!(true));
    assert_eq!(value["total"], 2);
    assert_eq!(value["certified"], 1);
    assert_eq!(value["skipped"], 1);
    assert_eq!(value["blocked"], 0);
    assert_eq!(value["failed"], 0);
    assert!(value["providers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|provider| {
            provider["provider_id"] == "openai"
                && provider["status"] == "skipped"
                && provider["missing_env"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|name| name == "SWITCHBACK_TEST_CERTIFY_ALL_MISSING_ENV")
        }));

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn mcp_stdio_lists_switchback_tools() {
    let dir = temp_dir("mcp-list");
    let config = write_config(&dir);
    let mut child = Command::new(switchback_bin())
        .arg("mcp")
        .arg("--config")
        .arg(&config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{}}}}"#
        )
        .unwrap();
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{{}}}}"#
        )
        .unwrap();
    }
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let lines = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(lines[0]["id"], 1);
    assert_eq!(lines[1]["id"], 2);
    let tools = lines[1]["result"]["tools"].as_array().unwrap();
    assert!(tools
        .iter()
        .any(|tool| tool["name"] == "switchback_route_preview"));
    assert!(tools
        .iter()
        .any(|tool| tool["name"] == "switchback_provider_presets"));

    fs::remove_dir_all(dir).unwrap();
}
