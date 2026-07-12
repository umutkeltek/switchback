use std::path::Path;

use serde_json::Value;

use super::{json, source_error, stable_read, ImportBatch, Observation};
use crate::provider_accounts::{
    normalize_alias, AccountCapacityHealth, CapacityReset, CapacityUsed, CapacityWindow,
    CapacityWindowKind, Freshness, ImportSource, ProviderAccountAlias, ProviderAccountId,
    SourceReadStatus,
};

pub(crate) fn import_inventory(path: &Path, _now_ms: i64) -> ImportBatch {
    let source = ImportSource::CodexMultiAuth;
    let result = (|| {
        let (bytes, fp) = stable_read(path, 16 * 1024 * 1024, true)?;
        let root = json(&bytes, 32)?;
        if root.get("version").and_then(Value::as_u64).is_none() {
            return Err(crate::provider_accounts::AuthorityError::MalformedSource(
                "unsupported multi-auth version".into(),
            ));
        }
        let accounts = root
            .get("accounts")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                crate::provider_accounts::AuthorityError::MalformedSource(
                    "accounts array missing".into(),
                )
            })?;
        if accounts.len() > 10_000 {
            return Err(crate::provider_accounts::AuthorityError::MalformedSource(
                "too many accounts".into(),
            ));
        }
        let mut observations = Vec::new();
        for (index, row) in accounts.iter().enumerate() {
            let obj = row.as_object().ok_or_else(|| {
                crate::provider_accounts::AuthorityError::MalformedSource(
                    "account row is not object".into(),
                )
            })?;
            let id = obj
                .get("accountId")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    crate::provider_accounts::AuthorityError::MalformedSource(
                        "accountId missing".into(),
                    )
                })?;
            let mut aliases = vec![normalize_alias(
                ProviderAccountAlias::CodexMultiAuthAccountId(id.into()),
            )?];
            if obj.get("accountIdSource").and_then(Value::as_str) == Some("org") {
                aliases.push(normalize_alias(ProviderAccountAlias::OpenAiOrgId(
                    id.into(),
                ))?);
            }
            if let Some(label) = obj.get("accountLabel").and_then(Value::as_str) {
                aliases.push(normalize_alias(ProviderAccountAlias::Label(label.into()))?)
            }
            if let Some(email) = obj.get("email").and_then(Value::as_str) {
                aliases.push(normalize_alias(ProviderAccountAlias::Email(email.into()))?)
            }
            observations.push(Observation {
                source,
                record_key: format!("accounts[{index}]"),
                fingerprint: fp.clone(),
                aliases,
                credential: None,
                capacity: vec![],
                active: false,
                runs: vec![],
            });
        }
        Ok((fp, observations))
    })();
    match result {
        Ok((fp, observations)) => ImportBatch {
            source,
            fingerprint: Some(fp),
            status: SourceReadStatus::Ok,
            detail: None,
            observations,
        },
        Err(e) => source_error(source, e),
    }
}

pub(crate) fn import_quota(path: &Path, now_ms: i64) -> ImportBatch {
    let source = ImportSource::CodexMultiAuthQuota;
    let result = (|| {
        let (bytes, fp) = stable_read(path, 16 * 1024 * 1024, false)?;
        let root = json(&bytes, 32)?;
        if root.get("version").and_then(Value::as_u64).is_none() {
            return Err(crate::provider_accounts::AuthorityError::MalformedSource(
                "unsupported quota version".into(),
            ));
        }
        let by = root
            .get("byAccountId")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                crate::provider_accounts::AuthorityError::MalformedSource(
                    "byAccountId missing".into(),
                )
            })?;
        let mut observations = vec![];
        for (id, row) in by {
            let mut aliases = vec![normalize_alias(
                ProviderAccountAlias::CodexMultiAuthAccountId(id.clone()),
            )?];
            if crate::provider_accounts::normalize::normalize_uuid(id).is_ok() {
                aliases.push(normalize_alias(ProviderAccountAlias::OpenAiAccountUuid(
                    id.clone(),
                ))?)
            }
            let sampled = row
                .get("updatedAt")
                .and_then(Value::as_i64)
                .unwrap_or(now_ms);
            if sampled > now_ms + 300_000 {
                return Err(crate::provider_accounts::AuthorityError::MalformedSource(
                    "sample timestamp too far in future".into(),
                ));
            }
            let mut windows = vec![];
            for (name, kind) in [
                ("primary", CapacityWindowKind::Primary),
                ("secondary", CapacityWindowKind::Secondary),
            ] {
                if let Some(window) = row.get(name) {
                    windows.push(CapacityWindow {
                        window_kind: kind,
                        window_minutes: window
                            .get("windowMinutes")
                            .and_then(Value::as_u64)
                            .and_then(|v| u32::try_from(v).ok()),
                        used: window
                            .get("usedPercent")
                            .and_then(Value::as_f64)
                            .filter(|v| v.is_finite() && *v >= 0.0)
                            .map(|used_percent| CapacityUsed::Percent { used_percent })
                            .unwrap_or(CapacityUsed::Unknown),
                        resets_at: window
                            .get("resetAtMs")
                            .and_then(Value::as_i64)
                            .map(|resets_at_ms| CapacityReset::At { resets_at_ms })
                            .unwrap_or(CapacityReset::Unknown),
                        source,
                    })
                }
            }
            let health = AccountCapacityHealth {
                account_id: ProviderAccountId(String::new()),
                model: row.get("model").and_then(Value::as_str).map(str::to_owned),
                windows,
                sampled_at_ms: sampled,
                ingested_at_ms: now_ms,
                source_revision: fp.clone(),
                freshness: if now_ms - sampled <= 900_000 {
                    Freshness::Fresh
                } else {
                    Freshness::Stale
                },
            };
            observations.push(Observation {
                source,
                record_key: id.clone(),
                fingerprint: fp.clone(),
                aliases,
                credential: None,
                capacity: vec![health],
                active: false,
                runs: vec![],
            });
        }
        Ok((fp, observations))
    })();
    match result {
        Ok((fp, observations)) => ImportBatch {
            source,
            fingerprint: Some(fp),
            status: SourceReadStatus::Ok,
            detail: None,
            observations,
        },
        Err(e) => source_error(source, e),
    }
}
