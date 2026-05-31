use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
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
