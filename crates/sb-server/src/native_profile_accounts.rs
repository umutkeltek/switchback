use super::*;
#[path = "native_profile_settings.rs"]
pub(super) mod settings;
pub(super) use settings::SharedSettings;
#[path = "native_pool_capacity.rs"]
mod capacity;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NativeAccount {
    provider: String,
    label: String,
    #[serde(default = "enabled")]
    enabled: bool,
    native_home: NativeHome,
    #[serde(default)]
    declared: DeclaredAccountMetadata,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    observed: Option<ObservedAccountIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider_account_ref: Option<String>,
}
fn enabled() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum NativeHome {
    Managed { label: String },
    ExistingDefault {},
}
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DeclaredAccountMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    organization_label: Option<String>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ObservedAccountIdentity {
    account_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    organization_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plan: Option<String>,
    observed_at_ms: i64,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NativePool {
    provider: String,
    members: Vec<String>,
    selection: PoolSelection,
    continuation: PoolContinuation,
    unknown_quota: UnknownQuota,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum PoolSelection {
    Ordered,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum PoolContinuation {
    SameAccount,
    SharedTranscript,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum UnknownQuota {
    AllowAttempt,
    Refuse,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NativeTarget {
    pool: String,
}

#[derive(Subcommand, Debug)]
pub(crate) enum AccountsCommand {
    List(LaunchProfilePathsArgs),
    Show(LaunchProfileNameArgs),
    Enroll(AccountWriteArgs),
    Update(AccountWriteArgs),
}
#[derive(Subcommand, Debug)]
pub(crate) enum PoolsCommand {
    List(LaunchProfilePathsArgs),
    Show(LaunchProfileNameArgs),
    Plan(PoolPlanArgs),
    Apply(AccountWriteArgs),
    /// Inspect identity-bound saved usage for every member; never refresh or admit work.
    Capacity(capacity::CapacityArgs),
}
#[derive(Args, Debug)]
pub(crate) struct AccountWriteArgs {
    name: String,
    #[arg(long)]
    spec_json: String,
    #[arg(long)]
    expected_revision: String,
    #[command(flatten)]
    paths: LaunchProfilePathsArgs,
}
#[derive(Args, Debug)]
pub(crate) struct PoolPlanArgs {
    name: String,
    #[arg(long)]
    spec_json: String,
    #[command(flatten)]
    paths: LaunchProfilePathsArgs,
}

fn safe_name(value: &str, label: &str) -> anyhow::Result<()> {
    validate_safe_name(value, label)?;
    if matches!(value, "." | "..") || value.len() > 128 {
        anyhow::bail!("invalid {label}");
    }
    Ok(())
}
pub(super) fn unique_map<'de, D, T>(deserializer: D) -> Result<BTreeMap<String, T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Visitor<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for Visitor<T> {
        type Value = BTreeMap<String, T>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a map with unique ids")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> Result<Self::Value, A::Error> {
            let mut result = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, T>()? {
                if result.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("duplicate authority id"));
                }
            }
            Ok(result)
        }
    }
    deserializer.deserialize_map(Visitor(std::marker::PhantomData))
}
fn provider(value: &str) -> anyhow::Result<()> {
    if !matches!(value, "claude" | "codex") {
        anyhow::bail!("native accounts support only claude or codex");
    }
    Ok(())
}
fn text(value: &str, field: &str) -> anyhow::Result<()> {
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        anyhow::bail!("invalid {field}");
    }
    Ok(())
}
pub(super) fn validate(document: &LaunchProfilesDocument) -> anyhow::Result<()> {
    let mut homes = BTreeSet::new();
    let mut identities = BTreeSet::new();
    for (id, account) in &document.accounts {
        safe_name(id, "native account")?;
        provider(&account.provider)?;
        text(&account.label, "account label")?;
        let home = match &account.native_home {
            NativeHome::Managed { label } => {
                safe_name(label, "native home label")?;
                if label.eq_ignore_ascii_case("default") {
                    anyhow::bail!("managed native home cannot be default");
                }
                format!("managed:{label}")
            }
            NativeHome::ExistingDefault {} => "existing_default".to_string(),
        };
        if !homes.insert((account.provider.clone(), home)) {
            anyhow::bail!("native home is already enrolled");
        }
        for value in [
            &account.declared.plan,
            &account.declared.organization_label,
            &account.provider_account_ref,
        ]
        .into_iter()
        .flatten()
        {
            text(value, "account metadata")?;
        }
        if let Some(observed) = &account.observed {
            text(&observed.account_id, "observed account id")?;
            if account.provider == "claude" && observed.organization_id.is_none() {
                anyhow::bail!("observed Claude identity requires organization_id");
            }
            if observed.observed_at_ms <= 0 {
                anyhow::bail!("observed identity requires a positive timestamp");
            }
            if let Some(value) = &observed.organization_id {
                text(value, "observed organization")?;
            }
            if let Some(value) = &observed.plan {
                text(value, "observed plan")?;
            }
            if !identities.insert((
                account.provider.clone(),
                observed.account_id.clone(),
                observed.organization_id.clone(),
            )) {
                anyhow::bail!("observed identity is already bound to another account");
            }
        }
    }
    for (id, pool) in &document.account_pools {
        safe_name(id, "account pool")?;
        provider(&pool.provider)?;
        if pool.members.is_empty() || pool.members.len() > 16 {
            anyhow::bail!("pool requires 1 to 16 explicit members");
        }
        let mut members = BTreeSet::new();
        for member in &pool.members {
            safe_name(member, "pool member")?;
            if !members.insert(member) {
                anyhow::bail!("pool contains duplicate members");
            }
            let account = document
                .accounts
                .get(member)
                .ok_or_else(|| anyhow::anyhow!("unknown pool member {member}"))?;
            if account.provider != pool.provider {
                anyhow::bail!("pool members must have the pool provider");
            }
        }
    }
    for (id, spec) in &document.launch_profiles {
        if spec.native_target.is_some() != spec.provider_lane.is_empty() {
            anyhow::bail!("profile {id} requires exactly one provider_lane or native_target");
        }
    }
    settings::validate_rules(document)?;
    for (id, spec) in &document.launch_profiles {
        if spec.native_target.is_some() {
            native_profile(document, id)?;
        }
    }
    Ok(())
}

fn emit(value: &Value, json_output: bool) -> anyhow::Result<()> {
    if json_output {
        println!("{}", serde_json::to_string(value)?);
    } else {
        println!("{}", serde_json::to_string_pretty(value)?);
    }
    Ok(())
}
fn account_report(document: &LaunchProfilesDocument, revision: &str) -> Value {
    json!({"schema":"switchback/profile-accounts@1","authority_revision":revision,"accounts":document.accounts})
}
fn pool_report(document: &LaunchProfilesDocument, revision: &str) -> Value {
    json!({"schema":"switchback/profile-account-pools@1","authority_revision":revision,"account_pools":document.account_pools})
}

/// Directory creation is the cross-process mutex. A crash leaves a fail-closed
/// lock for explicit recovery; it never silently steals another writer's lock.
struct AuthorityLock(PathBuf);
impl Drop for AuthorityLock {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir(&self.0) {
            eprintln!("authority lock cleanup failed: {error}");
        }
    }
}
fn lock_authority(path: &Path) -> anyhow::Result<AuthorityLock> {
    let lock = path.with_extension("accounts.lock");
    std::fs::create_dir(&lock).map_err(|error| {
        anyhow::anyhow!("authority is busy or its lock needs recovery: {error}")
    })?;
    let guard = AuthorityLock(lock.clone());
    set_mode(&lock, 0o700)?;
    Ok(guard)
}
fn verify_revision(actual: &str, expected: &str) -> anyhow::Result<()> {
    if actual != expected {
        anyhow::bail!("authority revision conflict: expected {expected}, current {actual}");
    }
    Ok(())
}
fn persist(path: &Path, document: &LaunchProfilesDocument) -> anyhow::Result<String> {
    validate(document)?;
    let mut body = serde_json::to_string_pretty(document)?;
    body.push('\n');
    super::profile_transaction::write_private(path, body.as_bytes(), 0o600)?;
    Ok(load_profile_authority(path)?.1)
}
pub(super) fn accounts(action: AccountsCommand, json_output: bool) -> anyhow::Result<()> {
    match action {
        AccountsCommand::List(args) => {
            let (doc, rev) = load_profile_authority(&resolve_profile_paths(&args).authority)?;
            emit(&account_report(&doc, &rev), json_output)
        }
        AccountsCommand::Show(args) => {
            let (doc, rev) = load_profile_authority(&resolve_profile_paths(&args.paths).authority)?;
            let account = doc
                .accounts
                .get(&args.name)
                .ok_or_else(|| anyhow::anyhow!("unknown account {}", args.name))?;
            emit(
                &json!({"authority_revision":rev,"id":args.name,"account":account}),
                json_output,
            )
        }
        AccountsCommand::Enroll(args) => write_account(args, false, json_output),
        AccountsCommand::Update(args) => write_account(args, true, json_output),
    }
}
fn write_account(args: AccountWriteArgs, update: bool, json_output: bool) -> anyhow::Result<()> {
    safe_name(&args.name, "account")?;
    let account: NativeAccount = serde_json::from_str(&args.spec_json)?;
    let path = resolve_profile_paths(&args.paths).authority;
    let _lock = lock_authority(&path)?;
    let (mut document, revision) = load_profile_authority(&path)?;
    verify_revision(&revision, &args.expected_revision)?;
    if update {
        let old = document
            .accounts
            .get(&args.name)
            .ok_or_else(|| anyhow::anyhow!("account is not enrolled"))?;
        if old.provider != account.provider || old.native_home != account.native_home {
            anyhow::bail!("account provider and native home are immutable");
        }
        if let Some(identity) = &old.observed {
            if account
                .observed
                .as_ref()
                .map(|x| (&x.account_id, &x.organization_id))
                != Some((&identity.account_id, &identity.organization_id))
            {
                anyhow::bail!("enrolled account identity is immutable");
            }
        }
    } else if document.accounts.contains_key(&args.name) {
        anyhow::bail!("account already exists; use update");
    }
    document.accounts.insert(args.name.clone(), account);
    let revision = persist(&path, &document)?;
    emit(
        &json!({"id":args.name,"authority_revision":revision,"account":document.accounts[&args.name]}),
        json_output,
    )
}
pub(super) fn pools(action: PoolsCommand, json_output: bool) -> anyhow::Result<()> {
    match action {
        PoolsCommand::Capacity(args) => capacity::run(args, json_output),
        PoolsCommand::List(args) => {
            let (doc, rev) = load_profile_authority(&resolve_profile_paths(&args).authority)?;
            emit(&pool_report(&doc, &rev), json_output)
        }
        PoolsCommand::Show(args) => {
            let (doc, rev) = load_profile_authority(&resolve_profile_paths(&args.paths).authority)?;
            let pool = doc
                .account_pools
                .get(&args.name)
                .ok_or_else(|| anyhow::anyhow!("unknown pool {}", args.name))?;
            emit(
                &json!({"authority_revision":rev,"id":args.name,"pool":pool}),
                json_output,
            )
        }
        PoolsCommand::Plan(args) => {
            safe_name(&args.name, "pool")?;
            let (mut doc, rev) =
                load_profile_authority(&resolve_profile_paths(&args.paths).authority)?;
            let pool: NativePool = serde_json::from_str(&args.spec_json)?;
            doc.account_pools.insert(args.name.clone(), pool);
            validate(&doc)?;
            emit(
                &json!({"authority_revision":rev,"id":args.name,"pool":doc.account_pools[&args.name],"applied":false}),
                json_output,
            )
        }
        PoolsCommand::Apply(args) => {
            safe_name(&args.name, "pool")?;
            let pool: NativePool = serde_json::from_str(&args.spec_json)?;
            let path = resolve_profile_paths(&args.paths).authority;
            let _lock = lock_authority(&path)?;
            let (mut doc, rev) = load_profile_authority(&path)?;
            verify_revision(&rev, &args.expected_revision)?;
            doc.account_pools.insert(args.name.clone(), pool);
            let rev = persist(&path, &doc)?;
            emit(
                &json!({"authority_revision":rev,"id":args.name,"pool":doc.account_pools[&args.name],"applied":true}),
                json_output,
            )
        }
    }
}
fn native_profile(document: &LaunchProfilesDocument, id: &str) -> anyhow::Result<Value> {
    let spec = document
        .launch_profiles
        .get(id)
        .ok_or_else(|| anyhow::anyhow!("unknown profile {id}"))?;
    if !spec.provider_lane.is_empty()
        || spec.client_profile.is_some()
        || spec.client_credential_ref.is_some()
        || spec.profile_label.is_some()
    {
        anyhow::bail!("native profiles cannot declare gateway provider or client credentials");
    }
    let target = spec
        .native_target
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("profile is not native"))?;
    let pool = document
        .account_pools
        .get(&target.pool)
        .ok_or_else(|| anyhow::anyhow!("unknown native pool {}", target.pool))?;
    let preset = document
        .harness_presets
        .get(&spec.harness_preset)
        .ok_or_else(|| anyhow::anyhow!("unknown harness preset"))?;
    let expected = match pool.provider.as_str() {
        "claude" => HarnessKind::ClaudeCode,
        "codex" => HarnessKind::Codex,
        _ => anyhow::bail!("unsupported native provider"),
    };
    if preset.harness != expected {
        anyhow::bail!("native provider and harness do not match");
    }
    if !matches!(preset.permissions_mode, PermissionsMode::Pinned)
        || !matches!(preset.settings_mode, SettingsMode::Pinned)
        || !matches!(preset.mcp_mode, McpMode::None)
        || !matches!(preset.skills_mode, SkillsMode::Disabled)
        || !preset.launch_args.is_empty()
        || !preset.mcp_servers.is_empty()
        || preset.compaction_window.is_some()
        || serde_json::to_value(&preset.model_aliases)?
            .as_object()
            .is_some_and(|m| m.values().any(|v| !v.is_null()))
    {
        anyhow::bail!("native profiles require pinned settings/permissions, disabled generated MCP/skills, and no model aliases, launch args or compaction override");
    }
    let capture = document
        .capture_policies
        .get(&spec.capture_policy)
        .ok_or_else(|| anyhow::anyhow!("unknown capture policy"))?;
    let accounts = pool.members.iter().map(|id| {
        let account = &document.accounts[id];
        json!({"id":id,"provider":account.provider,"enabled":account.enabled,"native_home":account.native_home,"observed":account.observed.as_ref().map(|observed|json!({"account_id":observed.account_id,"organization_id":observed.organization_id,"plan":observed.plan})),"provider_account_ref":account.provider_account_ref})
    }).collect::<Vec<_>>();
    let account_policy = json!({"pool_ref":format!("switchback://account-pools/{}",target.pool),"pool":target.pool,"provider":pool.provider,"members":pool.members,"accounts":accounts,"selection":pool.selection,"continuation":pool.continuation,"unknown_quota":pool.unknown_quota});
    let account_revision = stable_json_revision(&account_policy)?;
    let settings = match &spec.native_shared_settings {
        Some(id) => json!({"mode":"shared_allowlisted","policy_ref":id,
            "policy_revision":stable_json_revision(&serde_json::to_value(&document.native_shared_settings[id])?)?,
            "permissions_mode":"pinned","settings_mode":"pinned"}),
        None => json!({"mode":"native_owned","permissions_mode":"pinned","settings_mode":"pinned"}),
    };
    let profile = json!({
        "id":id,"harness":preset.harness.compound_identity(),"harness_kind":preset.harness.as_str(),
        "execution_backend":"native_account_pool","expected_executable":preset.harness.executable(),
        "expected_version":preset.expected_version,"requested_effort":preset.native_effort.as_str(),
        "native_target":target,"wrappers":spec.wrappers,"account_policy":account_policy,"account_policy_revision":account_revision,
        "settings":settings,"settings_revision":stable_json_revision(&settings)?,
        "capture_policy":{"mode":capture.mode.as_str(),"verification":"required_per_attempt"},"capture_revision":stable_json_revision(&serde_json::to_value(capture)?)?,
        "headless":true,"workspace_mode":"caller","output_mode":"native","permission_posture":"explicit_at_run",
        "unsupported_floors":["generated_mcp","generated_skills"]
    });
    Ok(profile)
}
fn artifacts(paths: &ProfilePaths, profile: &Value) -> anyhow::Result<Vec<PlannedProfileArtifact>> {
    let id = profile["id"].as_str().expect("native profile id");
    let revision = stable_json_revision(profile)?;
    let conformance = json!({"schema":PROFILE_CONFORMANCE_SCHEMA,"authority":profile_authority_projection(),
        "launch_profile_ref":format!("switchback://launch-profiles/{id}"),"conformance_revision":revision,
        "profile":profile,"unsupported_floors":profile["unsupported_floors"]});
    let mut result = vec![PlannedProfileArtifact {
        kind: "profile_conformance",
        path: paths.projection_root.join(format!("{id}.json")),
        contents: format!("{}\n", serde_json::to_string_pretty(&conformance)?),
        mode: 0o600,
        comparison: ArtifactComparison::Bytes,
    }];
    if let Some(wrappers) = profile["wrappers"].as_array() {
        for wrapper in wrappers {
            let wrapper = wrapper
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("invalid wrapper"))?;
            safe_name(wrapper, "native wrapper")?;
            result.push(PlannedProfileArtifact {kind:"wrapper",path:paths.wrapper_root.join(wrapper),
                contents:format!("#!/bin/sh\n{PROFILE_WRAPPER_OWNER_MARKER}\nexec sb account run --profile {} \"$@\"\n",shell_single_quote(id)),
                mode:0o700,comparison:ArtifactComparison::Bytes});
        }
    }
    Ok(result)
}
pub(super) fn try_handle(
    action: &LaunchProfileCmd,
    cfg: &Config,
    json_output: bool,
) -> anyhow::Result<bool> {
    let (name, args, operation) = match action {
        LaunchProfileCmd::Show(a) => (Some(a.name.as_str()), &a.paths, "show"),
        LaunchProfileCmd::Plan(a) => (Some(a.name.as_str()), &a.paths, "plan"),
        LaunchProfileCmd::Diff(a) => (Some(a.name.as_str()), &a.paths, "diff"),
        LaunchProfileCmd::Apply(a) => (Some(a.name.as_str()), &a.paths, "apply"),
        LaunchProfileCmd::Recover(a) => (Some(a.name.as_str()), &a.paths, "recover"),
        LaunchProfileCmd::Doctor(a) => (a.name.as_deref(), &a.paths, "doctor"),
        LaunchProfileCmd::List(a) => (None, a, "list"),
        _ => return Ok(false),
    };
    let paths = resolve_profile_paths(args);
    let (document, authority_revision) = load_profile_authority(&paths.authority)?;
    if let Some(id) = name {
        if !document
            .launch_profiles
            .get(id)
            .is_some_and(|s| s.native_target.is_some())
        {
            return Ok(false);
        }
        let profile = native_profile(&document, id)?;
        let revision = stable_json_revision(&profile)?;
        let artifacts = artifacts(&paths, &profile)?;
        if operation == "recover" {
            emit(
                &super::profile_transaction::recover(&artifacts)?,
                json_output,
            )?;
            return Ok(true);
        }
        let statuses = artifact_statuses(&artifacts)?;
        let changed = statuses.iter().any(|s| s.changed);
        if operation == "apply" {
            apply_profile_artifacts(&artifacts)?;
        }
        let current = artifact_statuses(&artifacts)?.iter().all(|s| !s.changed);
        let report = json!({"schema":if operation=="show" {"switchback/launch-profile@1".to_string()} else {format!("switchback/launch-profile-{operation}@1")},"authority":profile_authority_projection(),
            "authority_revision":authority_revision,"revision":revision,"profile":profile,"artifacts":statuses,
            "changed":changed,"applied":operation=="apply","ok":operation!="doctor" || current,
            "conformance":serde_json::from_str::<Value>(&artifacts[0].contents)?});
        emit(&report, json_output)?;
        if operation == "doctor" && !current {
            anyhow::bail!("native profile conformance is not materialized");
        }
        return Ok(true);
    }
    if !document
        .launch_profiles
        .values()
        .any(|s| s.native_target.is_some())
    {
        return Ok(false);
    }
    let mut profiles = Vec::new();
    let mut ok = true;
    for (id, spec) in &document.launch_profiles {
        if spec.native_target.is_some() {
            let profile = native_profile(&document, id)?;
            let artifacts = artifacts(&paths, &profile)?;
            let current = artifact_statuses(&artifacts)?.iter().all(|s| !s.changed);
            ok &= current;
            profiles.push(json!({"id":id,"profile":profile,"harness":profile["harness"],
                "harness_kind":profile["harness_kind"],"requested_effort":profile["requested_effort"],
                "capture_mode":profile["capture_policy"]["mode"],"execution_backend":"native_account_pool",
                "account_pool":profile["native_target"]["pool"],"revision":stable_json_revision(&profile)?,"ok":current}));
        } else {
            let bundle = resolve_launch_profile(&document, cfg, id)?;
            if operation == "doctor" {
                let artifacts = build_profile_artifacts(&paths, &bundle)?;
                let report = profile_doctor_report(
                    cfg,
                    &authority_revision,
                    &paths.authority,
                    &bundle,
                    &artifacts,
                    ProfileDoctorScope::Materialized,
                )?;
                ok &= report.ok;
                profiles.push(serde_json::to_value(report)?);
            } else {
                profiles.push(serde_json::to_value(profile_summary(&bundle))?);
            }
        }
    }
    emit(
        &json!({"schema":format!("switchback/launch-profile-{operation}@1"),"authority":profile_authority_projection(),
        "authority_revision":authority_revision,"profiles":profiles,"ok":operation!="doctor" || ok}),
        json_output,
    )?;
    if operation == "doctor" && !ok {
        anyhow::bail!("profile conformance failed");
    }
    Ok(true)
}

#[derive(Subcommand, Debug)]
pub(crate) enum DefinitionsCommand {
    Enroll(ProfileWriteArgs),
    Update(ProfileWriteArgs),
}
#[derive(Args, Debug)]
pub(crate) struct ProfileWriteArgs {
    #[command(flatten)]
    write: AccountWriteArgs,
    #[arg(long)]
    preset_json: Option<String>,
    #[arg(long)]
    capture_json: Option<String>,
}
pub(super) fn definitions(action: DefinitionsCommand, json_output: bool) -> anyhow::Result<()> {
    let (args, update) = match action {
        DefinitionsCommand::Enroll(a) => (a, false),
        DefinitionsCommand::Update(a) => (a, true),
    };
    let input = args.write;
    safe_name(&input.name, "profile")?;
    let spec: LaunchProfileSpec = serde_json::from_str(&input.spec_json)?;
    if spec.native_target.is_none() {
        anyhow::bail!("definition writes currently require a native profile");
    }
    let path = resolve_profile_paths(&input.paths).authority;
    let _lock = lock_authority(&path)?;
    let (mut document, revision) = load_profile_authority(&path)?;
    verify_revision(&revision, &input.expected_revision)?;
    if document.launch_profiles.contains_key(&input.name) != update {
        anyhow::bail!("profile existence does not match enroll/update operation");
    }
    if let Some(preset) = args.preset_json {
        let preset: HarnessPresetSpec = serde_json::from_str(&preset)?;
        if document
            .launch_profiles
            .iter()
            .any(|(id, p)| id != &input.name && p.harness_preset == spec.harness_preset)
        {
            anyhow::bail!("cannot replace a preset consumed by another profile");
        }
        document
            .harness_presets
            .insert(spec.harness_preset.clone(), preset);
    }
    if let Some(capture) = args.capture_json {
        let capture: CapturePolicySpec = serde_json::from_str(&capture)?;
        if document
            .launch_profiles
            .iter()
            .any(|(id, p)| id != &input.name && p.capture_policy == spec.capture_policy)
        {
            anyhow::bail!("cannot replace a capture policy consumed by another profile");
        }
        document
            .capture_policies
            .insert(spec.capture_policy.clone(), capture);
    }
    document.launch_profiles.insert(input.name.clone(), spec);
    validate(&document)?;
    let profile = native_profile(&document, &input.name)?;
    let revision = persist(&path, &document)?;
    emit(
        &json!({"authority_revision":revision,"profile":profile,"revision":stable_json_revision(&profile)?}),
        json_output,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn doc() -> LaunchProfilesDocument {
        serde_json::from_value(json!({
            "schema":LAUNCH_PROFILES_SCHEMA,"provider_lanes":{},"harness_presets":{"native":{
                "harness":"claude-code","native_effort":"default","permissions_mode":"pinned",
                "mcp_mode":"none","skills_mode":"disabled","settings_mode":"pinned"
            }},"capture_policies":{"capture":{"mode":"segmented_full_wire"}},
            "launch_profiles":{"team":{"native_target":{"pool":"team"},"harness_preset":"native","capture_policy":"capture"}},
            "accounts":{
                "a":{"provider":"claude","label":"A","native_home":{"kind":"managed","label":"a"},
                    "observed":{"account_id":"person","organization_id":"team","observed_at_ms":1}},
                "personal":{"provider":"claude","label":"Personal","native_home":{"kind":"existing_default"},
                    "observed":{"account_id":"person","organization_id":"personal","observed_at_ms":1}}
            },
            "account_pools":{"team":{"provider":"claude","members":["a"],"selection":"ordered","continuation":"same_account","unknown_quota":"allow_attempt"}}
        })).unwrap()
    }
    #[test]
    fn native_profile_separates_account_and_org_and_never_adds_personal() {
        let document = doc();
        validate(&document).unwrap();
        let value = native_profile(&document, "team").unwrap();
        assert_eq!(value["account_policy"]["members"], json!(["a"]));
        assert!(value.get("client_credential_env").is_none());
        assert_eq!(value["capture_policy"]["mode"], "segmented_full_wire");
        assert_eq!(
            value["capture_policy"]["verification"],
            "required_per_attempt"
        );
    }
    #[test]
    fn duplicate_native_identity_and_home_are_rejected() {
        let mut document = doc();
        document
            .accounts
            .get_mut("personal")
            .unwrap()
            .observed
            .as_mut()
            .unwrap()
            .organization_id = Some("team".into());
        assert!(validate(&document)
            .unwrap_err()
            .to_string()
            .contains("identity"));
        let mut document = doc();
        document.accounts.get_mut("personal").unwrap().native_home =
            NativeHome::Managed { label: "a".into() };
        assert!(validate(&document)
            .unwrap_err()
            .to_string()
            .contains("home"));
    }
    #[test]
    fn native_identity_requires_claude_org_and_managed_home_is_bounded() {
        let mut document = doc();
        document
            .accounts
            .get_mut("a")
            .unwrap()
            .observed
            .as_mut()
            .unwrap()
            .organization_id = None;
        assert!(validate(&document).is_err());
        for label in ["..", ".", "default", "../outside", "/tmp/native"] {
            let mut document = doc();
            document.accounts.get_mut("a").unwrap().native_home = NativeHome::Managed {
                label: label.into(),
            };
            assert!(validate(&document).is_err(), "{label}");
        }
    }
    #[test]
    fn native_gateway_mixture_and_credentials_are_rejected() {
        let mut document = doc();
        document
            .launch_profiles
            .get_mut("team")
            .unwrap()
            .provider_lane = "gateway".into();
        assert!(validate(&document).is_err());
        let mut document = doc();
        document
            .launch_profiles
            .get_mut("team")
            .unwrap()
            .client_credential_ref = Some(CredentialReference::Env {
            name: "TOKEN".into(),
        });
        assert!(native_profile(&document, "team").is_err());
        assert!(
            serde_json::from_value::<NativeAccount>(json!({"provider":"claude","label":"A",
            "native_home":{"kind":"managed","label":"a"},"access_token":"secret"}))
            .is_err()
        );
    }
    #[test]
    fn native_pool_membership_is_explicit_and_provider_bound() {
        for members in [vec![], vec!["a", "a"], vec!["missing"]] {
            let mut document = doc();
            document.account_pools.get_mut("team").unwrap().members =
                members.iter().map(|s| s.to_string()).collect();
            assert!(validate(&document).is_err());
        }
        let mut document = doc();
        document.account_pools.get_mut("team").unwrap().provider = "codex".into();
        assert!(validate(&document).is_err());
        let mut document = doc();
        document.account_pools.get_mut("team").unwrap().continuation =
            PoolContinuation::SharedTranscript;
        assert_eq!(
            native_profile(&document, "team").unwrap()["account_policy"]["continuation"],
            "shared_transcript"
        );
    }
    #[test]
    fn native_revision_conflicts_and_unknown_spec_fields_fail_closed() {
        assert!(verify_revision("sha256:new", "sha256:old").is_err());
        assert!(
            serde_json::from_value::<NativeAccount>(json!({"provider":"claude","label":"A",
            "native_home":{"kind":"existing_default","path":"/somewhere"}}))
            .is_err()
        );
    }
    #[test]
    fn native_profile_rejects_unsupported_generated_settings() {
        let mut document = doc();
        document
            .harness_presets
            .get_mut("native")
            .unwrap()
            .settings_mode = SettingsMode::InheritAllowlisted;
        assert!(native_profile(&document, "team").is_err());
        let mut document = doc();
        document.harness_presets.get_mut("native").unwrap().harness = HarnessKind::Codex;
        assert!(native_profile(&document, "team").is_err());
    }
    #[test]
    fn native_projection_policy_revision_tracks_settings_and_membership() {
        let mut document = doc();
        document.native_shared_settings.insert(
            "common".into(),
            serde_json::from_value(json!({"settings":{"language":"en"}})).unwrap(),
        );
        document
            .launch_profiles
            .get_mut("team")
            .unwrap()
            .native_shared_settings = Some("common".into());
        let before = native_profile(&document, "team").unwrap();
        document.native_shared_settings.insert(
            "common".into(),
            serde_json::from_value(json!({"settings":{"language":"tr"}})).unwrap(),
        );
        let after = native_profile(&document, "team").unwrap();
        document
            .accounts
            .get_mut("a")
            .unwrap()
            .observed
            .as_mut()
            .unwrap()
            .observed_at_ms += 1;
        let refreshed = native_profile(&document, "team").unwrap();
        assert_eq!(
            stable_json_revision(&after).unwrap(),
            stable_json_revision(&refreshed).unwrap()
        );
        assert!(refreshed["account_policy"]["accounts"][0]["observed"]
            .get("observed_at_ms")
            .is_none());
        document
            .account_pools
            .get_mut("team")
            .unwrap()
            .members
            .push("personal".into());
        let expanded = native_profile(&document, "team").unwrap();
        assert_ne!(
            refreshed["account_policy_revision"],
            expanded["account_policy_revision"]
        );
        assert_ne!(before["settings_revision"], after["settings_revision"]);
        assert_ne!(
            stable_json_revision(&before).unwrap(),
            stable_json_revision(&after).unwrap()
        );
    }
    #[test]
    fn legacy_authority_serialization_omits_native_additions() {
        let document: LaunchProfilesDocument =
            serde_json::from_value(json!({"schema":LAUNCH_PROFILES_SCHEMA,
            "provider_lanes":{},"harness_presets":{},"capture_policies":{},"launch_profiles":{}}))
            .unwrap();
        let value = serde_json::to_value(document).unwrap();
        assert!(value.get("accounts").is_none());
        assert!(value.get("account_pools").is_none());
        assert!(value.get("native_shared_settings").is_none());
    }
    #[test]
    fn native_plan_and_conformance_are_actual_artifacts() {
        let document = doc();
        let profile = native_profile(&document, "team").unwrap();
        let root = std::env::temp_dir().join(format!("sb-profile-native-{}", std::process::id()));
        let paths = resolve_profile_paths(&LaunchProfilePathsArgs {
            authority: None,
            lane_root: None,
            profile_root: None,
            wrapper_root: Some(root.join("bin")),
            projection_root: Some(root.join("profiles")),
        });
        let planned = artifacts(&paths, &profile).unwrap();
        let conformance: Value = serde_json::from_str(&planned[0].contents).unwrap();
        assert_eq!(
            conformance["launch_profile_ref"],
            "switchback://launch-profiles/team"
        );
        assert_eq!(
            conformance["profile"]["execution_backend"],
            "native_account_pool"
        );
        assert_eq!(
            conformance["conformance_revision"],
            stable_json_revision(&profile).unwrap()
        );
    }
}

#[cfg(test)]
mod owner_command_tests {
    use super::*;
    use std::fs;
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "sb-profile-owner-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn paths(authority: PathBuf) -> LaunchProfilePathsArgs {
        LaunchProfilePathsArgs {
            authority: Some(authority),
            lane_root: None,
            profile_root: None,
            wrapper_root: None,
            projection_root: None,
        }
    }
    #[test]
    fn account_owner_enrollment_and_revision_conflicts_preserve_authority() {
        let root = Temp::new();
        let path = root.0.join("launch-profiles.json");
        let body = json!({"schema":LAUNCH_PROFILES_SCHEMA,"provider_lanes":{},"harness_presets":{},"capture_policies":{},"launch_profiles":{}});
        fs::write(&path, serde_json::to_vec(&body).unwrap()).unwrap();
        let revision = load_profile_authority(&path).unwrap().1;
        let spec = json!({"provider":"claude","label":"Team","native_home":{"kind":"managed","label":"a"},"observed":{"account_id":"person","organization_id":"team","observed_at_ms":1}});
        let make = |name: &str, spec: &Value, revision: &str| AccountWriteArgs {
            name: name.into(),
            spec_json: spec.to_string(),
            expected_revision: revision.into(),
            paths: paths(path.clone()),
        };
        write_account(make("a", &spec, &revision), false, true).unwrap();
        let after = fs::read(&path).unwrap();
        assert!(write_account(make("b", &spec, &revision), false, true).is_err());
        assert_eq!(fs::read(&path).unwrap(), after);
        let revision = load_profile_authority(&path).unwrap().1;
        assert!(write_account(make("a", &spec, &revision), false, true).is_err());
        let mut same_person = spec.clone();
        same_person["native_home"] = json!({"kind":"existing_default"});
        same_person["observed"]["organization_id"] = json!("personal");
        write_account(make("personal", &same_person, &revision), false, true).unwrap();
        let revision = load_profile_authority(&path).unwrap().1;
        let mut changed = spec.clone();
        changed["observed"]["organization_id"] = json!("different");
        assert!(write_account(make("a", &changed, &revision), true, true).is_err());
    }
    #[test]
    fn duplicate_authority_ids_are_not_silently_overwritten() {
        let source = r#"{"schema":"switchback/launch-profiles@1","provider_lanes":{},"harness_presets":{},"capture_policies":{},"launch_profiles":{},"accounts":{"same":{"provider":"claude","label":"one","native_home":{"kind":"existing_default"}},"same":{"provider":"claude","label":"two","native_home":{"kind":"managed","label":"two"}}}}"#;
        assert!(serde_json::from_str::<LaunchProfilesDocument>(source).is_err());
    }
}
