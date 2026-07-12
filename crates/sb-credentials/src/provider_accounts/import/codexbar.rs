use std::path::Path;

use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use super::{source_error, stable_read, ImportBatch, Observation};
use crate::provider_accounts::{
    normalize_alias, AccountCapacityHealth, CapacityReset, CapacityUsed, CapacityWindow,
    CapacityWindowKind, Freshness, ImportSource, ProviderAccountAlias, ProviderAccountId,
    SourceReadStatus,
};

/// Parse a CodexBar timestamp field, accepting either an i64 epoch-ms number
/// (the historical shape) or an RFC3339 string (what the live recorder at
/// `~/Library/Application Support/CodexBar/usage-history.jsonl` actually
/// writes, e.g. `"2026-07-12T17:33:48Z"`). Returns epoch-ms either way.
fn parse_epoch_ms(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => OffsetDateTime::parse(s, &Rfc3339)
            .ok()
            .map(|dt| (dt.unix_timestamp_nanos() / 1_000_000) as i64),
        _ => None,
    }
}

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
            let sampled = parse_epoch_ms(row.get("sampledAt")).ok_or_else(|| {
                crate::provider_accounts::AuthorityError::MalformedSource(
                    "sampledAt missing or invalid".into(),
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
                Some(Value::String(v)) if v == "rolling" => CapacityReset::Rolling,
                Some(value) => parse_epoch_ms(Some(value))
                    .map(|resets_at_ms| CapacityReset::At { resets_at_ms })
                    .unwrap_or(CapacityReset::Unknown),
                None => CapacityReset::Unknown,
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::import;
    use crate::provider_accounts::SourceReadStatus;

    const NOW_MS: i64 = 1_783_878_000_000;

    fn import_row(name: &str, row: &str) -> super::ImportBatch {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "switchback-codexbar-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("temp root");
        let path = root.join("usage-history.jsonl");
        fs::write(&path, format!("{row}\n")).expect("write CodexBar row");

        let batch = import(&path, NOW_MS);
        fs::remove_dir_all(root).expect("remove temp root");
        batch
    }

    #[test]
    fn sampled_at_accepts_rfc3339_string() {
        let row = r#"{"v":1,"provider":"codex","accountKey":"codex:v1:synthetic:01234567-89ab-cdef-8123-456789abcdef","windowKind":"primary","windowMinutes":300,"usedPercent":12.5,"resetsAt":"2026-07-12T22:33:48Z","sampledAt":"2026-07-12T17:33:48Z","source":"synthetic"}"#;

        let batch = import_row("rfc3339", row);

        assert_eq!(batch.status, SourceReadStatus::Ok, "{:?}", batch.detail);
        assert_eq!(batch.observations.len(), 1);
        assert_eq!(
            batch.observations[0].capacity[0].sampled_at_ms,
            1_783_877_628_000
        );
    }

    #[test]
    fn sampled_at_accepts_i64_epoch_ms() {
        let row = r#"{"v":1,"provider":"codex","accountKey":"codex:v1:synthetic:01234567-89ab-cdef-8123-456789abcdef","windowKind":"primary","windowMinutes":300,"usedPercent":12.5,"resetsAt":1783895628000,"sampledAt":1783877628000,"source":"synthetic"}"#;

        let batch = import_row("i64-ms", row);

        assert_eq!(batch.status, SourceReadStatus::Ok, "{:?}", batch.detail);
        assert_eq!(batch.observations.len(), 1);
        assert_eq!(
            batch.observations[0].capacity[0].sampled_at_ms,
            1_783_877_628_000
        );
    }

    #[test]
    fn sampled_at_rejects_unparseable_string() {
        let row = r#"{"v":1,"provider":"codex","accountKey":"codex:v1:synthetic:01234567-89ab-cdef-8123-456789abcdef","windowKind":"primary","windowMinutes":300,"usedPercent":12.5,"resetsAt":"rolling","sampledAt":"not-a-timestamp","source":"synthetic"}"#;

        let batch = import_row("garbage", row);

        assert_eq!(batch.status, SourceReadStatus::Malformed, "{:?}", batch.detail);
    }
}
