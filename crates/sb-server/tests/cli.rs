use std::fs;
use std::io::Write;
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
