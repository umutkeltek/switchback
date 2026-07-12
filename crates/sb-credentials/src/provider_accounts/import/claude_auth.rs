use std::path::Path;

use serde_json::Value;

use super::{json, source_error, stable_read, ImportBatch, Observation};
use crate::provider_accounts::{
    normalize_alias, AuthorityError, ImportSource, ProviderAccountAlias, SourceReadStatus,
};

/// Imports the canonical Anthropic (Claude) account identity out of
/// `~/.claude.json`'s top-level `oauthAccount` object. The file also
/// carries ~90 unrelated app-state keys (theme, history, feature flags,
/// ...); everything outside `oauthAccount` is ignored. There is no
/// OAuth secret material in this file (tokens live elsewhere), so it is
/// read as non-token-bearing.
pub(crate) fn import(path: &Path, _now_ms: i64) -> ImportBatch {
    let source = ImportSource::ClaudeAuth;
    let result = (|| {
        let (bytes, fp) = stable_read(path, 8 * 1024 * 1024, false)?;
        let root = json(&bytes, 32)?;
        let account = root
            .get("oauthAccount")
            .and_then(Value::as_object)
            .ok_or_else(|| AuthorityError::MalformedSource("oauthAccount missing".into()))?;
        let uuid = account
            .get("accountUuid")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                AuthorityError::MalformedSource("oauthAccount.accountUuid missing".into())
            })?;
        let mut aliases = vec![normalize_alias(
            ProviderAccountAlias::AnthropicAccountUuid(uuid.into()),
        )?];
        if let Some(org) = account.get("organizationUuid").and_then(Value::as_str) {
            aliases.push(normalize_alias(ProviderAccountAlias::AnthropicOrgUuid(
                org.into(),
            ))?);
        }
        if let Some(email) = account.get("emailAddress").and_then(Value::as_str) {
            aliases.push(normalize_alias(ProviderAccountAlias::Email(email.into()))?);
        }
        let observation = Observation {
            source,
            record_key: "oauthAccount".into(),
            fingerprint: fp.clone(),
            aliases,
            credential: None,
            capacity: vec![],
            active: false,
            runs: vec![],
        };
        Ok((fp, vec![observation]))
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
