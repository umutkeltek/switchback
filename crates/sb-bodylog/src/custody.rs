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

/// Caller holds the segment lock. A store's own spool has exactly one writer,
/// so a sealed segment there with no custody record can only be that store's:
/// the record is missing because its writer predates custody records. Write the
/// record that writer would have written at creation. An existing record,
/// complete or not, is never rewritten, and archive roots keep the strict rule.
pub(super) fn adopt_legacy_spool(
    segment: &Path,
    spool_segments: &Path,
    archive_root: &Path,
    state_dir: &Path,
) -> Result<()> {
    if read(segment)?.is_some() {
        return Ok(());
    }
    let refuse = |reason: &str| {
        Err(BodyLogError::new(format!(
            "legacy spool adoption refused: {reason}: {}",
            segment.display()
        )))
    };
    if !fs::symlink_metadata(segment)?.file_type().is_file() {
        return refuse("segment is not a regular file");
    }
    let canonical = fs::canonicalize(segment)?;
    if !canonical.starts_with(fs::canonicalize(spool_segments)?) {
        return refuse("segment lies outside this store's spool");
    }
    if fs::canonicalize(archive_root).is_ok_and(|root| canonical.starts_with(root)) {
        return refuse("segment lies inside an archive root");
    }
    if !super::read_verified_segment_manifest(segment)?.is_some_and(|manifest| manifest.sealed) {
        return refuse("segment is not sealed");
    }
    register_owner(segment, state_dir, true)
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

#[cfg(test)]
mod tests {
    use super::*;

    // Drain never offers an unsealed segment; adoption must refuse one anyway.
    #[test]
    fn legacy_adoption_refuses_an_unsealed_segment_until_it_is_sealed() {
        let root =
            std::env::temp_dir().join(format!("switchback-custody-adopt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let state_dir = root.join("state");
        let spool_segments = state_dir.join("body/spool/segments");
        let day = spool_segments.join("2026/01/02");
        fs::create_dir_all(&day).unwrap();
        let segment = day.join("capture-1-p1-1.sbcap");
        fs::write(&segment, crate::CAPTURE_SEGMENT_MAGIC).unwrap();
        let archive = root.join("archive");
        crate::write_segment_manifest(&segment, &[], false).unwrap();

        let error = adopt_legacy_spool(&segment, &spool_segments, &archive, &state_dir)
            .expect_err("an unsealed segment is never adopted");
        assert!(error.to_string().contains("not sealed"), "{error}");
        assert!(read(&segment).unwrap().is_none());

        crate::write_segment_manifest(&segment, &[], true).unwrap();
        adopt_legacy_spool(&segment, &spool_segments, &archive, &state_dir).unwrap();
        let custody = read(&segment).unwrap().expect("sealed segment is adopted");
        assert!(custody.complete);
        assert_eq!(custody.owners, [fs::canonicalize(&state_dir).unwrap()]);
    }
}
