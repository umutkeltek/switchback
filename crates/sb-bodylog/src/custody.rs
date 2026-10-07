//! Durable projection owners, serialized by the existing segment lock.
//! Missing legacy records never attest that the owner set is complete.

use std::collections::HashSet;
use std::fs;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{backup::atomic_write_private, BodyLogError, Result};

const SCHEMA: &str = "switchback/segment-custody@1";

#[derive(Serialize, Deserialize)]
pub(super) struct SegmentCustody {
    schema: String,
    segment_file: String,
    pub complete: bool,
    pub owners: Vec<PathBuf>,
}

fn custody_path(segment: &Path) -> PathBuf {
    let mut name = segment.as_os_str().to_os_string();
    name.push(".custody.json");
    PathBuf::from(name)
}

pub(super) fn read(segment: &Path) -> Result<Option<SegmentCustody>> {
    let path = custody_path(segment);
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = match options.open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > 64 * 1024 {
        return Err(BodyLogError::new("unsafe segment custody record"));
    }
    let mut bytes = Vec::new();
    file.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 64 * 1024 {
        return Err(BodyLogError::new(
            "segment custody record exceeds size bound",
        ));
    }
    let custody: SegmentCustody = serde_json::from_slice(&bytes)?;
    let name = segment.file_name().and_then(|name| name.to_str());
    if custody.schema != SCHEMA
        || name != Some(custody.segment_file.as_str())
        || custody.owners.is_empty()
        || custody.owners.len() > 64
    {
        return Err(BodyLogError::new("invalid segment custody record"));
    }
    let mut owners = HashSet::new();
    for owner in &custody.owners {
        // Missing/retired peers must hold reclaim, not kill unrelated new capture.
        // Current canonical identity and receipt availability are checked at reclaim.
        if !owner.is_absolute()
            || owner
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
            || !owners.insert(owner)
        {
            return Err(BodyLogError::new("invalid segment custody owner"));
        }
    }
    Ok(Some(custody))
}

/// Caller holds the segment lock. Only the original writer can attest completeness.
pub(super) fn register_owner(segment: &Path, state_dir: &Path, created: bool) -> Result<()> {
    let owner = fs::canonicalize(state_dir)?;
    let existing = read(segment)?;
    if created && existing.is_some() {
        return Err(BodyLogError::new(
            "new segment already has a custody record",
        ));
    }
    let mut custody = existing.unwrap_or(SegmentCustody {
        schema: SCHEMA.to_string(),
        segment_file: segment
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| BodyLogError::new("segment has no UTF-8 file name"))?
            .to_string(),
        complete: created,
        owners: Vec::new(),
    });
    if custody.owners.contains(&owner) {
        return Ok(());
    }
    if custody.owners.len() >= 64 {
        return Err(BodyLogError::new("segment custody owner limit reached"));
    }
    custody.owners.push(owner);
    custody.owners.sort();
    atomic_write_private(
        &custody_path(segment),
        &serde_json::to_vec_pretty(&custody)?,
    )
}

/// Drain holds both segment locks and preserves exclusive, complete source custody.
pub(super) fn prepare_drain(source: &Path, destination: &Path, state_dir: &Path) -> Result<()> {
    let owner = fs::canonicalize(state_dir)?;
    let source_custody = read(source)?
        .ok_or_else(|| BodyLogError::new("spool drain lacks complete segment custody"))?;
    if !source_custody.complete || source_custody.owners != [owner.clone()] {
        return Err(BodyLogError::new(
            "spool drain requires complete exclusive custody",
        ));
    }
    match read(destination)? {
        Some(custody) if custody.complete && custody.owners.contains(&owner) => Ok(()),
        Some(_) => Err(BodyLogError::new("drain destination has ambiguous custody")),
        None if destination.try_exists()? => Err(BodyLogError::new(
            "existing drain destination lacks custody",
        )),
        None => atomic_write_private(
            &custody_path(destination),
            &serde_json::to_vec_pretty(&source_custody)?,
        ),
    }
}
