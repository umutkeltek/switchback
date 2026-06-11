use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use sb_store::StateStore;

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

const LANE_CFG: &str = r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
combos:
  nonstop-code:
    strategy: fallback
    models:
      - "mock/code-primary"
      - "mock/code-fallback"
  nonstop-chat:
    strategy: fallback
    models:
      - "mock/chat-primary"
      - "mock/chat-fallback"
client_profiles:
  - id: codex-scout
    kind: codex
    models: ["nonstop-code"]
    accounts: ["mock/default"]
routes:
  - name: default
    match:
      model: "*"
    targets:
      - "mock/echo"
"#;

const CODEX_SCOUT_CONFIG: &str = r#"
[profiles.switchback-scout]
model_provider = "switchback-scout"
model = "scout/code"
model_reasoning_effort = "xhigh"

[model_providers.switchback-scout]
name = "Switchback Scout"
base_url = "http://127.0.0.1:0/v1"
wire_api = "responses"
env_key = "SWITCHBACK_SCOUT_API_KEY"
requires_openai_auth = false
"#;

const NATIVE_STATUS_CFG: &str = r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
    accounts:
      - id: local
        auth: { kind: api_key, inline: "not-a-real-key" }
  - id: openai-native
    type: openai_compatible
    base_url: "https://example.invalid/v1"
    accounts:
      - id: codex-native
        auth: { kind: codex_oauth }
  - id: anthropic-native
    type: anthropic
    base_url: "https://example.invalid"
    auth_scheme: { kind: bearer }
    accounts:
      - id: claude-code-native
        auth: { kind: claude_code_oauth }
client_profiles:
  - id: codex
    kind: codex
    models: ["scout/code"]
    accounts: ["mock/local"]
  - id: claude-code
    kind: claude_code
    models: ["claude"]
    accounts: ["mock/local"]
routes:
  - name: scout-code
    match:
      model: "scout/code"
    targets:
      - "mock/echo"
  - name: claude
    match:
      model: "claude"
    targets:
      - "mock/echo"
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

fn write_config_text(dir: &Path, text: &str) -> PathBuf {
    let config = dir.join("switchback.yaml");
    fs::write(&config, text).unwrap();
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
fn lane_doctor_json_reports_lane_identity_and_transition_warnings() {
    let dir = temp_dir("lane-doctor-json");
    let config = write_config_text(&dir, LANE_CFG);

    let output = Command::new(switchback_bin())
        .arg("--json")
        .arg("lane")
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
        .expect("lane doctor --json stdout should be parseable JSON");

    assert_eq!(value["schema"], "switchback/lane-doctor@1");
    assert_eq!(value["ok"], serde_json::json!(true));

    let lanes = value["lanes"].as_array().unwrap();
    let scout_code = lanes
        .iter()
        .find(|lane| lane["id"] == "scout/code")
        .expect("scout/code lane should be reported");
    assert_eq!(scout_code["state"], "yellow");
    assert_eq!(scout_code["source"]["kind"], "legacy_combo");
    assert_eq!(scout_code["source"]["name"], "nonstop-code");
    assert_eq!(scout_code["primary_target"], "mock/code-primary");
    assert_eq!(scout_code["fallback_count"], 1);

    let scout_chat = lanes
        .iter()
        .find(|lane| lane["id"] == "scout/chat")
        .expect("scout/chat lane should be reported");
    assert_eq!(scout_chat["state"], "yellow");
    assert_eq!(scout_chat["primary_target"], "mock/chat-primary");

    let codex_api = lanes
        .iter()
        .find(|lane| lane["id"] == "codex/api")
        .expect("codex/api lane should be reported");
    assert_eq!(codex_api["state"], "yellow");
    let aliases = codex_api["aliases"].as_array().unwrap();
    assert!(aliases.iter().any(|alias| alias == "codex-api"));
    assert!(!aliases.iter().any(|alias| alias == "codex"));
    assert!(codex_api["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning
            .as_str()
            .unwrap()
            .contains("interactive `codex` shell command uses `scout/code`")));

    let codex_native = lanes
        .iter()
        .find(|lane| lane["id"] == "codex-native")
        .expect("codex-native lane should be reported");
    assert_eq!(codex_native["state"], "red");
    assert_eq!(codex_native["source"]["kind"], "native_relay_gate");

    let warnings = value["warnings"].as_array().unwrap();
    assert!(warnings.iter().any(|warning| warning
        .as_str()
        .unwrap()
        .contains("default wildcard route has a single target")));
    assert!(value["next_actions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|action| action
            .as_str()
            .unwrap()
            .contains("Promote legacy combos into exact lane routes")));

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn lane_audit_codex_scout_reports_alignment_and_drift() {
    let dir = temp_dir("lane-audit-codex-scout");
    let config = write_config_text(&dir, LANE_CFG);
    let codex_config = dir.join("codex-config.toml");
    fs::write(&codex_config, CODEX_SCOUT_CONFIG).unwrap();

    let output = Command::new(switchback_bin())
        .arg("--json")
        .arg("lane")
        .arg("audit")
        .arg("codex-scout")
        .arg("--config")
        .arg(&config)
        .arg("--codex-config")
        .arg(&codex_config)
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
        .expect("lane audit codex-scout --json stdout should be parseable JSON");
    assert_eq!(value["schema"], "switchback/lane-codex-scout-audit@1");
    assert_eq!(value["ok"], serde_json::json!(true));
    assert!(value["checks"]
        .as_array()
        .unwrap()
        .iter()
        .all(|check| check["ok"] == true));

    fs::write(
        &codex_config,
        CODEX_SCOUT_CONFIG.replace("model = \"scout/code\"", "model = \"nonstop-code\""),
    )
    .unwrap();
    let drift = Command::new(switchback_bin())
        .arg("--json")
        .arg("lane")
        .arg("audit")
        .arg("codex-scout")
        .arg("--config")
        .arg(&config)
        .arg("--codex-config")
        .arg(&codex_config)
        .env("RUST_LOG", "info")
        .output()
        .unwrap();

    assert!(
        drift.status.success(),
        "json audit reports drift in-band without failing the command"
    );
    let drift_json: serde_json::Value =
        serde_json::from_slice(&drift.stdout).expect("drift audit stdout should be parseable JSON");
    assert_eq!(drift_json["ok"], serde_json::json!(false));
    let model_check = drift_json["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "profile.model")
        .expect("profile.model check should be present");
    assert_eq!(model_check["ok"], serde_json::json!(false));
    assert_eq!(model_check["expected"], serde_json::json!("scout/code"));
    assert_eq!(model_check["actual"], serde_json::json!("nonstop-code"));

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn lane_install_codex_scout_repairs_config_with_backup() {
    let dir = temp_dir("lane-install-codex-scout");
    let config = write_config_text(&dir, LANE_CFG);
    let codex_config = dir.join("codex-config.toml");
    fs::write(
        &codex_config,
        CODEX_SCOUT_CONFIG.replace("model = \"scout/code\"", "model = \"nonstop-code\""),
    )
    .unwrap();

    let dry_run = Command::new(switchback_bin())
        .arg("--json")
        .arg("lane")
        .arg("install")
        .arg("codex-scout")
        .arg("--dry-run")
        .arg("--config")
        .arg(&config)
        .arg("--codex-config")
        .arg(&codex_config)
        .env("RUST_LOG", "info")
        .output()
        .unwrap();
    assert!(
        dry_run.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&dry_run.stdout),
        String::from_utf8_lossy(&dry_run.stderr)
    );
    let dry_run_json: serde_json::Value =
        serde_json::from_slice(&dry_run.stdout).expect("dry-run install should emit JSON");
    assert_eq!(
        dry_run_json["schema"],
        "switchback/lane-codex-scout-install@1"
    );
    assert_eq!(dry_run_json["changed"], serde_json::json!(true));
    assert_eq!(dry_run_json["dry_run"], serde_json::json!(true));
    assert_eq!(dry_run_json["audit"]["ok"], serde_json::json!(true));
    assert!(
        fs::read_to_string(&codex_config)
            .unwrap()
            .contains("nonstop-code"),
        "dry-run must not write"
    );

    let installed = Command::new(switchback_bin())
        .arg("--json")
        .arg("lane")
        .arg("install")
        .arg("codex-scout")
        .arg("--config")
        .arg(&config)
        .arg("--codex-config")
        .arg(&codex_config)
        .env("RUST_LOG", "info")
        .output()
        .unwrap();
    assert!(
        installed.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&installed.stdout),
        String::from_utf8_lossy(&installed.stderr)
    );
    let installed_json: serde_json::Value =
        serde_json::from_slice(&installed.stdout).expect("install should emit JSON");
    assert_eq!(installed_json["ok"], serde_json::json!(true));
    assert_eq!(installed_json["changed"], serde_json::json!(true));
    let backup = installed_json["backup"]
        .as_str()
        .expect("install should report backup path");
    assert!(Path::new(backup).exists(), "backup should exist: {backup}");

    let repaired = fs::read_to_string(&codex_config).unwrap();
    assert!(repaired.contains("model = \"scout/code\""), "{repaired}");
    assert!(
        repaired.contains("base_url = \"http://127.0.0.1:0/v1\""),
        "{repaired}"
    );

    let audit = Command::new(switchback_bin())
        .arg("--json")
        .arg("lane")
        .arg("audit")
        .arg("codex-scout")
        .arg("--config")
        .arg(&config)
        .arg("--codex-config")
        .arg(&codex_config)
        .env("RUST_LOG", "info")
        .output()
        .unwrap();
    assert!(audit.status.success());
    let audit_json: serde_json::Value =
        serde_json::from_slice(&audit.stdout).expect("audit should emit JSON");
    assert_eq!(audit_json["ok"], serde_json::json!(true));

    let second_dry_run = Command::new(switchback_bin())
        .arg("--json")
        .arg("lane")
        .arg("install")
        .arg("codex-scout")
        .arg("--dry-run")
        .arg("--config")
        .arg(&config)
        .arg("--codex-config")
        .arg(&codex_config)
        .env("RUST_LOG", "info")
        .output()
        .unwrap();
    assert!(second_dry_run.status.success());
    let second_json: serde_json::Value = serde_json::from_slice(&second_dry_run.stdout)
        .expect("second dry-run install should emit JSON");
    assert_eq!(second_json["ok"], serde_json::json!(true));
    assert_eq!(second_json["changed"], serde_json::json!(false));
    assert_eq!(second_json["audit"]["ok"], serde_json::json!(true));

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
fn native_status_reports_readonly_local_shape_without_leaking_tokens() {
    let dir = temp_dir("native-status");
    let config = write_config_text(&dir, NATIVE_STATUS_CFG);
    let home = dir.join("home");
    let bin = dir.join("bin");
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::create_dir_all(&bin).unwrap();
    fs::write(
        home.join(".codex/auth.json"),
        r#"{"tokens":{"access_token":"codex-status-secret"}}"#,
    )
    .unwrap();
    fs::write(
        home.join(".claude/.credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"claude-status-secret"}}"#,
    )
    .unwrap();
    write_executable(&bin.join("codex"), "#!/bin/sh\necho codex 1.2.3\n");
    write_executable(&bin.join("claude"), "#!/bin/sh\necho claude 4.5.6\n");
    write_executable(
        &bin.join("launchctl"),
        "#!/bin/sh\nprintf 'PID\\tStatus\\tLabel\\n-\\t0\\tcom.example.codex.runtime-router\\n'\n",
    );

    let output = Command::new(switchback_bin())
        .arg("--json")
        .arg("native")
        .arg("status")
        .arg("--config")
        .arg(&config)
        .env("HOME", &home)
        .env("PATH", &bin)
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
    assert!(!stdout.contains("codex-status-secret"), "{stdout}");
    assert!(!stdout.contains("claude-status-secret"), "{stdout}");
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("native status should emit JSON");

    assert_eq!(value["schema"], "switchback/native-status@1");
    assert_eq!(value["read_only"], serde_json::json!(true));
    assert_eq!(value["validation"]["ok"], serde_json::json!(true));
    assert_eq!(
        value["server"]["health"]["status"],
        serde_json::json!("skipped_ephemeral_bind")
    );
    assert_eq!(
        value["lane_separation"]["scout_code"]["configured"],
        serde_json::json!(true)
    );
    assert!(value["lane_separation"]["native_routes"]
        .as_array()
        .unwrap()
        .iter()
        .all(|route| route["fail_closed"] == serde_json::json!(true)));

    let clients = value["clients"].as_array().unwrap();
    assert_eq!(clients.len(), 2);
    assert!(clients
        .iter()
        .all(|client| client["installed"] == serde_json::json!(true)));
    assert!(clients
        .iter()
        .all(|client| client["token_available"] == serde_json::json!(true)));
    assert!(clients
        .iter()
        .all(|client| client["native_account_configured"] == serde_json::json!(true)));
    assert!(clients
        .iter()
        .all(|client| client["modes"]["direct_native"]["ready"] == serde_json::json!(true)));

    let conflicts = value["local_runtime"]["possible_conflicts"]
        .as_array()
        .unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0]["id_redacted"], serde_json::json!(true));
    assert!(conflicts[0]["id"]
        .as_str()
        .unwrap()
        .starts_with("runtime-helper-"));
    assert!(
        !stdout.contains("com.example.codex.runtime-router"),
        "{stdout}"
    );

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn native_import_history_dry_run_reports_metadata_without_content_or_paths() {
    let dir = temp_dir("native-import-history");
    let home = dir.join("home");
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::create_dir_all(home.join(".claude/projects/private-workspace")).unwrap();
    fs::write(
        home.join(".codex/history.jsonl"),
        r#"{"timestamp":"2026-06-01T10:00:00Z","text":"codex-private-prompt"}"#,
    )
    .unwrap();
    fs::write(
        home.join(".codex/session_index.jsonl"),
        r#"{"updated_at":"2026-06-02T10:00:00Z","preview":"codex-private-preview"}"#,
    )
    .unwrap();
    fs::write(
        home.join(".claude/history.jsonl"),
        r#"{"timestamp":"2026-06-03T10:00:00Z","message":"claude-private-prompt"}"#,
    )
    .unwrap();
    fs::write(
        home.join(".claude/projects/private-workspace/session.jsonl"),
        r#"{"timestamp":"2026-06-04T10:00:00Z","response":"claude-private-response"}"#,
    )
    .unwrap();
    let conn = rusqlite::Connection::open(home.join(".codex/state_5.sqlite")).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE threads (
            id TEXT PRIMARY KEY,
            created_at_ms INTEGER,
            updated_at_ms INTEGER
        );
        INSERT INTO threads (id, created_at_ms, updated_at_ms)
        VALUES ('thread-private-id', 100, 200);
        CREATE TABLE agent_jobs (
            id TEXT PRIMARY KEY,
            created_at INTEGER,
            updated_at INTEGER
        );
        INSERT INTO agent_jobs (id, created_at, updated_at)
        VALUES ('job-private-id', 10, 20);
        "#,
    )
    .unwrap();

    let output = Command::new(switchback_bin())
        .arg("--json")
        .arg("native")
        .arg("import-history")
        .arg("--dry-run")
        .arg("--sample-files")
        .arg("5")
        .env("HOME", &home)
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
    for forbidden in [
        "codex-private-prompt",
        "codex-private-preview",
        "claude-private-prompt",
        "claude-private-response",
        "thread-private-id",
        "job-private-id",
        "private-workspace",
    ] {
        assert!(
            !stdout.contains(forbidden),
            "{forbidden} leaked in {stdout}"
        );
    }

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("native import-history emits JSON");
    assert_eq!(value["schema"], "switchback/native-history-import@1");
    assert_eq!(value["dry_run"], serde_json::json!(true));
    assert_eq!(value["applied"], serde_json::json!(false));
    assert_eq!(value["read_only"], serde_json::json!(true));
    assert_eq!(
        value["content_policy"]["transport"],
        serde_json::json!("client_native_import")
    );
    assert_eq!(
        value["content_policy"]["stores_prompts"],
        serde_json::json!(false)
    );
    assert_eq!(
        value["content_policy"]["stores_responses"],
        serde_json::json!(false)
    );
    assert_eq!(
        value["content_policy"]["stores_local_paths"],
        serde_json::json!(false)
    );
    assert_eq!(value["totals"]["source_count"], serde_json::json!(7));
    assert_eq!(
        value["totals"]["existing_source_count"],
        serde_json::json!(5)
    );
    assert_eq!(value["totals"]["record_count"], serde_json::json!(6));
    assert_eq!(value["totals"]["parse_error_count"], serde_json::json!(0));
    assert!(value["clients"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|client| client["sources"].as_array().unwrap().iter())
        .filter(|source| source["exists"] == serde_json::json!(true))
        .all(|source| source["path_redacted"] == serde_json::json!(true)
            || source["path_pattern"] == serde_json::json!("${HOME}/.claude/projects/**/*.jsonl")));
    assert!(value["clients"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|client| client["sources"].as_array().unwrap().iter())
        .flat_map(|source| source["sample_files"].as_array().into_iter().flatten())
        .all(|sample| sample["path_redacted"] == serde_json::json!(true)));

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn native_import_history_apply_persists_metadata_without_content_or_paths() {
    let dir = temp_dir("native-import-history-apply");
    let home = dir.join("home");
    let state_store = dir.join("state.sqlite");
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::create_dir_all(home.join(".claude/projects/private-workspace")).unwrap();
    fs::write(
        home.join(".codex/history.jsonl"),
        r#"{"timestamp":"2026-06-01T10:00:00Z","text":"codex-private-prompt"}"#,
    )
    .unwrap();
    fs::write(
        home.join(".claude/projects/private-workspace/session.jsonl"),
        r#"{"timestamp":"2026-06-04T10:00:00Z","response":"claude-private-response"}"#,
    )
    .unwrap();
    let config = write_config_text(
        &dir,
        &format!(
            r#"
server:
  bind: "127.0.0.1:0"
  state_store: "{}"
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
            state_store.display()
        ),
    );

    let output = Command::new(switchback_bin())
        .arg("--json")
        .arg("native")
        .arg("import-history")
        .arg("--apply")
        .arg("--config")
        .arg(&config)
        .env("HOME", &home)
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
    for forbidden in [
        "codex-private-prompt",
        "claude-private-response",
        "private-workspace",
        state_store.to_string_lossy().as_ref(),
    ] {
        assert!(
            !stdout.contains(forbidden),
            "{forbidden} leaked in {stdout}"
        );
    }

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("native import-history apply emits JSON");
    assert_eq!(value["schema"], "switchback/native-history-import@1");
    assert_eq!(value["dry_run"], serde_json::json!(false));
    assert_eq!(value["applied"], serde_json::json!(true));
    assert_eq!(value["read_only"], serde_json::json!(false));
    assert_eq!(
        value["storage"]["kind"],
        serde_json::json!("sqlite_state_store")
    );
    assert_eq!(
        value["storage"]["state_store_path_redacted"],
        serde_json::json!(true)
    );
    assert_eq!(
        value["storage"]["source_rows_written"],
        serde_json::json!(7)
    );
    assert_eq!(value["storage"]["stores_prompts"], serde_json::json!(false));
    assert_eq!(
        value["storage"]["stores_responses"],
        serde_json::json!(false)
    );
    assert_eq!(
        value["storage"]["stores_local_paths"],
        serde_json::json!(false)
    );
    let import_id = value["storage"]["import_id"]
        .as_str()
        .expect("apply should report import id");

    let store = sb_store::SqliteStore::open(&state_store.to_string_lossy()).unwrap();
    let imports = store.recent_native_history_imports(10).unwrap();
    assert_eq!(imports.len(), 1);
    assert_eq!(imports[0].import_id, import_id);
    assert!(imports[0].metadata_only);
    assert!(!imports[0].stores_prompts);
    assert!(!imports[0].stores_responses);
    assert!(!imports[0].stores_local_paths);

    let sources = store.native_history_sources(import_id).unwrap();
    assert_eq!(sources.len(), 7);
    assert!(sources
        .iter()
        .all(|source| source.path_id.starts_with("path-")));
    let stored_json = serde_json::to_string(&(imports, sources)).unwrap();
    for forbidden in [
        "codex-private-prompt",
        "claude-private-response",
        "private-workspace",
        home.to_string_lossy().as_ref(),
    ] {
        assert!(
            !stored_json.contains(forbidden),
            "{forbidden} leaked in {stored_json}"
        );
    }

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn native_import_history_requires_explicit_mode() {
    let output = Command::new(switchback_bin())
        .arg("--json")
        .arg("native")
        .arg("import-history")
        .env("RUST_LOG", "info")
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("requires exactly one of --dry-run or --apply"),
        "stderr={stderr}"
    );
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
    assert_eq!(value["status"], "partial_codex_and_claude_code_implemented");
    assert_eq!(value["relay_implemented"], serde_json::json!(false));
    assert_eq!(
        value["fixture_manifest"],
        "crates/sb-protocols/tests/fixtures/native-relay/manifest.json"
    );
    assert!(value["adapter_gate"]
        .as_str()
        .unwrap()
        .contains("HTTP Responses slice"));
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
        "x-api-key": "sk-capture-secret",
        "chatgpt-account-id": "acct-capture-secret"
      },
      "body": {
        "prompt": "ping",
        "access_token": "access-capture-secret",
        "account_id": "account-capture-secret",
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
    assert!(!stdout.contains("acct-capture-secret"), "{stdout}");
    assert!(!stdout.contains("account-capture-secret"), "{stdout}");

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("native relay capture should emit JSON");
    assert_eq!(value["schema"], "switchback/native-relay-capture@1");
    assert_eq!(value["ok"], serde_json::json!(true));
    assert_eq!(value["client"], "codex");
    assert_eq!(value["fixture"], "non_stream_request_response");
    assert!(value["redactions"].as_u64().unwrap() >= 6);

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
    assert!(
        !fixture_text.contains("acct-capture-secret"),
        "{fixture_text}"
    );
    assert!(
        !fixture_text.contains("account-capture-secret"),
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
    assert_eq!(
        written["capture"]["json"]["request"]["headers"]["chatgpt-account-id"],
        "<redacted>"
    );
    assert_eq!(
        written["capture"]["json"]["request"]["body"]["account_id"],
        "<redacted>"
    );

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn setup_native_relay_capture_filters_text_logs_to_protocol_lines() {
    let dir = temp_dir("setup-native-relay-capture-text");
    let raw = dir.join("raw.log");
    let fixture = dir.join("fixture.json");
    let home = dir.join("home");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        &raw,
        format!(
            "\
2026-06-04 [DEBUG] Loading settings from {}/.claude/settings.json
2026-06-04 [DEBUG] [API:auth] OAuth token check complete
2026-06-04 [DEBUG] [API:timing] dispatching to firstParty model=claude-opus-4
2026-06-04 [DEBUG] [API REQUEST] /v1/messages x-client-request-id=abc Authorization: Bearer text-secret-token
user email test@example.com
",
            home.display()
        ),
    )
    .unwrap();

    let output = Command::new(switchback_bin())
        .arg("--json")
        .arg("setup")
        .arg("native-relay")
        .arg("capture")
        .arg("--client")
        .arg("claude-code")
        .arg("--fixture")
        .arg("non_stream_request_response")
        .arg("--from-file")
        .arg(&raw)
        .arg("--out-file")
        .arg(&fixture)
        .env("HOME", &home)
        .env("RUST_LOG", "info")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let fixture_text = fs::read_to_string(&fixture).unwrap();
    assert!(!fixture_text.contains("settings.json"), "{fixture_text}");
    assert!(
        !fixture_text.contains("text-secret-token"),
        "{fixture_text}"
    );
    assert!(!fixture_text.contains("test@example.com"), "{fixture_text}");
    assert!(fixture_text.contains("[API:auth]"), "{fixture_text}");
    assert!(fixture_text.contains("/v1/messages"), "{fixture_text}");

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
    assert!(commands_json["commands"]
        .as_array()
        .unwrap()
        .iter()
        .any(|cmd| cmd["name"] == "native status"));
    assert!(commands_json["commands"]
        .as_array()
        .unwrap()
        .iter()
        .any(|cmd| cmd["name"] == "native import-history"));
    assert!(commands_json["commands"]
        .as_array()
        .unwrap()
        .iter()
        .any(|cmd| cmd["name"] == "lane doctor"));
    assert!(commands_json["commands"]
        .as_array()
        .unwrap()
        .iter()
        .any(|cmd| cmd["name"] == "lane audit codex-scout"));
    assert!(commands_json["commands"]
        .as_array()
        .unwrap()
        .iter()
        .any(|cmd| cmd["name"] == "lane install codex-scout"));

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
