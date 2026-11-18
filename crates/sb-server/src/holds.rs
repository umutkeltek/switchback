//! Host holds: a caller's lease that keeps a managed machine powered on.
//!
//! An agent that works on a machine directly (a shell, a benchmark,
//! maintenance) takes a hold for a bounded time; while any hold on a host is
//! active, the host's idle power-off does not run. A hold keeps the machine
//! on only: it takes no GPU group and counts as no lane work. A hold ends
//! by itself at its expiry, or when its owner deletes it; the host's normal
//! idle countdown then starts from the moment the last hold ended.
//!
//! With a state directory the holds are kept in `holds.json` there, so a
//! gateway restart never powers off a machine an agent holds; without one
//! they live in memory only (the capacity report says which).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

use serde::{Deserialize, Serialize};

/// At most this many active holds per tenant (keys without a tenant share
/// one count).
pub const MAX_ACTIVE_HOLDS_PER_TENANT: usize = 8;

/// The file under the state directory that keeps the holds.
pub const HOLDS_FILE: &str = "holds.json";

/// Where a hold's optional wake stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldState {
    /// The hold keeps the machine on (no wake asked, or the wake is done).
    Active,
    /// `wake: true` and the wake still runs.
    Waking,
    /// `wake: true` and the wake failed; the hold still keeps the machine
    /// on if it comes up some other way.
    WakeFailed,
}

/// One hold on one host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hold {
    pub id: String,
    pub host: String,
    pub reason: String,
    /// The tenant (and project) of the key that took it: a client key sees
    /// and deletes only its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
    pub state: HoldState,
    /// What the wake did (`wake: true` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_detail: Option<String>,
}

impl PartialEq for Hold {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

/// A hold as the capacity report shows it: never a key, only the name of
/// the tenant that took it.
#[derive(Debug, Clone, Serialize)]
pub struct HoldView {
    pub id: String,
    pub reason: String,
    pub expires_at: String,
    pub expires_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub state: HoldState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wake_detail: Option<String>,
    /// For filtering a client's own holds; not shown.
    #[serde(skip)]
    pub project: Option<String>,
}

impl Hold {
    pub fn view(&self) -> HoldView {
        HoldView {
            id: self.id.clone(),
            reason: self.reason.clone(),
            expires_at: rfc3339_ms(self.expires_at_ms),
            expires_at_ms: self.expires_at_ms,
            owner: self.tenant.clone(),
            state: self.state,
            wake_detail: self.wake_detail.clone(),
            project: self.project.clone(),
        }
    }
}

/// Why a hold could not be taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoldError {
    /// The tenant already has [`MAX_ACTIVE_HOLDS_PER_TENANT`] active holds.
    TooMany,
}

/// What the holds say about one host's power-off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostHolds {
    /// At least one hold is active: no idle power-off.
    pub held: bool,
    /// When the host's last hold ended (clock ms): its idle countdown runs
    /// from no earlier than this.
    pub ended_at_ms: Option<u64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct HoldFile {
    holds: Vec<Hold>,
    /// Per host, when its latest hold ended (clock ms).
    #[serde(default)]
    ended: BTreeMap<String, u64>,
}

/// Every hold the gateway knows, kept in step with `holds.json` when a
/// state directory is attached.
#[derive(Debug, Default)]
pub struct Holds {
    state: Mutex<HoldFile>,
    path: RwLock<Option<PathBuf>>,
    last_save_error: Mutex<Option<String>>,
    /// Why `holds.json` could not be read at start-up, if it could not.
    load_error: Mutex<Option<String>>,
}

impl Holds {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HoldFile> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Keep the holds in `dir/holds.json` from now on, taking back the holds
    /// a previous run left there (expired ones end at their expiry).
    pub fn attach_dir(&self, dir: &Path, now: u64) {
        let path = dir.join(HOLDS_FILE);
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<HoldFile>(&bytes) {
                Ok(saved) => {
                    let mut state = self.lock();
                    for hold in saved.holds {
                        if !state.holds.iter().any(|kept| kept.id == hold.id) {
                            state.holds.push(hold);
                        }
                    }
                    for (host, at) in saved.ended {
                        let ended = state.ended.entry(host).or_insert(at);
                        *ended = (*ended).max(at);
                    }
                    tracing::info!(
                        holds = state.holds.len(),
                        "host holds taken back from the state directory"
                    );
                }
                Err(error) => {
                    // Keep the unreadable file for a person to look at: the
                    // save below would otherwise replace it with no holds.
                    let kept = dir.join(format!("{HOLDS_FILE}.corrupt"));
                    let kept_note = match std::fs::rename(&path, &kept) {
                        Ok(()) => format!("kept as {HOLDS_FILE}.corrupt"),
                        Err(rename) => format!("could not keep it aside: {rename}"),
                    };
                    tracing::error!(%error, kept = %kept_note, "host holds file unreadable; starting with none");
                    *self
                        .load_error
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(format!(
                        "{HOLDS_FILE} was unreadable ({error}); started with no holds, {kept_note}"
                    ));
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::error!(%error, "host holds file unreadable; starting with none"),
        }
        *self
            .path
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(path);
        let mut state = self.lock();
        Self::prune(&mut state, now);
        self.save(&state);
    }

    /// `true` when the holds are kept on disk (they survive a restart).
    pub fn durable(&self) -> bool {
        self.path
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }

    /// Why `holds.json` could not be read at start-up, if it could not.
    pub fn load_error(&self) -> Option<String> {
        self.load_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Why the last write of `holds.json` failed, if it did.
    pub fn last_save_error(&self) -> Option<String> {
        self.last_save_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// End the holds whose expiry has passed. `true` when any ended.
    fn prune(state: &mut HoldFile, now: u64) -> bool {
        let before = state.holds.len();
        let mut ended = Vec::new();
        state.holds.retain(|hold| {
            let active = hold.expires_at_ms > now;
            if !active {
                ended.push((hold.host.clone(), hold.expires_at_ms));
            }
            active
        });
        for (host, at) in ended {
            let entry = state.ended.entry(host).or_insert(at);
            *entry = (*entry).max(at);
        }
        state.holds.len() != before
    }

    /// Write the holds through a temporary file and a rename, so a crash
    /// mid-write never leaves half a file. A failure is logged and shown in
    /// the capacity report; the holds stay in memory.
    fn save(&self, state: &HoldFile) {
        let Some(path) = self
            .path
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        else {
            return;
        };
        let result = serde_json::to_vec_pretty(state)
            .map_err(|error| error.to_string())
            .and_then(|bytes| {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
                }
                let staged = path.with_extension("json.tmp");
                std::fs::write(&staged, bytes).map_err(|error| error.to_string())?;
                std::fs::rename(&staged, &path).map_err(|error| error.to_string())
            });
        let mut last = self
            .last_save_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match result {
            Ok(()) => *last = None,
            Err(error) => {
                tracing::error!(%error, "could not write the host holds file; holds stay in memory");
                *last = Some(format!("could not write {HOLDS_FILE}: {error}"));
            }
        }
    }

    /// Prune, run `f`, and save when either changed something.
    fn with<T>(&self, now: u64, f: impl FnOnce(&mut HoldFile) -> (T, bool)) -> T {
        let mut state = self.lock();
        let pruned = Self::prune(&mut state, now);
        let (value, changed) = f(&mut state);
        if pruned || changed {
            self.save(&state);
        }
        value
    }

    /// Take a hold on `host` until `now + ttl_ms`.
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        &self,
        host: &str,
        reason: &str,
        ttl_ms: u64,
        tenant: Option<String>,
        project: Option<String>,
        state: HoldState,
        now: u64,
    ) -> Result<Hold, HoldError> {
        self.with(now, |file| {
            let held = file
                .holds
                .iter()
                .filter(|hold| hold.tenant == tenant)
                .count();
            if held >= MAX_ACTIVE_HOLDS_PER_TENANT {
                return (Err(HoldError::TooMany), false);
            }
            let hold = Hold {
                id: sb_core::new_id("hold"),
                host: host.to_string(),
                reason: reason.to_string(),
                tenant,
                project,
                created_at_ms: now,
                expires_at_ms: now.saturating_add(ttl_ms),
                state,
                wake_detail: None,
            };
            file.holds.push(hold.clone());
            (Ok(hold), true)
        })
    }

    /// The active holds on `host` (all hosts with `None`).
    pub fn list(&self, host: Option<&str>, now: u64) -> Vec<Hold> {
        self.with(now, |file| {
            let holds = file
                .holds
                .iter()
                .filter(|hold| host.is_none_or(|host| hold.host == host))
                .cloned()
                .collect();
            (holds, false)
        })
    }

    /// One active hold on `host`.
    pub fn get(&self, host: &str, id: &str, now: u64) -> Option<Hold> {
        self.list(Some(host), now)
            .into_iter()
            .find(|hold| hold.id == id)
    }

    /// End a hold now. Its host's idle countdown runs from this moment.
    pub fn delete(&self, host: &str, id: &str, now: u64) -> Option<Hold> {
        self.with(now, |file| {
            let Some(at) = file
                .holds
                .iter()
                .position(|hold| hold.host == host && hold.id == id)
            else {
                return (None, false);
            };
            let hold = file.holds.remove(at);
            let ended = file.ended.entry(host.to_string()).or_insert(now);
            *ended = (*ended).max(now);
            (Some(hold), true)
        })
    }

    /// Extend a hold: it now ends at `now + ttl_ms` (shorter is allowed).
    pub fn extend(&self, host: &str, id: &str, ttl_ms: u64, now: u64) -> Option<Hold> {
        self.with(now, |file| {
            let Some(hold) = file
                .holds
                .iter_mut()
                .find(|hold| hold.host == host && hold.id == id)
            else {
                return (None, false);
            };
            hold.expires_at_ms = now.saturating_add(ttl_ms);
            (Some(hold.clone()), true)
        })
    }

    /// Record where a hold's wake stands.
    pub fn set_state(&self, id: &str, state: HoldState, detail: Option<String>, now: u64) {
        self.with(now, |file| {
            let Some(hold) = file.holds.iter_mut().find(|hold| hold.id == id) else {
                return ((), false);
            };
            hold.state = state;
            hold.wake_detail = detail;
            ((), true)
        });
    }

    /// What the holds say about `host`'s power-off right now.
    pub fn host(&self, host: &str, now: u64) -> HostHolds {
        self.with(now, |file| {
            let view = HostHolds {
                held: file.holds.iter().any(|hold| hold.host == host),
                ended_at_ms: file.ended.get(host).copied(),
            };
            (view, false)
        })
    }
}

/// A clock time (ms since the Unix epoch) as RFC 3339 UTC text.
pub(crate) fn rfc3339_ms(ms: u64) -> String {
    let nanos = i128::from(ms).saturating_mul(1_000_000);
    time::OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .ok()
        .and_then(|at| {
            at.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn take(holds: &Holds, tenant: &str, now: u64) -> Result<Hold, HoldError> {
        holds.create(
            "gpu-box",
            "benchmark",
            60_000,
            Some(tenant.to_string()),
            None,
            HoldState::Active,
            now,
        )
    }

    // A hold ends by itself at its expiry, and the host remembers when.
    #[test]
    fn a_hold_ends_at_its_expiry() {
        let holds = Holds::new();
        let hold = take(&holds, "agents", 1_000).unwrap();
        assert_eq!(hold.expires_at_ms, 61_000);
        assert!(holds.host("gpu-box", 60_999).held);
        let after = holds.host("gpu-box", 61_000);
        assert_eq!(
            after,
            HostHolds {
                held: false,
                ended_at_ms: Some(61_000)
            }
        );
        assert!(holds.list(None, 61_000).is_empty());
    }

    // At most eight active holds per tenant; another tenant has its own.
    #[test]
    fn a_tenant_has_at_most_eight_active_holds() {
        let holds = Holds::new();
        for _ in 0..MAX_ACTIVE_HOLDS_PER_TENANT {
            take(&holds, "agents", 0).unwrap();
        }
        assert_eq!(take(&holds, "agents", 0), Err(HoldError::TooMany));
        take(&holds, "other", 0).expect("another tenant counts apart");
        // An expired hold no longer counts.
        assert!(take(&holds, "agents", 60_000).is_ok());
    }

    // A corrupt holds file does not stop start-up: the gateway starts with
    // no holds, says so, and keeps the unreadable file aside instead of
    // writing over it.
    #[test]
    fn a_corrupt_holds_file_is_kept_aside_and_start_up_goes_on() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("sb-holds-corrupt-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(HOLDS_FILE), b"{\"holds\": [trunc").unwrap();
        let holds = Holds::new();
        holds.attach_dir(&dir, 0);
        assert!(holds.durable());
        assert!(holds.list(None, 0).is_empty());
        assert_eq!(
            std::fs::read(dir.join(format!("{HOLDS_FILE}.corrupt"))).unwrap(),
            b"{\"holds\": [trunc"
        );
        assert!(holds.load_error().unwrap().contains("unreadable"));
        // The fresh file is valid and takes new holds.
        take(&holds, "agents", 0).unwrap();
        let again = Holds::new();
        again.attach_dir(&dir, 0);
        assert_eq!(again.list(None, 0).len(), 1);
        assert!(again.load_error().is_none());
    }

    // An extension moves the expiry; a delete ends the hold now.
    #[test]
    fn extend_and_delete() {
        let holds = Holds::new();
        let hold = take(&holds, "agents", 0).unwrap();
        let longer = holds.extend("gpu-box", &hold.id, 120_000, 30_000).unwrap();
        assert_eq!(longer.expires_at_ms, 150_000);
        assert!(holds.extend("other-box", &hold.id, 1, 30_000).is_none());
        assert!(holds.delete("gpu-box", &hold.id, 40_000).is_some());
        assert_eq!(holds.host("gpu-box", 40_000).ended_at_ms, Some(40_000));
        assert!(holds.delete("gpu-box", &hold.id, 40_000).is_none());
    }
}
