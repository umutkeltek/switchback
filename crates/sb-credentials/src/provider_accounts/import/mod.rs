use std::fs::{File, Metadata, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use super::normalize::hex_digest;
use super::AuthorityError;
use super::{
    AccountCapacityHealth, ActiveRun, CredentialMetadata, ImportSource, NormalizedAlias,
    SourceReadStatus,
};

pub mod codex_auth;
pub mod codex_multi_auth;
pub mod codexbar;

#[derive(Debug, Clone)]
pub(crate) struct Observation {
    pub source: ImportSource,
    pub record_key: String,
    pub fingerprint: String,
    pub aliases: Vec<NormalizedAlias>,
    pub credential: Option<CredentialMetadata>,
    pub capacity: Vec<AccountCapacityHealth>,
    pub active: bool,
    pub runs: Vec<ActiveRun>,
}

#[derive(Debug, Clone)]
pub(crate) struct ImportBatch {
    pub source: ImportSource,
    pub fingerprint: Option<String>,
    pub status: SourceReadStatus,
    pub detail: Option<String>,
    pub observations: Vec<Observation>,
}

impl ImportBatch {
    pub fn failed(
        source: ImportSource,
        status: SourceReadStatus,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            source,
            fingerprint: None,
            status,
            detail: Some(detail.into()),
            observations: vec![],
        }
    }
}

pub(crate) fn stable_read(
    path: &Path,
    limit: u64,
    token_bearing: bool,
) -> Result<(Vec<u8>, String), AuthorityError> {
    let mut retried = false;
    loop {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        let mut file = options
            .open(path)
            .map_err(|e| AuthorityError::SourceRead(format!("{}: {e}", source_kind_path(path))))?;
        let before = file
            .metadata()
            .map_err(|e| AuthorityError::SourceRead(e.to_string()))?;
        validate_metadata(path, &before, token_bearing)?;
        if before.len() > limit {
            return Err(AuthorityError::SourceRead(
                "source exceeds size limit".into(),
            ));
        }
        let mut bytes = Vec::with_capacity(before.len() as usize);
        file.by_ref()
            .take(limit + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| AuthorityError::SourceRead(e.to_string()))?;
        if bytes.len() as u64 > limit {
            return Err(AuthorityError::SourceRead(
                "source exceeds size limit".into(),
            ));
        }
        let after = std::fs::symlink_metadata(path)
            .map_err(|e| AuthorityError::SourceRead(e.to_string()))?;
        if stamp(&before) == stamp(&after) {
            return Ok((bytes.clone(), hex_digest(&bytes)));
        }
        if retried {
            return Err(AuthorityError::ConcurrentWrite);
        }
        retried = true;
    }
}

fn validate_metadata(
    path: &Path,
    meta: &Metadata,
    token_bearing: bool,
) -> Result<(), AuthorityError> {
    if !meta.file_type().is_file() {
        return Err(AuthorityError::UnsafeSource(
            "source is not a regular file".into(),
        ));
    }
    if meta.uid() != unsafe { libc::geteuid() } {
        return Err(AuthorityError::UnsafeSource(
            "source owner differs from current user".into(),
        ));
    }
    let mode = meta.permissions().mode();
    if (token_bearing && mode & 0o077 != 0) || (!token_bearing && mode & 0o022 != 0) {
        return Err(AuthorityError::UnsafeSource(
            "unsafe source permissions".into(),
        ));
    }
    if token_bearing {
        if let Some(parent) = path.parent() {
            let pmode = parent
                .metadata()
                .map_err(|e| AuthorityError::SourceRead(e.to_string()))?
                .permissions()
                .mode();
            if pmode & 0o022 != 0 {
                return Err(AuthorityError::UnsafeSource(
                    "unsafe parent permissions".into(),
                ));
            }
        }
    }
    Ok(())
}

fn stamp(meta: &Metadata) -> (u64, u64, u64, i64, i64) {
    (
        meta.dev(),
        meta.ino(),
        meta.len(),
        meta.mtime(),
        meta.mtime_nsec(),
    )
}
fn source_kind_path(path: &Path) -> &'static str {
    if path.file_name().and_then(|v| v.to_str()) == Some("auth.json") {
        "codex_active_auth"
    } else {
        "provider_account_source"
    }
}

pub(crate) fn json(bytes: &[u8], max_depth: usize) -> Result<serde_json::Value, AuthorityError> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|e| AuthorityError::MalformedSource(e.to_string()))?;
    if depth(&value) > max_depth {
        return Err(AuthorityError::MalformedSource(
            "JSON nesting exceeds limit".into(),
        ));
    }
    Ok(value)
}
fn depth(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Array(v) => 1 + v.iter().map(depth).max().unwrap_or(0),
        serde_json::Value::Object(v) => 1 + v.values().map(depth).max().unwrap_or(0),
        _ => 1,
    }
}

pub(crate) fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    use base64::Engine as _;
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

pub(crate) fn source_error(source: ImportSource, err: AuthorityError) -> ImportBatch {
    let status = match err {
        AuthorityError::ConcurrentWrite => SourceReadStatus::ConcurrentWrite,
        AuthorityError::UnsafeSource(_) => SourceReadStatus::UnsafePermissions,
        AuthorityError::MalformedSource(_) => SourceReadStatus::Malformed,
        _ => SourceReadStatus::Error,
    };
    ImportBatch::failed(source, status, err.to_string())
}

#[allow(dead_code)]
fn _assert_file_send(_: File) {}
