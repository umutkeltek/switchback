use std::path::Path;

use serde_json::Value;

use super::{json, source_error, stable_read, ImportBatch, Observation};
use crate::provider_accounts::{
    normalize_alias, AuthorityError, ImportSource, ProviderAccountAlias, SourceReadStatus,
};

/// Imports z.ai token-account identities out of `~/.codexbar/config.json`'s
/// `providers[id=="zai"].tokenAccounts.accounts[]` list. Each account's
/// `token` field carries a live secret; this importer only ever reads
/// `id` (a uuid-shaped account identifier) and `label` off each row. The
/// `token` key is never accessed, so its value can never reach an
/// alias, a credential record, the sqlite store, or any JSON output.
/// `activeIndex` is deliberately not surfaced (no active-client tracking
/// for non-Codex clients per policy).
pub(crate) fn import(path: &Path, _now_ms: i64) -> ImportBatch {
    let source = ImportSource::CodexBarConfig;
    let result = (|| {
        let (bytes, fp) = stable_read(path, 4 * 1024 * 1024, true)?;
        let root = json(&bytes, 32)?;
        let providers = root
            .get("providers")
            .and_then(Value::as_array)
            .ok_or_else(|| AuthorityError::MalformedSource("providers array missing".into()))?;
        let zai = providers
            .iter()
            .find(|p| p.get("id").and_then(Value::as_str) == Some("zai"));
        let accounts = zai
            .and_then(|z| z.pointer("/tokenAccounts/accounts"))
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if accounts.len() > 1000 {
            return Err(AuthorityError::MalformedSource(
                "too many zai token accounts".into(),
            ));
        }
        let mut observations = Vec::with_capacity(accounts.len());
        for (index, row) in accounts.iter().enumerate() {
            let id = row.get("id").and_then(Value::as_str).ok_or_else(|| {
                AuthorityError::MalformedSource("zai tokenAccounts[].id missing".into())
            })?;
            let mut aliases = vec![normalize_alias(ProviderAccountAlias::ZaiTokenAccountId(
                id.into(),
            ))?];
            if let Some(label) = row.get("label").and_then(Value::as_str) {
                aliases.push(normalize_alias(ProviderAccountAlias::Label(label.into()))?);
            }
            observations.push(Observation {
                source,
                record_key: format!("tokenAccounts[{index}]"),
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
