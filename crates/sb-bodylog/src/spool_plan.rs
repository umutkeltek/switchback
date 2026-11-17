//! Bounded spool discovery. A native directory offset is a replayable hint,
//! never evidence of custody or a complete filesystem inventory.
//!
//! Darwin getdirentries(2) and Linux getdents(2) advance the directory file
//! offset; lseek(2) restores that offset. Unlike telldir(3), this does not
//! persist a token owned by a closed DIR stream. Concurrent directory edits
//! can invalidate coverage: rotate the frontier and restart from zero after
//! every exhausted pass. Selected recovery must revalidate every artifact.
use super::{BodyLogError, Result};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path};

pub const SPOOL_CURSOR_MAX_BYTES: usize = 1024 * 1024;
const PAGE_BYTES: usize = 4096;
const MAX_FRONTIER: usize = 128;
const MAX_PAGE_CALLS: usize = 128;
const SCHEMA: &str = "switchback/spool-recovery-cursor@1";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpoolRecoveryCursor {
    schema: String,
    state_dir: String,
    root_device: u64,
    root_inode: u64,
    pass: u64,
    frontier: VecDeque<DirectoryCursor>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectoryCursor {
    relative: String,
    device: u64,
    inode: u64,
    offset: u64,
    pending: VecDeque<String>,
}
#[derive(Debug, Serialize)]
pub struct SpoolRecoveryPlan {
    pub schema: &'static str,
    pub state_dir: String,
    pub archive_scanned: bool,
    pub offset_semantics: &'static str,
    pub entries_examined: usize,
    pub native_page_calls: usize,
    pub selected_segments: Vec<String>,
    pub skipped_manifest: usize,
    pub holds: Vec<String>,
    pub pass_exhausted: bool,
    pub next_cursor: Option<SpoolRecoveryCursor>,
}

fn error(message: &str) -> BodyLogError {
    BodyLogError::new(message)
}
fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 1023
        && !name.contains('\0')
        && Path::new(name).components().count() == 1
        && matches!(
            Path::new(name).components().next(),
            Some(Component::Normal(_))
        )
}
fn safe_relative(relative: &str) -> bool {
    relative.is_empty()
        || (relative.len() <= 3072
            && Path::new(relative).components().count() <= 3
            && Path::new(relative)
                .components()
                .all(|p| matches!(p, Component::Normal(_))))
}
fn owned_directory(file: &File) -> Result<(u64, u64)> {
    let m = file.metadata()?;
    if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o022 != 0 {
        return Err(error("spool planner requires an owned private directory"));
    }
    Ok((m.dev(), m.ino()))
}
fn open_at(parent: &File, name: &str) -> Result<File> {
    let name = CString::new(name).map_err(|_| error("invalid spool directory name"))?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    owned_directory(&file)?;
    Ok(file)
}
fn open_relative(root: &File, relative: &str) -> Result<File> {
    let mut current = root.try_clone()?;
    for part in Path::new(relative).components() {
        let Component::Normal(name) = part else {
            return Err(error("unsafe spool cursor path"));
        };
        current = open_at(
            &current,
            name.to_str().ok_or_else(|| error("non UTF-8 spool path"))?,
        )?;
    }
    Ok(current)
}
fn stat_at(parent: &File, name: &str) -> Result<Option<libc::stat>> {
    let name = CString::new(name).map_err(|_| error("invalid spool entry name"))?;
    let mut m = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            m.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(e.into());
    }
    Ok(Some(unsafe { m.assume_init() }))
}
fn load_cursor(path: &Path) -> Result<SpoolRecoveryCursor> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let m = file.metadata()?;
    if !m.is_file()
        || m.uid() != unsafe { libc::geteuid() }
        || m.nlink() != 1
        || m.mode() & 0o022 != 0
        || m.len() > SPOOL_CURSOR_MAX_BYTES as u64
    {
        return Err(error("unsafe or oversized spool cursor"));
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(SPOOL_CURSOR_MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > SPOOL_CURSOR_MAX_BYTES {
        return Err(error("spool cursor exceeds size limit"));
    }
    Ok(serde_json::from_slice(&bytes)?)
}
fn directory_cursor(relative: String, file: &File) -> Result<DirectoryCursor> {
    let (device, inode) = owned_directory(file)?;
    Ok(DirectoryCursor {
        relative,
        device,
        inode,
        offset: 0,
        pending: VecDeque::new(),
    })
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    #[link_name = "__getdirentries64"]
    fn native_getdirentries(
        fd: libc::c_int,
        buf: *mut libc::c_char,
        len: usize,
        base: *mut libc::off_t,
    ) -> libc::ssize_t;
}
fn native_page(file: &File, offset: u64) -> Result<(VecDeque<String>, u64, bool)> {
    if offset > libc::off_t::MAX as u64 {
        return Err(error("invalid spool directory offset"));
    }
    if unsafe { libc::lseek(file.as_raw_fd(), offset as libc::off_t, libc::SEEK_SET) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut bytes = [0u8; PAGE_BYTES];
    #[cfg(target_os = "macos")]
    let count = unsafe {
        let mut base = 0;
        native_getdirentries(
            file.as_raw_fd(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
            &mut base,
        )
    };
    #[cfg(target_os = "linux")]
    let count = unsafe {
        libc::syscall(
            libc::SYS_getdents64,
            file.as_raw_fd(),
            bytes.as_mut_ptr(),
            bytes.len(),
        ) as libc::ssize_t
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return Err(error("native spool paging is unsupported on this platform"));
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        if count < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let next = unsafe { libc::lseek(file.as_raw_fd(), 0, libc::SEEK_CUR) };
        if next < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if count > 0 && next as u64 == offset {
            return Err(error("native directory page made no progress"));
        }
        let mut names = VecDeque::new();
        let mut at = 0;
        while at < count as usize {
            #[cfg(target_os = "macos")]
            let (length_at, name_at) = (16, 21);
            #[cfg(target_os = "linux")]
            let (length_at, name_at) = (16, 19);
            if at + name_at >= count as usize {
                return Err(error("truncated native directory page"));
            }
            let length = u16::from_ne_bytes(
                bytes[at + length_at..at + length_at + 2]
                    .try_into()
                    .unwrap(),
            ) as usize;
            if length <= name_at || at + length > count as usize {
                return Err(error("invalid native directory record"));
            }
            let raw = &bytes[at + name_at..at + length];
            let end = raw
                .iter()
                .position(|b| *b == 0)
                .ok_or_else(|| error("unterminated native directory name"))?;
            let name = std::str::from_utf8(&raw[..end])
                .map_err(|_| error("non UTF-8 spool entry; discovery held"))?;
            if name != "." && name != ".." {
                if !safe_name(name) {
                    return Err(error("unsafe native spool entry"));
                }
                names.push_back(name.to_owned());
            }
            at += length;
        }
        Ok((names, next as u64, count == 0))
    }
}

/// A read-only bounded page. The caller atomically checkpoints next_cursor only
/// after handling the selected inputs. Replaying an uncommitted page is safe.
/// Cursor paths/offsets grant no recovery, custody, or deletion authority.
pub fn plan_spool_recovery(
    state_dir: &Path,
    cursor_path: Option<&Path>,
    max_entries: usize,
    max_candidates: usize,
) -> Result<SpoolRecoveryPlan> {
    if !(1..=4096).contains(&max_entries) || !(1..=4).contains(&max_candidates) {
        return Err(error("spool plan limits exceed bounded contract"));
    }
    let state_dir = fs::canonicalize(state_dir)?;
    let state = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&state_dir)?;
    owned_directory(&state)?;
    let state_text = state_dir
        .to_str()
        .ok_or_else(|| error("non UTF-8 state root"))?
        .to_owned();
    let mut report = SpoolRecoveryPlan {
        schema: "switchback/spool-recovery-plan@1",
        state_dir: state_text.clone(),
        archive_scanned: false,
        offset_semantics: "inode_bound_hint_periodic_zero_reconcile",
        entries_examined: 0,
        native_page_calls: 0,
        selected_segments: Vec::new(),
        skipped_manifest: 0,
        holds: Vec::new(),
        pass_exhausted: false,
        next_cursor: None,
    };
    let mut root = state.try_clone()?;
    for part in ["body", "spool", "segments"] {
        let Some(m) = stat_at(&root, part)? else {
            report.pass_exhausted = true;
            return Ok(report);
        };
        if m.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(error("spool directory path is not a real directory"));
        }
        root = open_at(&root, part)?;
    }
    let (device, inode) = owned_directory(&root)?;
    let mut cursor = if let Some(path) = cursor_path {
        load_cursor(path)?
    } else {
        SpoolRecoveryCursor {
            schema: SCHEMA.into(),
            state_dir: state_text.clone(),
            root_device: device,
            root_inode: inode,
            pass: 0,
            frontier: VecDeque::new(),
        }
    };
    if cursor.schema != SCHEMA
        || cursor.state_dir != state_text
        || cursor.root_device != device
        || cursor.root_inode != inode
        || cursor.frontier.len() > MAX_FRONTIER
        || cursor.frontier.iter().any(|d| {
            !safe_relative(&d.relative)
                || d.pending.len() > 256
                || d.pending.iter().any(|n| !safe_name(n))
        })
    {
        return Err(error("spool cursor identity or bounded frontier differs"));
    }
    if cursor.frontier.is_empty() {
        cursor.pass = cursor
            .pass
            .checked_add(1)
            .ok_or_else(|| error("spool cursor pass overflow"))?;
        cursor
            .frontier
            .push_back(directory_cursor(String::new(), &root)?);
    }
    let mut turns = 0;
    while report.entries_examined < max_entries
        && report.selected_segments.len() < max_candidates
        && turns < 256
        && report.native_page_calls < MAX_PAGE_CALLS
    {
        let Some(mut node) = cursor.frontier.pop_front() else {
            break;
        };
        turns += 1;
        let directory = match open_relative(&root, &node.relative) {
            Ok(d) => d,
            Err(_) => {
                report.holds.push("directory_unavailable".into());
                continue;
            }
        };
        if owned_directory(&directory)? != (node.device, node.inode) {
            report.holds.push("directory_identity_changed".into());
            continue;
        }
        let mut finished = false;
        for _ in 0..32 {
            if report.entries_examined >= max_entries
                || report.selected_segments.len() >= max_candidates
                || report.native_page_calls >= MAX_PAGE_CALLS
            {
                break;
            }
            if node.pending.is_empty() {
                let (names, offset, eof) = native_page(&directory, node.offset)?;
                report.native_page_calls += 1;
                node.pending = names;
                node.offset = offset;
                if eof {
                    finished = true;
                    break;
                }
                if node.pending.is_empty() {
                    continue;
                }
            }
            let name = node.pending.pop_front().unwrap();
            report.entries_examined += 1;
            let Some(m) = stat_at(&directory, &name)? else {
                continue;
            };
            let relative = if node.relative.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", node.relative, name)
            };
            if m.st_mode & libc::S_IFMT == libc::S_IFDIR {
                if relative.split('/').count() > 3 {
                    report.holds.push("directory_depth_limit".into());
                    continue;
                }
                if cursor.frontier.len() >= MAX_FRONTIER - 1 {
                    node.pending.push_front(name);
                    report.holds.push("frontier_limit".into());
                    break;
                }
                let child = open_at(&directory, &name)?;
                cursor
                    .frontier
                    .push_back(directory_cursor(relative, &child)?);
            } else if m.st_mode & libc::S_IFMT == libc::S_IFLNK {
                report.holds.push("symlink_entry".into());
            } else if name.ends_with(".sbcap") {
                if m.st_mode & libc::S_IFMT != libc::S_IFREG
                    || m.st_uid != unsafe { libc::geteuid() }
                    || m.st_nlink != 1
                    || m.st_mode & 0o022 != 0
                {
                    report.holds.push("unsafe_candidate".into());
                    continue;
                }
                if stat_at(&directory, &format!("{name}.manifest.json"))?.is_some() {
                    report.skipped_manifest += 1;
                    continue;
                }
                report.selected_segments.push(
                    state_dir
                        .join("body/spool/segments")
                        .join(relative)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        if !finished {
            cursor.frontier.push_back(node);
        }
    }
    report.pass_exhausted = cursor.frontier.is_empty();
    if !report.pass_exhausted {
        report.holds.push("bounded_frontier_pending".into());
    }
    report.holds.sort();
    report.holds.dedup();
    if serde_json::to_vec(&cursor)?.len() > SPOOL_CURSOR_MAX_BYTES {
        return Err(error("spool cursor exceeds size limit"));
    }
    report.next_cursor = Some(cursor);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    static NEXT_FIXTURE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    struct Fixture {
        root: std::path::PathBuf,
        state: std::path::PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "spool-page-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT_FIXTURE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let state = root.join("state");
            fs::create_dir_all(state.join("body/spool/segments")).unwrap();
            Self { root, state }
        }
        fn spool(&self) -> std::path::PathBuf {
            self.state.join("body/spool/segments")
        }
        fn cursor(&self, plan: &SpoolRecoveryPlan) -> std::path::PathBuf {
            let path = self.root.join("cursor.json");
            fs::write(
                &path,
                serde_json::to_vec(plan.next_cursor.as_ref().unwrap()).unwrap(),
            )
            .unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    #[test]
    fn pages_cross_reopened_handles_and_rotate_cold_hot_directories() {
        let f = Fixture::new();
        for day in ["2026/01/01", "2026/11/17"] {
            let dir = f.spool().join(day);
            fs::create_dir_all(&dir).unwrap();
            for n in 0..4500 {
                fs::write(dir.join(format!("sidecar-{n:05}")), b"metadata").unwrap();
            }
            fs::write(dir.join("capture-last.sbcap"), b"unchanged source").unwrap();
        }
        let mut cursor = None;
        let mut all = std::collections::BTreeSet::new();
        let mut counts = Vec::new();
        // APFS and Linux may return unsorted entries; the assertion is eventual
        // coverage through bounded reopened pages, independent of their order.
        for _ in 0..12 {
            let p = plan_spool_recovery(&f.state, cursor.as_deref(), 4096, 4).unwrap();
            assert!(p.entries_examined <= 4096 && p.native_page_calls <= MAX_PAGE_CALLS);
            counts.push(p.entries_examined);
            all.extend(p.selected_segments.iter().cloned());
            let exhausted = p.pass_exhausted;
            cursor = Some(f.cursor(&p));
            if exhausted {
                break;
            }
        }
        assert_eq!(all.len(), 2, "both cold and hot candidates must be visited");
        assert!(
            counts.len() >= 3,
            "large frontier must resume, not fit in one page"
        );
        assert!(counts.iter().sum::<usize>() >= 9000);
        for p in all {
            assert_eq!(fs::read(p).unwrap(), b"unchanged source");
        }
    }
    #[test]
    fn refused_candidates_do_not_monopolize_later_plans_and_replay_is_idempotent() {
        let f = Fixture::new();
        for n in 0..12 {
            fs::write(f.spool().join(format!("capture-{n:03}.sbcap")), b"source").unwrap();
        }
        let first = plan_spool_recovery(&f.state, None, 4096, 4).unwrap();
        let replay = plan_spool_recovery(&f.state, None, 4096, 4).unwrap();
        assert_eq!(first.selected_segments, replay.selected_segments);
        assert_eq!(
            serde_json::to_vec(&first.next_cursor).unwrap(),
            serde_json::to_vec(&replay.next_cursor).unwrap()
        );
        let path = f.cursor(&first);
        // Even if every selected native recovery refuses, committing the page
        // moves discovery past those four; originals and manifests stay intact.
        let second = plan_spool_recovery(&f.state, Some(&path), 4096, 4).unwrap();
        assert_eq!(first.selected_segments.len(), 4);
        assert_eq!(second.selected_segments.len(), 4);
        assert!(first
            .selected_segments
            .iter()
            .all(|p| !second.selected_segments.contains(p)));
        assert_eq!(fs::read_dir(f.spool()).unwrap().count(), 12);
    }
    #[test]
    fn cursor_identity_and_paths_are_untrusted() {
        let f = Fixture::new();
        fs::write(f.spool().join("capture.sbcap"), b"source").unwrap();
        let p = plan_spool_recovery(&f.state, None, 1, 1).unwrap();
        let path = f.cursor(&p);
        let mut c = p.next_cursor.unwrap();
        c.frontier[0].relative = "../archive".into();
        fs::write(&path, serde_json::to_vec(&c).unwrap()).unwrap();
        assert!(plan_spool_recovery(&f.state, Some(&path), 4096, 4).is_err());
        c.frontier[0].relative = String::new();
        c.root_inode += 1;
        fs::write(&path, serde_json::to_vec(&c).unwrap()).unwrap();
        assert!(plan_spool_recovery(&f.state, Some(&path), 4096, 4).is_err());
        fs::write(&path, vec![b' '; SPOOL_CURSOR_MAX_BYTES + 1]).unwrap();
        assert!(plan_spool_recovery(&f.state, Some(&path), 4096, 4).is_err());
    }
    #[test]
    fn discovery_never_opens_bodies_archive_or_symlinked_directories() {
        let f = Fixture::new();
        let archive = f.root.join("unrelated-archive");
        fs::create_dir(&archive).unwrap();
        fs::write(archive.join("capture-external.sbcap"), b"untouched").unwrap();
        symlink(&archive, f.spool().join("external")).unwrap();
        // A selected regular body is not opened; mode 000 remains selectable.
        let body = f.spool().join("capture-protected.sbcap");
        fs::write(&body, b"source").unwrap();
        fs::set_permissions(&body, fs::Permissions::from_mode(0o000)).unwrap();
        let p = plan_spool_recovery(&f.state, None, 4096, 4).unwrap();
        assert_eq!(
            p.selected_segments,
            vec![fs::canonicalize(&body)
                .unwrap()
                .to_string_lossy()
                .into_owned()]
        );
        assert!(p.holds.contains(&"symlink_entry".to_string()));
        assert!(!p.archive_scanned);
        assert_eq!(fs::read_dir(&archive).unwrap().count(), 1);
        let g = Fixture::new();
        fs::remove_dir(g.spool()).unwrap();
        symlink(&archive, g.spool()).unwrap();
        assert!(plan_spool_recovery(&g.state, None, 4096, 4).is_err());
    }
    #[test]
    fn only_missing_manifests_are_candidates_and_missing_spool_is_read_only() {
        let f = Fixture::new();
        let body = f.spool().join("capture-sealed.sbcap");
        fs::write(&body, b"not read").unwrap();
        fs::write(
            body.with_file_name("capture-sealed.sbcap.manifest.json"),
            b"also not read",
        )
        .unwrap();
        let p = plan_spool_recovery(&f.state, None, 4096, 4).unwrap();
        assert!(p.selected_segments.is_empty(), "{p:?}");
        assert_eq!(p.skipped_manifest, 1);
        fs::remove_dir_all(f.state.join("body")).unwrap();
        let p = plan_spool_recovery(&f.state, None, 4096, 4).unwrap();
        assert!(p.selected_segments.is_empty());
        assert!(p.next_cursor.is_none());
        assert!(!f.state.join("body").exists());
        assert!(plan_spool_recovery(&f.state, None, 4097, 4).is_err());
    }
}
