use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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

const CLAUDE_LANE_CFG: &str = r#"
server:
  bind: "127.0.0.1:0"
  retry:
    max_retries: 2
  circuit_breaker:
    enabled: true
    failure_threshold: 3
    open_secs: 30
providers:
  - id: primary
    type: mock
  - id: fallback
    type: mock
routes:
  - name: sol-canonical
    match:
      model: "openai/gpt-5.6-sol"
    targets:
      - "primary/gpt-5.6-sol"
      - "fallback/gpt-5.5"
  - name: sol-alias
    match:
      model: "gpt-5.6-sol"
    targets:
      - "primary/gpt-5.6-sol"
      - "fallback/gpt-5.5"
"#;

const LAUNCH_PROFILE_CFG: &str = r#"
server:
  bind: "127.0.0.1:0"
  retry:
    max_retries: 2
  circuit_breaker:
    enabled: true
    failure_threshold: 3
    open_secs: 30
providers:
  - id: primary
    type: mock
  - id: fallback
    type: mock
routes:
  - name: zai
    match:
      model: "zai/glm-5.2"
    targets:
      - "primary/glm-5.2"
      - "fallback/glm-5.2"
  - name: qwen
    match:
      model: "qwen/qwen3.8-max-preview"
    targets:
      - "primary/qwen3.8-max-preview"
      - "fallback/qwen3.8-max-preview"
"#;

const LAUNCH_PROFILE_AUTHORITY: &str = r#"{
  "schema": "switchback/launch-profiles@1",
  "provider_lanes": {
    "zai": {
      "route": "zai/glm-5.2",
      "requested_model": "glm-5.2",
      "transport": "headroom",
      "credential_ref": {"kind": "env", "name": "ZAI_API_KEY"},
      "anthropic_tap_port": 18772,
      "headroom_port": 8790,
      "claude_via_tap": true,
      "min_fallbacks": 1
    },
    "qwen": {
      "route": "qwen/qwen3.8-max-preview",
      "requested_model": "qwen3.8-max-preview",
      "transport": "headroom",
      "credential_ref": {"kind": "env", "name": "QWEN_API_KEY"},
      "anthropic_tap_port": 18777,
      "headroom_port": 8791,
      "claude_via_tap": true,
      "min_fallbacks": 1
    }
  },
  "harness_presets": {
    "claude-rich-zai": {
      "harness": "claude-code",
      "native_effort": "xhigh",
      "model_aliases": {
        "default": "glm-5.2[1m]",
        "opus": "glm-5.2[1m]",
        "sonnet": "glm-5.2[1m]",
        "haiku": "glm-4.5-air"
      },
      "compaction_window": 1000000,
      "permissions_mode": "inherit_allowlisted",
      "mcp_mode": "all",
      "skills_mode": "enabled",
      "settings_mode": "inherit_allowlisted",
      "launch_args": ["--rich"]
    },
    "claude-rich-qwen": {
      "harness": "claude-code",
      "native_effort": "xhigh",
      "model_aliases": {
        "default": "qwen3.8-max-preview",
        "opus": "qwen3.8-max-preview",
        "sonnet": "qwen3.7-plus",
        "haiku": "qwen3.6-flash",
        "subagent": "qwen3.7-max"
      },
      "compaction_window": 983616,
      "permissions_mode": "inherit_allowlisted",
      "mcp_mode": "all",
      "skills_mode": "enabled",
      "settings_mode": "inherit_allowlisted",
      "launch_args": ["--rich"]
    }
  },
  "capture_policies": {
    "observed": {"mode": "segmented_full_wire"}
  },
  "launch_profiles": {
    "claude-zai-full": {
      "provider_lane": "zai",
      "harness_preset": "claude-rich-zai",
      "capture_policy": "observed",
      "profile_label": "zai-lane",
      "wrappers": ["claude-zai-full"]
    },
    "claude-qwen": {
      "provider_lane": "qwen",
      "harness_preset": "claude-rich-qwen",
      "capture_policy": "observed",
      "profile_label": "qwen",
      "wrappers": ["claude-qwen"]
    }
  }
}"#;

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
  taps:
    - id: codex-tap
      bind: "127.0.0.1:19071"
      upstream: "https://chatgpt.com/backend-api/codex"
      capture_bodies: true
    - id: claude-tap
      bind: "127.0.0.1:19070"
      upstream: "https://api.anthropic.com"
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

const NATIVE_TAP_ONLY_CFG: &str = r#"
server:
  bind: "127.0.0.1:0"
  taps:
    - id: codex-tap
      bind: "127.0.0.1:19071"
      upstream: "https://chatgpt.com/backend-api/codex"
    - id: claude-tap
      bind: "127.0.0.1:19070"
      upstream: "https://api.anthropic.com"
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

fn claude_lane_define_command(config: &Path, lane_root: &Path, profile_root: &Path) -> Command {
    let mut command = Command::new(switchback_bin());
    command
        .arg("--json")
        .arg("lane")
        .arg("define")
        .arg("gpt56-sol-ultra")
        .arg("--model")
        .arg("openai/gpt-5.6-sol")
        .arg("--route")
        .arg("openai/gpt-5.6-sol")
        .arg("--alias")
        .arg("gpt-5.6-sol")
        .arg("--transport")
        .arg("headroom")
        .arg("--effort")
        .arg("ultra")
        .arg("--anthropic-port")
        .arg("8788")
        .arg("--profile-label")
        .arg("gpt56-sol-ultra")
        .arg("--display-name")
        .arg("GPT-5.6 Sol Ultra")
        .arg("--min-fallbacks")
        .arg("1")
        .arg("--lane-root")
        .arg(lane_root)
        .arg("--profile-root")
        .arg(profile_root)
        .arg("--config")
        .arg(config);
    command
}

fn claude_lane_audit_command(config: &Path, lane_root: &Path, profile_root: &Path) -> Command {
    let mut command = Command::new(switchback_bin());
    command
        .arg("--json")
        .arg("lane")
        .arg("audit")
        .arg("claude-profile")
        .arg("gpt56-sol-ultra")
        .arg("--lane-root")
        .arg(lane_root)
        .arg("--profile-root")
        .arg(profile_root)
        .arg("--config")
        .arg(config);
    command
}

struct LaunchProfileTestPaths<'a> {
    config: &'a Path,
    authority: &'a Path,
    lane_root: &'a Path,
    profile_root: &'a Path,
    wrapper_root: &'a Path,
    projection_root: &'a Path,
}

fn launch_profile_test_paths<'a>(
    config: &'a Path,
    authority: &'a Path,
    lane_root: &'a Path,
    profile_root: &'a Path,
    wrapper_root: &'a Path,
    projection_root: &'a Path,
) -> LaunchProfileTestPaths<'a> {
    LaunchProfileTestPaths {
        config,
        authority,
        lane_root,
        profile_root,
        wrapper_root,
        projection_root,
    }
}

fn launch_profile_command(
    action: &str,
    name: Option<&str>,
    paths: &LaunchProfileTestPaths<'_>,
) -> Command {
    let mut command = Command::new(switchback_bin());
    command.arg("--json").arg("profile").arg(action);
    if let Some(name) = name {
        command.arg(name);
    }
    command
        .arg("--authority")
        .arg(paths.authority)
        .arg("--lane-root")
        .arg(paths.lane_root)
        .arg("--profile-root")
        .arg(paths.profile_root)
        .arg("--wrapper-root")
        .arg(paths.wrapper_root)
        .arg("--projection-root")
        .arg(paths.projection_root)
        .arg("--config")
        .arg(paths.config);
    command
}

#[test]
fn native_accounts_dry_run_is_json_and_creates_no_state() {
    let dir = temp_dir("native-accounts-dry-run");
    let home = dir.join("home");
    let state = dir.join("state");
    fs::create_dir_all(&home).unwrap();
    let output = Command::new(switchback_bin())
        .args(["--json", "native", "accounts", "reconcile", "--dry-run"])
        .env("HOME", &home)
        .env("SWITCHBACK_STATE_DIR", &state)
        .env("SB_AUTHREG", home.join("authreg"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let body: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(body["schema"], "switchback/native-accounts-reconcile@1");
    assert_eq!(body["base_revision"], 0);
    assert!(!state.exists(), "dry-run created state directory");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn native_accounts_apply_resolve_and_stale_merge_are_safe() {
    let dir = temp_dir("native-accounts-flow");
    let home = dir.join("home");
    let state = dir.join("state");
    let authreg = home.join(".config/switchback/codex-auth");
    let multi = home.join(".codex/multi-auth");
    let codexbar = home.join("Library/Application Support/CodexBar");
    for path in [&authreg, &multi, &codexbar] {
        fs::create_dir_all(path).unwrap();
    }
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../sb-credentials/tests/fixtures/provider-accounts");
    let copies = [
        ("codex-auth/active.json", home.join(".codex/auth.json")),
        (
            "switchback-auth-registry/default.json",
            authreg.join("default.json"),
        ),
        (
            "switchback-auth-registry/work.json",
            authreg.join("work.json"),
        ),
        (
            "switchback-auth-registry/active.txt",
            authreg.join(".active"),
        ),
        ("switchback-auth-registry/runs.tsv", authreg.join(".runs")),
        (
            "codex-multi-auth/accounts.json",
            multi.join("openai-codex-accounts.json"),
        ),
        (
            "codex-multi-auth/quota-cache.json",
            multi.join("quota-cache.json"),
        ),
        (
            "codexbar/usage-history.jsonl",
            codexbar.join("usage-history.jsonl"),
        ),
    ];
    for (source, target) in &copies {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).unwrap()
        }
        fs::copy(fixtures.join(source), target).unwrap();
    }
    for path in [
        home.join(".codex/auth.json"),
        authreg.join("default.json"),
        authreg.join("work.json"),
        multi.join("openai-codex-accounts.json"),
    ] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let run = |args: &[&str]| {
        Command::new(switchback_bin())
            .args(args)
            .env("HOME", &home)
            .env("SWITCHBACK_STATE_DIR", &state)
            .env("SB_AUTHREG", &authreg)
            .env("RUST_LOG", "off")
            .output()
            .unwrap()
    };
    let first = run(&["--json", "native", "accounts", "reconcile"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_json: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(first_json["revision"], 1);
    assert_eq!(first_json["changed"], true);
    let evidence_before: Vec<_> = copies.iter().map(|(_, p)| file_evidence_cli(p)).collect();
    let second = run(&["--json", "native", "accounts", "reconcile"]);
    assert!(second.status.success());
    let second_json: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(second_json["revision"], 1);
    assert_eq!(second_json["changed"], false);
    let resolve = run(&[
        "--json",
        "native",
        "accounts",
        "resolve",
        "--provider",
        "openai",
        "--client",
        "codex",
        "--alias-scheme",
        "label",
        "--alias-value",
        "default",
        "--expected-revision",
        "1",
    ]);
    assert!(
        resolve.status.success(),
        "{}",
        String::from_utf8_lossy(&resolve.stderr)
    );
    let resolution: serde_json::Value = serde_json::from_slice(&resolve.stdout).unwrap();
    assert_eq!(resolution["credential_pointer"]["slot"], "default");
    let list = run(&["--json", "native", "accounts", "list"]);
    assert!(list.status.success());
    let dry = run(&["--json", "native", "accounts", "reconcile", "--dry-run"]);
    assert!(dry.status.success());
    let evidence_after: Vec<_> = copies.iter().map(|(_, p)| file_evidence_cli(p)).collect();
    assert_eq!(
        evidence_before, evidence_after,
        "read-only commands changed source files"
    );
    let emitted = format!(
        "{}{}{}{}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&resolve.stdout),
        String::from_utf8_lossy(&list.stdout),
        String::from_utf8_lossy(&dry.stdout)
    );
    for forbidden in [
        "synthetic-default-access",
        "alpha@example.test",
        home.to_string_lossy().as_ref(),
        "access_token",
        "refresh_token",
        "id_token",
    ] {
        assert!(!emitted.contains(forbidden), "unsafe output: {forbidden}")
    }
    let accounts = first_json["snapshot"]["accounts"].as_array().unwrap();
    let enrolled: Vec<_> = accounts
        .iter()
        .filter(|a| a["state"] == "enrolled")
        .collect();
    let from = enrolled[0]["id"].as_str().unwrap();
    let into = enrolled[1]["id"].as_str().unwrap();
    let stale = run(&[
        "--json",
        "native",
        "accounts",
        "merge",
        "--from",
        from,
        "--into",
        into,
        "--expected-revision",
        "0",
    ]);
    assert!(!stale.status.success());
    let after_stale = run(&["--json", "native", "accounts", "list"]);
    let after_stale_json: serde_json::Value = serde_json::from_slice(&after_stale.stdout).unwrap();
    assert_eq!(after_stale_json["authority"]["revision"], 1);
    let missing_expected = run(&[
        "--json", "native", "accounts", "merge", "--from", from, "--into", into,
    ]);
    assert!(!missing_expected.status.success());
    fs::remove_dir_all(dir).unwrap();
}

fn file_evidence_cli(path: &Path) -> (u64, u64, i64, u32, String) {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(path).unwrap();
    let digest = Sha256::digest(fs::read(path).unwrap());
    (
        meta.ino(),
        meta.len(),
        meta.mtime(),
        meta.permissions().mode(),
        format!("{digest:x}"),
    )
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

struct ServeChild {
    child: Child,
}

impl Drop for ServeChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_bind_addr() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    addr.to_string()
}

fn wait_for_health(base: &str) {
    let started = std::time::Instant::now();
    loop {
        if let Ok((status, _body)) = http_json("GET", &format!("{base}/health"), None) {
            if status == 200 {
                return;
            }
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "switchback serve did not become healthy"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn http_json(
    method: &str,
    url: &str,
    body: Option<&serde_json::Value>,
) -> std::io::Result<(u16, serde_json::Value)> {
    let without_scheme = url.strip_prefix("http://").unwrap();
    let (host_port, path) = without_scheme.split_once('/').unwrap();
    let mut stream = TcpStream::connect(host_port)?;
    let body_string = body.map(serde_json::Value::to_string).unwrap_or_default();
    let request = format!(
        "{method} /{path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body_string.len(),
        body_string
    );
    stream.write_all(request.as_bytes())?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0);
    let json = if body.trim().is_empty() {
        serde_json::json!(null)
    } else {
        serde_json::from_str(body.trim()).unwrap()
    };
    Ok((status, json))
}

fn eval_activation_config(bind: &str, store: &Path) -> String {
    format!(
        r#"
server:
  bind: "{bind}"
  state_store: "{}"
providers:
  - id: mock
    type: mock
harnesses:
  - name: codex-cli
    version: "contract/v1"
    capabilities:
      artifacts: true
      latency_metadata: true
    supported_task_types: [coding]
    required_tools: ["shell"]
    input_contract: "execution-job/v1"
    output_contract: "harness-run-summary/v1"
routes:
  - name: default
    match:
      model: "*"
    targets:
      - "mock/echo"
"#,
        store.display()
    )
}

fn eval_snapshot_for_activation(runs: u64, generated_at_ms: u64) -> sb_eval::EvalEvidenceSnapshot {
    let passing = sb_eval::EvalSignalBreakdown {
        evaluated_count: runs,
        pass_count: runs,
        coverage_rate: Some(1.0),
        success_rate: Some(1.0),
        inconclusive_rate: Some(0.0),
        ..Default::default()
    };
    sb_eval::EvalEvidenceSnapshot::from_report(
        &sb_eval::EvalReportQuery {
            task_type: Some(sb_core::ExecutionTaskType::Coding),
            min_runs: 1,
            group_by_harness_version: true,
            ..Default::default()
        },
        sb_eval::EvalReport {
            rows: vec![sb_eval::EvalReportRow {
                harness: "codex-cli".to_string(),
                harness_version: Some("1.0.0".to_string()),
                strategy_id: Some("default".to_string()),
                runs,
                distinct_cases: runs,
                pass_count: runs,
                correctness_evaluated_count: runs,
                mechanical: passing.clone(),
                correctness: passing,
                success_rate: Some(1.0),
                median_latency_ms: Some(1_000 + runs),
                median_cost_micros: Some(10_000 + runs),
                ..Default::default()
            }],
        },
        generated_at_ms,
    )
}

fn workspace_file(relative: &str) -> PathBuf {
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.pop();
    root.pop();
    root.join(relative)
}

fn contains_forbidden_eval_body_key(value: &serde_json::Value) -> bool {
    const FORBIDDEN: &[&str] = &[
        "raw_prompt",
        "prompt",
        "raw_response",
        "response",
        "stdout",
        "stderr",
        "raw_log",
        "log",
        "raw_diff",
        "diff",
        "secret",
        "token",
        "api_key",
        "password",
    ];
    match value {
        serde_json::Value::Object(map) => map.iter().any(|(key, value)| {
            FORBIDDEN.contains(&key.as_str()) || contains_forbidden_eval_body_key(value)
        }),
        serde_json::Value::Array(values) => values.iter().any(contains_forbidden_eval_body_key),
        _ => false,
    }
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

fn write_eval_case(dir: &Path) -> PathBuf {
    let path = dir.join("react-bug-001.case.json");
    fs::write(
        &path,
        r#"{
  "schema_version": "switchback.eval.case/v1",
  "case_id": "react-bug-001",
  "case_revision": "rev-1",
  "task_type": "coding",
  "privacy_level": "standard",
  "tags": ["react"],
  "fixture": {
    "kind": "git_repo",
    "uri": "https://example.invalid/repo.git",
    "revision": "abc123",
    "fingerprint": "fixture-sha"
  },
  "prompt_ref": {
    "kind": "sha256",
    "reference": "prompt-sha",
    "sha256": "prompt-sha"
  },
  "success_criteria": [
    {
      "id": "tests",
      "kind": "tests_pass",
      "required": true,
      "params": {}
    }
  ],
  "commands": [],
  "allowed_paths": ["src/**"],
  "forbidden_paths": [".env"]
}"#,
    )
    .unwrap();
    path
}

fn write_eval_run(dir: &Path, filename: &str, artifact_metadata: &str) -> PathBuf {
    let path = dir.join(filename);
    fs::write(
        &path,
        format!(
            r#"{{
  "schema_version": "switchback.eval.run/v1",
  "source_run_id": "codex-react-001",
  "case_id": "react-bug-001",
  "case_revision": "rev-1",
  "harness": "codex-cli",
  "harness_version": "1.0.0",
  "strategy_id": "default",
  "strategy_version": "v1",
  "started_at_ms": 1000,
  "finished_at_ms": 3000,
"status": "succeeded",
"outcome": {{
"source": "mechanical_check",
"verdict": "pass",
"confidence": 0.9,
    "checks": [],
    "evidence": []
  }},
  "metrics": [
    {{
      "name": "latency_ms",
      "value": 2000,
      "unit": "ms",
      "source": "harness"
    }},
    {{
      "name": "cost_micros",
      "value": 42000,
      "unit": "micros_usd",
      "source": "harness"
    }}
  ],
  "artifacts": [
    {{
      "kind": "trace",
      "reference": "trace:codex-react-001",
      "sha256": "trace-sha",
      "privacy_level": "standard",
      "metadata": {artifact_metadata}
    }}
  ],
  "retry_count": 0,
  "cache_status": "hit"
}}"#
        ),
    )
    .unwrap();
    path
}

fn write_eval_judge_result(dir: &Path, filename: &str, extra: &str) -> PathBuf {
    let path = dir.join(filename);
    fs::write(
        &path,
        format!(
            r#"{{
  "schema_version": "switchback.eval.judge/v1",
  "check_id": "llm-judge:react-rubric:v1",
  "verdict": "fail",
  "confidence": 0.72,
  "rubric_id": "react-rubric",
  "rubric_version": "v1",
  "model_id": "auto/judge",
  "prompt_template_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "message": "missed a regression requirement",
  "evidence_refs": [
    {{
      "kind": "summary",
      "reference": "artifact:judge-summary-sha"
    }}
  ]{extra}
}}"#
        ),
    )
    .unwrap();
    path
}

fn write_codex_converter_input(dir: &Path) -> PathBuf {
    let path = dir.join("codex-result.json");
    fs::write(
        &path,
        r#"{
  "session_id": "codex-session-1",
  "status": "succeeded",
  "version": "0.12.3",
  "duration_ms": 3210,
  "total_cost_usd": 0.0123,
  "artifacts": [
    {
      "kind": "trace",
      "reference": "trace:codex-session-1",
      "sha256": "trace-sha",
      "privacy_level": "standard",
      "metadata": { "trace_id": "codex-session-1" }
    }
  ]
}"#,
    )
    .unwrap();
    path
}

#[test]
fn eval_cli_converts_codex_result_to_run_manifest() {
    let dir = temp_dir("eval-cli-convert");
    let input = write_codex_converter_input(&dir);

    let output = Command::new(switchback_bin())
        .arg("--json")
        .arg("eval")
        .arg("convert")
        .arg("codex-cli")
        .arg("--input")
        .arg(&input)
        .arg("--case-id")
        .arg("react-bug-001")
        .arg("--case-revision")
        .arg("rev-1")
        .arg("--strategy-id")
        .arg("default")
        .arg("--verdict")
        .arg("pass")
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
        serde_json::from_slice(&output.stdout).expect("eval convert emits run JSON");
    assert_eq!(value["schema_version"], "switchback.eval.run/v1");
    assert_eq!(value["harness"], "codex-cli");
    assert_eq!(value["harness_version"], "0.12.3");
    assert_eq!(value["source_run_id"], "codex-session-1");
    assert_eq!(value["outcome"]["verdict"], "pass");
    assert_eq!(value["metrics"][0]["name"], "latency_ms");

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn eval_cli_real_data_sanity_converts_ingests_and_snapshots_three_harnesses() {
    let dir = temp_dir("eval-real-data-sanity");
    let store = dir.join("eval.sqlite");
    let case = workspace_file("examples/eval/real-data-sanity/case.json");
    let import = Command::new(switchback_bin())
        .arg("--json")
        .arg("eval")
        .arg("--store")
        .arg(&store)
        .arg("case")
        .arg("import")
        .arg(&case)
        .output()
        .unwrap();
    assert!(
        import.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        import.status.code(),
        String::from_utf8_lossy(&import.stdout),
        String::from_utf8_lossy(&import.stderr)
    );

    let conversions = [
        (
            "codex-cli",
            "examples/eval/real-data-sanity/inputs/codex-cli.json",
            "default",
            "codex-cli.run.json",
            "pass",
            "mechanical_check",
            14_200,
        ),
        (
            "claude-code",
            "examples/eval/real-data-sanity/inputs/claude-code.json",
            "review-repair",
            "claude-code.run.json",
            "pass",
            "llm_judge",
            31_400,
        ),
        (
            "aider",
            "examples/eval/real-data-sanity/inputs/aider.json",
            "default",
            "aider.run.json",
            "fail",
            "llm_judge",
            4_200,
        ),
    ];

    for (kind, input, strategy, output_name, verdict, source, cost_micros) in conversions {
        let output_path = dir.join(output_name);
        let convert = Command::new(switchback_bin())
            .arg("--json")
            .arg("eval")
            .arg("convert")
            .arg(kind)
            .arg("--input")
            .arg(workspace_file(input))
            .arg("--case-id")
            .arg("real-data-sanity-001")
            .arg("--case-revision")
            .arg("rev-1")
            .arg("--strategy-id")
            .arg(strategy)
            .arg("--output")
            .arg(&output_path)
            .output()
            .unwrap();
        assert!(
            convert.status.success(),
            "kind={kind} status={:?}\nstdout={}\nstderr={}",
            convert.status.code(),
            String::from_utf8_lossy(&convert.stdout),
            String::from_utf8_lossy(&convert.stderr)
        );
        let run_json: serde_json::Value =
            serde_json::from_slice(&fs::read(&output_path).unwrap()).unwrap();
        assert_eq!(run_json["schema_version"], "switchback.eval.run/v1");
        assert_eq!(run_json["harness"], kind);
        assert_eq!(run_json["outcome"]["verdict"], verdict);
        assert_eq!(run_json["outcome"]["source"], source);
        assert_eq!(run_json["metrics"][1]["name"], "cost_micros");
        assert_eq!(
            run_json["metrics"][1]["value"].as_f64(),
            Some(cost_micros as f64)
        );
        assert!(!contains_forbidden_eval_body_key(&run_json));

        let ingest = Command::new(switchback_bin())
            .arg("--json")
            .arg("eval")
            .arg("--store")
            .arg(&store)
            .arg("ingest")
            .arg("--result")
            .arg(&output_path)
            .output()
            .unwrap();
        assert!(
            ingest.status.success(),
            "kind={kind} status={:?}\nstdout={}\nstderr={}",
            ingest.status.code(),
            String::from_utf8_lossy(&ingest.stdout),
            String::from_utf8_lossy(&ingest.stderr)
        );
    }

    let report = Command::new(switchback_bin())
        .arg("--json")
        .arg("eval")
        .arg("--store")
        .arg(&store)
        .arg("report")
        .arg("--by")
        .arg("harness,harness_version")
        .arg("--task-type")
        .arg("coding")
        .arg("--tag")
        .arg("real_data_sanity")
        .arg("--min-runs")
        .arg("1")
        .output()
        .unwrap();
    assert!(
        report.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        report.status.code(),
        String::from_utf8_lossy(&report.stdout),
        String::from_utf8_lossy(&report.stderr)
    );
    let report_json: serde_json::Value =
        serde_json::from_slice(&report.stdout).expect("eval report emits JSON");
    let rows = report_json["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().any(|row| row["harness"] == "codex-cli"
        && row["success_rate"] == 1.0
        && row["mechanical"]["success_rate"] == 1.0
        && row["median_cost_micros"] == 14_200));
    assert!(rows.iter().any(|row| row["harness"] == "claude-code"
        && row["success_rate"] == 1.0
        && row["llm_judge"]["success_rate"] == 1.0
        && row["median_cost_micros"] == 31_400));
    assert!(rows.iter().any(|row| row["harness"] == "aider"
        && row["success_rate"] == 0.0
        && row["llm_judge"]["success_rate"] == 0.0
        && row["median_cost_micros"] == 4_200));

    let snapshot_path = dir.join("snapshot.json");
    let snapshot = Command::new(switchback_bin())
        .arg("--json")
        .arg("eval")
        .arg("--store")
        .arg(&store)
        .arg("snapshot")
        .arg("build")
        .arg("--by")
        .arg("harness,harness_version")
        .arg("--task-type")
        .arg("coding")
        .arg("--tag")
        .arg("real_data_sanity")
        .arg("--min-runs")
        .arg("1")
        .arg("--generated-at-ms")
        .arg("70000")
        .arg("--output")
        .arg(&snapshot_path)
        .output()
        .unwrap();
    assert!(
        snapshot.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        snapshot.status.code(),
        String::from_utf8_lossy(&snapshot.stdout),
        String::from_utf8_lossy(&snapshot.stderr)
    );
    let snapshot_json: serde_json::Value =
        serde_json::from_slice(&snapshot.stdout).expect("eval snapshot emits JSON");
    assert_eq!(snapshot_json["rows"].as_array().unwrap().len(), 3);
    assert!(snapshot_json["rows"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["preview_eligible"] == false));

    let publish = Command::new(switchback_bin())
        .arg("--json")
        .arg("eval")
        .arg("--store")
        .arg(&store)
        .arg("snapshot")
        .arg("publish")
        .arg("--snapshot")
        .arg(&snapshot_path)
        .arg("--name")
        .arg("current")
        .output()
        .unwrap();
    assert!(
        publish.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        publish.status.code(),
        String::from_utf8_lossy(&publish.stdout),
        String::from_utf8_lossy(&publish.stderr)
    );

    let current = Command::new(switchback_bin())
        .arg("--json")
        .arg("eval")
        .arg("--store")
        .arg(&store)
        .arg("snapshot")
        .arg("current")
        .arg("--name")
        .arg("current")
        .output()
        .unwrap();
    assert!(
        current.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        current.status.code(),
        String::from_utf8_lossy(&current.stdout),
        String::from_utf8_lossy(&current.stderr)
    );

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn eval_published_snapshot_activation_is_startup_and_reload_bounded() {
    let dir = temp_dir("eval-activation");
    let store_path = dir.join("eval.sqlite");
    let config_path = dir.join("switchback.yaml");
    let bind = free_bind_addr();
    fs::write(
        &config_path,
        eval_activation_config(&bind, store_path.as_path()),
    )
    .unwrap();

    let store = sb_store::SqliteStore::open(&store_path.to_string_lossy()).unwrap();
    let first = eval_snapshot_for_activation(1, 70_000);
    let first_id = first.snapshot_id.clone();
    store
        .publish_eval_evidence_snapshot("current", &first)
        .unwrap();

    let child = Command::new(switchback_bin())
        .arg("serve")
        .arg("--config")
        .arg(&config_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _serve = ServeChild { child };
    let base = format!("http://{bind}");
    wait_for_health(&base);

    let (status, current) =
        http_json("GET", &format!("{base}/cp/v1/eval/snapshots/current"), None).unwrap();
    assert_eq!(status, 200);
    assert_eq!(current["metadata"]["snapshot_id"], first_id);
    assert_eq!(current["metadata"]["pinned"], true);
    assert_eq!(current["spec"]["snapshot_id"], first_id);

    let preview_body = serde_json::json!({
        "model": "auto/coding",
        "messages": [{"role": "user", "content": "preview only"}]
    });
    let (status, preview) = http_json(
        "POST",
        &format!("{base}/cp/v1/route-preview"),
        Some(&preview_body),
    )
    .unwrap();
    assert_eq!(status, 200);
    assert_eq!(preview["eval_evidence_snapshot_id"], first_id);

    let second = eval_snapshot_for_activation(2, 80_000);
    let second_id = second.snapshot_id.clone();
    assert_ne!(first_id, second_id);
    store
        .publish_eval_evidence_snapshot("current", &second)
        .unwrap();

    let (status, list) = http_json("GET", &format!("{base}/cp/v1/eval/snapshots"), None).unwrap();
    assert_eq!(status, 200);
    assert_eq!(list["pinned_snapshot_id"], first_id);
    assert_eq!(list["items"][0]["snapshot_id"], second_id);
    assert_eq!(list["items"][0]["pinned"], false);

    let (status, current) =
        http_json("GET", &format!("{base}/cp/v1/eval/snapshots/current"), None).unwrap();
    assert_eq!(status, 200);
    assert_eq!(
        current["metadata"]["snapshot_id"], first_id,
        "current endpoint must report the pinned snapshot, not a newly published inactive one"
    );
    assert_eq!(current["spec"]["snapshot_id"], first_id);

    let (status, reload) = http_json("POST", &format!("{base}/v1/reload"), None).unwrap();
    assert_eq!(status, 200);
    assert_eq!(reload["ok"], true);
    assert_eq!(reload["eval_evidence_snapshot_id"], second_id);

    let (status, current) =
        http_json("GET", &format!("{base}/cp/v1/eval/snapshots/current"), None).unwrap();
    assert_eq!(status, 200);
    assert_eq!(current["metadata"]["snapshot_id"], second_id);
    assert_eq!(current["metadata"]["pinned"], true);
    assert_eq!(current["spec"]["snapshot_id"], second_id);

    let (status, preview) = http_json(
        "POST",
        &format!("{base}/cp/v1/route-preview"),
        Some(&preview_body),
    )
    .unwrap();
    assert_eq!(status, 200);
    assert_eq!(preview["eval_evidence_snapshot_id"], second_id);

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn eval_cli_validates_ingests_and_reports_harness_evidence() {
    let dir = temp_dir("eval-cli");
    let store = dir.join("eval.sqlite");
    let case = write_eval_case(&dir);
    let run = write_eval_run(&dir, "codex-react-001.run.json", "{}");

    let validate = Command::new(switchback_bin())
        .arg("--json")
        .arg("eval")
        .arg("--store")
        .arg(&store)
        .arg("case")
        .arg("validate")
        .arg(&case)
        .output()
        .unwrap();
    assert!(
        validate.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        validate.status.code(),
        String::from_utf8_lossy(&validate.stdout),
        String::from_utf8_lossy(&validate.stderr)
    );
    let validate_json: serde_json::Value =
        serde_json::from_slice(&validate.stdout).expect("eval case validate emits JSON");
    assert_eq!(validate_json["ok"], serde_json::json!(true));
    assert_eq!(validate_json["case_id"], "react-bug-001");

    let ingest = Command::new(switchback_bin())
        .arg("--json")
        .arg("eval")
        .arg("--store")
        .arg(&store)
        .arg("ingest")
        .arg("--case")
        .arg(&case)
        .arg("--result")
        .arg(&run)
        .output()
        .unwrap();
    assert!(
        ingest.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        ingest.status.code(),
        String::from_utf8_lossy(&ingest.stdout),
        String::from_utf8_lossy(&ingest.stderr)
    );
    let ingest_json: serde_json::Value =
        serde_json::from_slice(&ingest.stdout).expect("eval ingest emits JSON");
    assert_eq!(ingest_json["ok"], serde_json::json!(true));
    assert_eq!(ingest_json["inserted"], serde_json::json!(true));
    assert_eq!(ingest_json["harness"], "codex-cli");

    let report = Command::new(switchback_bin())
        .arg("--json")
        .arg("eval")
        .arg("--store")
        .arg(&store)
        .arg("report")
        .arg("--by")
        .arg("harness")
        .arg("--task-type")
        .arg("coding")
        .arg("--tag")
        .arg("react")
        .arg("--min-runs")
        .arg("1")
        .output()
        .unwrap();
    assert!(
        report.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        report.status.code(),
        String::from_utf8_lossy(&report.stdout),
        String::from_utf8_lossy(&report.stderr)
    );
    let report_json: serde_json::Value =
        serde_json::from_slice(&report.stdout).expect("eval report emits JSON");
    assert_eq!(report_json["schema"], "switchback.eval.report/v1");
    let row = &report_json["rows"][0];
    assert_eq!(row["harness"], "codex-cli");
    assert_eq!(row["runs"], serde_json::json!(1));
    assert_eq!(row["pass_count"], serde_json::json!(1));
    assert_eq!(row["correctness_evaluated_count"], serde_json::json!(1));
    assert_eq!(row["mechanical"]["pass_count"], serde_json::json!(1));
    assert_eq!(row["median_latency_ms"], serde_json::json!(2000));
    assert_eq!(row["median_cost_micros"], serde_json::json!(42000));

    let snapshot_path = dir.join("eval-snapshot.json");
    let snapshot = Command::new(switchback_bin())
        .arg("--json")
        .arg("eval")
        .arg("--store")
        .arg(&store)
        .arg("snapshot")
        .arg("build")
        .arg("--by")
        .arg("harness")
        .arg("--task-type")
        .arg("coding")
        .arg("--tag")
        .arg("react")
        .arg("--min-runs")
        .arg("1")
        .arg("--output")
        .arg(&snapshot_path)
        .output()
        .unwrap();
    assert!(
        snapshot.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        snapshot.status.code(),
        String::from_utf8_lossy(&snapshot.stdout),
        String::from_utf8_lossy(&snapshot.stderr)
    );
    let snapshot_stdout: serde_json::Value =
        serde_json::from_slice(&snapshot.stdout).expect("eval snapshot emits JSON");
    assert_eq!(
        snapshot_stdout["schema_version"],
        "switchback.eval.evidence_snapshot/v1"
    );
    assert_eq!(snapshot_stdout["rows"][0]["harness"], "codex-cli");
    let snapshot_file: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&snapshot_path).expect("snapshot output file is written"),
    )
    .expect("snapshot output file contains JSON");
    assert_eq!(snapshot_file["snapshot_id"], snapshot_stdout["snapshot_id"]);
    let publish = Command::new(switchback_bin())
        .arg("--json")
        .arg("eval")
        .arg("--store")
        .arg(&store)
        .arg("snapshot")
        .arg("publish")
        .arg("--snapshot")
        .arg(&snapshot_path)
        .arg("--name")
        .arg("current")
        .output()
        .unwrap();
    assert!(
        publish.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        publish.status.code(),
        String::from_utf8_lossy(&publish.stdout),
        String::from_utf8_lossy(&publish.stderr)
    );
    let publish_json: serde_json::Value =
        serde_json::from_slice(&publish.stdout).expect("eval snapshot publish emits JSON");
    assert_eq!(publish_json["name"], "current");
    assert_eq!(publish_json["snapshot_id"], snapshot_stdout["snapshot_id"]);
    let current = Command::new(switchback_bin())
        .arg("--json")
        .arg("eval")
        .arg("--store")
        .arg(&store)
        .arg("snapshot")
        .arg("current")
        .arg("--name")
        .arg("current")
        .output()
        .unwrap();
    assert!(
        current.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        current.status.code(),
        String::from_utf8_lossy(&current.stdout),
        String::from_utf8_lossy(&current.stderr)
    );
    let current_json: serde_json::Value =
        serde_json::from_slice(&current.stdout).expect("eval snapshot current emits JSON");
    assert_eq!(current_json["snapshot_id"], snapshot_stdout["snapshot_id"]);
    assert_eq!(current_json["rows"], serde_json::json!(1));

    let filtered = Command::new(switchback_bin())
        .arg("--json")
        .arg("eval")
        .arg("--store")
        .arg(&store)
        .arg("report")
        .arg("--by")
        .arg("harness,strategy,harness_version")
        .arg("--strategy-id")
        .arg("default")
        .arg("--harness-version")
        .arg("1.0.0")
        .arg("--since-ms")
        .arg("1000")
        .arg("--until-ms")
        .arg("3000")
        .output()
        .unwrap();
    assert!(
        filtered.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        filtered.status.code(),
        String::from_utf8_lossy(&filtered.stdout),
        String::from_utf8_lossy(&filtered.stderr)
    );
    let filtered_json: serde_json::Value =
        serde_json::from_slice(&filtered.stdout).expect("filtered eval report emits JSON");
    assert_eq!(filtered_json["rows"][0]["strategy_id"], "default");
    assert_eq!(filtered_json["rows"][0]["harness_version"], "1.0.0");

    let no_cache_hits = Command::new(switchback_bin())
        .arg("--json")
        .arg("eval")
        .arg("--store")
        .arg(&store)
        .arg("report")
        .arg("--by")
        .arg("harness")
        .arg("--exclude-cache-hits")
        .output()
        .unwrap();
    assert!(no_cache_hits.status.success());
    let no_cache_json: serde_json::Value =
        serde_json::from_slice(&no_cache_hits.stdout).expect("cache-filtered report emits JSON");
    assert!(no_cache_json["rows"].as_array().unwrap().is_empty());

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn eval_cli_imports_llm_judge_result_onto_existing_run() {
    let dir = temp_dir("eval-cli-judge-import");
    let store = dir.join("eval.sqlite");
    let case = write_eval_case(&dir);
    let run = write_eval_run(&dir, "codex.run.json", "{}");
    let judge = write_eval_judge_result(&dir, "judge.result.json", "");

    let ingest = Command::new(switchback_bin())
        .arg("--json")
        .arg("eval")
        .arg("--store")
        .arg(&store)
        .arg("ingest")
        .arg("--case")
        .arg(&case)
        .arg("--result")
        .arg(&run)
        .output()
        .unwrap();
    assert!(
        ingest.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        ingest.status.code(),
        String::from_utf8_lossy(&ingest.stdout),
        String::from_utf8_lossy(&ingest.stderr)
    );
    let ingest_json: serde_json::Value =
        serde_json::from_slice(&ingest.stdout).expect("eval ingest emits JSON");
    let run_id = ingest_json["run_id"].as_str().unwrap();

    let import = Command::new(switchback_bin())
        .arg("--json")
        .arg("eval")
        .arg("--store")
        .arg(&store)
        .arg("judge")
        .arg("import")
        .arg("--run-id")
        .arg(run_id)
        .arg("--result")
        .arg(&judge)
        .output()
        .unwrap();
    assert!(
        import.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        import.status.code(),
        String::from_utf8_lossy(&import.stdout),
        String::from_utf8_lossy(&import.stderr)
    );
    let import_json: serde_json::Value =
        serde_json::from_slice(&import.stdout).expect("judge import emits JSON");
    assert_eq!(import_json["ok"], serde_json::json!(true));
    assert_eq!(import_json["run_id"], run_id);
    assert_eq!(import_json["check_id"], "llm-judge:react-rubric:v1");
    assert_eq!(import_json["inserted"], serde_json::json!(true));

    let report = Command::new(switchback_bin())
        .arg("--json")
        .arg("eval")
        .arg("--store")
        .arg(&store)
        .arg("report")
        .arg("--by")
        .arg("harness")
        .arg("--task-type")
        .arg("coding")
        .arg("--tag")
        .arg("react")
        .arg("--min-runs")
        .arg("1")
        .output()
        .unwrap();
    assert!(
        report.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        report.status.code(),
        String::from_utf8_lossy(&report.stdout),
        String::from_utf8_lossy(&report.stderr)
    );
    let report_json: serde_json::Value =
        serde_json::from_slice(&report.stdout).expect("eval report emits JSON");
    let row = &report_json["rows"][0];
    assert_eq!(row["mechanical"]["pass_count"], serde_json::json!(1));
    assert_eq!(row["llm_judge"]["fail_count"], serde_json::json!(1));
    assert_eq!(row["delivery"]["evaluated_count"], serde_json::json!(0));
    assert_eq!(row["correctness"]["pass_count"], serde_json::json!(1));

    let unsafe_judge = write_eval_judge_result(
        &dir,
        "unsafe.judge.result.json",
        r#",
  "prompt": "raw prompt",
  "response": "raw response""#,
    );
    let unsafe_import = Command::new(switchback_bin())
        .arg("--json")
        .arg("eval")
        .arg("--store")
        .arg(&store)
        .arg("judge")
        .arg("import")
        .arg("--run-id")
        .arg(run_id)
        .arg("--result")
        .arg(&unsafe_judge)
        .output()
        .unwrap();
    assert!(
        !unsafe_import.status.success(),
        "unsafe judge import should fail\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&unsafe_import.stdout),
        String::from_utf8_lossy(&unsafe_import.stderr)
    );
    assert!(
        String::from_utf8_lossy(&unsafe_import.stderr).contains("prompt"),
        "expected forbidden prompt key in stderr: {}",
        String::from_utf8_lossy(&unsafe_import.stderr)
    );

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn eval_cli_generates_sanitized_llm_judge_packet() {
    let dir = temp_dir("eval-cli-judge-packet");
    let store = dir.join("eval.sqlite");
    let case = write_eval_case(&dir);
    let run = write_eval_run(&dir, "codex.run.json", "{}");

    let ingest = Command::new(switchback_bin())
        .arg("--json")
        .arg("eval")
        .arg("--store")
        .arg(&store)
        .arg("ingest")
        .arg("--case")
        .arg(&case)
        .arg("--result")
        .arg(&run)
        .output()
        .unwrap();
    assert!(
        ingest.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        ingest.status.code(),
        String::from_utf8_lossy(&ingest.stdout),
        String::from_utf8_lossy(&ingest.stderr)
    );
    let ingest_json: serde_json::Value =
        serde_json::from_slice(&ingest.stdout).expect("eval ingest emits JSON");
    let run_id = ingest_json["run_id"].as_str().unwrap();
    let packet_path = dir.join("judge-packet.json");

    let packet = Command::new(switchback_bin())
        .arg("--json")
        .arg("eval")
        .arg("--store")
        .arg(&store)
        .arg("judge")
        .arg("packet")
        .arg("--run-id")
        .arg(run_id)
        .arg("--generated-at-ms")
        .arg("10000")
        .arg("--output")
        .arg(&packet_path)
        .output()
        .unwrap();
    assert!(
        packet.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        packet.status.code(),
        String::from_utf8_lossy(&packet.stdout),
        String::from_utf8_lossy(&packet.stderr)
    );
    let packet_json: serde_json::Value =
        serde_json::from_slice(&packet.stdout).expect("judge packet emits JSON");
    assert_eq!(
        packet_json["schema_version"],
        "switchback.eval.judge_packet/v1"
    );
    assert_eq!(packet_json["generated_at_ms"], serde_json::json!(10000));
    assert_eq!(packet_json["run"]["run_id"], run_id);
    assert_eq!(packet_json["case"]["case_id"], "react-bug-001");
    assert_eq!(
        packet_json["artifacts"][0]["reference"],
        "trace:codex-react-001"
    );
    assert_eq!(packet_json["artifacts"][0]["sha256"], "trace-sha");
    assert!(!contains_forbidden_eval_body_key(&packet_json));
    assert!(!packet_json
        .to_string()
        .contains("https://example.invalid/repo.git"));

    let packet_file_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(packet_path).unwrap()).unwrap();
    assert_eq!(packet_json, packet_file_json);

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn eval_cli_dry_run_rejects_raw_prompt_metadata() {
    let dir = temp_dir("eval-cli-unsafe");
    let store = dir.join("eval.sqlite");
    let run = write_eval_run(
        &dir,
        "unsafe.run.json",
        r#"{ "raw_prompt": "do not persist this" }"#,
    );

    let output = Command::new(switchback_bin())
        .arg("--json")
        .arg("eval")
        .arg("--store")
        .arg(&store)
        .arg("ingest")
        .arg("--dry-run")
        .arg("--result")
        .arg(&run)
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "unsafe eval ingest should fail\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("raw_prompt"),
        "stderr should name rejected metadata key: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    fs::remove_dir_all(dir).unwrap();
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
fn claude_lane_define_is_dry_run_by_default_and_preserves_unrelated_settings() {
    let dir = temp_dir("claude-lane-define");
    let config = write_config_text(&dir, CLAUDE_LANE_CFG);
    let lane_root = dir.join("lanes");
    let profile_root = dir.join("profiles");
    let settings = profile_root.join("gpt56-sol-ultra").join("settings.json");
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    let original = r#"{
  "theme": "dark",
  "env": {
    "KEEP_ME": "yes"
  }
}
"#;
    fs::write(&settings, original).unwrap();

    let dry_run = claude_lane_define_command(&config, &lane_root, &profile_root)
        .env("SWITCHBACK_SCOUT_API_KEY", "super-secret-sentinel")
        .output()
        .unwrap();
    assert!(
        dry_run.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&dry_run.stdout),
        String::from_utf8_lossy(&dry_run.stderr)
    );
    let dry_run_json: serde_json::Value = serde_json::from_slice(&dry_run.stdout).unwrap();
    assert_eq!(dry_run_json["schema"], "switchback/claude-lane-define@1");
    assert_eq!(dry_run_json["dry_run"], true);
    assert_eq!(dry_run_json["changed"], true);
    assert_eq!(dry_run_json["definition"]["requested_effort"], "ultra");
    assert_eq!(dry_run_json["definition"]["claude_effort"], "max");
    assert_eq!(dry_run_json["definition"]["transport"], "headroom");
    assert_eq!(dry_run_json["audit"]["ok"], true);
    assert!(!lane_root.join("gpt56-sol-ultra.env").exists());
    assert_eq!(fs::read_to_string(&settings).unwrap(), original);
    assert!(!String::from_utf8_lossy(&dry_run.stdout).contains("super-secret-sentinel"));

    let applied = claude_lane_define_command(&config, &lane_root, &profile_root)
        .arg("--apply")
        .env("SWITCHBACK_SCOUT_API_KEY", "super-secret-sentinel")
        .output()
        .unwrap();
    assert!(
        applied.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&applied.stdout),
        String::from_utf8_lossy(&applied.stderr)
    );
    let applied_json: serde_json::Value = serde_json::from_slice(&applied.stdout).unwrap();
    assert_eq!(applied_json["applied"], true);
    assert_eq!(applied_json["audit"]["ok"], true);
    let record = lane_root.join("gpt56-sol-ultra.env");
    let record_text = fs::read_to_string(&record).unwrap();
    let settings_text = fs::read_to_string(&settings).unwrap();
    let settings_json: serde_json::Value = serde_json::from_str(&settings_text).unwrap();
    assert_eq!(settings_json["theme"], "dark");
    assert_eq!(settings_json["env"]["KEEP_ME"], "yes");
    assert_eq!(settings_json["effortLevel"], "max");
    assert!(record_text.contains("SB_LANE_REQUESTED_EFFORT='ultra'"));
    assert!(record_text.contains("SB_LANE_CLAUDE_EFFORT='max'"));
    assert!(record_text.contains("SB_LANE_REVISION='sha256:"));
    assert!(record_text.contains("SB_LANE_KEY_ENV='SWITCHBACK_SCOUT_API_KEY'"));
    assert!(!record_text.contains("super-secret-sentinel"));
    assert!(!settings_text.contains("super-secret-sentinel"));
    assert!(!String::from_utf8_lossy(&applied.stdout).contains("super-secret-sentinel"));
    assert_eq!(
        fs::metadata(&record).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(&settings).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let audit = claude_lane_audit_command(&config, &lane_root, &profile_root)
        .output()
        .unwrap();
    assert!(audit.status.success());
    let audit_json: serde_json::Value = serde_json::from_slice(&audit.stdout).unwrap();
    assert_eq!(audit_json["schema"], "switchback/claude-lane-audit@1");
    assert_eq!(audit_json["ok"], true);

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn launch_profiles_plan_apply_and_doctor_share_one_revisioned_authority() {
    let dir = temp_dir("launch-profiles");
    // Without a declared tap the new `tap.exists` check now fails post-apply for
    // any claude_via_tap lane (this fixture's whole point before this change was
    // to exercise plan/apply/doctor end to end, not to exercise tap absence).
    let spliced = LAUNCH_PROFILE_CFG.replace(
        "providers:",
        "  taps:\n    - id: zai-claude-tap\n      bind: \"127.0.0.1:18772\"\n      upstream: \"http://127.0.0.1:8790\"\n      capture_bodies: true\nproviders:",
    );
    let config = write_config_text(&dir, &spliced);
    let authority = dir.join("launch-profiles.json");
    let lane_root = dir.join("lanes");
    let profile_root = dir.join("profiles");
    let wrapper_root = dir.join("bin");
    let projection_root = dir.join("conformance");
    fs::write(&authority, LAUNCH_PROFILE_AUTHORITY).unwrap();
    let command_paths = launch_profile_test_paths(
        &config,
        &authority,
        &lane_root,
        &profile_root,
        &wrapper_root,
        &projection_root,
    );

    let list = launch_profile_command("list", None, &command_paths)
        .output()
        .unwrap();
    assert!(
        list.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&list.stdout),
        String::from_utf8_lossy(&list.stderr)
    );
    let list_json: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(list_json["schema"], "switchback/launch-profile-list@1");
    assert_eq!(list_json["authority"]["owner"], "switchback");
    assert_eq!(list_json["profiles"].as_array().unwrap().len(), 2);
    assert!(list_json["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .any(|profile| profile["id"] == "claude-zai-full"));
    assert!(list_json["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .any(|profile| profile["id"] == "claude-qwen"));

    let plan = launch_profile_command("plan", Some("claude-zai-full"), &command_paths)
        .env("ZAI_API_KEY", "super-secret-sentinel")
        .output()
        .unwrap();
    assert!(
        plan.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&plan.stdout),
        String::from_utf8_lossy(&plan.stderr)
    );
    let plan_json: serde_json::Value = serde_json::from_slice(&plan.stdout).unwrap();
    assert_eq!(plan_json["schema"], "switchback/launch-profile-plan@1");
    assert_eq!(plan_json["profile"]["id"], "claude-zai-full");
    assert_eq!(
        plan_json["profile"]["capture"]["mode"],
        "segmented_full_wire"
    );
    assert_eq!(plan_json["changed"], true);
    assert!(plan_json["revision"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert!(!String::from_utf8_lossy(&plan.stdout).contains("super-secret-sentinel"));
    assert!(!lane_root.exists());
    assert!(!profile_root.exists());
    assert!(!wrapper_root.exists());
    assert!(!projection_root.exists());

    let apply = launch_profile_command("apply", Some("claude-zai-full"), &command_paths)
        .env("ZAI_API_KEY", "super-secret-sentinel")
        .output()
        .unwrap();
    assert!(
        apply.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&apply.stdout),
        String::from_utf8_lossy(&apply.stderr)
    );
    let apply_json: serde_json::Value = serde_json::from_slice(&apply.stdout).unwrap();
    assert_eq!(apply_json["schema"], "switchback/launch-profile-apply@1");
    assert_eq!(apply_json["applied"], true);
    assert_eq!(apply_json["doctor"]["ok"], true);
    assert_eq!(apply_json["revision"], plan_json["revision"]);

    let provider_lane_record = lane_root.join("zai.env");
    let lane_record = lane_root.join("profiles").join("claude-zai-full.env");
    let settings = profile_root.join("zai-lane").join("settings.json");
    let wrapper = wrapper_root.join("claude-zai-full");
    let conformance = projection_root.join("claude-zai-full.json");
    let lane_text = fs::read_to_string(&lane_record).unwrap();
    let provider_lane_text = fs::read_to_string(&provider_lane_record).unwrap();
    let settings_text = fs::read_to_string(&settings).unwrap();
    let wrapper_text = fs::read_to_string(&wrapper).unwrap();
    let conformance_json: serde_json::Value =
        serde_json::from_slice(&fs::read(&conformance).unwrap()).unwrap();
    assert!(provider_lane_record.exists());
    assert!(provider_lane_text.contains("SB_LANE_ANTHROPIC_TAP='18772'"));
    assert!(provider_lane_text.contains("SB_LANE_HEADROOM_PORT='8790'"));
    assert!(provider_lane_text.contains("SB_LANE_CLAUDE_VIA_TAP='1'"));
    assert!(lane_text.contains("SB_LAUNCH_PROFILE_ID='claude-zai-full'"));
    assert!(lane_text.contains("SB_LAUNCH_CAPTURE_POLICY='segmented_full_wire'"));
    assert!(lane_text.contains("SB_LAUNCH_PROFILE_REVISION='sha256:"));
    assert!(settings_text.contains("\"CLAUDE_CODE_AUTO_COMPACT_WINDOW\": \"1000000\""));
    assert!(settings_text.contains("\"ANTHROPIC_DEFAULT_HAIKU_MODEL\": \"glm-4.5-air\""));
    assert!(wrapper_text.contains("exec sb run claude --with zai --rich"));
    assert_eq!(
        conformance_json["schema"],
        "switchback/profile-conformance@1"
    );
    assert_eq!(conformance_json["authority"]["owner"], "switchback");
    assert_eq!(conformance_json["authority"]["compound_role"], "consumer");
    assert_eq!(
        conformance_json["launch_profile_ref"],
        "switchback://launch-profiles/claude-zai-full"
    );
    assert_eq!(
        conformance_json["conformance_revision"],
        plan_json["revision"]
    );
    assert!(conformance_json.get("credential_ref").is_none());
    for artifact in [&lane_record, &settings, &conformance] {
        assert_eq!(
            fs::metadata(artifact).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert_eq!(
        fs::metadata(&wrapper).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&authority).unwrap().permissions().mode() & 0o777,
        0o600
    );
    for directory in [
        &lane_root,
        &profile_root.join("zai-lane"),
        &wrapper_root,
        &projection_root,
    ] {
        assert_eq!(
            fs::metadata(directory).unwrap().permissions().mode() & 0o777,
            0o700,
            "generated private directory was not owner-only: {}",
            directory.display()
        );
    }
    for contents in [&lane_text, &settings_text, &wrapper_text] {
        assert!(!contents.contains("super-secret-sentinel"));
    }
    for forbidden in [
        "credential_ref",
        "settings",
        "capture_storage",
        "backup_receipts",
        "repair_plan",
    ] {
        assert!(
            conformance_json.get(forbidden).is_none(),
            "Compound projection duplicated Switchback-owned state: {forbidden}"
        );
    }

    let second_apply = launch_profile_command("apply", Some("claude-zai-full"), &command_paths)
        .output()
        .unwrap();
    assert!(second_apply.status.success());
    let second_json: serde_json::Value = serde_json::from_slice(&second_apply.stdout).unwrap();
    assert_eq!(second_json["applied"], false);
    assert_eq!(second_json["changed"], false);
    assert_eq!(second_json["revision"], plan_json["revision"]);

    let doctor = launch_profile_command("doctor", Some("claude-zai-full"), &command_paths)
        .output()
        .unwrap();
    assert!(doctor.status.success());
    let doctor_json: serde_json::Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert_eq!(doctor_json["schema"], "switchback/launch-profile-doctor@1");
    assert_eq!(doctor_json["profile"]["id"], "claude-zai-full");
    assert!(doctor_json["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|check| check["name"] == "listener.anthropic_tap"));
    assert!(doctor_json["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|check| check["name"] == "listener.headroom"));

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn qwen_profile_has_parity_and_apply_heals_only_generated_drift() {
    let dir = temp_dir("launch-profile-qwen-parity");
    // Same reason as the zai fixtures above: `tap.exists` now fails post-apply
    // without a declared tap for the qwen lane's own port.
    let spliced = LAUNCH_PROFILE_CFG.replace(
        "providers:",
        "  taps:\n    - id: qwen-claude-tap\n      bind: \"127.0.0.1:18777\"\n      upstream: \"http://127.0.0.1:8791\"\n      capture_bodies: true\nproviders:",
    );
    let config = write_config_text(&dir, &spliced);
    let authority = dir.join("launch-profiles.json");
    let lane_root = dir.join("lanes");
    let profile_root = dir.join("profiles");
    let wrapper_root = dir.join("bin");
    let projection_root = dir.join("conformance");
    fs::write(&authority, LAUNCH_PROFILE_AUTHORITY).unwrap();
    let command_paths = launch_profile_test_paths(
        &config,
        &authority,
        &lane_root,
        &profile_root,
        &wrapper_root,
        &projection_root,
    );

    let apply = launch_profile_command("apply", Some("claude-qwen"), &command_paths)
        .output()
        .unwrap();
    assert!(
        apply.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&apply.stdout),
        String::from_utf8_lossy(&apply.stderr)
    );
    let apply_json: serde_json::Value = serde_json::from_slice(&apply.stdout).unwrap();
    assert_eq!(apply_json["doctor"]["ok"], true);

    let settings_path = profile_root.join("qwen").join("settings.json");
    let provider_lane_path = lane_root.join("qwen.env");
    let wrapper_path = wrapper_root.join("claude-qwen");
    let conformance_path = projection_root.join("claude-qwen.json");
    let settings: serde_json::Value =
        serde_json::from_slice(&fs::read(&settings_path).unwrap()).unwrap();
    let provider_lane = fs::read_to_string(&provider_lane_path).unwrap();
    assert!(provider_lane.contains("SB_LANE_ANTHROPIC_TAP='18777'"));
    assert!(provider_lane.contains("SB_LANE_HEADROOM_PORT='8791'"));
    assert!(provider_lane.contains("SB_LANE_CLAUDE_VIA_TAP='1'"));
    assert_eq!(settings["model"], "qwen3.8-max-preview");
    assert_eq!(settings["effortLevel"], "xhigh");
    assert_eq!(
        settings["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"],
        "qwen3.8-max-preview"
    );
    assert_eq!(
        settings["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"],
        "qwen3.7-plus"
    );
    assert_eq!(
        settings["env"]["ANTHROPIC_DEFAULT_HAIKU_MODEL"],
        "qwen3.6-flash"
    );
    assert_eq!(settings["env"]["CLAUDE_CODE_SUBAGENT_MODEL"], "qwen3.7-max");
    assert_eq!(settings["env"]["CLAUDE_CODE_AUTO_COMPACT_WINDOW"], "983616");
    let wrapper = fs::read_to_string(&wrapper_path).unwrap();
    assert!(wrapper.contains("export SB_LAUNCH_PROFILE_ID='claude-qwen'"));
    assert!(wrapper.contains("export SB_LAUNCH_CAPTURE_POLICY='segmented_full_wire'"));
    assert!(wrapper.contains("exec sb run claude --with qwen --rich"));
    let conformance: serde_json::Value =
        serde_json::from_slice(&fs::read(&conformance_path).unwrap()).unwrap();
    assert_eq!(conformance["authority"]["owner"], "switchback");
    assert_eq!(conformance["authority"]["compound_role"], "consumer");
    assert!(conformance.get("credential_ref").is_none());

    let clean_diff = launch_profile_command("diff", Some("claude-qwen"), &command_paths)
        .output()
        .unwrap();
    assert!(clean_diff.status.success());
    let clean_json: serde_json::Value = serde_json::from_slice(&clean_diff.stdout).unwrap();
    assert_eq!(clean_json["changed"], false);

    fs::set_permissions(&settings_path, fs::Permissions::from_mode(0o644)).unwrap();
    let permission_drift = launch_profile_command("diff", Some("claude-qwen"), &command_paths)
        .output()
        .unwrap();
    assert!(permission_drift.status.success());
    let permission_json: serde_json::Value =
        serde_json::from_slice(&permission_drift.stdout).unwrap();
    assert_eq!(permission_json["changed"], true);
    assert!(permission_json["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .any(
            |artifact| artifact["kind"] == "harness_settings" && artifact["mode_matches"] == false
        ));
    let permission_heal = launch_profile_command("apply", Some("claude-qwen"), &command_paths)
        .output()
        .unwrap();
    assert!(permission_heal.status.success());
    let permission_heal_json: serde_json::Value =
        serde_json::from_slice(&permission_heal.stdout).unwrap();
    assert_eq!(permission_heal_json["applied"], true);
    assert_eq!(
        fs::metadata(&settings_path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let mut drifted = settings;
    drifted["model"] = serde_json::json!("tampered-model");
    fs::write(
        &settings_path,
        format!("{}\n", serde_json::to_string_pretty(&drifted).unwrap()),
    )
    .unwrap();
    let drift = launch_profile_command("diff", Some("claude-qwen"), &command_paths)
        .output()
        .unwrap();
    assert!(drift.status.success());
    let drift_json: serde_json::Value = serde_json::from_slice(&drift.stdout).unwrap();
    assert_eq!(drift_json["changed"], true);
    assert!(drift_json["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|artifact| artifact["kind"] == "harness_settings" && artifact["changed"] == true));

    let doctor = launch_profile_command("doctor", Some("claude-qwen"), &command_paths)
        .output()
        .unwrap();
    assert!(doctor.status.success(), "JSON doctor reports drift in-band");
    let doctor_json: serde_json::Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert_eq!(doctor_json["ok"], false);

    let heal = launch_profile_command("apply", Some("claude-qwen"), &command_paths)
        .output()
        .unwrap();
    assert!(heal.status.success());
    let heal_json: serde_json::Value = serde_json::from_slice(&heal.stdout).unwrap();
    assert_eq!(heal_json["applied"], true);
    assert_eq!(heal_json["doctor"]["ok"], true);
    let healed: serde_json::Value =
        serde_json::from_slice(&fs::read(&settings_path).unwrap()).unwrap();
    assert_eq!(healed["model"], "qwen3.8-max-preview");

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn launch_profile_apply_preserves_existing_shared_wrapper_directory_mode() {
    let dir = temp_dir("launch-profile-shared-wrapper-mode");
    // Same reason as the zai fixtures above: `tap.exists` now fails post-apply
    // without a declared tap for the qwen lane's own port.
    let spliced = LAUNCH_PROFILE_CFG.replace(
        "providers:",
        "  taps:\n    - id: qwen-claude-tap\n      bind: \"127.0.0.1:18777\"\n      upstream: \"http://127.0.0.1:8791\"\n      capture_bodies: true\nproviders:",
    );
    let config = write_config_text(&dir, &spliced);
    let authority = dir.join("launch-profiles.json");
    let lane_root = dir.join("lanes");
    let profile_root = dir.join("profiles");
    let wrapper_root = dir.join("shared-bin");
    let projection_root = dir.join("conformance");
    fs::write(&authority, LAUNCH_PROFILE_AUTHORITY).unwrap();
    fs::create_dir_all(&wrapper_root).unwrap();
    fs::set_permissions(&wrapper_root, fs::Permissions::from_mode(0o755)).unwrap();
    let command_paths = launch_profile_test_paths(
        &config,
        &authority,
        &lane_root,
        &profile_root,
        &wrapper_root,
        &projection_root,
    );

    let apply = launch_profile_command("apply", Some("claude-qwen"), &command_paths)
        .output()
        .unwrap();

    assert!(
        apply.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&apply.stdout),
        String::from_utf8_lossy(&apply.stderr)
    );
    assert_eq!(
        fs::metadata(&wrapper_root).unwrap().permissions().mode() & 0o777,
        0o755,
        "profile apply must not chmod a pre-existing shared wrapper directory"
    );

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn launch_profile_apply_refuses_to_overwrite_unowned_wrapper() {
    let dir = temp_dir("launch-profile-unowned-wrapper");
    let config = write_config_text(&dir, LAUNCH_PROFILE_CFG);
    let authority = dir.join("launch-profiles.json");
    let lane_root = dir.join("lanes");
    let profile_root = dir.join("profiles");
    let wrapper_root = dir.join("bin");
    let projection_root = dir.join("conformance");
    let wrapper = wrapper_root.join("claude-qwen");
    let unrelated = "#!/bin/zsh\nexec custom-qwen \"$@\"\n";
    fs::write(&authority, LAUNCH_PROFILE_AUTHORITY).unwrap();
    fs::create_dir_all(&wrapper_root).unwrap();
    fs::write(&wrapper, unrelated).unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    let command_paths = launch_profile_test_paths(
        &config,
        &authority,
        &lane_root,
        &profile_root,
        &wrapper_root,
        &projection_root,
    );

    let apply = launch_profile_command("apply", Some("claude-qwen"), &command_paths)
        .output()
        .unwrap();

    assert!(
        !apply.status.success(),
        "profile apply unexpectedly replaced an unrelated wrapper"
    );
    assert!(
        String::from_utf8_lossy(&apply.stderr).contains("unowned wrapper"),
        "stderr={}",
        String::from_utf8_lossy(&apply.stderr)
    );
    assert_eq!(fs::read_to_string(&wrapper).unwrap(), unrelated);
    assert!(
        !lane_root.exists() && !profile_root.exists() && !projection_root.exists(),
        "ownership must be checked before any profile artifacts are written"
    );

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn provider_lane_record_owns_declared_fields_and_preserves_undeclared_ones() {
    // The authority used to emit a fixed 15 fields and let everything else
    // survive as "preserved compatibility" — unvalidated, unregenerated, and
    // free to contradict the owned half. Declaring a field must move it under
    // the authority (exactly one answer in the record), while a field the spec
    // has not adopted must still survive, so lanes migrate incrementally instead
    // of losing configuration the day they are adopted.
    let dir = temp_dir("provider-lane-field-ownership");
    // `tap.exists` now fails post-apply without a declared tap for the lane's
    // own port; this test is about field ownership, not tap absence.
    let spliced = LAUNCH_PROFILE_CFG.replace(
        "providers:",
        "  taps:\n    - id: zai-claude-tap\n      bind: \"127.0.0.1:18772\"\n      upstream: \"http://127.0.0.1:8790\"\n      capture_bodies: true\nproviders:",
    );
    let config = write_config_text(&dir, &spliced);
    let authority = dir.join("launch-profiles.json");
    let mut document: serde_json::Value = serde_json::from_str(LAUNCH_PROFILE_AUTHORITY).unwrap();
    document["provider_lanes"]["zai"]["wire_api"] = serde_json::json!("chat");
    document["provider_lanes"]["zai"]["headroom_bypass"] = serde_json::json!(true);
    document["provider_lanes"]["zai"]["fast_model"] = serde_json::json!("glm-4.5-air");
    document["provider_lanes"]["zai"]["notes"] =
        serde_json::json!("480k stays under the provider's 512k price cliff");
    document["harness_presets"]["claude-rich-zai"]["display_name"] =
        serde_json::json!("z.ai GLM-5.2 1M");
    fs::write(&authority, serde_json::to_vec_pretty(&document).unwrap()).unwrap();

    let lane_root = dir.join("lanes");
    fs::create_dir_all(&lane_root).unwrap();
    // A legacy record holding one field the spec now declares and one it doesn't.
    fs::write(
        lane_root.join("zai.env"),
        "SB_LANE_WIRE_API=\"anthropic_messages\"\nSB_LANE_OPENAI_URL=\"https://example.invalid/v1\"\n",
    )
    .unwrap();
    let profile_root = dir.join("profiles");
    let wrapper_root = dir.join("bin");
    let projection_root = dir.join("conformance");
    let command_paths = launch_profile_test_paths(
        &config,
        &authority,
        &lane_root,
        &profile_root,
        &wrapper_root,
        &projection_root,
    );
    let apply = launch_profile_command("apply", Some("claude-zai-full"), &command_paths)
        .output()
        .unwrap();
    assert!(
        apply.status.success(),
        "apply failed: {}",
        String::from_utf8_lossy(&apply.stderr)
    );

    let record = fs::read_to_string(lane_root.join("zai.env")).unwrap();
    assert!(
        record.contains("SB_LANE_WIRE_API='chat'"),
        "declared wire_api must be emitted, not hardcoded: {record}"
    );
    assert_eq!(
        record.matches("SB_LANE_WIRE_API").count(),
        1,
        "a declared field must not also be preserved — the record would hold two answers: {record}"
    );
    assert!(
        record.contains("SB_LANE_CLAUDE_HEADROOM_BYPASS='1'"),
        "headroom_bypass must come from the spec instead of a hardcoded 0: {record}"
    );
    assert!(
        record.contains("SB_LANE_NOTES="),
        "operator rationale needs a field or migration deletes it: {record}"
    );
    assert!(
        record.contains("SB_LANE_CLAUDE_CUSTOM_MODEL_NAME='z.ai GLM-5.2 1M'"),
        "display_name must reach the record: {record}"
    );
    assert!(
        record.contains("SB_LANE_FAST_MODEL='glm-4.5-air'"),
        "fast_model must reach the record: {record}"
    );
    assert!(
        record.contains("SB_LANE_OPENAI_URL=\"https://example.invalid/v1\""),
        "a field the spec has not adopted must still be preserved: {record}"
    );
}

#[test]
fn launch_profile_doctor_rejects_a_tap_bound_to_a_shared_headroom() {
    // The exact shape that broke the z.ai lane: the tap forwards to the SHARED
    // Headroom instead of the lane's own, and carries x-headroom-base-url as if
    // that redirected the provider. It does not — that header is honored only on
    // OpenAI-shaped paths, so /v1/messages went to whatever the shared process
    // was pinned to and answered 401 with a valid key. Both ports were listening
    // the whole time, so liveness checks reported healthy.
    let dir = temp_dir("launch-profile-shared-headroom");
    // `taps` belongs under `server:`, which the fixture closes before
    // `providers:`, so splice it in there rather than appending at the root.
    let spliced = LAUNCH_PROFILE_CFG.replace(
        "providers:",
        "  taps:\n    - id: zai-claude-tap\n      bind: \"127.0.0.1:18772\"\n      upstream: \"http://127.0.0.1:8787\"\n      capture_bodies: true\n      headers:\n        x-headroom-base-url: \"https://api.z.ai/api/anthropic\"\nproviders:",
    );
    let config = write_config_text(&dir, &spliced);
    let authority = dir.join("launch-profiles.json");
    fs::write(&authority, LAUNCH_PROFILE_AUTHORITY).unwrap();
    let lane_root = dir.join("lanes");
    let profile_root = dir.join("profiles");
    let wrapper_root = dir.join("bin");
    let projection_root = dir.join("conformance");
    let command_paths = launch_profile_test_paths(
        &config,
        &authority,
        &lane_root,
        &profile_root,
        &wrapper_root,
        &projection_root,
    );
    launch_profile_command("apply", Some("claude-zai-full"), &command_paths)
        .output()
        .unwrap();

    let report = launch_profile_command("doctor", Some("claude-zai-full"), &command_paths)
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
    let checks = json["checks"].as_array().unwrap();
    let find = |name: &str| {
        checks
            .iter()
            .find(|check| check["name"] == name)
            .unwrap_or_else(|| panic!("missing check {name}: {json}"))
    };
    let binding = find("tap.headroom_binding");
    assert_eq!(
        binding["ok"], false,
        "a tap pointed at the shared Headroom must fail the binding check"
    );
    assert_eq!(binding["expected"], "http://127.0.0.1:8790");
    assert_eq!(binding["actual"], "http://127.0.0.1:8787");
    assert_eq!(
        find("tap.no_openai_base_url_override")["ok"],
        false,
        "x-headroom-base-url on an Anthropic tap must fail: it is a silent no-op there"
    );
    assert_eq!(json["ok"], false);
}

#[test]
fn launch_profile_doctor_accepts_a_tap_bound_to_its_own_headroom() {
    let dir = temp_dir("launch-profile-own-headroom");
    let spliced = LAUNCH_PROFILE_CFG.replace(
        "providers:",
        "  taps:\n    - id: zai-claude-tap\n      bind: \"127.0.0.1:18772\"\n      upstream: \"http://127.0.0.1:8790\"\n      capture_bodies: true\nproviders:",
    );
    let config = write_config_text(&dir, &spliced);
    let authority = dir.join("launch-profiles.json");
    fs::write(&authority, LAUNCH_PROFILE_AUTHORITY).unwrap();
    let lane_root = dir.join("lanes");
    let profile_root = dir.join("profiles");
    let wrapper_root = dir.join("bin");
    let projection_root = dir.join("conformance");
    let command_paths = launch_profile_test_paths(
        &config,
        &authority,
        &lane_root,
        &profile_root,
        &wrapper_root,
        &projection_root,
    );
    launch_profile_command("apply", Some("claude-zai-full"), &command_paths)
        .output()
        .unwrap();

    let report = launch_profile_command("doctor", Some("claude-zai-full"), &command_paths)
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
    let checks = json["checks"].as_array().unwrap();
    for name in ["tap.headroom_binding", "tap.no_openai_base_url_override"] {
        let check = checks
            .iter()
            .find(|check| check["name"] == name)
            .unwrap_or_else(|| panic!("missing check {name}: {json}"));
        assert_eq!(check["ok"], true, "{name} should pass: {check}");
    }
}

#[test]
fn launch_profile_doctor_rejects_a_lane_with_no_tap_bound_to_its_declared_port() {
    // The exact gap the gpt56-sol-ultra incident exposed: the lane record names
    // an `anthropic_tap_port`, but the config's `taps:` list has no entry bound
    // there at all — 8788 turned out to be a Headroom process, not a tap. The
    // plain fixture already has no `taps:` section, so this is the bare
    // scenario with nothing spliced in.
    let dir = temp_dir("launch-profile-no-tap-declared");
    let config = write_config_text(&dir, LAUNCH_PROFILE_CFG);
    let authority = dir.join("launch-profiles.json");
    fs::write(&authority, LAUNCH_PROFILE_AUTHORITY).unwrap();
    let lane_root = dir.join("lanes");
    let profile_root = dir.join("profiles");
    let wrapper_root = dir.join("bin");
    let projection_root = dir.join("conformance");
    let command_paths = launch_profile_test_paths(
        &config,
        &authority,
        &lane_root,
        &profile_root,
        &wrapper_root,
        &projection_root,
    );
    launch_profile_command("apply", Some("claude-zai-full"), &command_paths)
        .output()
        .unwrap();

    let report = launch_profile_command("doctor", Some("claude-zai-full"), &command_paths)
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
    let checks = json["checks"].as_array().unwrap();
    let exists = checks
        .iter()
        .find(|check| check["name"] == "tap.exists")
        .unwrap_or_else(|| panic!("missing check tap.exists: {json}"));
    assert_eq!(
        exists["ok"], false,
        "no tap in config binds the lane's declared anthropic_tap_port: {json}"
    );
    assert_eq!(json["ok"], false);
}

#[test]
fn launch_profile_doctor_accepts_a_lane_with_a_tap_bound_to_its_declared_port() {
    // Mirror of the rejection above: a tap really is bound to the lane's
    // declared port, so `tap.exists` must pass.
    let dir = temp_dir("launch-profile-tap-declared");
    let spliced = LAUNCH_PROFILE_CFG.replace(
        "providers:",
        "  taps:\n    - id: zai-claude-tap\n      bind: \"127.0.0.1:18772\"\n      upstream: \"http://127.0.0.1:8790\"\n      capture_bodies: true\nproviders:",
    );
    let config = write_config_text(&dir, &spliced);
    let authority = dir.join("launch-profiles.json");
    fs::write(&authority, LAUNCH_PROFILE_AUTHORITY).unwrap();
    let lane_root = dir.join("lanes");
    let profile_root = dir.join("profiles");
    let wrapper_root = dir.join("bin");
    let projection_root = dir.join("conformance");
    let command_paths = launch_profile_test_paths(
        &config,
        &authority,
        &lane_root,
        &profile_root,
        &wrapper_root,
        &projection_root,
    );
    launch_profile_command("apply", Some("claude-zai-full"), &command_paths)
        .output()
        .unwrap();

    let report = launch_profile_command("doctor", Some("claude-zai-full"), &command_paths)
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
    let checks = json["checks"].as_array().unwrap();
    let exists = checks
        .iter()
        .find(|check| check["name"] == "tap.exists")
        .unwrap_or_else(|| panic!("missing check tap.exists: {json}"));
    assert_eq!(
        exists["ok"], true,
        "a tap bound to the lane's declared port must satisfy tap.exists: {json}"
    );
}

#[test]
fn launch_profile_doctor_checks_required_live_tap_and_headroom_listeners() {
    let dir = temp_dir("launch-profile-live-listeners");
    let authority = dir.join("launch-profiles.json");
    let lane_root = dir.join("lanes");
    let profile_root = dir.join("profiles");
    let wrapper_root = dir.join("bin");
    let projection_root = dir.join("conformance");
    let tap_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let headroom_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let tap_port = tap_listener.local_addr().unwrap().port();
    let headroom_port = headroom_listener.local_addr().unwrap().port();
    // `tap.exists` now requires a `taps:` entry bound to the lane's own port —
    // the bare fixture has none, so splice one in at the random port this test
    // just bound (ports are random here, unlike the fixed-port shared-Headroom
    // tests above, so the entry has to be built after binding, not baked into
    // the constant). Point its upstream at the matching random Headroom port
    // too, or `tap.headroom_binding` would fail alongside the checks this test
    // exists to exercise.
    let spliced = LAUNCH_PROFILE_CFG.replace(
        "providers:",
        &format!(
            "  taps:\n    - id: zai-claude-tap\n      bind: \"127.0.0.1:{tap_port}\"\n      upstream: \"http://127.0.0.1:{headroom_port}\"\n      capture_bodies: true\nproviders:"
        ),
    );
    let config = write_config_text(&dir, &spliced);
    let mut document: serde_json::Value = serde_json::from_str(LAUNCH_PROFILE_AUTHORITY).unwrap();
    document["provider_lanes"]["zai"]["anthropic_tap_port"] = serde_json::json!(tap_port);
    document["provider_lanes"]["zai"]["headroom_port"] = serde_json::json!(headroom_port);
    fs::write(&authority, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    let command_paths = launch_profile_test_paths(
        &config,
        &authority,
        &lane_root,
        &profile_root,
        &wrapper_root,
        &projection_root,
    );

    let apply = launch_profile_command("apply", Some("claude-zai-full"), &command_paths)
        .output()
        .unwrap();
    assert!(
        apply.status.success(),
        "profile materialization must not require test listeners: {}",
        String::from_utf8_lossy(&apply.stderr)
    );

    let healthy = launch_profile_command("doctor", Some("claude-zai-full"), &command_paths)
        .output()
        .unwrap();
    assert!(healthy.status.success());
    let healthy_json: serde_json::Value = serde_json::from_slice(&healthy.stdout).unwrap();
    assert_eq!(healthy_json["ok"], true);
    assert!(healthy_json["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|check| check["name"] == "listener.anthropic_tap" && check["ok"] == true));
    assert!(healthy_json["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|check| check["name"] == "listener.headroom" && check["ok"] == true));

    drop(tap_listener);
    let unhealthy = launch_profile_command("doctor", Some("claude-zai-full"), &command_paths)
        .output()
        .unwrap();
    assert!(
        unhealthy.status.success(),
        "JSON doctor reports failure in-band"
    );
    let unhealthy_json: serde_json::Value = serde_json::from_slice(&unhealthy.stdout).unwrap();
    assert_eq!(unhealthy_json["ok"], false);
    assert!(unhealthy_json["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|check| check["name"] == "listener.anthropic_tap" && check["ok"] == false));

    drop(headroom_listener);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn launch_profile_doctor_surveys_every_profile_instead_of_stopping_at_the_first_fault() {
    let dir = temp_dir("launch-profile-doctor-multi-fault");
    let config = write_config_text(&dir, LAUNCH_PROFILE_CFG);
    let authority = dir.join("launch-profiles.json");
    let lane_root = dir.join("lanes");
    let profile_root = dir.join("profiles");
    let wrapper_root = dir.join("bin");
    let projection_root = dir.join("conformance");
    let mut doc: serde_json::Value = serde_json::from_str(LAUNCH_PROFILE_AUTHORITY).unwrap();
    let command_paths = launch_profile_test_paths(
        &config,
        &authority,
        &lane_root,
        &profile_root,
        &wrapper_root,
        &projection_root,
    );

    // Break BOTH Anthropic-wire lanes identically. The no-argument doctor exists
    // to survey everything, so one fault must not conceal the next: propagating
    // the first resolve error costs the operator a round trip per fault to learn
    // what a single run already knew.
    //
    // The fault is a missing `anthropic_tap_port` under `claude_via_tap`. This test
    // previously manufactured its faults by switching the lanes to `transport: tap`,
    // which relied on a rule since removed as a false positive — so the test went
    // green-adjacent for the wrong reason and then panicked on an empty
    // `unresolvable` list the moment the rule left. A survey test must break its
    // subjects with a fault it does not share an owner with.
    for lane in ["zai", "qwen"] {
        doc["provider_lanes"][lane]
            .as_object_mut()
            .unwrap()
            .remove("anthropic_tap_port");
    }
    fs::write(&authority, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();

    let out = launch_profile_command("doctor", None, &command_paths)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let report: serde_json::Value =
        serde_json::from_str(&stdout).expect("doctor --json must emit a report, not abort");
    assert_eq!(
        report["ok"],
        serde_json::json!(false),
        "a survey containing faults is not ok"
    );
    let ids: Vec<&str> = report["unresolvable"]
        .as_array()
        .expect("unresolvable list")
        .iter()
        .filter_map(|failure| failure["id"].as_str())
        .collect();
    for want in ["claude-zai-full", "claude-qwen"] {
        assert!(
            ids.contains(&want),
            "every faulted profile must appear; {want} missing from {ids:?}"
        );
    }
}

#[test]
fn launch_profile_authority_fails_closed_on_unsupported_tuple_or_compound_overlay() {
    let dir = temp_dir("launch-profile-authority-boundary");
    let config = write_config_text(&dir, LAUNCH_PROFILE_CFG);
    let authority = dir.join("launch-profiles.json");
    let lane_root = dir.join("lanes");
    let profile_root = dir.join("profiles");
    let wrapper_root = dir.join("bin");
    let projection_root = dir.join("conformance");
    let original: serde_json::Value = serde_json::from_str(LAUNCH_PROFILE_AUTHORITY).unwrap();
    let command_paths = launch_profile_test_paths(
        &config,
        &authority,
        &lane_root,
        &profile_root,
        &wrapper_root,
        &projection_root,
    );

    let mut unsupported = original.clone();
    unsupported["provider_lanes"]["zai"]["credential_ref"] =
        serde_json::json!({"kind": "vault", "name": "zai"});
    fs::write(&authority, serde_json::to_vec_pretty(&unsupported).unwrap()).unwrap();
    let unsupported_result =
        launch_profile_command("plan", Some("claude-zai-full"), &command_paths)
            .output()
            .unwrap();
    assert!(!unsupported_result.status.success());
    assert!(String::from_utf8_lossy(&unsupported_result.stderr)
        .contains("requires an env credential reference"));
    assert!(!lane_root.exists());
    assert!(!profile_root.exists());
    assert!(!wrapper_root.exists());
    assert!(!projection_root.exists());

    let mut missing_headroom_port = original.clone();
    missing_headroom_port["provider_lanes"]["zai"]
        .as_object_mut()
        .unwrap()
        .remove("headroom_port");
    fs::write(
        &authority,
        serde_json::to_vec_pretty(&missing_headroom_port).unwrap(),
    )
    .unwrap();
    let missing_headroom_result =
        launch_profile_command("plan", Some("claude-zai-full"), &command_paths)
            .output()
            .unwrap();
    assert!(!missing_headroom_result.status.success());
    assert!(String::from_utf8_lossy(&missing_headroom_result.stderr)
        .contains("headroom transport requires headroom_port"));

    let mut missing_tap_port = original.clone();
    missing_tap_port["provider_lanes"]["zai"]
        .as_object_mut()
        .unwrap()
        .remove("anthropic_tap_port");
    fs::write(
        &authority,
        serde_json::to_vec_pretty(&missing_tap_port).unwrap(),
    )
    .unwrap();
    let missing_tap_result =
        launch_profile_command("plan", Some("claude-zai-full"), &command_paths)
            .output()
            .unwrap();
    assert!(!missing_tap_result.status.success());
    assert!(String::from_utf8_lossy(&missing_tap_result.stderr)
        .contains("claude_via_tap requires anthropic_tap_port"));

    let mut observed_headroom_bypass = original.clone();
    observed_headroom_bypass["provider_lanes"]["zai"]["claude_via_tap"] = serde_json::json!(false);
    fs::write(
        &authority,
        serde_json::to_vec_pretty(&observed_headroom_bypass).unwrap(),
    )
    .unwrap();
    let bypass_result = launch_profile_command("plan", Some("claude-zai-full"), &command_paths)
        .output()
        .unwrap();
    assert!(!bypass_result.status.success());
    assert!(String::from_utf8_lossy(&bypass_result.stderr)
        .contains("observed Claude Code headroom profiles must set claude_via_tap=true"));

    let mut unsupported_mcp = original.clone();
    unsupported_mcp["harness_presets"]["claude-rich-zai"]["mcp_mode"] =
        serde_json::json!("selected");
    fs::write(
        &authority,
        serde_json::to_vec_pretty(&unsupported_mcp).unwrap(),
    )
    .unwrap();
    let unsupported_mcp_result =
        launch_profile_command("plan", Some("claude-zai-full"), &command_paths)
            .output()
            .unwrap();
    assert!(!unsupported_mcp_result.status.success());
    assert!(String::from_utf8_lossy(&unsupported_mcp_result.stderr)
        .contains("mcp_mode selected requires an explicit server selection"));

    // An Anthropic-wire lane on tap transport — the `gpt56-sol-wpcom` shape — must
    // PLAN, not fail closed. A rule here once rejected it, reasoning that Claude
    // Code sends `x-api-key` and the gateway 401s that header. Claude Code sends
    // `x-api-key` only under `ANTHROPIC_API_KEY`; this binary launches it with
    // `ANTHROPIC_AUTH_TOKEN`, so it sends `Authorization: Bearer`, which the gateway
    // honours. Measured against the live gateway, same key and body: `x-api-key`
    // -> 401, `Authorization: Bearer` -> 200 with a real completion.
    //
    // This assertion is deliberately the inverse of the old one. The rule it
    // replaces did not merely warn — it bailed at resolve time, so one lane it
    // misjudged took `profile list` and `profile doctor` down for every lane.
    let mut claude_lane_on_gateway = original.clone();
    claude_lane_on_gateway["provider_lanes"]["zai"]["transport"] = serde_json::json!("tap");
    claude_lane_on_gateway["provider_lanes"]["zai"]
        .as_object_mut()
        .unwrap()
        .remove("headroom_port");
    fs::write(
        &authority,
        serde_json::to_vec_pretty(&claude_lane_on_gateway).unwrap(),
    )
    .unwrap();
    let claude_lane_on_gateway_result =
        launch_profile_command("plan", Some("claude-zai-full"), &command_paths)
            .output()
            .unwrap();
    assert!(
        claude_lane_on_gateway_result.status.success(),
        "a claude_via_tap lane on tap transport must plan (it authenticates with \
         Bearer via ANTHROPIC_AUTH_TOKEN); stderr: {}",
        String::from_utf8_lossy(&claude_lane_on_gateway_result.stderr)
    );

    let mut compound_overlay = original;
    compound_overlay["compound"] = serde_json::json!({
        "mutate_capture_state": true,
        "apply_repairs": true
    });
    fs::write(
        &authority,
        serde_json::to_vec_pretty(&compound_overlay).unwrap(),
    )
    .unwrap();
    let overlay_result = launch_profile_command("list", None, &command_paths)
        .output()
        .unwrap();
    assert!(!overlay_result.status.success());
    assert!(String::from_utf8_lossy(&overlay_result.stderr).contains("unknown field `compound`"));

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn launch_profile_ids_are_resolved_from_authority_not_compiled_names() {
    let dir = temp_dir("launch-profile-dynamic-name");
    let config = write_config_text(&dir, LAUNCH_PROFILE_CFG);
    let authority = dir.join("launch-profiles.json");
    let lane_root = dir.join("lanes");
    let profile_root = dir.join("profiles");
    let wrapper_root = dir.join("bin");
    let projection_root = dir.join("conformance");
    let mut document: serde_json::Value = serde_json::from_str(LAUNCH_PROFILE_AUTHORITY).unwrap();
    let future_profile = document["launch_profiles"]
        .as_object_mut()
        .unwrap()
        .remove("claude-qwen")
        .unwrap();
    document["launch_profiles"]
        .as_object_mut()
        .unwrap()
        .insert("future-harness-profile".to_string(), future_profile);
    fs::write(&authority, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    let command_paths = launch_profile_test_paths(
        &config,
        &authority,
        &lane_root,
        &profile_root,
        &wrapper_root,
        &projection_root,
    );

    let plan = launch_profile_command("plan", Some("future-harness-profile"), &command_paths)
        .output()
        .unwrap();
    assert!(
        plan.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&plan.stdout),
        String::from_utf8_lossy(&plan.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&plan.stdout).unwrap();
    assert_eq!(report["profile"]["id"], "future-harness-profile");
    assert_eq!(report["profile"]["provider_lane"], "qwen");
    assert_eq!(report["profile"]["harness"], "claude-code");
    assert_eq!(report["changed"], true);

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn launch_profile_apply_preserves_unowned_provider_lane_compatibility_fields() {
    let dir = temp_dir("launch-profile-provider-lane-merge");
    // `tap.exists` now fails post-apply without a declared tap for the lane's
    // own port; this test is about compatibility-field preservation, not tap
    // absence.
    let spliced = LAUNCH_PROFILE_CFG.replace(
        "providers:",
        "  taps:\n    - id: zai-claude-tap\n      bind: \"127.0.0.1:18772\"\n      upstream: \"http://127.0.0.1:8790\"\n      capture_bodies: true\nproviders:",
    );
    let config = write_config_text(&dir, &spliced);
    let authority = dir.join("launch-profiles.json");
    let lane_root = dir.join("lanes");
    let profile_root = dir.join("profiles");
    let wrapper_root = dir.join("bin");
    let projection_root = dir.join("conformance");
    fs::write(&authority, LAUNCH_PROFILE_AUTHORITY).unwrap();
    let command_paths = launch_profile_test_paths(
        &config,
        &authority,
        &lane_root,
        &profile_root,
        &wrapper_root,
        &projection_root,
    );
    fs::create_dir_all(&lane_root).unwrap();
    fs::write(
        lane_root.join("zai.env"),
        concat!(
            "# Existing lane record with direct/headroom compatibility fields.\n",
            "SB_LANE_SCHEMA='legacy'\n",
            "SB_LANE_NAME='zai'\n",
            "SB_LANE_MODEL='stale-model'\n",
            "SB_LANE_ROUTE='stale-route'\n",
            "SB_LANE_ANTHROPIC_URL='https://api.z.ai/api/anthropic'\n",
            "SB_LANE_OPENAI_URL='https://api.z.ai/api/paas/v4'\n",
            "SB_LANE_DIRECT_ROUTE='zai-direct/glm-5.2'\n",
            "SB_LANE_DIRECT_ANTHROPIC_TAP='18774'\n",
            "SB_LANE_HEADROOM_PORT='18773'\n",
            "SB_LANE_CLAUDE_VIA_TAP='0'\n",
            "SB_LANE_CODEX_ROUTE='zai/glm-5.2'\n",
            "SB_LANE_CLAUDE_MODEL='glm-5.2[1m]'\n",
            "SB_LANE_CLAUDE_HEADROOM_BYPASS='1'\n",
        ),
    )
    .unwrap();

    let apply = launch_profile_command("apply", Some("claude-zai-full"), &command_paths)
        .output()
        .unwrap();
    assert!(
        apply.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&apply.stdout),
        String::from_utf8_lossy(&apply.stderr)
    );

    let lane = fs::read_to_string(lane_root.join("zai.env")).unwrap();
    assert!(lane.contains("SB_LANE_SCHEMA='switchback/provider-lane@1'"));
    assert!(lane.contains("SB_LANE_MODEL='glm-5.2'"));
    assert!(lane.contains("SB_LANE_ROUTE='zai/glm-5.2'"));
    assert!(!lane.contains("stale-model"));
    assert!(!lane.contains("stale-route"));
    for field in [
        "SB_LANE_ANTHROPIC_URL='https://api.z.ai/api/anthropic'",
        "SB_LANE_OPENAI_URL='https://api.z.ai/api/paas/v4'",
        "SB_LANE_DIRECT_ROUTE='zai-direct/glm-5.2'",
        "SB_LANE_DIRECT_ANTHROPIC_TAP='18774'",
        "SB_LANE_CODEX_ROUTE='zai/glm-5.2'",
        "SB_LANE_CLAUDE_MODEL='glm-5.2[1m]'",
    ] {
        assert!(
            lane.contains(field),
            "profile apply removed compatibility field: {field}\n{lane}"
        );
    }
    assert!(
        lane.contains("SB_LANE_HEADROOM_PORT='8790'"),
        "provider_lanes owns the Headroom port and must replace its stale value"
    );
    assert!(
        lane.contains("SB_LANE_CLAUDE_VIA_TAP='1'"),
        "provider_lanes owns Claude tap routing and must replace its stale value"
    );
    assert!(
        lane.contains("SB_LANE_CLAUDE_HEADROOM_BYPASS='0'"),
        "provider_lanes owns the bypass policy and must replace its stale value"
    );

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn claude_lane_audit_reports_effort_alias_and_resilience_drift_in_band() {
    let dir = temp_dir("claude-lane-drift");
    let config = write_config_text(&dir, CLAUDE_LANE_CFG);
    let lane_root = dir.join("lanes");
    let profile_root = dir.join("profiles");
    let applied = claude_lane_define_command(&config, &lane_root, &profile_root)
        .arg("--apply")
        .output()
        .unwrap();
    assert!(applied.status.success());
    let settings = profile_root.join("gpt56-sol-ultra").join("settings.json");
    let coherent_settings = fs::read_to_string(&settings).unwrap();
    fs::write(&settings, coherent_settings.replace("\"max\"", "\"xhigh\"")).unwrap();

    let effort_drift = claude_lane_audit_command(&config, &lane_root, &profile_root)
        .output()
        .unwrap();
    assert!(
        effort_drift.status.success(),
        "JSON drift is reported in-band"
    );
    let effort_json: serde_json::Value = serde_json::from_slice(&effort_drift.stdout).unwrap();
    assert_eq!(effort_json["ok"], false);
    assert!(effort_json["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|check| { check["name"] == "settings.effort" && check["ok"] == false }));

    fs::write(&settings, coherent_settings).unwrap();
    let alias_drift = CLAUDE_LANE_CFG.replace(
        "  - name: sol-alias\n    match:\n      model: \"gpt-5.6-sol\"\n    targets:\n      - \"primary/gpt-5.6-sol\"\n      - \"fallback/gpt-5.5\"",
        "  - name: sol-alias\n    match:\n      model: \"gpt-5.6-sol\"\n    targets:\n      - \"primary/gpt-5.6-sol\"\n      - \"fallback/gpt-5.4\"",
    );
    fs::write(&config, alias_drift).unwrap();
    let alias_audit = claude_lane_audit_command(&config, &lane_root, &profile_root)
        .output()
        .unwrap();
    assert!(
        alias_audit.status.success(),
        "JSON drift is reported in-band"
    );
    let alias_json: serde_json::Value = serde_json::from_slice(&alias_audit.stdout).unwrap();
    assert_eq!(alias_json["ok"], false);
    assert!(alias_json["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|check| { check["name"] == "route.aliases" && check["ok"] == false }));

    fs::write(
        &config,
        CLAUDE_LANE_CFG.replace("    max_retries: 2", "    max_retries: 0"),
    )
    .unwrap();
    let resilience_audit = claude_lane_audit_command(&config, &lane_root, &profile_root)
        .output()
        .unwrap();
    assert!(
        resilience_audit.status.success(),
        "JSON drift is reported in-band"
    );
    let resilience_json: serde_json::Value =
        serde_json::from_slice(&resilience_audit.stdout).unwrap();
    assert_eq!(resilience_json["ok"], false);
    assert!(resilience_json["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|check| { check["name"] == "server.retry" && check["ok"] == false }));

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn claude_lane_audit_defers_to_the_launch_profile_owner_instead_of_offering_to_rewrite_it() {
    let dir = temp_dir("claude-lane-foreign-owner");
    let config = write_config_text(&dir, CLAUDE_LANE_CFG);
    let lane_root = dir.join("lanes");
    let profile_root = dir.join("profiles");
    fs::create_dir_all(&lane_root).unwrap();

    // A lane materialized by the launch-profile authority: the record carries the
    // provider-lane schema and none of the profile-owned fields this command reads.
    // The label deliberately differs from the lane name, so resolving it by lane
    // name (the old fallback) points at a directory that does not exist.
    let lane_record = lane_root.join("gpt56-sol-ultra.env");
    fs::write(
        &lane_record,
        "SB_LANE_SCHEMA='switchback/provider-lane@1'\n\
         SB_LANE_NAME='gpt56-sol-ultra'\n\
         SB_LANE_MODEL='openai/gpt-5.6-sol'\n\
         SB_LANE_ROUTE='openai/gpt-5.6-sol'\n\
         SB_LANE_TRANSPORT='headroom'\n",
    )
    .unwrap();
    let authority = dir.join("launch-profiles.json");
    fs::write(
        &authority,
        r#"{
  "schema": "switchback/launch-profiles@1",
  "provider_lanes": {},
  "harness_presets": {},
  "capture_policies": {},
  "launch_profiles": {
    "claude-gpt56-sol-ultra": {
      "provider_lane": "gpt56-sol-ultra",
      "harness_preset": "claude-rich",
      "capture_policy": "observed",
      "profile_label": "gpt56-lane"
    }
  }
}"#,
    )
    .unwrap();

    let audit = claude_lane_audit_command(&config, &lane_root, &profile_root)
        .arg("--authority")
        .arg(&authority)
        .output()
        .unwrap();
    let audit_json: serde_json::Value = serde_json::from_slice(&audit.stdout).unwrap();
    assert_eq!(audit_json["ok"], false);

    let checks = audit_json["checks"].as_array().unwrap();
    assert!(
        checks.iter().any(|check| check["name"] == "record.owner"),
        "the failure names the ownership gap: {checks:?}"
    );
    assert!(
        !checks
            .iter()
            .any(|check| check["name"] == "materialized_files"),
        "a lane owned elsewhere is not a lane with missing files: {checks:?}"
    );

    // The whole point: never hand back a next action that would overwrite a
    // healthy lane with the retired shape.
    let next_actions = audit_json["next_actions"].as_array().unwrap();
    assert_eq!(next_actions.len(), 1);
    assert_eq!(next_actions[0], "sb profile doctor claude-gpt56-sol-ultra");
    assert!(
        !next_actions
            .iter()
            .any(|action| action.as_str().unwrap().contains("lane define")),
        "must not recommend the destructive path: {next_actions:?}"
    );

    // The reported settings path follows the profile label, not the lane name.
    assert!(audit_json["settings"]
        .as_str()
        .unwrap()
        .contains("gpt56-lane"));

    // And the destructive path itself refuses, leaving the record byte-identical.
    let before = fs::read(&lane_record).unwrap();
    let define = claude_lane_define_command(&config, &lane_root, &profile_root)
        .arg("--apply")
        .output()
        .unwrap();
    assert!(
        !define.status.success(),
        "define --apply must refuse a launch-profile-owned lane"
    );
    let stderr = String::from_utf8_lossy(&define.stderr);
    assert!(
        stderr.contains("sb profile apply"),
        "the refusal names the owning command: {stderr}"
    );
    assert_eq!(
        fs::read(&lane_record).unwrap(),
        before,
        "a refused define must not touch the record"
    );

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn claude_lane_define_enforces_fallback_count_before_writing() {
    let dir = temp_dir("claude-lane-fallbacks");
    let single_target = CLAUDE_LANE_CFG.replace("      - \"fallback/gpt-5.5\"\n", "");
    let config = write_config_text(&dir, &single_target);
    let lane_root = dir.join("lanes");
    let profile_root = dir.join("profiles");
    let output = claude_lane_define_command(&config, &lane_root, &profile_root)
        .arg("--apply")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires 1"));
    assert!(!lane_root.exists());
    assert!(!profile_root.exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn claude_lane_define_rolls_back_first_file_when_second_file_fails() {
    let dir = temp_dir("claude-lane-rollback");
    let config = write_config_text(&dir, CLAUDE_LANE_CFG);
    let lane_root = dir.join("lanes");
    let profile_root = dir.join("profiles");
    let profile = profile_root.join("gpt56-sol-ultra");
    let settings = profile.join("settings.json");
    fs::create_dir_all(&profile).unwrap();
    let original = "{\"keep\":true}\n";
    fs::write(&settings, original).unwrap();
    fs::set_permissions(&profile, fs::Permissions::from_mode(0o500)).unwrap();

    let output = claude_lane_define_command(&config, &lane_root, &profile_root)
        .arg("--apply")
        .output()
        .unwrap();
    fs::set_permissions(&profile, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("first file rolled back"));
    assert!(!lane_root.join("gpt56-sol-ultra.env").exists());
    assert_eq!(fs::read_to_string(&settings).unwrap(), original);

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
    assert!(
        clients
            .iter()
            .all(|client| client["modes"]["native_tap"]["ready"] == serde_json::json!(true)),
        "{stdout}"
    );
    assert!(clients.iter().all(|client| {
        client["fidelity"]["best_mode"] == serde_json::json!("native_tap")
            && client["fidelity"]["guarantee"] == serde_json::json!("observed_native_verbatim")
            && client["fidelity"]["native_wire_verbatim"] == serde_json::json!(true)
            && client["fidelity"]["switchback_reissues_auth"] == serde_json::json!(false)
    }));
    let codex = clients
        .iter()
        .find(|client| client["id"] == serde_json::json!("codex"))
        .unwrap();
    assert_eq!(
        codex["modes"]["native_tap"]["listener"]["id"],
        serde_json::json!("codex-tap")
    );
    assert_eq!(
        codex["modes"]["native_tap"]["listener"]["capture_bodies"],
        serde_json::json!(true)
    );

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
fn native_status_accepts_claude_oauth_account_metadata_for_native_tap() {
    let dir = temp_dir("native-status-claude-oauth-account");
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
        r#"{"mcpOauth":{"cloudflare-api":{"accessToken":"mcp-token-only"}}}"#,
    )
    .unwrap();
    fs::write(
        home.join(".claude.json"),
        r#"{"oauthAccount":{"emailAddress":"user@example.com","accountUuid":"account-secret-uuid","organizationUuid":"org-secret-uuid","billingType":"max_subscription"}}"#,
    )
    .unwrap();
    write_executable(&bin.join("codex"), "#!/bin/sh\necho codex 1.2.3\n");
    write_executable(&bin.join("claude"), "#!/bin/sh\necho claude 4.5.6\n");
    write_executable(
        &bin.join("launchctl"),
        "#!/bin/sh\nprintf 'PID\\tStatus\\tLabel\\n'\n",
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
    assert!(!stdout.contains("mcp-token-only"), "{stdout}");
    assert!(!stdout.contains("user@example.com"), "{stdout}");
    assert!(!stdout.contains("account-secret-uuid"), "{stdout}");
    assert!(!stdout.contains("org-secret-uuid"), "{stdout}");

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("native status should emit JSON");
    let clients = value["clients"].as_array().unwrap();
    let claude = clients
        .iter()
        .find(|client| client["id"] == serde_json::json!("claude-code"))
        .expect("claude-code client status");

    assert_eq!(claude["token_available"], serde_json::json!(true));
    assert_eq!(
        claude["modes"]["native_tap"]["ready"],
        serde_json::json!(true)
    );
    assert_eq!(
        claude["fidelity"]["guarantee"],
        serde_json::json!("observed_native_verbatim")
    );
    assert!(claude["token_sources"]
        .as_array()
        .unwrap()
        .iter()
        .any(|source| {
            source["kind"] == serde_json::json!("native_login_file")
                && source["label"] == serde_json::json!("${HOME}/.claude.json /oauthAccount")
                && source["available"] == serde_json::json!(true)
        }));

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn native_status_does_not_recommend_native_token_adapter_when_taps_are_ready() {
    let dir = temp_dir("native-status-tap-only");
    let config = write_config_text(&dir, NATIVE_TAP_ONLY_CFG);
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
        home.join(".claude.json"),
        r#"{"oauthAccount":{"emailAddress":"user@example.com","accountUuid":"account-secret-uuid"}}"#,
    )
    .unwrap();
    write_executable(&bin.join("codex"), "#!/bin/sh\necho codex 1.2.3\n");
    write_executable(&bin.join("claude"), "#!/bin/sh\necho claude 4.5.6\n");
    write_executable(
        &bin.join("launchctl"),
        "#!/bin/sh\nprintf 'PID\\tStatus\\tLabel\\n'\n",
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
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("native status should emit JSON");
    let clients = value["clients"].as_array().unwrap();
    assert!(clients
        .iter()
        .all(|client| client["modes"]["native_tap"]["ready"] == serde_json::json!(true)));
    assert!(clients.iter().all(|client| {
        client["fidelity"]["guarantee"] == serde_json::json!("observed_native_verbatim")
    }));
    let has_adapter_action = value["next_actions"]
        .as_array()
        .map(|actions| {
            actions.iter().any(|action| {
                action
                    .as_str()
                    .unwrap_or_default()
                    .contains("native-token-adapter")
            })
        })
        .unwrap_or(false);
    assert!(!has_adapter_action, "{value}");

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn native_verify_runs_requested_exercises_without_leaking_tokens() {
    let dir = temp_dir("native-verify-exercises");
    let config = write_config_text(&dir, NATIVE_TAP_ONLY_CFG);
    let home = dir.join("home");
    let bin = dir.join("bin");
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::create_dir_all(&bin).unwrap();
    fs::write(
        home.join(".codex/auth.json"),
        r#"{"tokens":{"access_token":"codex-verify-secret"}}"#,
    )
    .unwrap();
    write_executable(&bin.join("codex"), "#!/bin/sh\necho codex 1.2.3\n");
    write_executable(&bin.join("claude"), "#!/bin/sh\necho claude 4.5.6\n");
    write_executable(
        &bin.join("launchctl"),
        "#!/bin/sh\nprintf 'PID\\tStatus\\tLabel\\n'\n",
    );

    let output = Command::new(switchback_bin())
        .arg("--json")
        .arg("native")
        .arg("verify")
        .arg("--client")
        .arg("codex")
        .arg("--exercise")
        .arg("large-payload")
        .arg("--exercise")
        .arg("stream")
        .arg("--exercise")
        .arg("websocket")
        .arg("--large-payload-bytes")
        .arg("1048577")
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
    assert!(!stdout.contains("codex-verify-secret"), "{stdout}");
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("native verify should emit JSON");

    assert_eq!(value["schema"], "switchback/native-verify@1");
    assert_eq!(value["ok"], serde_json::json!(true));
    assert_eq!(value["client"], serde_json::json!("codex"));
    assert_eq!(
        value["exercise_scope"],
        serde_json::json!("synthetic_tap_harness")
    );
    let exercises = value["exercises"].as_array().unwrap();
    assert_eq!(exercises.len(), 3);
    for name in ["large-payload", "stream", "websocket"] {
        assert!(
            exercises
                .iter()
                .any(|exercise| exercise["name"] == serde_json::json!(name)
                    && exercise["ok"] == serde_json::json!(true)),
            "{value}"
        );
    }

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
fn native_profiles_list_and_env_report_named_profile_without_secrets() {
    let dir = temp_dir("native-profiles-list-env");
    let config = dir.join("switchback.yaml");
    fs::write(
        &config,
        r#"
server:
  bind: "127.0.0.1:18765"
  taps:
    - id: codex-tap
      bind: "127.0.0.1:18771"
      upstream: "https://chatgpt.com/backend-api/codex"
providers:
  - id: codex-relay
    type: codex_native_relay
    accounts:
      - id: work
        auth: { kind: codex_oauth }
client_profiles:
  - id: codex-work
    kind: codex
    mode: native_relay
    models: ["codex/work"]
    accounts: ["codex-relay/work"]
  - id: codex-main-tap
    kind: codex
    mode: tap
    models: ["gpt-5.5"]
routes:
  - name: codex-work
    match: { model: "codex/work" }
    targets:
      - "codex-relay/gpt-5.5"
"#,
    )
    .unwrap();

    let list = Command::new(switchback_bin())
        .arg("--json")
        .arg("native")
        .arg("profiles")
        .arg("list")
        .arg("--config")
        .arg(&config)
        .env("RUST_LOG", "info")
        .output()
        .unwrap();
    assert!(
        list.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        list.status.code(),
        String::from_utf8_lossy(&list.stdout),
        String::from_utf8_lossy(&list.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_slice(&list.stdout).expect("profiles list emits JSON");
    assert_eq!(value["schema"], "switchback/native-profiles@1");
    assert_eq!(value["ok"], serde_json::json!(true));
    let profiles = value["profiles"].as_array().unwrap();
    let relay = profiles
        .iter()
        .find(|profile| profile["id"] == serde_json::json!("codex-work"))
        .unwrap();
    assert_eq!(relay["mode"], "native_relay");
    assert_eq!(
        relay["accounts"][0]["native_relay_compatible"],
        serde_json::json!(true)
    );
    assert_eq!(
        relay["fidelity"]["guarantee"],
        serde_json::json!("native_auth_reissued")
    );
    let tap = profiles
        .iter()
        .find(|profile| profile["id"] == serde_json::json!("codex-main-tap"))
        .unwrap();
    assert_eq!(tap["mode"], "tap");
    assert_eq!(tap["ready"], serde_json::json!(true));
    assert_eq!(tap["accounts"].as_array().unwrap().len(), 0);
    assert_eq!(
        tap["fidelity"]["guarantee"],
        serde_json::json!("observed_native_verbatim")
    );
    assert_eq!(
        tap["fidelity"]["switchback_reissues_auth"],
        serde_json::json!(false)
    );

    let env = Command::new(switchback_bin())
        .arg("--json")
        .arg("native")
        .arg("profiles")
        .arg("env")
        .arg("codex-work")
        .arg("--config")
        .arg(&config)
        .env("RUST_LOG", "info")
        .output()
        .unwrap();
    assert!(
        env.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        env.status.code(),
        String::from_utf8_lossy(&env.stdout),
        String::from_utf8_lossy(&env.stderr)
    );
    let stdout = String::from_utf8_lossy(&env.stdout);
    assert!(!stdout.contains("access_token"), "{stdout}");
    assert!(!stdout.contains("refresh_token"), "{stdout}");
    let value: serde_json::Value =
        serde_json::from_slice(&env.stdout).expect("profiles env emits JSON");
    assert_eq!(value["profile"], "codex-work");
    assert_eq!(value["headers"][0]["name"], "x-switchback-client-profile");
    assert!(value["command_hint"]
        .as_str()
        .unwrap()
        .contains("--model codex/work"));

    let tap_env = Command::new(switchback_bin())
        .arg("--json")
        .arg("native")
        .arg("profiles")
        .arg("env")
        .arg("codex-main-tap")
        .arg("--config")
        .arg(&config)
        .env("RUST_LOG", "info")
        .output()
        .unwrap();
    assert!(
        tap_env.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        tap_env.status.code(),
        String::from_utf8_lossy(&tap_env.stdout),
        String::from_utf8_lossy(&tap_env.stderr)
    );
    let tap_value: serde_json::Value =
        serde_json::from_slice(&tap_env.stdout).expect("tap profile env emits JSON");
    assert_eq!(
        tap_value["fidelity"]["guarantee"],
        serde_json::json!("observed_native_verbatim")
    );
    assert_eq!(
        tap_value["base_url"],
        serde_json::json!("http://127.0.0.1:18771")
    );
    assert_eq!(
        tap_value["env"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == serde_json::json!("OPENAI_BASE_URL"))
            .unwrap()["value"],
        serde_json::json!("http://127.0.0.1:18771")
    );
    assert!(tap_value["command_hint"]
        .as_str()
        .unwrap()
        .contains("OPENAI_BASE_URL=http://127.0.0.1:18771"));

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
