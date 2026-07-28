#![cfg(target_os = "macos")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

fn temp_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "switchback-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn assert_success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn installed_launcher_owns_runtime_from_any_working_directory() {
    let dir = temp_dir("installer-launcher");
    let home = dir.join("home");
    let checkout = dir.join("checkout");
    let runtime = checkout.join(".switchback");
    let prefix = home.join("bin");
    let fake_engine = dir.join("fake-switchback");
    let fake_log = dir.join("launcher.log");
    let elsewhere = dir.join("elsewhere");
    let legacy_env = dir.join("legacy.env");
    for path in [&home, &checkout, &elsewhere] {
        fs::create_dir_all(path).unwrap();
    }
    write_executable(
        &fake_engine,
        r#"#!/bin/zsh
set -euo pipefail
if [[ "$*" == "--version" ]]; then
  print -r -- "switchback 0.1.0-test"
  exit 0
fi
if [[ "$*" == *"setup --root"* ]]; then
  root="${@: -1}"
  mkdir -p "$root"/{config,state/body,eval,receipts,bin,backups}
  print -r -- '{"schema":"switchback/runtime-manifest@1","owner":"switchback"}' > "$root/manifest.json"
  exit 0
fi
{
  print -r -- "runtime=${SWITCHBACK_RUNTIME_ROOT:-}"
  print -r -- "runtime_alias=${SB_RUNTIME_ROOT:-}"
  print -r -- "runtime_env=${SB_LAUNCHER_SENTINEL:-}"
  print -r -- "legacy_env=${SB_LEGACY_SENTINEL:-}"
  print -r -- "cwd=$PWD"
  print -r -- "args=$*"
} > "${FAKE_LOG:?FAKE_LOG is required}"
"#,
    );

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let installer = manifest_dir.join("../../cli/install.sh");
    let installed = Command::new("/bin/zsh")
        .arg(&installer)
        .env("HOME", &home)
        .env("SWITCHBACK_ROOT", &checkout)
        .env("SWITCHBACK_RUNTIME_ROOT", &runtime)
        .env("PREFIX", &prefix)
        .env("SB_BIN", &fake_engine)
        .env("SB_BUILD_COMMIT", "0123456789abcdef0123456789abcdef01234567")
        .output()
        .unwrap();
    assert_success(&installed);
    assert!(runtime.join("bin/switchback").is_file());
    assert!(
        runtime.join("bin/switchback-bin").is_file(),
        "installer did not separate launcher from engine"
    );
    let provenance_path = runtime.join("bin/install-provenance.json");
    let provenance: serde_json::Value =
        serde_json::from_slice(&fs::read(&provenance_path).unwrap()).unwrap();
    let installed_engine = runtime.join("bin/switchback-bin");
    let expected_sha = format!("{:x}", Sha256::digest(fs::read(&installed_engine).unwrap()));
    assert_eq!(provenance["schema"], "switchback/install-provenance@1");
    assert_eq!(provenance["version"], "switchback 0.1.0-test");
    assert_eq!(
        provenance["git_commit"],
        "0123456789abcdef0123456789abcdef01234567"
    );
    assert_eq!(
        provenance["source_engine"],
        fs::canonicalize(&fake_engine)
            .unwrap()
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(
        provenance["installed_engine"],
        fs::canonicalize(&installed_engine)
            .unwrap()
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(provenance["sha256"], expected_sha);
    assert_eq!(
        fs::metadata(&provenance_path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    fs::write(
        runtime.join("config/sb.env"),
        "export SB_LAUNCHER_SENTINEL=runtime-owned\n",
    )
    .unwrap();
    fs::write(&legacy_env, "export SB_LEGACY_SENTINEL=legacy-opt-in\n").unwrap();
    let launched = Command::new(prefix.join("switchback"))
        .args(["probe", "--flag"])
        .current_dir(&elsewhere)
        .env("HOME", &home)
        .env_remove("SWITCHBACK_RUNTIME_ROOT")
        .env_remove("SB_RUNTIME_ROOT")
        .env("SWITCHBACK_LEGACY_ENV", &legacy_env)
        .env("FAKE_LOG", &fake_log)
        .output()
        .unwrap();
    assert_success(&launched);
    let log = fs::read_to_string(&fake_log).unwrap();
    let canonical_runtime = fs::canonicalize(&runtime).unwrap();
    let canonical_elsewhere = fs::canonicalize(&elsewhere).unwrap();
    for expected in [
        format!("runtime={}", canonical_runtime.display()),
        format!("runtime_alias={}", canonical_runtime.display()),
        "runtime_env=runtime-owned".to_string(),
        "legacy_env=legacy-opt-in".to_string(),
        format!("cwd={}", canonical_elsewhere.display()),
        "args=probe --flag".to_string(),
    ] {
        assert!(log.contains(&expected), "missing {expected:?} in:\n{log}");
    }

    fs::remove_dir_all(dir).unwrap();
}
