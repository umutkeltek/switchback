use super::super::profile_transaction;
use super::*;

const ALLOWED: &[&str] = &[
    "permissions",
    "hooks",
    "enabledPlugins",
    "extraKnownMarketplaces",
    "statusLine",
    "autoMode",
    "skipWorkflowUsageWarning",
    "skipAutoPermissionPrompt",
    "skipDangerousModePermissionPrompt",
    "alwaysThinkingEnabled",
    "language",
    "respectGitignore",
    "autoUpdatesChannel",
    "cleanupPeriodDays",
    "spinnerTipsEnabled",
];
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SharedSettings {
    pub(super) settings: BTreeMap<String, Value>,
}
pub(super) fn validate_rules(document: &LaunchProfilesDocument) -> anyhow::Result<()> {
    for (id, rule) in &document.native_shared_settings {
        safe_name(id, "native shared settings")?;
        for key in rule.settings.keys() {
            if !ALLOWED.contains(&key.as_str()) {
                anyhow::bail!("shared setting {key} is not allowlisted");
            }
        }
    }
    for spec in document.launch_profiles.values() {
        if let Some(id) = &spec.native_shared_settings {
            if spec.native_target.is_none() {
                anyhow::bail!("native_shared_settings requires native_target");
            }
            if !document.native_shared_settings.contains_key(id) {
                anyhow::bail!("unknown native shared settings {id}");
            }
        }
    }
    Ok(())
}
#[derive(Subcommand, Debug)]
pub(crate) enum SettingsCommand {
    Policy {
        #[command(subcommand)]
        action: SharedSettingsCommand,
    },
    Plan(SettingsArgs),
    Status(SettingsArgs),
    Apply(SettingsApplyArgs),
    Resolve(SettingsResolveArgs),
    Recover(SettingsArgs),
}
#[derive(Args, Debug)]
pub(crate) struct SettingsArgs {
    name: String,
    #[arg(long)]
    account: String,
    /// Explicit operator operation; automatic run sync must never set this.
    #[arg(long)]
    allow_existing_default: bool,
    #[command(flatten)]
    paths: LaunchProfilePathsArgs,
}
#[derive(Args, Debug)]
pub(crate) struct SettingsApplyArgs {
    #[command(flatten)]
    target: SettingsArgs,
    /// profile_revision returned by settings plan (not authority_revision).
    #[arg(long)]
    expected_revision: String,
    /// settings_revision returned by settings plan; "missing" for a new file.
    #[arg(long)]
    expected_settings_revision: String,
}
#[derive(Args, Debug)]
pub(crate) struct SettingsResolveArgs {
    #[command(flatten)]
    apply: SettingsApplyArgs,
    /// Resolve only these explicit conflicting root keys.
    #[arg(long, required = true)]
    key: Vec<String>,
    #[arg(long, value_enum)]
    prefer: Resolution,
}
#[derive(Clone, Copy, Debug, ValueEnum)]
enum Resolution {
    Source,
    Local,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Baseline {
    #[serde(default)]
    values: BTreeMap<String, Option<Value>>,
}
struct Plan {
    report: Value,
    artifacts: Vec<PlannedProfileArtifact>,
    settings_revision: String,
    profile_revision: String,
    existing_default: bool,
}
fn bytes(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_file() {
                anyhow::bail!("settings input must be a regular file");
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if meta.mode() & 0o022 != 0 {
                    anyhow::bail!("settings input is writable by other users");
                }
            }
            let data = std::fs::read(path)?;
            if data.len() > 1024 * 1024 {
                anyhow::bail!("settings input exceeds limit");
            }
            Ok(Some(data))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn revision(data: Option<&[u8]>) -> String {
    match data {
        Some(bytes) => sha256_hex(bytes),
        None => "missing".into(),
    }
}
fn safe_home(path: &Path) -> anyhow::Result<()> {
    for ancestor in path.ancestors() {
        match std::fs::symlink_metadata(ancestor) {
            Ok(meta) => {
                if meta.file_type().is_symlink() || !meta.is_dir() {
                    anyhow::bail!("unsafe native settings home");
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
fn merge(
    current: &Map<String, Value>,
    baseline: &Baseline,
    desired: &BTreeMap<String, Value>,
    resolution: Option<(&BTreeSet<String>, Resolution)>,
) -> (Map<String, Value>, Baseline, Vec<Value>) {
    let mut result = current.clone();
    let mut next = baseline.clone();
    let mut conflicts = Vec::new();
    let keys = baseline
        .values
        .keys()
        .chain(desired.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for key in keys {
        let cur = current.get(&key).cloned();
        let want = desired.get(&key).cloned();
        let prior = baseline.values.get(&key);
        let conflict = match prior {
            Some(old) => cur != *old && want != *old && cur != want,
            None => cur.is_some() && cur != want,
        };
        let choice = resolution.and_then(|(keys, prefer)| keys.contains(&key).then_some(prefer));
        if conflict && choice.is_none() {
            conflicts.push(json!({"key":key,"baseline":prior,"current":cur,"desired":want}));
            continue;
        }
        if conflict && matches!(choice, Some(Resolution::Local)) {
            // Accept this source generation while preserving the local value.
            next.values.insert(key, want);
            continue;
        }
        if conflict || prior.is_none() || prior.is_some_and(|old| cur == *old) || cur == want {
            match &want {
                Some(value) => {
                    result.insert(key.clone(), value.clone());
                }
                None => {
                    result.remove(&key);
                }
            }
        }
        next.values.insert(key, want);
    }
    (result, next, conflicts)
}
fn plan(
    args: &SettingsArgs,
    resolution: Option<(&BTreeSet<String>, Resolution)>,
) -> anyhow::Result<Plan> {
    let paths = resolve_profile_paths(&args.paths);
    let (document, authority_revision) = load_profile_authority(&paths.authority)?;
    let profile = native_profile(&document, &args.name)?;
    let spec = &document.launch_profiles[&args.name];
    let rule_id = spec
        .native_shared_settings
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("profile has no native_shared_settings rule"))?;
    let rule = &document.native_shared_settings[rule_id];
    let target = spec.native_target.as_ref().expect("native target checked");
    let pool = &document.account_pools[&target.pool];
    if !pool.members.contains(&args.account) {
        anyhow::bail!("settings target is outside the profile pool");
    }
    let account = &document.accounts[&args.account];
    if account.provider != "claude" {
        anyhow::bail!("native shared settings currently supports Claude only");
    }
    let (home, existing_default) = match &account.native_home {
        NativeHome::Managed { label } => (
            paths
                .authority
                .parent()
                .ok_or_else(|| anyhow::anyhow!("authority has no config parent"))?
                .join("claude")
                .join(label),
            false,
        ),
        NativeHome::ExistingDefault {} => {
            let home = std::env::var_os("HOME").ok_or_else(|| {
                anyhow::anyhow!("HOME is required for default settings reference")
            })?;
            (PathBuf::from(home).join(".claude"), true)
        }
    };
    safe_home(&home)?;
    let path = home.join("settings.json");
    let baseline_path = home.join(".switchback-shared-settings-baseline.json");
    let raw = bytes(&path)?;
    let settings_revision = revision(raw.as_deref());
    let current: Value = match &raw {
        Some(bytes) => serde_json::from_slice(bytes)?,
        None => json!({}),
    };
    let current = current
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("native settings must be an object"))?;
    let baseline_raw = bytes(&baseline_path)?;
    let baseline: Baseline = match baseline_raw.as_deref() {
        Some(bytes) => serde_json::from_slice(bytes)?,
        None => Baseline::default(),
    };
    if let Some((keys, _)) = resolution {
        for key in keys {
            if !ALLOWED.contains(&key.as_str())
                || (!rule.settings.contains_key(key) && !baseline.values.contains_key(key))
            {
                anyhow::bail!("resolution key is outside the managed shared settings");
            }
        }
    }
    let (effective, next, conflicts) = merge(current, &baseline, &rule.settings, resolution);
    let mut settings_body = serde_json::to_string_pretty(&effective)?;
    settings_body.push('\n');
    let mut baseline_body = serde_json::to_string_pretty(&next)?;
    baseline_body.push('\n');
    let profile_revision = stable_json_revision(&profile)?;
    let mut report = json!({"schema":"switchback/native-shared-settings-plan@1","profile":args.name,"account":args.account,
        "authority_revision":authority_revision,"profile_revision":profile_revision,"policy_ref":rule_id,
        "policy_revision":stable_json_revision(&serde_json::to_value(rule)?)?,"settings_revision":settings_revision,
        "effective_revision":revision(Some(settings_body.as_bytes())),"existing_default":existing_default,
        "automatic_apply_allowed":!existing_default,"conflicts":conflicts,"conflict_count":conflicts.len(),
        "changed":raw.as_deref()!=Some(settings_body.as_bytes()),
        "baseline_changed":baseline_raw.as_deref()!=Some(baseline_body.as_bytes()),"managed_keys":next.values.keys().collect::<Vec<_>>()});
    let artifacts = vec![
        PlannedProfileArtifact {
            kind: "native_shared_settings",
            path,
            contents: settings_body,
            mode: 0o600,
            comparison: ArtifactComparison::CanonicalJson,
        },
        PlannedProfileArtifact {
            kind: "native_shared_settings_baseline",
            path: baseline_path,
            contents: baseline_body,
            mode: 0o600,
            comparison: ArtifactComparison::CanonicalJson,
        },
    ];
    report["pending_recovery"] = json!(profile_transaction::pending(&artifacts)?);
    Ok(Plan {
        report,
        artifacts,
        settings_revision,
        profile_revision,
        existing_default,
    })
}
fn apply(
    args: SettingsApplyArgs,
    resolution: Option<(&BTreeSet<String>, Resolution)>,
    json_output: bool,
) -> anyhow::Result<()> {
    let plan = plan(&args.target, resolution)?;
    verify_revision(&plan.profile_revision, &args.expected_revision)?;
    if plan.settings_revision != args.expected_settings_revision {
        anyhow::bail!("settings revision conflict");
    }
    if plan.existing_default && !args.target.allow_existing_default {
        anyhow::bail!("default settings require explicit --allow-existing-default");
    }
    if plan.report["conflict_count"] != 0 {
        emit(&plan.report, json_output)?;
        anyhow::bail!("shared settings conflicts require explicit resolution");
    }
    profile_transaction::apply(&plan.artifacts, || {
        for artifact in &plan.artifacts {
            if bytes(&artifact.path)?.as_deref() != Some(artifact.contents.as_bytes()) {
                anyhow::bail!("shared settings post-apply verification failed");
            }
        }
        Ok(())
    })?;
    emit(&json!({"applied":true,"plan":plan.report}), json_output)
}
pub(crate) fn run(action: SettingsCommand, json_output: bool) -> anyhow::Result<()> {
    match action {
        SettingsCommand::Policy { action } => policies(action, json_output),
        SettingsCommand::Plan(args) | SettingsCommand::Status(args) => {
            emit(&plan(&args, None)?.report, json_output)
        }
        SettingsCommand::Apply(args) => apply(args, None, json_output),
        SettingsCommand::Resolve(args) => {
            let keys = args.key.into_iter().collect::<BTreeSet<_>>();
            apply(args.apply, Some((&keys, args.prefer)), json_output)
        }
        SettingsCommand::Recover(args) => {
            let plan = plan(&args, None)?;
            if plan.existing_default && !args.allow_existing_default {
                anyhow::bail!(
                    "default settings recovery requires explicit --allow-existing-default"
                );
            }
            let report = profile_transaction::recover(&plan.artifacts)?;
            emit(&report, json_output)?;
            if report["recovered"] == false
                && report["conflicts"]
                    .as_array()
                    .is_some_and(|x| !x.is_empty())
            {
                anyhow::bail!(
                    "recovery preserved later edits; explicit conflict resolution is required"
                );
            }
            Ok(())
        }
    }
}

#[derive(Subcommand, Debug)]
pub(crate) enum SharedSettingsCommand {
    List(LaunchProfilePathsArgs),
    Show(LaunchProfileNameArgs),
    Plan(PoolPlanArgs),
    Apply(AccountWriteArgs),
}
fn policies(action: SharedSettingsCommand, json_output: bool) -> anyhow::Result<()> {
    match action {
        SharedSettingsCommand::List(args) => {
            let (document, revision) =
                load_profile_authority(&resolve_profile_paths(&args).authority)?;
            emit(
                &json!({"authority_revision":revision,"native_shared_settings":document.native_shared_settings}),
                json_output,
            )
        }
        SharedSettingsCommand::Show(args) => {
            let (document, revision) =
                load_profile_authority(&resolve_profile_paths(&args.paths).authority)?;
            let rule = document
                .native_shared_settings
                .get(&args.name)
                .ok_or_else(|| anyhow::anyhow!("unknown shared settings policy"))?;
            emit(
                &json!({"authority_revision":revision,"id":args.name,"policy":rule}),
                json_output,
            )
        }
        SharedSettingsCommand::Plan(args) => {
            safe_name(&args.name, "shared settings policy")?;
            let (mut document, revision) =
                load_profile_authority(&resolve_profile_paths(&args.paths).authority)?;
            let rule: SharedSettings = serde_json::from_str(&args.spec_json)?;
            document
                .native_shared_settings
                .insert(args.name.clone(), rule);
            validate(&document)?;
            emit(
                &json!({"authority_revision":revision,"id":args.name,"policy":document.native_shared_settings[&args.name],"applied":false}),
                json_output,
            )
        }
        SharedSettingsCommand::Apply(args) => {
            safe_name(&args.name, "shared settings policy")?;
            let rule: SharedSettings = serde_json::from_str(&args.spec_json)?;
            let path = resolve_profile_paths(&args.paths).authority;
            let _lock = lock_authority(&path)?;
            let (mut document, revision) = load_profile_authority(&path)?;
            verify_revision(&revision, &args.expected_revision)?;
            document
                .native_shared_settings
                .insert(args.name.clone(), rule);
            let revision = persist(&path, &document)?;
            emit(
                &json!({"authority_revision":revision,"id":args.name,"policy":document.native_shared_settings[&args.name],"applied":true}),
                json_output,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn object(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }
    #[test]
    fn shared_three_way_preserves_local_edits_and_reports_both_changed() {
        let baseline = Baseline {
            values: BTreeMap::from([
                ("language".into(), Some(json!("en"))),
                ("autoMode".into(), Some(json!(false))),
            ]),
        };
        let current = object(json!({"language":"tr","autoMode":false,"unowned":"keep"}));
        let desired = BTreeMap::from([
            ("language".into(), json!("de")),
            ("autoMode".into(), json!(true)),
        ]);
        let (effective, _, conflicts) = merge(&current, &baseline, &desired, None);
        assert_eq!(effective["language"], "tr");
        assert_eq!(effective["autoMode"], true);
        assert_eq!(effective["unowned"], "keep");
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0]["key"], "language");
    }
    #[test]
    fn shared_first_apply_does_not_clobber_existing_unadopted_values() {
        let current = object(json!({"language":"tr"}));
        let desired = BTreeMap::from([("language".into(), json!("en"))]);
        let (effective, _, conflicts) = merge(&current, &Baseline::default(), &desired, None);
        assert_eq!(effective["language"], "tr");
        assert_eq!(conflicts.len(), 1);
    }
    #[test]
    fn shared_source_deletion_and_local_only_change_are_preserved_correctly() {
        let baseline = Baseline {
            values: BTreeMap::from([
                ("language".into(), Some(json!("en"))),
                ("autoMode".into(), Some(json!(false))),
            ]),
        };
        let current = object(json!({"language":"en","autoMode":true}));
        let desired = BTreeMap::from([("autoMode".into(), json!(false))]);
        let (effective, _, conflicts) = merge(&current, &baseline, &desired, None);
        assert!(effective.get("language").is_none());
        assert_eq!(effective["autoMode"], true);
        assert!(conflicts.is_empty());
    }
    #[test]
    fn shared_resolution_requires_explicit_conflicting_keys() {
        let current = object(json!({"language":"tr"}));
        let baseline = Baseline::default();
        let desired = BTreeMap::from([("language".into(), json!("en"))]);
        let keys = BTreeSet::from(["language".to_string()]);
        let (effective, _, conflicts) = merge(
            &current,
            &baseline,
            &desired,
            Some((&keys, Resolution::Source)),
        );
        assert_eq!(effective["language"], "en");
        assert!(conflicts.is_empty());
        let (effective, next, _) = merge(
            &current,
            &baseline,
            &desired,
            Some((&keys, Resolution::Local)),
        );
        assert_eq!(effective["language"], "tr");
        assert!(merge(&effective, &next, &desired, None).2.is_empty());
    }
    #[test]
    fn shared_rules_exclude_auth_provider_env_and_native_homes() {
        for key in [
            "env",
            "apiKeyHelper",
            "awsAuthRefresh",
            "CLAUDE_CONFIG_DIR",
            "oauthAccount",
        ] {
            let mut document:LaunchProfilesDocument=serde_json::from_value(json!({"schema":LAUNCH_PROFILES_SCHEMA,"provider_lanes":{},"harness_presets":{},"capture_policies":{},"launch_profiles":{}})).unwrap();
            document.native_shared_settings.insert(
                "common".into(),
                SharedSettings {
                    settings: BTreeMap::from([(key.into(), json!("forbidden"))]),
                },
            );
            assert!(validate_rules(&document).is_err());
        }
    }
}

#[cfg(test)]
mod filesystem_tests {
    use super::*;
    use std::fs;
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "sb-profile-settings-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(fs::canonicalize(path).unwrap())
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn args(path: &Path) -> SettingsArgs {
        SettingsArgs {
            name: "native".into(),
            account: "a".into(),
            allow_existing_default: false,
            paths: LaunchProfilePathsArgs {
                authority: Some(path.to_path_buf()),
                lane_root: None,
                profile_root: None,
                wrapper_root: None,
                projection_root: None,
            },
        }
    }
    fn document() -> Value {
        json!({
            "schema":LAUNCH_PROFILES_SCHEMA,"provider_lanes":{},
            "harness_presets":{"native":{"harness":"claude-code","native_effort":"default","permissions_mode":"pinned","mcp_mode":"none","skills_mode":"disabled","settings_mode":"pinned"}},
            "capture_policies":{"capture":{"mode":"metadata_only"}},
            "launch_profiles":{"native":{"native_target":{"pool":"team"},"harness_preset":"native","capture_policy":"capture","native_shared_settings":"common"}},
            "accounts":{"a":{"provider":"claude","label":"A","native_home":{"kind":"managed","label":"a"}}},
            "account_pools":{"team":{"provider":"claude","members":["a"],"selection":"ordered","continuation":"same_account","unknown_quota":"allow_attempt"}},
            "native_shared_settings":{"common":{"settings":{"language":"en"}}}
        })
    }
    fn write_document(path: &Path, value: &Value) {
        fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
    }
    #[test]
    fn native_settings_owner_applies_and_resolves_real_three_way_conflicts() {
        let root = Temp::new();
        let authority = root.0.join("launch-profiles.json");
        let mut doc = document();
        write_document(&authority, &doc);
        let first = plan(&args(&authority), None).unwrap();
        assert_eq!(first.report["conflict_count"], 0);
        let desired = first.artifacts[0].contents.as_bytes();
        let expected = format!("sha256:{:x}", Sha256::digest(desired));
        assert_eq!(first.report["effective_revision"], expected);
        apply(
            SettingsApplyArgs {
                target: args(&authority),
                expected_revision: first.profile_revision,
                expected_settings_revision: first.settings_revision,
            },
            None,
            true,
        )
        .unwrap();
        let settings = root.0.join("claude/a/settings.json");
        assert_eq!(
            plan(&args(&authority), None).unwrap().settings_revision,
            expected
        );
        let mut local: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
        local["language"] = json!("tr");
        local["unowned"] = json!("preserve");
        fs::write(&settings, serde_json::to_vec(&local).unwrap()).unwrap();
        doc["native_shared_settings"]["common"]["settings"]["language"] = json!("de");
        write_document(&authority, &doc);
        let conflict = plan(&args(&authority), None).unwrap();
        assert_eq!(conflict.report["conflict_count"], 1);
        let before = fs::read(&settings).unwrap();
        assert!(apply(
            SettingsApplyArgs {
                target: args(&authority),
                expected_revision: conflict.profile_revision.clone(),
                expected_settings_revision: conflict.settings_revision.clone()
            },
            None,
            true
        )
        .is_err());
        assert_eq!(fs::read(&settings).unwrap(), before);
        let keys = BTreeSet::from(["language".to_string()]);
        apply(
            SettingsApplyArgs {
                target: args(&authority),
                expected_revision: conflict.profile_revision,
                expected_settings_revision: conflict.settings_revision,
            },
            Some((&keys, Resolution::Local)),
            true,
        )
        .unwrap();
        let after: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
        assert_eq!(after["language"], "tr");
        assert_eq!(after["unowned"], "preserve");
        assert_eq!(
            plan(&args(&authority), None).unwrap().report["conflict_count"],
            0
        );
    }
    #[test]
    fn native_settings_status_reports_baseline_adoption_without_byte_changes() {
        let root = Temp::new();
        let authority = root.0.join("launch-profiles.json");
        write_document(&authority, &document());
        let settings = root.0.join("claude/a/settings.json");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        let mut body = serde_json::to_string_pretty(&json!({"language":"en"})).unwrap();
        body.push('\n');
        fs::write(&settings, body).unwrap();
        let before = plan(&args(&authority), None).unwrap();
        assert_eq!(before.report["changed"], false);
        assert_eq!(before.report["baseline_changed"], true);
        assert_eq!(before.report["pending_recovery"], false);
        apply(
            SettingsApplyArgs {
                target: args(&authority),
                expected_revision: before.profile_revision,
                expected_settings_revision: before.settings_revision,
            },
            None,
            true,
        )
        .unwrap();
        let after = plan(&args(&authority), None).unwrap();
        assert_eq!(after.report["changed"], false);
        assert_eq!(after.report["baseline_changed"], false);
        assert_eq!(after.report["pending_recovery"], false);
    }
    #[test]
    fn native_settings_status_reports_pending_journal_and_live_mutex() {
        let root = Temp::new();
        let authority = root.0.join("launch-profiles.json");
        write_document(&authority, &document());
        let before = plan(&args(&authority), None).unwrap();
        let journal = profile_transaction::journal_path(&before.artifacts).unwrap();
        fs::create_dir_all(journal.parent().unwrap()).unwrap();
        fs::write(&journal, "pending journal").unwrap();
        assert_eq!(
            plan(&args(&authority), None).unwrap().report["pending_recovery"],
            true
        );
        fs::remove_file(&journal).unwrap();
        let mutex = journal.with_extension("lock");
        fs::create_dir(&mutex).unwrap();
        assert_eq!(
            plan(&args(&authority), None).unwrap().report["pending_recovery"],
            true
        );
        fs::remove_dir(mutex).unwrap();
        assert_eq!(
            plan(&args(&authority), None).unwrap().report["pending_recovery"],
            false
        );
    }
    #[test]
    fn native_settings_stale_revision_never_publishes() {
        let root = Temp::new();
        let authority = root.0.join("launch-profiles.json");
        write_document(&authority, &document());
        let before = plan(&args(&authority), None).unwrap();
        assert!(apply(
            SettingsApplyArgs {
                target: args(&authority),
                expected_revision: before.profile_revision,
                expected_settings_revision: "sha256:stale".into()
            },
            None,
            true
        )
        .is_err());
        assert!(!root.0.join("claude/a/settings.json").exists());
    }
}
