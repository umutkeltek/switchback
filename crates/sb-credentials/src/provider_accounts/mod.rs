mod import;
mod normalize;
mod reconcile;
mod store;
mod types;

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub use normalize::{binding_id, deterministic_id, normalize_alias};
pub use types::*;

use store::AuthorityStore;

#[derive(Debug, thiserror::Error)]
pub enum AuthorityError {
    #[error("provider-account authority is absent")]
    Absent,
    #[error("authority is read-only")]
    ReadOnly,
    #[error("invalid alias: {0}")]
    InvalidAlias(String),
    #[error("malformed source: {0}")]
    MalformedSource(String),
    #[error("unsafe source: {0}")]
    UnsafeSource(String),
    #[error("source read failed: {0}")]
    SourceRead(String),
    #[error("source changed while being read")]
    ConcurrentWrite,
    #[error("credential identity conflict: {0}")]
    CredentialConflict(String),
    #[error("authority revision conflict")]
    RevisionConflict,
    #[error("stale revision: expected {expected}, current {current}")]
    StaleRevision { expected: u64, current: u64 },
    #[error("account resolution failed: {0}")]
    Resolution(String),
    #[error("authority store: {0}")]
    Store(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub struct ProviderAccountAuthority {
    store: AuthorityStore,
}

impl ProviderAccountAuthority {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, AuthorityError> {
        Ok(Self {
            store: AuthorityStore::new(path.into(), false)?,
        })
    }

    pub fn open_read_only(path: impl Into<PathBuf>) -> Self {
        Self {
            store: AuthorityStore::new(path.into(), true)
                .expect("read-only store construction cannot write"),
        }
    }

    pub fn path(&self) -> &Path {
        self.store.path()
    }
    pub fn has_revision(&self) -> Result<bool, AuthorityError> {
        self.store.exists_with_revision()
    }

    pub fn reconcile(&self, request: ReconcileRequest) -> Result<ReconcileResult, AuthorityError> {
        let dry_run = request.dry_run;
        for attempt in 0..2 {
            let (revision, digest) = self.store.current_revision()?;
            let current = if revision == 0 {
                ProviderAccountSnapshot::empty()
            } else {
                self.store.snapshot()?
            };
            let plan = reconcile::build(&request, &current, digest.as_deref())?;
            let result = ReconcileResult {
                schema: "switchback/native-accounts-reconcile@1".into(),
                base_revision: plan.base_revision,
                would_revision: if plan.changed {
                    plan.base_revision + 1
                } else {
                    plan.base_revision
                },
                revision: if dry_run {
                    plan.base_revision
                } else {
                    plan.snapshot.revision
                },
                changed: plan.changed,
                counts: ReconcileCounts::from_snapshot(&plan.snapshot),
                snapshot: plan.snapshot.clone(),
            };
            if dry_run || !plan.changed {
                return Ok(result);
            }
            match self.store.apply(&plan, "reconcile") {
                Ok(()) => {
                    return Ok(ReconcileResult {
                        revision: plan.snapshot.revision,
                        ..result
                    })
                }
                Err(AuthorityError::RevisionConflict) if attempt == 0 => continue,
                Err(e) => return Err(e),
            }
        }
        Err(AuthorityError::RevisionConflict)
    }

    pub fn snapshot(&self) -> Result<ProviderAccountSnapshot, AuthorityError> {
        self.store.snapshot()
    }

    pub fn resolve(
        &self,
        query: AccountResolutionQuery,
    ) -> Result<AccountResolution, AuthorityError> {
        let snapshot = self.snapshot()?;
        if let Some(expected) = query.expected_revision {
            if expected != snapshot.revision {
                return Err(AuthorityError::StaleRevision {
                    expected,
                    current: snapshot.revision,
                });
            }
        }
        if query.provider != "openai" || query.client != "codex" {
            return Err(AuthorityError::Resolution(
                "v0 supports only provider=openai client=codex".into(),
            ));
        }
        let normalized = normalize_query(query.alias_scheme, &query.alias_value)?;
        let account = snapshot
            .accounts
            .iter()
            .find(|a| {
                a.provider.0 == query.provider
                    && a.aliases.iter().any(|b| {
                        b.scheme == query.alias_scheme
                            && b.normalized_value == normalized
                            && b.binding_state == AliasBindingState::Bound
                            && (query.alias_scheme != AliasScheme::Label
                                || b.source == ImportSource::SwitchbackCodexRegistry)
                    })
            })
            .ok_or_else(|| {
                AuthorityError::Resolution("alias does not identify an account".into())
            })?;
        if account.state != EnrollmentState::Enrolled {
            return Err(AuthorityError::Resolution("account is not enrolled".into()));
        }
        if account
            .aliases
            .iter()
            .any(|b| b.binding_state == AliasBindingState::ParkedConflict)
        {
            return Err(AuthorityError::Resolution(
                "account has conflicting bindings".into(),
            ));
        }
        let credential = snapshot
            .credentials
            .iter()
            .filter(|c| c.account_id == account.id && c.readiness == CredentialReadiness::Ready)
            .find(|c| match (&c.pointer, query.alias_scheme) {
                (CredentialPointer::SwitchbackCodexRegistry { slot, .. }, AliasScheme::Label) => {
                    slot == query.alias_value.trim()
                }
                _ => false,
            })
            .or_else(|| {
                snapshot.credentials.iter().find(|c| {
                    c.account_id == account.id
                        && c.readiness == CredentialReadiness::Ready
                        && matches!(c.pointer, CredentialPointer::SwitchbackCodexRegistry { .. })
                })
            })
            .ok_or_else(|| {
                AuthorityError::Resolution(
                    "no fresh, conflict-free Switchback registry credential".into(),
                )
            })?;
        Ok(AccountResolution {
            schema: "switchback/native-account-resolution@1".into(),
            authority_revision: snapshot.revision,
            account_id: account.id.clone(),
            credential_pointer: credential.pointer.clone(),
            selection_reason: "explicit_label_to_enrolled_account".into(),
            fresh: true,
        })
    }

    pub fn adjudicate(
        &self,
        command: AdjudicationCommand,
    ) -> Result<AdjudicationResult, AuthorityError> {
        let mut snapshot = self.snapshot()?;
        let current = snapshot.revision;
        let expected = match &command {
            AdjudicationCommand::Merge {
                expected_revision, ..
            }
            | AdjudicationCommand::Split {
                expected_revision, ..
            } => *expected_revision,
        };
        if expected != current {
            return Err(AuthorityError::StaleRevision { expected, current });
        }
        match command {
            AdjudicationCommand::Merge { from, into, .. } => merge(&mut snapshot, &from, &into)?,
            AdjudicationCommand::Split {
                account, binding, ..
            } => split(&mut snapshot, &account, &binding)?,
        }
        snapshot.revision = current + 1;
        let now = now_ms();
        for a in &mut snapshot.accounts {
            if a.updated_revision == current + 1 {
                a.updated_at_ms = now
            }
        }
        let digest = format!("adjudication:{}", snapshot.revision);
        let plan = reconcile::ReconcilePlan {
            base_revision: current,
            digest,
            changed: true,
            snapshot: snapshot.clone(),
            now_ms: now,
        };
        self.store.apply(&plan, "adjudication")?;
        Ok(AdjudicationResult {
            schema: "switchback/native-accounts-adjudication@1".into(),
            revision: snapshot.revision,
            changed: true,
            snapshot,
        })
    }
}

fn normalize_query(scheme: AliasScheme, value: &str) -> Result<String, AuthorityError> {
    let alias = match scheme {
        AliasScheme::OpenAiAccountUuid => ProviderAccountAlias::OpenAiAccountUuid(value.into()),
        AliasScheme::OpenAiOrgId => ProviderAccountAlias::OpenAiOrgId(value.into()),
        AliasScheme::CodexBarAccountKey => ProviderAccountAlias::CodexBarAccountKey(value.into()),
        AliasScheme::CodexMultiAuthAccountId => {
            ProviderAccountAlias::CodexMultiAuthAccountId(value.into())
        }
        AliasScheme::AnthropicAccountUuid => {
            ProviderAccountAlias::AnthropicAccountUuid(value.into())
        }
        AliasScheme::AnthropicOrgUuid => ProviderAccountAlias::AnthropicOrgUuid(value.into()),
        AliasScheme::ZaiTokenAccountId => ProviderAccountAlias::ZaiTokenAccountId(value.into()),
        AliasScheme::Email => ProviderAccountAlias::Email(value.into()),
        AliasScheme::Label => ProviderAccountAlias::Label(value.into()),
    };
    Ok(normalize_alias(alias)?.value)
}

fn merge(
    snapshot: &mut ProviderAccountSnapshot,
    from: &ProviderAccountId,
    into: &ProviderAccountId,
) -> Result<(), AuthorityError> {
    if from == into {
        return Err(AuthorityError::Resolution(
            "merge source and survivor must differ".into(),
        ));
    }
    let from_index = snapshot
        .accounts
        .iter()
        .position(|a| &a.id == from)
        .ok_or_else(|| AuthorityError::Resolution("merge source missing".into()))?;
    let into_index = snapshot
        .accounts
        .iter()
        .position(|a| &a.id == into)
        .ok_or_else(|| AuthorityError::Resolution("merge survivor missing".into()))?;
    let moved = snapshot.accounts[from_index].aliases.clone();
    snapshot.accounts[into_index]
        .aliases
        .extend(moved.into_iter().map(|mut b| {
            b.binding_state = AliasBindingState::Bound;
            b
        }));
    snapshot.accounts[into_index].state = EnrollmentState::Enrolled;
    snapshot.accounts[into_index].updated_revision = snapshot.revision + 1;
    snapshot.accounts[from_index].state = EnrollmentState::Retired;
    snapshot.accounts[from_index].superseded_by = Some(into.clone());
    snapshot.accounts[from_index].updated_revision = snapshot.revision + 1;
    for c in &mut snapshot.credentials {
        if &c.account_id == from {
            c.account_id = into.clone()
        }
    }
    for c in &mut snapshot.capacity {
        if &c.account_id == from {
            c.account_id = into.clone()
        }
    }
    Ok(())
}
fn split(
    snapshot: &mut ProviderAccountSnapshot,
    account: &ProviderAccountId,
    binding: &str,
) -> Result<(), AuthorityError> {
    let index = snapshot
        .accounts
        .iter()
        .position(|a| &a.id == account)
        .ok_or_else(|| AuthorityError::Resolution("split account missing".into()))?;
    let position = snapshot.accounts[index]
        .aliases
        .iter()
        .position(|candidate| binding_id(candidate) == binding)
        .ok_or_else(|| AuthorityError::Resolution("split binding missing".into()))?;
    let selected = snapshot.accounts[index].aliases[position].clone();
    let source = selected.source;
    let source_record_key = selected.source_record_key.clone();
    let mut moved = Vec::new();
    snapshot.accounts[index].aliases.retain(|alias| {
        let take = alias.source == source && alias.source_record_key == source_record_key;
        if take {
            moved.push(alias.clone());
        }
        !take
    });
    let anchor = moved
        .iter()
        .min_by_key(|alias| alias.scheme.rank())
        .ok_or_else(|| AuthorityError::Resolution("split observation has no aliases".into()))?;
    let provider = anchor.scheme.provider().unwrap_or("openai");
    let id = deterministic_id(
        provider,
        &format!(
            "split\0{}\0{}\0{}\0{}",
            source.as_str(),
            source_record_key,
            anchor.scheme.as_str(),
            anchor.normalized_value
        ),
    );
    let state = if moved
        .iter()
        .any(|alias| alias.scheme.strong_identity_provider().is_some())
    {
        EnrollmentState::Enrolled
    } else {
        EnrollmentState::Parked
    };
    snapshot.accounts.push(ProviderAccountEnrollment {
        id,
        provider: ProviderKey(provider.into()),
        state,
        aliases: moved,
        created_revision: snapshot.revision + 1,
        updated_revision: snapshot.revision + 1,
        created_at_ms: now_ms(),
        updated_at_ms: now_ms(),
        superseded_by: None,
    });
    let split_id = snapshot
        .accounts
        .last()
        .expect("split account inserted")
        .id
        .clone();
    for credential in &mut snapshot.credentials {
        let belongs = match &credential.pointer {
            CredentialPointer::SwitchbackCodexRegistry { slot, .. } => {
                source == ImportSource::SwitchbackCodexRegistry && slot == &source_record_key
            }
            CredentialPointer::CodexActiveAuth { .. } => source == ImportSource::CodexActiveAuth,
            CredentialPointer::ConfiguredAccount { .. } => false,
        };
        if belongs {
            credential.account_id = split_id.clone();
        }
    }
    Ok(())
}

pub fn default_state_dir() -> PathBuf {
    std::env::var_os("SWITCHBACK_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state/switchback"))
        })
        .unwrap_or_else(|| PathBuf::from(".switchback/state"))
}
pub fn default_database_path() -> PathBuf {
    default_state_dir().join("provider-accounts.sqlite")
}

/// Re-open a resolved registry pointer without following symlinks and prove
/// that its live account UUID still belongs to the canonical enrollment.
pub fn validate_live_resolution(
    resolution: &AccountResolution,
    sources: &SourcePaths,
) -> Result<(), AuthorityError> {
    let CredentialPointer::SwitchbackCodexRegistry { slot, .. } = &resolution.credential_pointer
    else {
        return Err(AuthorityError::Resolution(
            "v0 shared activation requires a Switchback registry pointer".into(),
        ));
    };
    normalize_alias(ProviderAccountAlias::Label(slot.clone()))?;
    let root = sources
        .switchback_auth_registry
        .as_ref()
        .ok_or_else(|| AuthorityError::Resolution("Switchback registry source missing".into()))?;
    let path = root.join(format!("{slot}.json"));
    let (bytes, _) = import::stable_read(&path, 1024 * 1024, true)?;
    let value = import::json(&bytes, 32)?;
    let live = value
        .pointer("/tokens/account_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            AuthorityError::Resolution("registry slot has no tokens.account_id".into())
        })?;
    let live = normalize_alias(ProviderAccountAlias::OpenAiAccountUuid(live.into()))?.value;
    let snapshot = ProviderAccountAuthority::open_read_only(default_database_path()).snapshot()?;
    let account = snapshot
        .accounts
        .iter()
        .find(|a| a.id == resolution.account_id)
        .ok_or_else(|| AuthorityError::Resolution("resolved account disappeared".into()))?;
    if !account.aliases.iter().any(|a| {
        a.scheme == AliasScheme::OpenAiAccountUuid
            && a.normalized_value == live
            && a.binding_state == AliasBindingState::Bound
    }) {
        return Err(AuthorityError::CredentialConflict(
            "registry slot UUID differs from canonical account".into(),
        ));
    }
    Ok(())
}
pub fn default_source_paths() -> SourcePaths {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let authreg = std::env::var_os("SB_AUTHREG")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config/switchback/codex-auth"));
    SourcePaths {
        codex_auth: Some(home.join(".codex/auth.json")),
        switchback_auth_registry: Some(authreg),
        codex_multi_auth: Some(home.join(".codex/multi-auth/openai-codex-accounts.json")),
        quota_cache: Some(home.join(".codex/multi-auth/quota-cache.json")),
        codexbar_history: Some(
            home.join("Library/Application Support/CodexBar/usage-history.jsonl"),
        ),
        claude_auth: Some(home.join(".claude.json")),
        codexbar_config: Some(home.join(".codexbar/config.json")),
    }
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
