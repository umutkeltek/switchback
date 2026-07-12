use std::fs;
use std::path::Path;

use serde_json::Value;

use super::{json, jwt_claims, source_error, stable_read, ImportBatch, Observation};
use crate::provider_accounts::{
    normalize_alias, AccountCapacityHealth, ActiveRun, CredentialKind, CredentialMetadata,
    CredentialPointer, CredentialReadiness, ImportSource, NativeTokenPointer, ProviderAccountAlias,
    ProviderAccountId, SourceReadStatus,
};

pub(crate) fn import_active(path: &Path, now_ms: i64) -> ImportBatch {
    match import_auth_file(
        path,
        ImportSource::CodexActiveAuth,
        "active",
        None,
        now_ms,
        true,
    ) {
        Ok((fingerprint, observation)) => ImportBatch {
            source: ImportSource::CodexActiveAuth,
            fingerprint: Some(fingerprint),
            status: SourceReadStatus::Ok,
            detail: None,
            observations: vec![observation],
        },
        Err(e) => source_error(ImportSource::CodexActiveAuth, e),
    }
}

pub(crate) fn import_registry(path: &Path, now_ms: i64) -> ImportBatch {
    let source = ImportSource::SwitchbackCodexRegistry;
    let read_dir = match fs::read_dir(path) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return ImportBatch::failed(source, SourceReadStatus::Missing, "registry missing")
        }
        Err(e) => return ImportBatch::failed(source, SourceReadStatus::Error, e.to_string()),
    };
    let mut entries = match read_dir.collect::<Result<Vec<_>, _>>() {
        Ok(v) => v,
        Err(e) => return ImportBatch::failed(source, SourceReadStatus::Error, e.to_string()),
    };
    entries.sort_by_key(|e| e.file_name());
    let mut observations = Vec::new();
    let mut fingerprints = Vec::new();
    for entry in entries {
        let path = entry.path();
        let name = match path.file_name().and_then(|v| v.to_str()) {
            Some(v) => v,
            None => continue,
        };
        if !name.ends_with(".json")
            || path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|v| v.to_str())
                == Some("backups")
        {
            continue;
        }
        let slot = &name[..name.len() - 5];
        if invalid_slot(slot) {
            return ImportBatch::failed(
                source,
                SourceReadStatus::Malformed,
                "invalid registry slot label",
            );
        }
        match import_auth_file(&path, source, slot, Some(slot), now_ms, false) {
            Ok((fp, ob)) => {
                fingerprints.push(fp);
                observations.push(ob)
            }
            Err(e) => return source_error(source, e),
        }
    }
    let active = read_small_text(&path.join(".active"));
    let runs = read_runs(&path.join(".runs"));
    for observation in &mut observations {
        if active.as_deref() == Some(&observation.record_key) {
            observation.active = true;
        }
        observation.runs = runs.clone();
    }
    let fingerprint =
        crate::provider_accounts::normalize::hex_digest(fingerprints.join("\0").as_bytes());
    ImportBatch {
        source,
        fingerprint: Some(fingerprint),
        status: SourceReadStatus::Ok,
        detail: None,
        observations,
    }
}

fn import_auth_file(
    path: &Path,
    source: ImportSource,
    key: &str,
    slot: Option<&str>,
    now_ms: i64,
    active: bool,
) -> Result<(String, Observation), crate::provider_accounts::AuthorityError> {
    let (bytes, fingerprint) = stable_read(path, 1024 * 1024, true)?;
    let root = json(&bytes, 32)?;
    let tokens = root
        .get("tokens")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            crate::provider_accounts::AuthorityError::MalformedSource(
                "tokens object missing".into(),
            )
        })?;
    let account = tokens
        .get("account_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            crate::provider_accounts::AuthorityError::MalformedSource(
                "tokens.account_id missing".into(),
            )
        })?;
    let uuid = normalize_alias(ProviderAccountAlias::OpenAiAccountUuid(account.into()))?;
    let mut aliases = vec![uuid.clone()];
    let id_claims = tokens
        .get("id_token")
        .and_then(Value::as_str)
        .and_then(jwt_claims);
    if let Some(claims) = &id_claims {
        if let Some(claim_uuid) = claims
            .get("https://api.openai.com/auth.chatgpt_account_id")
            .and_then(Value::as_str)
        {
            let normalized =
                normalize_alias(ProviderAccountAlias::OpenAiAccountUuid(claim_uuid.into()))?;
            if normalized.value != uuid.value {
                return Err(
                    crate::provider_accounts::AuthorityError::CredentialConflict(
                        "ID token account differs from tokens.account_id".into(),
                    ),
                );
            }
        }
        if let Some(email) = claims.get("email").and_then(Value::as_str) {
            aliases.push(normalize_alias(ProviderAccountAlias::Email(email.into()))?);
        }
        if let Some(orgs) = claims
            .get("https://api.openai.com/auth.organizations")
            .and_then(Value::as_array)
        {
            for org in orgs {
                if let Some(id) = org.get("id").and_then(Value::as_str) {
                    aliases.push(normalize_alias(ProviderAccountAlias::OpenAiOrgId(
                        id.into(),
                    ))?);
                }
            }
        }
    }
    if let Some(slot) = slot {
        aliases.push(normalize_alias(ProviderAccountAlias::Label(slot.into()))?);
    }
    let access_claims = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .and_then(jwt_claims);
    let exp = access_claims
        .as_ref()
        .or(id_claims.as_ref())
        .and_then(|v| v.get("exp"))
        .and_then(Value::as_i64)
        .map(|v| v * 1000);
    let pointer = match slot {
        Some(slot) => CredentialPointer::SwitchbackCodexRegistry {
            slot: slot.into(),
            json_pointer: NativeTokenPointer::AccessToken,
        },
        None => CredentialPointer::CodexActiveAuth {
            json_pointer: NativeTokenPointer::AccessToken,
        },
    };
    let credential = CredentialMetadata {
        account_id: ProviderAccountId(String::new()),
        pointer,
        credential_kind: if root.get("auth_mode").and_then(Value::as_str) == Some("apikey") {
            CredentialKind::ApiKey
        } else {
            CredentialKind::OAuth
        },
        access_present: tokens.get("access_token").is_some_and(Value::is_string),
        refresh_present: tokens.get("refresh_token").is_some_and(Value::is_string),
        id_token_present: tokens.get("id_token").is_some_and(Value::is_string),
        expires_at_ms: exp,
        source_revision: fingerprint.clone(),
        observed_at_ms: now_ms,
        readiness: if exp.is_some_and(|v| v <= now_ms) {
            CredentialReadiness::Expired
        } else {
            CredentialReadiness::Ready
        },
    };
    Ok((
        fingerprint.clone(),
        Observation {
            source,
            record_key: key.into(),
            fingerprint,
            aliases,
            credential: Some(credential),
            capacity: Vec::<AccountCapacityHealth>::new(),
            active,
            runs: vec![],
        },
    ))
}

fn invalid_slot(slot: &str) -> bool {
    slot.is_empty()
        || slot.len() > 128
        || slot == "."
        || slot == ".."
        || slot.contains('/')
        || slot.contains('\\')
        || slot.contains('\0')
}
fn read_small_text(path: &Path) -> Option<String> {
    let bytes = stable_read(path, 4096, false).ok()?.0;
    std::str::from_utf8(&bytes)
        .ok()
        .map(str::trim)
        .filter(|v| !invalid_slot(v))
        .map(str::to_owned)
}
fn read_runs(path: &Path) -> Vec<ActiveRun> {
    let Ok((bytes, _)) = stable_read(path, 1024 * 1024, false) else {
        return vec![];
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return vec![];
    };
    text.lines()
        .filter_map(|line| {
            let mut p = line.split('\t');
            Some(ActiveRun {
                pid: p.next()?.parse().ok()?,
                account_label: p.next()?.to_owned(),
                started: p.next().unwrap_or("").to_owned(),
            })
        })
        .collect()
}
