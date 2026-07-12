use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};

use super::import::{self, ImportBatch, Observation};
use super::normalize::{deterministic_id, hex_digest};
use super::*;

pub(crate) struct ReconcilePlan {
    pub base_revision: u64,
    pub digest: String,
    pub changed: bool,
    pub snapshot: ProviderAccountSnapshot,
    pub now_ms: i64,
}

pub(crate) fn build(
    request: &ReconcileRequest,
    current: &ProviderAccountSnapshot,
    current_digest: Option<&str>,
) -> Result<ReconcilePlan, AuthorityError> {
    let now_ms = request.now_ms.unwrap_or_else(super::now_ms);
    let mut batches = vec![];
    push_source(
        &mut batches,
        request.sources.codex_auth.as_deref(),
        ImportSource::CodexActiveAuth,
        |p| import::codex_auth::import_active(p, now_ms),
    );
    push_source(
        &mut batches,
        request.sources.switchback_auth_registry.as_deref(),
        ImportSource::SwitchbackCodexRegistry,
        |p| import::codex_auth::import_registry(p, now_ms),
    );
    push_source(
        &mut batches,
        request.sources.codex_multi_auth.as_deref(),
        ImportSource::CodexMultiAuth,
        |p| import::codex_multi_auth::import_inventory(p, now_ms),
    );
    push_source(
        &mut batches,
        request.sources.quota_cache.as_deref(),
        ImportSource::CodexMultiAuthQuota,
        |p| import::codex_multi_auth::import_quota(p, now_ms),
    );
    push_source(
        &mut batches,
        request.sources.codexbar_history.as_deref(),
        ImportSource::CodexBar,
        |p| import::codexbar::import(p, now_ms),
    );
    push_source(
        &mut batches,
        request.sources.claude_auth.as_deref(),
        ImportSource::ClaudeAuth,
        |p| import::claude_auth::import(p, now_ms),
    );
    push_source(
        &mut batches,
        request.sources.codexbar_config.as_deref(),
        ImportSource::CodexBarConfig,
        |p| import::codexbar_config::import(p, now_ms),
    );
    let digest = input_digest(&batches);
    if current.revision > 0 && current_digest == Some(digest.as_str()) {
        return Ok(ReconcilePlan {
            base_revision: current.revision,
            digest,
            changed: false,
            snapshot: current.clone(),
            now_ms,
        });
    }
    let next = current.revision + 1;
    let mut snapshot = compose(current, &batches, next, now_ms)?;
    snapshot.revision = next;
    Ok(ReconcilePlan {
        base_revision: current.revision,
        digest,
        changed: true,
        snapshot,
        now_ms,
    })
}

fn push_source<F>(
    batches: &mut Vec<ImportBatch>,
    path: Option<&std::path::Path>,
    source: ImportSource,
    load: F,
) where
    F: FnOnce(&std::path::Path) -> ImportBatch,
{
    batches.push(match path {
        Some(path) if path.exists() => load(path),
        Some(_) => ImportBatch::failed(source, SourceReadStatus::Missing, "source missing"),
        None => ImportBatch::failed(source, SourceReadStatus::Missing, "source not configured"),
    })
}

fn input_digest(batches: &[ImportBatch]) -> String {
    let mut tuples: Vec<String> = batches
        .iter()
        .map(|b| {
            format!(
                "{:?}\0configured\0{}\0provider-import/v0",
                b.source,
                b.fingerprint.as_deref().unwrap_or(match b.status {
                    SourceReadStatus::Missing => "missing",
                    SourceReadStatus::Malformed => "malformed",
                    SourceReadStatus::UnsafePermissions => "unsafe_permissions",
                    SourceReadStatus::ConcurrentWrite => "concurrent_write",
                    SourceReadStatus::Error => "error",
                    SourceReadStatus::Ok => "ok",
                })
            )
        })
        .collect();
    tuples.sort();
    let mut h = Sha256::new();
    for t in tuples {
        h.update(t.as_bytes())
    }
    h.update(NORMALIZATION_VERSION.as_bytes());
    h.update(POLICY_VERSION.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn compose(
    current: &ProviderAccountSnapshot,
    batches: &[ImportBatch],
    revision: u64,
    now_ms: i64,
) -> Result<ProviderAccountSnapshot, AuthorityError> {
    let successful: Vec<&ImportBatch> = batches
        .iter()
        .filter(|b| b.status == SourceReadStatus::Ok)
        .collect();
    let observations: Vec<&Observation> = successful
        .iter()
        .flat_map(|b| b.observations.iter())
        .collect();
    let mut accounts: BTreeMap<ProviderAccountId, ProviderAccountEnrollment> = current
        .accounts
        .iter()
        .cloned()
        .map(|a| (a.id.clone(), a))
        .collect();
    let mut uuid_to_id: BTreeMap<(AliasScheme, String), ProviderAccountId> = BTreeMap::new();
    for account in accounts.values() {
        for alias in &account.aliases {
            if alias.scheme.strong_identity_provider().is_some()
                && alias.binding_state == AliasBindingState::Bound
            {
                uuid_to_id.insert(
                    (alias.scheme, alias.normalized_value.clone()),
                    account.id.clone(),
                );
            }
        }
    }
    for observation in &observations {
        if let Some(uuid) = uuid_alias(observation) {
            uuid_to_id
                .entry((uuid.scheme, uuid.value.clone()))
                .or_insert_with(|| {
                    deterministic_id(observation_provider(observation), &uuid.value)
                });
        }
    }
    let mut assignments: Vec<(&Observation, ProviderAccountId)> = vec![];
    for observation in &observations {
        let id = if let Some(uuid) = uuid_alias(observation) {
            uuid_to_id
                .get(&(uuid.scheme, uuid.value.clone()))
                .cloned()
                .expect("inserted")
        } else if let Some(id) = match_existing(observation, &accounts) {
            id
        } else {
            let anchor = observation
                .aliases
                .iter()
                .min_by_key(|a| a.rank)
                .map(|a| {
                    format!(
                        "{:?}\0{}\0{}",
                        observation.source, observation.record_key, a.value
                    )
                })
                .unwrap_or_else(|| format!("{:?}\0{}", observation.source, observation.record_key));
            deterministic_id(observation_provider(observation), &anchor)
        };
        assignments.push((observation, id));
    }
    let mut strong_owner: BTreeMap<(AliasScheme, String), ProviderAccountId> = BTreeMap::new();
    let mut conflicts = vec![];
    for (observation, id) in &assignments {
        let proven = uuid_alias(observation).is_some();
        let account = accounts
            .entry(id.clone())
            .or_insert_with(|| ProviderAccountEnrollment {
                id: id.clone(),
                provider: ProviderKey(observation_provider(observation).into()),
                state: if proven {
                    EnrollmentState::Enrolled
                } else {
                    EnrollmentState::Parked
                },
                aliases: vec![],
                created_revision: revision,
                updated_revision: revision,
                created_at_ms: now_ms,
                updated_at_ms: now_ms,
                superseded_by: None,
            });
        if proven && account.state != EnrollmentState::Retired {
            account.state = EnrollmentState::Enrolled
        }
        account.updated_revision = revision;
        account.updated_at_ms = now_ms;
        for alias in &observation.aliases {
            let state = if alias.rank <= 3 {
                match strong_owner.get(&(alias.scheme, alias.value.clone())) {
                    Some(owner) if owner != id => {
                        conflicts.push(format!("{} alias collision", alias.scheme.as_str()));
                        AliasBindingState::ParkedConflict
                    }
                    _ => {
                        strong_owner.insert((alias.scheme, alias.value.clone()), id.clone());
                        AliasBindingState::Bound
                    }
                }
            } else {
                AliasBindingState::Bound
            };
            if !account.aliases.iter().any(|b| {
                b.source == observation.source
                    && b.source_record_key == observation.record_key
                    && b.scheme == alias.scheme
                    && b.normalized_value == alias.value
            }) {
                account.aliases.push(AliasBinding {
                    scheme: alias.scheme,
                    normalized_value: alias.value.clone(),
                    display: alias.display.clone(),
                    source: observation.source,
                    source_record_key: observation.record_key.clone(),
                    first_seen_at_ms: now_ms,
                    last_seen_at_ms: now_ms,
                    binding_state: state,
                });
            }
        }
    }
    let successful_sources: BTreeSet<_> = successful.iter().map(|b| b.source).collect();
    let mut credentials: Vec<CredentialMetadata> = current
        .credentials
        .iter()
        .filter(|c| !credential_source_replaced(c, &successful_sources))
        .cloned()
        .collect();
    let mut capacity: Vec<AccountCapacityHealth> = current.capacity.clone();
    for (observation, id) in &assignments {
        if let Some(mut credential) = observation.credential.clone() {
            credential.account_id = id.clone();
            if accounts
                .get(id)
                .is_some_and(|a| a.state != EnrollmentState::Enrolled)
            {
                credential.readiness = CredentialReadiness::Conflict
            }
            credentials.retain(|c| c.pointer != credential.pointer);
            credentials.push(credential)
        }
        for mut health in observation.capacity.clone() {
            health.account_id = id.clone();
            capacity.push(health)
        }
    }
    let mut flattened = Vec::new();
    for health in capacity {
        for window in health.windows.clone() {
            let mut sample = health.clone();
            sample.windows = vec![window];
            flattened.push(sample);
        }
    }
    flattened.sort_by(|a, b| {
        b.sampled_at_ms
            .cmp(&a.sampled_at_ms)
            .then(b.ingested_at_ms.cmp(&a.ingested_at_ms))
            .then(b.source_revision.cmp(&a.source_revision))
    });
    let mut retained: BTreeMap<
        (
            ProviderAccountId,
            Option<String>,
            CapacityWindowKind,
            ImportSource,
        ),
        usize,
    > = BTreeMap::new();
    flattened.retain(|sample| {
        let window = &sample.windows[0];
        let count = retained
            .entry((
                sample.account_id.clone(),
                sample.model.clone(),
                window.window_kind.clone(),
                window.source,
            ))
            .or_default();
        if *count >= 8 {
            return false;
        }
        *count += 1;
        true
    });
    flattened.sort_by(|a, b| {
        a.account_id
            .cmp(&b.account_id)
            .then(a.model.cmp(&b.model))
            .then(a.windows[0].window_kind.cmp(&b.windows[0].window_kind))
            .then(a.sampled_at_ms.cmp(&b.sampled_at_ms))
    });
    flattened.dedup_by(|a, b| {
        a.account_id == b.account_id
            && a.model == b.model
            && a.windows[0].window_kind == b.windows[0].window_kind
            && a.windows[0].source == b.windows[0].source
            && a.sampled_at_ms == b.sampled_at_ms
            && a.source_revision == b.source_revision
    });
    let capacity = flattened;
    let active_observation = assignments
        .iter()
        .find(|(o, _)| o.source == ImportSource::CodexActiveAuth && o.active);
    let active_clients = if let Some((o, id)) = active_observation {
        vec![ActiveClientState {
            client: NativeClientKind::Codex,
            session_mode: NativeSessionMode::Shared,
            active_account: Some(id.clone()),
            active_source: ActiveAccountSource::CodexActiveAuth,
            active_runs: assignments
                .iter()
                .find(|(row, _)| row.source == ImportSource::SwitchbackCodexRegistry)
                .map(|(row, _)| row.runs.clone())
                .unwrap_or_default(),
            source_revision: o.fingerprint.clone(),
            observed_at_ms: now_ms,
            freshness: Freshness::Fresh,
        }]
    } else {
        vec![ActiveClientState {
            client: NativeClientKind::Codex,
            session_mode: NativeSessionMode::Shared,
            active_account: None,
            active_source: ActiveAccountSource::Unknown,
            active_runs: vec![],
            source_revision: String::new(),
            observed_at_ms: now_ms,
            freshness: Freshness::Unknown,
        }]
    };
    let sources = batches
        .iter()
        .map(|b| ImportSourceStatus {
            source: b.source,
            fingerprint: b.fingerprint.clone(),
            status: b.status,
            observed_at_ms: now_ms,
            last_good_revision: if b.status == SourceReadStatus::Ok {
                Some(revision)
            } else {
                current
                    .sources
                    .iter()
                    .find(|s| s.source == b.source)
                    .and_then(|s| s.last_good_revision)
            },
            detail: b.detail.clone(),
        })
        .collect();
    let mut accounts: Vec<_> = accounts.into_values().collect();
    for a in &mut accounts {
        a.aliases.sort_by(|x, y| {
            x.scheme
                .rank()
                .cmp(&y.scheme.rank())
                .then(x.source.cmp(&y.source))
                .then(x.normalized_value.cmp(&y.normalized_value))
        })
    }
    accounts.sort_by(|a, b| a.provider.cmp(&b.provider).then(a.id.cmp(&b.id)));
    credentials.sort_by(|a, b| {
        a.account_id
            .cmp(&b.account_id)
            .then(format!("{:?}", a.pointer).cmp(&format!("{:?}", b.pointer)))
    });
    Ok(ProviderAccountSnapshot {
        schema: "switchback/provider-account-snapshot@1".into(),
        revision,
        freshness: if batches
            .iter()
            .all(|b| b.status == SourceReadStatus::Ok || b.status == SourceReadStatus::Missing)
        {
            Freshness::Fresh
        } else {
            Freshness::Error
        },
        accounts,
        credentials,
        active_clients,
        capacity,
        sources,
        conflicts,
        metadata_only: true,
    })
}

fn uuid_alias(observation: &Observation) -> Option<&NormalizedAlias> {
    observation
        .aliases
        .iter()
        .find(|a| a.scheme.strong_identity_provider().is_some())
}

/// The provider this observation belongs to, derived from its
/// lowest-rank (strongest) alias scheme instead of a literal. Every
/// importer emits at least one provider-specific alias per observation,
/// so this only falls back to "openai" (the pre-existing default) when an
/// observation somehow carries nothing but provider-agnostic aliases.
fn observation_provider(observation: &Observation) -> &'static str {
    observation
        .aliases
        .iter()
        .min_by_key(|a| a.rank)
        .and_then(|a| a.scheme.provider())
        .unwrap_or("openai")
}
fn match_existing(
    observation: &Observation,
    accounts: &BTreeMap<ProviderAccountId, ProviderAccountEnrollment>,
) -> Option<ProviderAccountId> {
    for alias in observation.aliases.iter().filter(|a| a.rank <= 3) {
        for account in accounts.values() {
            if account.aliases.iter().any(|b| {
                b.scheme == alias.scheme
                    && b.normalized_value == alias.value
                    && b.binding_state == AliasBindingState::Bound
            }) {
                return Some(account.id.clone());
            }
        }
    }
    None
}
fn credential_source_replaced(c: &CredentialMetadata, successful: &BTreeSet<ImportSource>) -> bool {
    match c.pointer {
        CredentialPointer::CodexActiveAuth { .. } => {
            successful.contains(&ImportSource::CodexActiveAuth)
        }
        CredentialPointer::SwitchbackCodexRegistry { .. } => {
            successful.contains(&ImportSource::SwitchbackCodexRegistry)
        }
        CredentialPointer::ConfiguredAccount { .. } => false,
    }
}

pub(crate) fn observation_key(source: ImportSource, record: &str, fingerprint: &str) -> String {
    hex_digest(
        format!(
            "{:?}{record}{fingerprint}{NORMALIZATION_VERSION}{POLICY_VERSION}",
            source
        )
        .as_bytes(),
    )
}
