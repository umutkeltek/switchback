use super::*;
use std::fs;
const MAX_TRANSACTION_BYTES: usize = 8 * 1024 * 1024;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    path: PathBuf,
    before: Option<Vec<u8>>,
    after: Vec<u8>,
    before_mode: Option<u32>,
    after_mode: u32,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    schema: String,
    phase: String,
    entries: Vec<Entry>,
}
fn read(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if !meta.is_file() || meta.file_type().is_symlink() {
                anyhow::bail!("refusing non-regular artifact {}", path.display());
            }
            let bytes = fs::read(path)?;
            if bytes.len() > MAX_TRANSACTION_BYTES {
                anyhow::bail!("artifact exceeds transaction limit");
            }
            Ok(Some(bytes))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
pub(super) fn journal_path(artifacts: &[PlannedProfileArtifact]) -> anyhow::Result<PathBuf> {
    let first = artifacts
        .first()
        .ok_or_else(|| anyhow::anyhow!("empty artifact transaction"))?;
    let name = first
        .path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid artifact path"))?;
    Ok(first
        .path
        .with_file_name(format!(".{name}.switchback-apply-journal.json")))
}

/// A live mutex and any journal both require resolution before a new launch.
pub(super) fn pending(artifacts: &[PlannedProfileArtifact]) -> anyhow::Result<bool> {
    let journal = journal_path(artifacts)?;
    for path in [&journal, &journal.with_extension("lock")] {
        match fs::symlink_metadata(path) {
            Ok(_) => return Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(false)
}

pub(super) fn write_private(path: &Path, bytes: &[u8], mode: u32) -> anyhow::Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("artifact has no parent"))?;
    create_private_parent_dirs(parent)?;
    let name = path
        .file_name()
        .and_then(|v| v.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid artifact filename"))?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let temporary = parent.join(format!(".{name}.{}.{}.tmp", std::process::id(), nonce));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        set_mode(&temporary, mode)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        #[cfg(unix)]
        {
            fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        match fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("temporary artifact cleanup failed: {e}"),
        }
    }
    result
}

fn store(path: &Path, journal: &Journal) -> anyhow::Result<()> {
    let body = format!("{}\n", serde_json::to_string(journal)?);
    // Reserve phase/report growth while guaranteeing every published journal
    // remains within the recovery reader's bound.
    if body.len().saturating_add(1024) > MAX_TRANSACTION_BYTES {
        anyhow::bail!("profile journal exceeds recovery limit; no artifacts published");
    }
    write_private(path, body.as_bytes(), 0o600)
}

struct Lock(PathBuf);
impl Drop for Lock {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_file(self.0.join("owner.json")) {
            eprintln!("profile transaction owner cleanup failed: {e}");
        }
        if let Err(e) = fs::remove_dir(&self.0) {
            eprintln!("profile transaction lock cleanup failed: {e}");
        }
    }
}
fn lock(path: &Path) -> anyhow::Result<Lock> {
    let path = path.with_extension("lock");
    if let Some(parent) = path.parent() {
        create_private_parent_dirs(parent)?;
    }
    fs::create_dir(&path).map_err(|e| {
        anyhow::anyhow!("profile transaction busy or lock requires explicit recovery: {e}")
    })?;
    let guard = Lock(path.clone());
    set_mode(&path, 0o700)?;
    fs::write(
        path.join("owner.json"),
        serde_json::to_vec(&json!({"pid":std::process::id()}))?,
    )?;
    set_mode(&path.join("owner.json"), 0o600)?;
    Ok(guard)
}
fn restore(journal: &Journal) -> anyhow::Result<Vec<String>> {
    let mut conflicts = Vec::new();
    for entry in journal.entries.iter().rev() {
        let current = read(&entry.path)?;
        let current_mode = path_mode(&entry.path)?;
        if current == entry.before && current_mode == entry.before_mode {
            continue;
        }
        if current == entry.before && current_mode == Some(entry.after_mode) {
            if let Some(mode) = entry.before_mode {
                set_mode(&entry.path, mode)?;
            }
            continue;
        }
        if current.as_deref() != Some(entry.after.as_slice())
            || current_mode != Some(entry.after_mode)
        {
            conflicts.push(entry.path.display().to_string());
            continue;
        }
        if let Some(before) = &entry.before {
            write_private(&entry.path, before, entry.before_mode.unwrap_or(0o600))?;
        } else {
            fs::remove_file(&entry.path)?;
        }
    }
    Ok(conflicts)
}
/// A pending journal is durable before the first publication. Both publication
/// and recovery compare bytes, so later harness/user edits are never restored over.
pub(super) fn apply<F: FnOnce() -> anyhow::Result<()>>(
    artifacts: &[PlannedProfileArtifact],
    verify: F,
) -> anyhow::Result<()> {
    if artifacts.is_empty() {
        return verify();
    }
    let path = journal_path(artifacts)?;
    let _lock = lock(&path)?;
    if read(&path)?.is_some() {
        anyhow::bail!(
            "pending profile transaction requires recovery: {}",
            path.display()
        );
    }
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();
    for artifact in artifacts {
        if !seen.insert(&artifact.path) {
            anyhow::bail!("duplicate transaction artifact");
        }
        let before = read(&artifact.path)?;
        if artifact.kind == "wrapper"
            && before
                .as_deref()
                .is_some_and(|bytes| !switchback_owns_wrapper(bytes, &artifact.contents))
        {
            anyhow::bail!(
                "refusing to overwrite unowned wrapper {}",
                artifact.path.display()
            );
        }
        entries.push(Entry {
            path: artifact.path.clone(),
            before,
            before_mode: path_mode(&artifact.path)?,
            after: artifact.contents.as_bytes().to_vec(),
            after_mode: artifact.mode,
        });
    }
    let mut journal = Journal {
        schema: "switchback/profile-apply-journal@1".into(),
        phase: "pending".into(),
        entries,
    };
    store(&path, &journal)?;
    let result = (|| {
        for entry in &journal.entries {
            if read(&entry.path)? != entry.before || path_mode(&entry.path)? != entry.before_mode {
                anyhow::bail!(
                    "artifact changed before publication: {}",
                    entry.path.display()
                );
            }
            if let Some(parent) = entry.path.parent() {
                create_private_parent_dirs(parent)?;
            }
            write_private(&entry.path, &entry.after, entry.after_mode)?;
            if read(&entry.path)?.as_deref() != Some(entry.after.as_slice()) {
                anyhow::bail!("artifact changed after publication");
            }
        }
        verify()
    })();
    if let Err(error) = result {
        let conflicts = restore(&journal)?;
        if conflicts.is_empty() {
            fs::remove_file(&path)?;
        } else {
            journal.phase = "conflicted".into();
            store(&path, &journal)?;
        }
        return Err(error.context(format!(
            "profile transaction rolled back; recovery conflicts: {}",
            conflicts.join(", ")
        )));
    }
    journal.phase = "committed".into();
    store(&path, &journal)?;
    fs::remove_file(path)?;
    Ok(())
}
pub(super) fn recover(artifacts: &[PlannedProfileArtifact]) -> anyhow::Result<Value> {
    let path = journal_path(artifacts)?;
    // Explicit recovery may reclaim only a recorded dead process's mutex.
    let mutex = path.with_extension("lock");
    if mutex.exists() {
        let owner_path = mutex.join("owner.json");
        let owner = read(&owner_path)?.ok_or_else(|| {
            anyhow::anyhow!("transaction lock has no owner; manual recovery required")
        })?;
        let owner: Value = serde_json::from_slice(&owner)?;
        let pid = owner["pid"]
            .as_u64()
            .filter(|pid| *pid > 0 && *pid <= i32::MAX as u64)
            .ok_or_else(|| anyhow::anyhow!("invalid transaction lock owner"))?;
        #[cfg(unix)]
        {
            let status = Command::new("/bin/kill")
                .args(["-0", &pid.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()?;
            if status.success() {
                anyhow::bail!("transaction owner is still alive");
            }
            if read(&owner_path)?.as_deref() != Some(serde_json::to_vec(&owner)?.as_slice()) {
                // Compare parsed identity below; serde map ordering can differ.
                let current: Value = serde_json::from_slice(&fs::read(&owner_path)?)?;
                if current != owner {
                    anyhow::bail!("transaction lock owner changed");
                }
            }
            fs::remove_file(owner_path)?;
            fs::remove_dir(&mutex)?;
        }
        #[cfg(not(unix))]
        {
            anyhow::bail!("dead-lock recovery requires an explicit platform implementation");
        }
    }
    let _lock = lock(&path)?;
    let Some(bytes) = read(&path)? else {
        return Ok(json!({"recovered":false,"conflicts":[]}));
    };
    let journal: Journal = serde_json::from_slice(&bytes)?;
    if journal.schema != "switchback/profile-apply-journal@1"
        || !matches!(
            journal.phase.as_str(),
            "pending" | "conflicted" | "committed"
        )
    {
        anyhow::bail!("unsupported transaction journal");
    }
    let allowed = artifacts
        .iter()
        .map(|a| a.path.clone())
        .collect::<BTreeSet<_>>();
    let recorded = journal
        .entries
        .iter()
        .map(|e| e.path.clone())
        .collect::<BTreeSet<_>>();
    if allowed != recorded || recorded.len() != journal.entries.len() {
        anyhow::bail!("journal artifact ownership mismatch");
    }
    if journal.phase == "committed" {
        fs::remove_file(&path)?;
        return Ok(json!({"recovered":true,"committed":true,"conflicts":[]}));
    }
    let conflicts = restore(&journal)?;
    if conflicts.is_empty() {
        fs::remove_file(path)?;
    }
    Ok(json!({"recovered":conflicts.is_empty(),"committed":false,"conflicts":conflicts}))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "sb-profile-txn-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn artifact(path: PathBuf, body: &str) -> PlannedProfileArtifact {
        PlannedProfileArtifact {
            kind: "settings",
            path,
            contents: body.into(),
            mode: 0o600,
            comparison: ArtifactComparison::Bytes,
        }
    }
    #[test]
    fn oversized_journal_refuses_before_publication() {
        let root = Temp::new();
        let path = root.0.join("settings.json");
        let before = vec![b'a'; 3 * 1024 * 1024];
        write_private(&path, &before, 0o600).unwrap();
        let artifacts = vec![artifact(path.clone(), "after")];
        let error = apply(&artifacts, || Ok(())).unwrap_err();
        assert!(error.to_string().contains("recovery limit"));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(!journal_path(&artifacts).unwrap().exists());
    }
    #[test]
    fn profile_transaction_includes_post_apply_verifier_in_rollback() {
        let root = Temp::new();
        let path = root.0.join("settings.json");
        fs::write(&path, "before").unwrap();
        set_mode(&path, 0o600).unwrap();
        let artifacts = vec![artifact(path.clone(), "after")];
        let result = apply(&artifacts, || anyhow::bail!("post-apply doctor failed"));
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "before");
        assert!(!journal_path(&artifacts).unwrap().exists());
    }
    #[test]
    fn profile_transaction_crash_recovery_restores_only_its_own_publication() {
        let root = Temp::new();
        let path = root.0.join("settings.json");
        let artifacts = vec![artifact(path.clone(), "after")];
        let journal = Journal {
            schema: "switchback/profile-apply-journal@1".into(),
            phase: "pending".into(),
            entries: vec![Entry {
                path: path.clone(),
                before: Some(b"before".to_vec()),
                after: b"after".to_vec(),
                before_mode: Some(0o600),
                after_mode: 0o600,
            }],
        };
        store(&journal_path(&artifacts).unwrap(), &journal).unwrap();
        write_private(&path, b"after", 0o600).unwrap();
        assert_eq!(recover(&artifacts).unwrap()["recovered"], true);
        assert_eq!(fs::read_to_string(path).unwrap(), "before");
    }
    #[test]
    fn profile_transaction_recovery_preserves_later_edits_and_retains_journal() {
        let root = Temp::new();
        let path = root.0.join("settings.json");
        fs::write(&path, "before").unwrap();
        let artifacts = vec![artifact(path.clone(), "after")];
        assert!(apply(&artifacts, || {
            fs::write(&path, "later user edit")?;
            anyhow::bail!("doctor rejected")
        })
        .is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "later user edit");
        let recovered = recover(&artifacts).unwrap();
        assert_eq!(recovered["recovered"], false);
        assert_eq!(recovered["conflicts"].as_array().unwrap().len(), 1);
        assert!(journal_path(&artifacts).unwrap().exists());
        assert_eq!(fs::read_to_string(path).unwrap(), "later user edit");
    }
    #[test]
    fn profile_transaction_recovery_refuses_unowned_journal_paths() {
        let root = Temp::new();
        let path = root.0.join("settings.json");
        let other = root.0.join("unowned");
        let artifacts = vec![artifact(path, "after")];
        let journal = Journal {
            schema: "switchback/profile-apply-journal@1".into(),
            phase: "pending".into(),
            entries: vec![Entry {
                path: other.clone(),
                before: None,
                after: b"after".to_vec(),
                before_mode: None,
                after_mode: 0o600,
            }],
        };
        store(&journal_path(&artifacts).unwrap(), &journal).unwrap();
        assert!(recover(&artifacts).is_err());
        assert!(!other.exists());
    }
}
