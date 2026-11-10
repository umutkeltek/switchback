//! Read-only pool evidence. Native login and usage collectors retain ownership
//! of their files; Compound retains admission policy. No credential bytes leave
//! this module, and missing or old observations never become spare capacity.
use super::*;
#[cfg(unix)]
use std::io::Read;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

#[cfg(unix)]
const MAX_JSON: u64 = 4 * 1024 * 1024;

#[derive(Args, Debug)]
pub(crate) struct CapacityArgs {
    name: String,
    #[arg(long, default_value_t = 900, value_parser = clap::value_parser!(u32).range(1..=3600))]
    freshness_seconds: u32,
    #[command(flatten)]
    paths: LaunchProfilePathsArgs,
}

#[cfg(unix)]
fn read_json(path: &Path, boundary: &Path) -> anyhow::Result<Option<Value>> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    if !path.starts_with(boundary) || !boundary.is_absolute() {
        anyhow::bail!("capacity source is outside its owned root");
    }
    // Parent symlinks are refused as well as the final component. This is an
    // owner-local observation, not a sandbox against a hostile same-user writer.
    for parent in path.ancestors().skip(1) {
        let info = std::fs::symlink_metadata(parent)?;
        if !info.is_dir()
            || info.file_type().is_symlink()
            || info.mode() & 0o022 != 0
            || info.uid() != unsafe { libc::geteuid() }
        {
            anyhow::bail!("unsafe capacity source directory");
        }
        if parent == boundary {
            break;
        }
    }
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let info = file.metadata()?;
    if !info.is_file()
        || info.mode() & 0o022 != 0
        || info.uid() != unsafe { libc::geteuid() }
        || info.len() > MAX_JSON
    {
        anyhow::bail!("unsafe or oversized capacity source");
    }
    // Keep the descriptor alive through the bounded read; never reopen by name.
    let mut raw = Vec::new();
    file.take(MAX_JSON + 1).read_to_end(&mut raw)?;
    if raw.len() as u64 > MAX_JSON {
        anyhow::bail!("capacity source grew past limit");
    }
    Ok(Some(serde_json::from_slice(&raw)?))
}

#[cfg(not(unix))]
fn read_json(_path: &Path, _boundary: &Path) -> anyhow::Result<Option<Value>> {
    anyhow::bail!("native pool capacity requires an owner-checked local filesystem");
}

fn millis(value: &Value) -> Option<i64> {
    OffsetDateTime::parse(value.as_str()?, &Rfc3339)
        .ok()
        .and_then(|date| i64::try_from(date.unix_timestamp_nanos() / 1_000_000).ok())
}

fn expected_identity(account: &NativeAccount, identity: &Value) -> bool {
    let Some(observed) = &account.observed else {
        return false;
    };
    match account.provider.as_str() {
        "claude" => {
            observed
                .organization_id
                .as_ref()
                .is_some_and(|org| !org.is_empty())
                && identity["account_uuid"].as_str() == Some(&observed.account_id)
                && identity["organization_uuid"].as_str() == observed.organization_id.as_deref()
        }
        "codex" => {
            matches!(
                identity["source"].as_str(),
                Some("vendor_account_id" | "vendor_email_sha256")
            ) && identity["value"].as_str() == Some(&observed.account_id)
        }
        _ => false,
    }
}

fn quota_state(
    provider: &str,
    windows: &[Value],
    now: i64,
    freshness: i64,
) -> (&'static str, Option<i64>) {
    let required: &[&str] = if provider == "claude" {
        &["five_hour", "seven_day"]
    } else {
        &["primary", "secondary"]
    };
    let mut unknown = false;
    let mut exhausted = false;
    let mut reset = None;
    let mut unknown_reset = false;
    for name in required {
        let matched: Vec<_> = windows.iter().filter(|w| w["name"] == *name).collect();
        if matched.len() != 1 {
            unknown = true;
            continue;
        }
        let w = matched[0];
        let used = w["used_percent"]
            .as_f64()
            .filter(|n| n.is_finite() && (0.0..=100.0).contains(n));
        let fresh = millis(&w["sampled_at"])
            .is_some_and(|at| at <= now + 30_000 && at >= now.saturating_sub(freshness));
        let at = millis(&w["resets_at"]);
        let reset_valid = w["resets_at"].is_null() || at.is_some_and(|at| at > now);
        if !fresh || !reset_valid || used.is_none() {
            unknown = true;
            continue;
        }
        if used == Some(100.0) {
            exhausted = true;
            unknown_reset |= at.is_none();
            // Every exhausted window must reset before this account can recover.
            reset = match (reset, at) {
                (Some(old), Some(at)) => Some(std::cmp::max(old, at)),
                (None, at) => at,
                _ => None,
            };
        }
    }
    if exhausted {
        ("exhausted", if unknown_reset { None } else { reset })
    } else if unknown {
        ("unknown", None)
    } else {
        ("available", None)
    }
}

#[cfg(unix)]
fn codex_login_matches(home: &Path, revision: &Value) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(directory) = std::fs::symlink_metadata(home) else {
        return false;
    };
    if !directory.is_dir()
        || directory.file_type().is_symlink()
        || directory.mode() & 0o022 != 0
        || directory.uid() != unsafe { libc::geteuid() }
    {
        return false;
    }
    let Ok(info) = std::fs::symlink_metadata(home.join("auth.json")) else {
        return false;
    };
    info.is_file()
        && !info.file_type().is_symlink()
        && info.mode() & 0o022 == 0
        && info.uid() == unsafe { libc::geteuid() }
        && revision["home"].as_str() == home.to_str()
        && revision["device"].as_u64() == Some(info.dev())
        && revision["inode"].as_u64() == Some(info.ino())
        && revision["size"].as_u64() == Some(info.size())
        && revision["mtime_ns"].as_i64() == Some(info.mtime() * 1_000_000_000 + info.mtime_nsec())
        && revision["ctime_ns"].as_i64() == Some(info.ctime() * 1_000_000_000 + info.ctime_nsec())
}
#[cfg(not(unix))]
fn codex_login_matches(_home: &Path, _revision: &Value) -> bool {
    false
}

fn account_row(
    id: &str,
    account: &NativeAccount,
    config: &Path,
    user_home: &Path,
    rows: &[Value],
    now: i64,
    freshness: i64,
) -> Value {
    let mut row = json!({"id":id,"state":"unknown","reason":"no_bound_usage","windows":[],"resets_at_ms":null});
    if !account.enabled {
        row["state"] = json!("disabled");
        row["reason"] = json!("account_disabled");
        return row;
    }
    let mut cooldown_reset = None;
    let mut inspect = || -> anyhow::Result<(String, Vec<Value>)> {
        let slot_path = config
            .join("native-account-slots")
            .join(&account.provider)
            .join(id)
            .join("slot.json");
        let slot = read_json(&slot_path, config)?.unwrap_or(Value::Null);
        if slot["provider"] != account.provider
            || slot["slot"] != id
            || slot["enabled"] != true
            || !expected_identity(account, &slot["identity"])
        {
            return Ok(("slot_unavailable".into(), vec![]));
        }
        if matches!(
            slot["cooldown"]["reason"].as_str(),
            Some("authentication_failed" | "unauthorized")
        ) {
            return Ok(("authentication_required".into(), vec![]));
        }
        if !slot["cooldown"].is_null() {
            if matches!(
                slot["cooldown"]["reason"].as_str(),
                Some("rate_limit" | "rateLimitExceeded" | "usageLimitExceeded")
            ) {
                cooldown_reset = slot["cooldown"]["reset"]
                    .as_f64()
                    .filter(|v| v.is_finite() && *v > 0.0 && *v < i64::MAX as f64 / 1000.0)
                    .map(|v| (v * 1000.0) as i64);
                if cooldown_reset.is_some_and(|reset| reset > now) {
                    return Ok(("rate_limited".into(), vec![]));
                }
            }
            return Ok(("quota_refresh_required".into(), vec![]));
        }
        let matched: Vec<_> = rows
            .iter()
            .filter(|r| r["provider"] == account.provider && r["slot"] == id)
            .collect();
        if matched.len() != 1 {
            return Ok(("no_unique_usage".into(), vec![]));
        }
        let usage = matched[0];
        if !expected_identity(account, &usage["account_id"])
            || usage["account_id"] != slot["identity"]
        {
            return Ok(("identity_mismatch".into(), vec![]));
        }
        if usage["relogin_required"] == true {
            return Ok(("authentication_required".into(), vec![]));
        }
        if !usage["refresh_error"].is_null() {
            return Ok(("refresh_failed".into(), vec![]));
        }
        let (home, boundary) = match &account.native_home {
            NativeHome::Managed { label } => (config.join(&account.provider).join(label), config),
            NativeHome::ExistingDefault {} => {
                (user_home.join(format!(".{}", account.provider)), user_home)
            }
        };
        if account.provider == "claude" {
            let path = if matches!(account.native_home, NativeHome::ExistingDefault {}) {
                user_home.join(".claude.json")
            } else {
                home.join(".claude.json")
            };
            let native = read_json(&path, boundary)?.unwrap_or(Value::Null);
            if native["oauthAccount"]["accountUuid"] != slot["identity"]["account_uuid"]
                || native["oauthAccount"]["organizationUuid"]
                    != slot["identity"]["organization_uuid"]
            {
                return Ok(("identity_mismatch".into(), vec![]));
            }
        } else if !codex_login_matches(&home, &usage["login_revision"]) {
            return Ok(("login_changed_or_unverified".into(), vec![]));
        }
        // Copy only known non-secret fields; never echo arbitrary cached payloads.
        let windows = usage["windows"].as_array().into_iter().flatten().take(32).map(|w|
            json!({"name":w["name"].as_str().filter(|s|s.len()<=80),"used_percent":w["used_percent"].as_f64(),
                "sampled_at":w["sampled_at"].as_str().filter(|s|s.len()<=40),"resets_at":w["resets_at"].as_str().filter(|s|s.len()<=40)})
        ).collect();
        Ok(("observed".into(), windows))
    };
    match inspect() {
        Ok((reason, windows)) => {
            row["reason"] = json!(reason);
            if reason == "authentication_required" {
                row["state"] = json!("needs_login");
            } else if reason == "rate_limited" {
                row["state"] = json!("exhausted");
                row["resets_at_ms"] = json!(cooldown_reset);
            } else if reason == "observed" {
                let (state, reset) = quota_state(&account.provider, &windows, now, freshness);
                row["state"] = json!(state);
                row["resets_at_ms"] = json!(reset);
                if state == "unknown" {
                    row["reason"] = json!("usage_stale_or_incomplete");
                }
            }
            row["windows"] = json!(windows);
        }
        Err(_) => row["reason"] = json!("source_unavailable_or_unsafe"),
    }
    row
}

pub(super) fn run(args: CapacityArgs, json_output: bool) -> anyhow::Result<()> {
    let paths = resolve_profile_paths(&args.paths);
    let (document, revision) = load_profile_authority(&paths.authority)?;
    let pool = document
        .account_pools
        .get(&args.name)
        .ok_or_else(|| anyhow::anyhow!("unknown native pool"))?;
    let config = paths
        .authority
        .parent()
        .ok_or_else(|| anyhow::anyhow!("authority has no config root"))?;
    let runtime = config
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config has no runtime root"))?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME required"))?;
    let cache =
        read_json(&runtime.join("state/account-usage.json"), runtime)?.unwrap_or(Value::Null);
    if !cache.is_null() && cache["schema"] != "switchback/account-usage@1" {
        anyhow::bail!("usage schema mismatch");
    }
    let rows = cache["accounts"].as_array().cloned().unwrap_or_default();
    let now = i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)?;
    let accounts: Vec<_> = pool
        .members
        .iter()
        .map(|id| {
            account_row(
                id,
                &document.accounts[id],
                config,
                &home,
                &rows,
                now,
                i64::from(args.freshness_seconds) * 1000,
            )
        })
        .collect();
    let counts = [
        "available",
        "exhausted",
        "needs_login",
        "disabled",
        "unknown",
    ]
    .into_iter()
    .map(|state| {
        (
            state,
            accounts.iter().filter(|row| row["state"] == state).count(),
        )
    })
    .collect::<BTreeMap<_, _>>();
    let state = if counts["available"] > 0 {
        "available"
    } else if counts["unknown"] > 0 {
        "unknown"
    } else if counts["exhausted"] > 0 {
        "exhausted"
    } else {
        "unavailable"
    };
    // Do not publish a mixed membership snapshot if enrollment changed mid-read.
    verify_revision(&load_profile_authority(&paths.authority)?.1, &revision)?;
    emit(
        &json!({"schema":"switchback/native-pool-capacity@1","pool":args.name,"provider":pool.provider,
        "authority_revision":revision,"generated_at_ms":now,"freshness_seconds":args.freshness_seconds,
        "state":state,"counts":counts,"accounts":accounts,"model_scope":"general",
        "purpose":"capacity_observation_not_admission_authority"}),
        json_output,
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn windows(now: i64) -> Vec<Value> {
        let iso = |ms| {
            OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
                .unwrap()
                .format(&Rfc3339)
                .unwrap()
        };
        vec![
            json!({"name":"five_hour","used_percent":25,"sampled_at":iso(now),"resets_at":iso(now+3600000)}),
            json!({"name":"seven_day","used_percent":40,"sampled_at":iso(now),"resets_at":iso(now+86400000)}),
        ]
    }

    #[test]
    fn capacity_requires_fresh_complete_windows_and_does_not_assume_a_reset() {
        let now = 1_800_000_000_000;
        let mut rows = windows(now);
        assert_eq!(quota_state("claude", &rows, now, 900000).0, "available");
        assert_eq!(
            quota_state("claude", &rows, now + 900001, 900000).0,
            "unknown"
        );
        assert_eq!(
            quota_state("claude", &rows, now - 30001, 900000).0,
            "unknown"
        );
        rows[0]["used_percent"] = json!(100);
        assert_eq!(
            quota_state("claude", &rows, now, 900000),
            ("exhausted", Some(now + 3600000))
        );
        assert_eq!(
            quota_state("claude", &rows, now + 3600000, 7200000).0,
            "unknown"
        );
        rows[1]["used_percent"] = json!(100);
        assert_eq!(
            quota_state("claude", &rows, now, 900000).1,
            Some(now + 86400000)
        );
        rows[0]["resets_at"] = Value::Null;
        assert_eq!(
            quota_state("claude", &rows, now, 900000),
            ("exhausted", None)
        );
        rows[0]["used_percent"] = json!(true);
        rows[1]["used_percent"] = json!(40);
        assert_eq!(quota_state("claude", &rows, now, 900000).0, "unknown");
        assert_eq!(quota_state("claude", &[], now, 900000).0, "unknown");
        assert_eq!(
            quota_state("claude", &[rows[1].clone(), rows[1].clone()], now, 900000).0,
            "unknown"
        );
    }

    struct Fixture {
        root: PathBuf,
        config: PathBuf,
        account: NativeAccount,
        slot: Value,
        usage: Value,
        now: i64,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "sb-pool-capacity-{}-{}",
                std::process::id(),
                OffsetDateTime::now_utc().unix_timestamp_nanos()
            ));
            fs::create_dir_all(&root).unwrap();
            let root = root.canonicalize().unwrap();
            let config = root.join("config");
            fs::create_dir_all(config.join("native-account-slots/claude/a")).unwrap();
            fs::create_dir_all(config.join("claude/a")).unwrap();
            let identity = json!({"account_uuid":"person","organization_uuid":"org"});
            let account = serde_json::from_value(json!({"provider":"claude","label":"Synthetic",
                "native_home":{"kind":"managed","label":"a"},"observed":{"account_id":"person","organization_id":"org","observed_at_ms":1}})).unwrap();
            let now = 1_800_000_000_000;
            let slot = json!({"provider":"claude","slot":"a","enabled":true,"identity":identity});
            let usage = json!({"provider":"claude","slot":"a","account_id":identity,"windows":windows(now)});
            let fixture = Self {
                root,
                config,
                account,
                slot,
                usage,
                now,
            };
            fixture.save_slot();
            fixture.native_identity("org");
            fixture
        }
        fn save_slot(&self) {
            fs::write(
                self.config.join("native-account-slots/claude/a/slot.json"),
                serde_json::to_vec(&self.slot).unwrap(),
            )
            .unwrap();
        }
        fn native_identity(&self, org: &str) {
            fs::write(self.config.join("claude/a/.claude.json"), serde_json::to_vec(&json!({"oauthAccount":{"accountUuid":"person","organizationUuid":org},"secret":"never-output-this"})).unwrap()).unwrap();
        }
        fn row(&self) -> Value {
            account_row(
                "a",
                &self.account,
                &self.config,
                &self.root,
                std::slice::from_ref(&self.usage),
                self.now,
                900000,
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).unwrap();
        }
    }

    #[test]
    fn capacity_binds_enrollment_slot_usage_and_live_native_identity() {
        let mut f = Fixture::new();
        assert_eq!(f.row()["state"], "available");
        assert!(!f.row().to_string().contains("never-output-this"));
        f.usage["account_id"]["organization_uuid"] = json!("other");
        assert_eq!(f.row()["reason"], "identity_mismatch");
        f.usage["account_id"] = f.slot["identity"].clone();
        f.native_identity("other");
        assert_eq!(f.row()["reason"], "identity_mismatch");
        f.native_identity("org");
        f.slot["cooldown"] = json!({"reason":"authentication_failed","reset":null});
        f.save_slot();
        assert_eq!(f.row()["state"], "needs_login");
        f.account.enabled = false;
        assert_eq!(f.row()["state"], "disabled");
    }

    #[test]
    fn capacity_rejects_unsafe_sources_and_refresh_failure() {
        let mut f = Fixture::new();
        f.usage["refresh_error"] = json!("timeout");
        assert_eq!(f.row()["state"], "unknown");
        f.usage.as_object_mut().unwrap().remove("refresh_error");
        let path = f.config.join("claude/a/.claude.json");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        assert_eq!(f.row()["reason"], "source_unavailable_or_unsafe");
        fs::remove_file(&path).unwrap();
        let other = f.root.join("private.json");
        fs::write(&other, "{}").unwrap();
        symlink(other, path).unwrap();
        assert_eq!(f.row()["reason"], "source_unavailable_or_unsafe");
    }

    #[test]
    fn latest_rejection_overrides_prior_available_usage_until_fresh_recovery() {
        let mut f = Fixture::new();
        f.slot["cooldown"] = json!({"reason":"rate_limit","reset":(f.now+3600000)/1000});
        f.save_slot();
        assert_eq!(f.row()["state"], "exhausted");
        assert_eq!(f.row()["resets_at_ms"], f.now + 3600000);
        f.now += 3600000;
        assert_eq!(f.row()["state"], "unknown");
        f.slot["cooldown"]["reset"] = Value::Null;
        f.save_slot();
        assert_eq!(f.row()["reason"], "quota_refresh_required");
    }
}
