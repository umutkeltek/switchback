use std::path::Path;

use serde_json::Value;

use super::{source_error, stable_read, ImportBatch, Observation};
use crate::provider_accounts::{
    normalize_alias, AccountCapacityHealth, CapacityReset, CapacityUsed, CapacityWindow,
    CapacityWindowKind, Freshness, ImportSource, ProviderAccountAlias, ProviderAccountId,
    SourceReadStatus,
};

pub(crate) fn import(path: &Path, now_ms: i64) -> ImportBatch {
    let source = ImportSource::CodexBar;
    let result = (|| {
        let (bytes, fp) = stable_read(path, 64 * 1024 * 1024, false)?;
        let text = std::str::from_utf8(&bytes).map_err(|e| {
            crate::provider_accounts::AuthorityError::MalformedSource(e.to_string())
        })?;
        let lines: Vec<_> = text.split_inclusive('\n').collect();
        if lines.len() > 100_000 {
            return Err(crate::provider_accounts::AuthorityError::MalformedSource(
                "too many CodexBar records".into(),
            ));
        }
        let mut observations = vec![];
        for (index, raw) in lines.iter().enumerate() {
            let line = raw.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                continue;
            }
            if line.len() > 64 * 1024 {
                return Err(crate::provider_accounts::AuthorityError::MalformedSource(
                    "CodexBar line exceeds limit".into(),
                ));
            }
            let row: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_e) if index + 1 == lines.len() && index > 0 => break,
                Err(e) => {
                    return Err(crate::provider_accounts::AuthorityError::MalformedSource(
                        e.to_string(),
                    ))
                }
            };
            if row.get("provider").and_then(Value::as_str) != Some("codex") {
                continue;
            }
            let key = row
                .get("accountKey")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    crate::provider_accounts::AuthorityError::MalformedSource(
                        "accountKey missing".into(),
                    )
                })?;
            let mut aliases = vec![normalize_alias(ProviderAccountAlias::CodexBarAccountKey(
                key.into(),
            ))?];
            if let Some(terminal) = key.rsplit(':').next() {
                if crate::provider_accounts::normalize::normalize_uuid(terminal).is_ok() {
                    aliases.push(normalize_alias(ProviderAccountAlias::OpenAiAccountUuid(
                        terminal.into(),
                    ))?)
                }
            }
            let sampled = row
                .get("sampledAt")
                .and_then(Value::as_i64)
                .ok_or_else(|| {
                    crate::provider_accounts::AuthorityError::MalformedSource(
                        "sampledAt missing".into(),
                    )
                })?;
            if sampled > now_ms + 300_000 {
                return Err(crate::provider_accounts::AuthorityError::MalformedSource(
                    "sample timestamp too far in future".into(),
                ));
            }
            let kind = match row
                .get("windowKind")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
            {
                "primary" => CapacityWindowKind::Primary,
                "secondary" => CapacityWindowKind::Secondary,
                "requests_per_minute" => CapacityWindowKind::RequestsPerMinute,
                "tokens_per_minute" => CapacityWindowKind::TokensPerMinute,
                "concurrent_sessions" => CapacityWindowKind::ConcurrentSessions,
                v => CapacityWindowKind::ProviderDefined(v.into()),
            };
            let used = row
                .get("usedPercent")
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite() && *v >= 0.0)
                .map(|used_percent| CapacityUsed::Percent { used_percent })
                .unwrap_or(CapacityUsed::Unknown);
            let reset = match row.get("resetsAt") {
                Some(Value::Number(v)) => v
                    .as_i64()
                    .map(|resets_at_ms| CapacityReset::At { resets_at_ms })
                    .unwrap_or(CapacityReset::Unknown),
                Some(Value::String(v)) if v == "rolling" => CapacityReset::Rolling,
                _ => CapacityReset::Unknown,
            };
            let window = CapacityWindow {
                window_kind: kind,
                window_minutes: row
                    .get("windowMinutes")
                    .and_then(Value::as_u64)
                    .and_then(|v| u32::try_from(v).ok()),
                used,
                resets_at: reset,
                source,
            };
            let health = AccountCapacityHealth {
                account_id: ProviderAccountId(String::new()),
                model: None,
                windows: vec![window],
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
                record_key: format!("line:{}", index + 1),
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
