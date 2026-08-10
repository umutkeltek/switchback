//! Kimi Code's native file-backed OAuth lifecycle.
//!
//! Kimi Code rotates its refresh token and persists the replacement in
//! `~/.kimi-code/credentials/kimi-code.json`. Switchback therefore cannot use
//! the generic read-only `json_token` source: it must share Kimi's lock,
//! re-read after acquiring it, and atomically persist the entire rotated token
//! bundle before leasing the new access token.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use sb_core::Secret;
use serde::{Deserialize, Serialize};

use crate::refresh::{TokenFetchError, TokenFetcher, TokenResponse};

const KIMI_CODE_TOKEN_URL: &str = "https://auth.kimi.com/api/oauth/token";
const KIMI_CODE_CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const LOCK_RETRIES: usize = 120;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(500);
const LOCK_STALE_AFTER: Duration = Duration::from_secs(5);
const LOCK_HEARTBEAT_EVERY: Duration = Duration::from_secs(2);
const MIN_REFRESH_THRESHOLD_SECS: u64 = 300;
const MAX_REFRESH_ATTEMPTS: usize = 3;

#[derive(Debug, Clone)]
pub struct KimiOauthRegistration {
    pub home: PathBuf,
}

impl KimiOauthRegistration {
    fn token_file(&self) -> PathBuf {
        self.home.join("credentials/kimi-code.json")
    }

    fn lock_target(&self) -> PathBuf {
        self.home.join("oauth/kimi-code")
    }
}

struct KimiOauthState {
    registration: KimiOauthRegistration,
}

/// De-duplicates Kimi refreshes per account and coordinates token rotation
/// with other Kimi Code/Switchback processes through Kimi's native lock.
pub struct KimiOauthCoordinator {
    states: Mutex<HashMap<String, Arc<tokio::sync::Mutex<KimiOauthState>>>>,
    fetcher: Arc<dyn TokenFetcher>,
    retry_sleeper: Arc<dyn RetrySleeper>,
    before_persistence_commit: Arc<dyn Fn() + Send + Sync>,
}

#[async_trait]
trait RetrySleeper: Send + Sync {
    async fn sleep(&self, duration: Duration);
}

struct TokioRetrySleeper;

#[async_trait]
impl RetrySleeper for TokioRetrySleeper {
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

impl KimiOauthCoordinator {
    pub fn new(fetcher: Arc<dyn TokenFetcher>) -> Self {
        Self::with_retry_sleeper(fetcher, Arc::new(TokioRetrySleeper))
    }

    fn with_retry_sleeper(
        fetcher: Arc<dyn TokenFetcher>,
        retry_sleeper: Arc<dyn RetrySleeper>,
    ) -> Self {
        Self::with_components(fetcher, retry_sleeper, Arc::new(|| {}))
    }

    fn with_components(
        fetcher: Arc<dyn TokenFetcher>,
        retry_sleeper: Arc<dyn RetrySleeper>,
        before_persistence_commit: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self {
            states: Mutex::new(HashMap::new()),
            fetcher,
            retry_sleeper,
            before_persistence_commit,
        }
    }

    #[cfg(test)]
    fn with_persistence_hook(
        fetcher: Arc<dyn TokenFetcher>,
        before_persistence_commit: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self::with_components(
            fetcher,
            Arc::new(TokioRetrySleeper),
            before_persistence_commit,
        )
    }

    fn key(provider: &str, account: &str) -> String {
        format!("{provider}/{account}")
    }

    pub fn register(&self, provider: &str, account: &str, registration: KimiOauthRegistration) {
        self.states.lock().expect("states mutex").insert(
            Self::key(provider, account),
            Arc::new(tokio::sync::Mutex::new(KimiOauthState { registration })),
        );
    }

    pub async fn access_token(
        &self,
        provider: &str,
        account: &str,
    ) -> Option<Result<Secret, String>> {
        let state = self
            .states
            .lock()
            .expect("states mutex")
            .get(&Self::key(provider, account))
            .cloned()?;
        let state = state.lock().await;
        Some(self.ensure_fresh(&state.registration).await)
    }

    async fn ensure_fresh(&self, registration: &KimiOauthRegistration) -> Result<Secret, String> {
        let token_file = registration.token_file();
        let initial = require_valid_state(load_state(&token_file)?)?;
        if !should_refresh(&initial, now_unix()) {
            return Ok(Secret::new(initial.access_token));
        }

        let lock = RefreshFileLock::acquire(&registration.lock_target()).await?;
        let result = self.refresh_under_lock(&lock, &token_file, initial).await;
        let release_result = lock.release().await;
        match (result, release_result) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(token), Ok(())) => Ok(token),
        }
    }

    async fn refresh_under_lock(
        &self,
        lock: &RefreshFileLock,
        token_file: &Path,
        initial: KimiToken,
    ) -> Result<Secret, String> {
        // Kimi's lock contract requires a second read after acquisition. A peer
        // may have rotated the one-time refresh token while we were waiting.
        let active = match load_state(token_file)? {
            StoredState::Missing => initial,
            StoredState::Revoked => return Err(relogin_error("stored credential was rejected")),
            StoredState::Valid(token) if !should_refresh(&token, now_unix()) => {
                return Ok(Secret::new(token.access_token));
            }
            StoredState::Valid(token) => token,
        };

        if active.refresh_token.trim().is_empty() {
            return Err(relogin_error("stored credential has no refresh token"));
        }

        lock.verify_owned_fresh()?;
        let response = self.refresh_with_retry(&active.refresh_token).await;

        let response = match response {
            Ok(response) => response,
            Err(TokenFetchError::Unauthorized) => {
                // Match Kimi's stale-token recovery: an independently rotating
                // peer may have won despite this process's best-effort lock.
                tokio::time::sleep(Duration::from_millis(100)).await;
                if let StoredState::Valid(recovered) = load_state(token_file)? {
                    if recovered.refresh_token != active.refresh_token {
                        return Ok(Secret::new(recovered.access_token));
                    }
                }

                save_token_with_lock(
                    token_file,
                    &KimiToken::revoked_from(&active),
                    &active.refresh_token,
                    lock,
                    self.before_persistence_commit.as_ref(),
                )?;
                return Err(relogin_error("refresh token was rejected"));
            }
            Err(error) => return Err(format!("kimi_code_oauth: token refresh failed: {error}")),
        };

        let refreshed = token_from_response(response)?;
        save_token_with_lock(
            token_file,
            &refreshed,
            &active.refresh_token,
            lock,
            self.before_persistence_commit.as_ref(),
        )?;
        Ok(Secret::new(refreshed.access_token))
    }

    async fn refresh_with_retry(
        &self,
        refresh_token: &str,
    ) -> Result<TokenResponse, TokenFetchError> {
        for attempt in 0..MAX_REFRESH_ATTEMPTS {
            let result = self
                .fetcher
                .refresh(
                    KIMI_CODE_TOKEN_URL,
                    Some(KIMI_CODE_CLIENT_ID),
                    None,
                    refresh_token,
                )
                .await;
            match result {
                Err(error) if error.is_retryable() && attempt + 1 < MAX_REFRESH_ATTEMPTS => {
                    self.retry_sleeper
                        .sleep(Duration::from_secs(1_u64 << attempt))
                        .await;
                }
                result => return result,
            }
        }
        unreachable!("bounded refresh loop always returns on its final attempt")
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct KimiToken {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    expires_at: u64,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    token_type: String,
    #[serde(default)]
    expires_in: u64,
}

impl KimiToken {
    fn revoked_from(prior: &Self) -> Self {
        Self {
            access_token: String::new(),
            refresh_token: String::new(),
            expires_at: 0,
            scope: prior.scope.clone(),
            token_type: prior.token_type.clone(),
            expires_in: 0,
        }
    }
}

enum StoredState {
    Missing,
    Revoked,
    Valid(KimiToken),
}

fn load_state(path: &Path) -> Result<StoredState, String> {
    let body = match fs::read(path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StoredState::Missing)
        }
        Err(error) => {
            return Err(format!(
                "kimi_code_oauth: unable to read credential file `{}`: {error}; run `kimi login`",
                path.display()
            ))
        }
    };
    let token: KimiToken = serde_json::from_slice(&body).map_err(|_| {
        format!(
            "kimi_code_oauth: credential file `{}` is malformed; run `kimi login`",
            path.display()
        )
    })?;
    if token.access_token.is_empty() {
        Ok(StoredState::Revoked)
    } else {
        Ok(StoredState::Valid(token))
    }
}

fn require_valid_state(state: StoredState) -> Result<KimiToken, String> {
    match state {
        StoredState::Valid(token) => Ok(token),
        StoredState::Missing => Err(relogin_error("no stored credential")),
        StoredState::Revoked => Err(relogin_error("stored credential was rejected")),
    }
}

fn relogin_error(reason: &str) -> String {
    format!("kimi_code_oauth: {reason}; run `kimi login`")
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn should_refresh(token: &KimiToken, now: u64) -> bool {
    if token.expires_at == 0 {
        return false;
    }
    // Kimi uses max(300 seconds, expires_in * 0.5). `remaining` is integral,
    // so ceil(expires_in / 2) preserves the comparison for odd lifetimes.
    let half_life = token.expires_in.saturating_add(1) / 2;
    let threshold = MIN_REFRESH_THRESHOLD_SECS.max(half_life);
    token.expires_at.saturating_sub(now) < threshold
}

fn token_from_response(response: TokenResponse) -> Result<KimiToken, String> {
    if response.access_token.is_empty() {
        return Err("kimi_code_oauth: OAuth response missing access_token".to_string());
    }
    let refresh_token = response
        .refresh_token
        .filter(|token| !token.is_empty())
        .ok_or_else(|| "kimi_code_oauth: OAuth response missing refresh_token".to_string())?;
    let expires_in = response
        .expires_in_secs
        .filter(|expires_in| *expires_in > 0)
        .ok_or_else(|| {
            "kimi_code_oauth: OAuth response missing or invalid expires_in".to_string()
        })?;

    Ok(KimiToken {
        access_token: response.access_token,
        refresh_token,
        expires_at: now_unix().saturating_add(expires_in),
        scope: response.scope.unwrap_or_default(),
        token_type: response
            .token_type
            .filter(|token_type| !token_type.is_empty())
            .unwrap_or_else(|| "Bearer".to_string()),
        expires_in,
    })
}

#[cfg(test)]
fn save_token(path: &Path, token: &KimiToken) -> Result<(), String> {
    save_token_before_rename(path, token, || Ok(()))
}

fn save_token_with_lock(
    path: &Path,
    token: &KimiToken,
    expected_refresh_token: &str,
    lock: &RefreshFileLock,
    before_persistence_commit: &(dyn Fn() + Send + Sync),
) -> Result<(), String> {
    lock.verify_owned()?;
    save_token_before_rename(path, token, || {
        before_persistence_commit();
        verify_authorizing_refresh_token(path, expected_refresh_token)?;
        // Refresh the held inode immediately before the final ownership check.
        // This gives us a full 5-second proper-lockfile lease before rename,
        // but it is not an atomic fence: a process suspension longer than that
        // can still let a compatible contender reclaim and commit first.
        lock.verify_owned_fresh()
    })
}

fn verify_authorizing_refresh_token(path: &Path, expected: &str) -> Result<(), String> {
    match load_state(path)? {
        StoredState::Valid(current) if current.refresh_token == expected => Ok(()),
        StoredState::Missing | StoredState::Revoked | StoredState::Valid(_) => Err(
            "kimi_code_oauth: credential file changed while refresh was in flight; refusing stale persistence"
                .to_string(),
        ),
    }
}

fn save_token_before_rename(
    path: &Path,
    token: &KimiToken,
    before_rename: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "kimi_code_oauth: credential file has no parent directory".to_string())?;
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "kimi_code_oauth: create credential directory `{}`: {error}",
            parent.display()
        )
    })?;
    set_directory_permissions(parent)?;

    let mut data = serde_json::to_vec_pretty(token)
        .map_err(|_| "kimi_code_oauth: serialize credential bundle".to_string())?;
    data.push(b'\n');
    let temp = temporary_path(path);
    let write_result = (|| -> Result<(), String> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp).map_err(|error| {
            format!(
                "kimi_code_oauth: create temporary credential file `{}`: {error}",
                temp.display()
            )
        })?;
        file.write_all(&data)
            .map_err(|error| format!("kimi_code_oauth: write credential file: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("kimi_code_oauth: fsync credential file: {error}"))?;
        set_file_permissions(&temp)?;
        drop(file);
        before_rename()?;
        fs::rename(&temp, path)
            .map_err(|error| format!("kimi_code_oauth: replace credential file: {error}"))?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    write_result
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(format!(
        ".tmp.{}.{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    PathBuf::from(value)
}

#[cfg(unix)]
fn set_directory_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("kimi_code_oauth: set credential directory permissions: {error}"))
}

#[cfg(not(unix))]
fn set_directory_permissions(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn set_file_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("kimi_code_oauth: set credential file permissions: {error}"))
}

#[cfg(not(unix))]
fn set_file_permissions(_path: &Path) -> Result<(), String> {
    Ok(())
}

struct RefreshFileLock {
    lock_path: Option<PathBuf>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    heartbeat: Option<tokio::task::JoinHandle<()>>,
    #[cfg(unix)]
    ownership: Option<LockOwnership>,
}

#[cfg(unix)]
struct LockOwnership {
    identity: LockIdentity,
    compromised: Arc<AtomicBool>,
    inode: Arc<OwnedLockInode>,
    #[cfg(test)]
    before_retire: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[cfg(unix)]
struct OwnedLockInode {
    directory: fs::File,
    retired: AtomicBool,
    timestamp_update: Mutex<()>,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LockIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
impl LockIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

#[cfg(unix)]
struct LockCandidate {
    path: PathBuf,
    inode: Arc<OwnedLockInode>,
    identity: LockIdentity,
    published: bool,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LockPublishResult {
    Published,
    Contended,
}

#[cfg(unix)]
impl LockCandidate {
    fn create(lock_path: &Path) -> Result<Self, String> {
        let mut candidate_name = lock_path.as_os_str().to_os_string();
        candidate_name.push(format!(
            ".candidate.{}.{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let path = PathBuf::from(candidate_name);
        fs::create_dir(&path).map_err(|error| {
            format!(
                "kimi_code_oauth: create OAuth refresh lock candidate `{}`: {error}",
                path.display()
            )
        })?;
        let directory = match fs::File::open(&path) {
            Ok(directory) => directory,
            Err(error) => {
                let _ = fs::remove_dir(&path);
                return Err(format!(
                    "kimi_code_oauth: open OAuth refresh lock candidate `{}`: {error}",
                    path.display()
                ));
            }
        };
        let metadata = match directory.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                drop(directory);
                let _ = fs::remove_dir(&path);
                return Err(format!(
                    "kimi_code_oauth: inspect OAuth refresh lock candidate `{}`: {error}",
                    path.display()
                ));
            }
        };
        let identity = LockIdentity::from_metadata(&metadata);
        Ok(Self {
            path,
            inode: Arc::new(OwnedLockInode {
                directory,
                retired: AtomicBool::new(false),
                timestamp_update: Mutex::new(()),
            }),
            identity,
            published: false,
        })
    }

    fn publish(
        &mut self,
        lock_path: &Path,
        before_publish: &(dyn Fn() + Send + Sync),
    ) -> Result<LockPublishResult, String> {
        before_publish();
        if !touch_owned_lock(&self.inode)? {
            return Err("kimi_code_oauth: OAuth lock candidate was retired".to_string());
        }
        match rename_noreplace(&self.path, lock_path)? {
            LockPublishResult::Published => {
                self.published = true;
                Ok(LockPublishResult::Published)
            }
            LockPublishResult::Contended => Ok(LockPublishResult::Contended),
        }
    }
}

#[cfg(unix)]
impl Drop for LockCandidate {
    fn drop(&mut self) {
        if !self.published && path_has_identity(&self.path, self.identity).unwrap_or(false) {
            let _ = fs::remove_dir(&self.path);
        }
    }
}

impl RefreshFileLock {
    #[cfg(not(unix))]
    async fn acquire(_target: &Path) -> Result<Self, String> {
        // Kimi Code itself disables the proper-lockfile path on Windows.
        Ok(Self {
            lock_path: None,
            stop: None,
            heartbeat: None,
        })
    }

    #[cfg(unix)]
    async fn acquire(target: &Path) -> Result<Self, String> {
        let noop = || {};
        Self::acquire_with_hooks(target, &noop, &noop).await
    }

    #[cfg(unix)]
    async fn acquire_with_hooks(
        target: &Path,
        before_publish: &(dyn Fn() + Send + Sync),
        after_publish: &(dyn Fn() + Send + Sync),
    ) -> Result<Self, String> {
        let parent = target.parent().ok_or_else(|| {
            "kimi_code_oauth: OAuth lock target has no parent directory".to_string()
        })?;
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "kimi_code_oauth: prepare OAuth refresh lock directory `{}`: {error}",
                parent.display()
            )
        })?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(target)
            .map_err(|error| {
                format!(
                    "kimi_code_oauth: prepare OAuth refresh lock `{}`: {error}",
                    target.display()
                )
            })?;

        let mut lock_name = target.as_os_str().to_os_string();
        lock_name.push(".lock");
        let lock_path = PathBuf::from(lock_name);
        let mut candidate = LockCandidate::create(&lock_path)?;
        let mut acquired = false;
        for attempt in 0..=LOCK_RETRIES {
            match candidate.publish(&lock_path, before_publish)? {
                LockPublishResult::Published => {
                    after_publish();
                    match path_has_identity(&lock_path, candidate.identity) {
                        Ok(true) => {}
                        Ok(false) => {
                            let _ = retire_owned_lock(&candidate.inode);
                            return Err(
                                "kimi_code_oauth: published OAuth refresh lock was replaced before adoption"
                                    .to_string(),
                            );
                        }
                        Err(error) => {
                            let _ = retire_owned_lock(&candidate.inode);
                            return Err(error);
                        }
                    }
                    acquired = true;
                    break;
                }
                LockPublishResult::Contended => {
                    let Some(metadata) = existing_lock_metadata(&lock_path)? else {
                        continue;
                    };
                    if !metadata.file_type().is_dir() {
                        return Err(format!(
                            "kimi_code_oauth: OAuth refresh lock `{}` is not a directory",
                            lock_path.display()
                        ));
                    }
                    let observed_identity = LockIdentity::from_metadata(&metadata);
                    let age = metadata
                        .modified()
                        .ok()
                        .and_then(|modified| SystemTime::now().duration_since(modified).ok());
                    if age.is_some_and(|age| age >= LOCK_STALE_AFTER)
                        && remove_stale_lock_if_unchanged(&lock_path, observed_identity)?
                    {
                        continue;
                    }
                }
            }
            if attempt < LOCK_RETRIES {
                tokio::time::sleep(LOCK_RETRY_DELAY).await;
            }
        }
        if !acquired {
            return Err(format!(
                "kimi_code_oauth: unable to acquire OAuth refresh lock `{}`",
                lock_path.display()
            ));
        }

        let identity = candidate.identity;
        let inode = candidate.inode.clone();
        if let Err(error) = touch_owned_lock(&inode) {
            let _ = retire_owned_lock(&inode);
            return Err(error);
        }

        let compromised = Arc::new(AtomicBool::new(false));
        let heartbeat_compromised = compromised.clone();
        let heartbeat_path = lock_path.clone();
        let heartbeat_identity = identity;
        let (stop, mut stopped) = tokio::sync::oneshot::channel();
        let heartbeat_inode = inode.clone();
        let heartbeat = tokio::spawn(async move {
            let mut interval = tokio::time::interval(LOCK_HEARTBEAT_EVERY);
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    _ = interval.tick() => {
                        if !path_has_identity(&heartbeat_path, heartbeat_identity).unwrap_or(false)
                            || !touch_owned_lock(&heartbeat_inode).unwrap_or(false)
                        {
                            heartbeat_compromised.store(true, Ordering::SeqCst);
                            break;
                        }
                    }
                }
            }
        });
        Ok(Self {
            lock_path: Some(lock_path),
            stop: Some(stop),
            heartbeat: Some(heartbeat),
            ownership: Some(LockOwnership {
                identity,
                compromised,
                inode,
                #[cfg(test)]
                before_retire: None,
            }),
        })
    }

    fn verify_owned(&self) -> Result<(), String> {
        #[cfg(unix)]
        {
            let ownership = self
                .ownership
                .as_ref()
                .ok_or_else(|| "kimi_code_oauth: OAuth refresh lock is compromised".to_string())?;
            if ownership.compromised.load(Ordering::SeqCst)
                || !path_has_identity(
                    self.lock_path.as_deref().ok_or_else(|| {
                        "kimi_code_oauth: OAuth refresh lock is compromised".to_string()
                    })?,
                    ownership.identity,
                )?
            {
                return Err(
                    "kimi_code_oauth: OAuth refresh lock was reclaimed and is compromised"
                        .to_string(),
                );
            }
        }
        Ok(())
    }

    fn verify_owned_fresh(&self) -> Result<(), String> {
        #[cfg(unix)]
        {
            let ownership = self
                .ownership
                .as_ref()
                .ok_or_else(|| "kimi_code_oauth: OAuth refresh lock is compromised".to_string())?;
            if !touch_owned_lock(&ownership.inode)? {
                return Err("kimi_code_oauth: OAuth refresh lock is retired".to_string());
            }
        }
        self.verify_owned()
    }

    async fn release(mut self) -> Result<(), String> {
        #[cfg(unix)]
        if let Some(ownership) = self.ownership.as_ref() {
            ownership.inode.retired.store(true, Ordering::SeqCst);
        }
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(heartbeat) = self.heartbeat.take() {
            let _ = heartbeat.await;
        }
        #[cfg(unix)]
        if let Some(ownership) = self.ownership.take() {
            #[cfg(test)]
            if let Some(before_retire) = ownership.before_retire.as_ref() {
                before_retire();
            }
            retire_owned_lock(&ownership.inode)?;
        }
        self.lock_path.take();
        Ok(())
    }
}

impl Drop for RefreshFileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(ownership) = self.ownership.as_ref() {
            ownership.inode.retired.store(true, Ordering::SeqCst);
        }
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(heartbeat) = self.heartbeat.take() {
            heartbeat.abort();
        }
        #[cfg(unix)]
        if let Some(ownership) = self.ownership.take() {
            #[cfg(test)]
            if let Some(before_retire) = ownership.before_retire.as_ref() {
                before_retire();
            }
            let _ = retire_owned_lock(&ownership.inode);
        }
        self.lock_path.take();
    }
}

#[cfg(unix)]
fn touch_owned_lock(inode: &OwnedLockInode) -> Result<bool, String> {
    let _update = inode
        .timestamp_update
        .lock()
        .map_err(|_| "kimi_code_oauth: OAuth lock timestamp mutex poisoned".to_string())?;
    if inode.retired.load(Ordering::SeqCst) {
        return Ok(false);
    }
    touch_lock_handle(&inode.directory)?;
    Ok(true)
}

#[cfg(unix)]
fn retire_owned_lock(inode: &OwnedLockInode) -> Result<(), String> {
    inode.retired.store(true, Ordering::SeqCst);
    let _update = inode
        .timestamp_update
        .lock()
        .map_err(|_| "kimi_code_oauth: OAuth lock timestamp mutex poisoned".to_string())?;
    // Leave the linked directory empty but unmistakably stale. Compatible
    // proper-lockfile contenders take one EEXIST/stat/rmdir retry; avoiding a
    // release-time pathname delete is what keeps a replacement inode safe.
    age_lock_handle(&inode.directory)
}

#[cfg(unix)]
fn touch_lock_handle(directory: &fs::File) -> Result<(), String> {
    use std::os::unix::io::AsRawFd;

    // Touch the directory inode this guard actually acquired. If another
    // process reclaimed the path, this open handle still names the unlinked old
    // inode and can never refresh the successor's mtime.
    let result = unsafe { libc::futimens(directory.as_raw_fd(), std::ptr::null()) };
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "kimi_code_oauth: update OAuth lock heartbeat: {}",
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(unix)]
fn age_lock_handle(directory: &fs::File) -> Result<(), String> {
    use std::os::unix::io::AsRawFd;

    let timestamp = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let times = [timestamp, timestamp];
    let result = unsafe { libc::futimens(directory.as_raw_fd(), times.as_ptr()) };
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "kimi_code_oauth: retire OAuth lock inode: {}",
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn rename_noreplace(from: &Path, to: &Path) -> Result<LockPublishResult, String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let from_c = CString::new(from.as_os_str().as_bytes()).map_err(|_| {
        format!(
            "kimi_code_oauth: OAuth lock candidate path contains a NUL byte: `{}`",
            from.display()
        )
    })?;
    let to_c = CString::new(to.as_os_str().as_bytes()).map_err(|_| {
        format!(
            "kimi_code_oauth: OAuth lock path contains a NUL byte: `{}`",
            to.display()
        )
    })?;

    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renamex_np(from_c.as_ptr(), to_c.as_ptr(), libc::RENAME_EXCL) as libc::c_long
    };

    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from_c.as_ptr(),
            libc::AT_FDCWD,
            to_c.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };

    if result == 0 {
        return Ok(LockPublishResult::Published);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(code) if code == libc::EEXIST => Ok(LockPublishResult::Contended),
        Some(code) if code == libc::ENOSYS || code == libc::EINVAL || code == libc::EOPNOTSUPP => {
            Err(format!(
                "kimi_code_oauth: atomic no-replace OAuth lock publication is unsupported: {error}"
            ))
        }
        _ => Err(format!(
            "kimi_code_oauth: publish OAuth refresh lock candidate `{}` as `{}`: {error}",
            from.display(),
            to.display()
        )),
    }
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn rename_noreplace(_from: &Path, _to: &Path) -> Result<LockPublishResult, String> {
    Err(
        "kimi_code_oauth: atomic no-replace OAuth lock publication is unsupported on this Unix platform"
            .to_string(),
    )
}

#[cfg(unix)]
fn path_has_identity(path: &Path, expected: LockIdentity) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(LockIdentity::from_metadata(&metadata) == expected),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "kimi_code_oauth: inspect OAuth refresh lock ownership `{}`: {error}",
            path.display()
        )),
    }
}

#[cfg(unix)]
fn existing_lock_metadata(path: &Path) -> Result<Option<fs::Metadata>, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "kimi_code_oauth: inspect OAuth refresh lock `{}`: {error}",
            path.display()
        )),
    }
}

#[cfg(unix)]
fn remove_stale_lock_if_unchanged(
    path: &Path,
    observed_identity: LockIdentity,
) -> Result<bool, String> {
    if !path_has_identity(path, observed_identity)? {
        return Ok(true);
    }

    // POSIX has no portable remove-directory-by-fd operation, so an
    // irreducible pathname TOCTOU remains between this identity recheck and
    // remove_dir. The recheck narrows the proper-lockfile-compatible stale
    // reclaim window; fd/inode persistence and release fences contain a stale
    // holder if another process nevertheless replaces the path in that gap.
    match fs::remove_dir(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(_) => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    type TestHook = Arc<dyn Fn() + Send + Sync>;
    #[cfg(unix)]
    type ReplacementLockHook = (Arc<Mutex<Option<LockIdentity>>>, TestHook);

    struct CountingFetcher {
        calls: AtomicUsize,
        delay: Duration,
        unauthorized: bool,
    }

    impl CountingFetcher {
        fn success(delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                delay,
                unauthorized: false,
            })
        }

        fn unauthorized() -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                delay: Duration::ZERO,
                unauthorized: true,
            })
        }
    }

    #[derive(Default)]
    struct RecordingSleeper {
        delays: Mutex<Vec<Duration>>,
    }

    #[async_trait]
    impl RetrySleeper for RecordingSleeper {
        async fn sleep(&self, duration: Duration) {
            self.delays.lock().unwrap().push(duration);
        }
    }

    #[async_trait]
    impl TokenFetcher for CountingFetcher {
        async fn refresh(
            &self,
            _token_url: &str,
            _client_id: Option<&str>,
            _client_secret: Option<&str>,
            _refresh_token: &str,
        ) -> Result<TokenResponse, TokenFetchError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if self.delay > Duration::ZERO {
                tokio::time::sleep(self.delay).await;
            }
            if self.unauthorized {
                return Err(TokenFetchError::Unauthorized);
            }
            Ok(TokenResponse {
                access_token: format!("fresh-access-{call}"),
                refresh_token: Some(format!("fresh-refresh-{call}")),
                expires_in_secs: Some(900),
                scope: Some("kimi-code".to_string()),
                token_type: Some("Bearer".to_string()),
            })
        }
    }

    fn fixture(tag: &str) -> (PathBuf, KimiOauthRegistration) {
        let root = std::env::temp_dir().join(format!(
            "sb-kimi-oauth-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let registration = KimiOauthRegistration { home: root.clone() };
        (root, registration)
    }

    fn token(access: &str, refresh: &str, expires_at: u64) -> KimiToken {
        KimiToken {
            access_token: access.to_string(),
            refresh_token: refresh.to_string(),
            expires_at,
            scope: "kimi-code".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 900,
        }
    }

    #[cfg(unix)]
    fn persistence_takeover_hook(
        registration: &KimiOauthRegistration,
        token_file: &Path,
        successor: Option<KimiToken>,
    ) -> (PathBuf, Arc<AtomicUsize>, Arc<dyn Fn() + Send + Sync>) {
        let mut lock_name = registration.lock_target().as_os_str().to_os_string();
        lock_name.push(".lock");
        let lock_path = PathBuf::from(lock_name);
        let hook_lock_path = lock_path.clone();
        let hook_token_file = token_file.to_path_buf();
        let calls = Arc::new(AtomicUsize::new(0));
        let hook_calls = calls.clone();
        let hook = Arc::new(move || {
            assert_eq!(hook_calls.fetch_add(1, Ordering::SeqCst), 0);
            fs::remove_dir(&hook_lock_path).unwrap();
            fs::create_dir(&hook_lock_path).unwrap();
            if let Some(successor) = &successor {
                save_token(&hook_token_file, successor).unwrap();
            }
        });
        (lock_path, calls, hook)
    }

    #[cfg(unix)]
    fn replacement_lock_hook(lock_path: &Path) -> ReplacementLockHook {
        let replacement_identity = Arc::new(Mutex::new(None));
        let hook_identity = replacement_identity.clone();
        let hook_path = lock_path.to_path_buf();
        let hook = Arc::new(move || {
            fs::remove_dir(&hook_path).unwrap();
            fs::create_dir(&hook_path).unwrap();
            *hook_identity.lock().unwrap() = Some(LockIdentity::from_metadata(
                &fs::symlink_metadata(&hook_path).unwrap(),
            ));
        });
        (replacement_identity, hook)
    }

    #[test]
    fn refresh_threshold_matches_kimi_dynamic_contract() {
        let now = 10_000;
        let mut short = token("access", "refresh", now + 299);
        short.expires_in = 100;
        assert!(should_refresh(&short, now));
        short.expires_at = now + 300;
        assert!(!should_refresh(&short, now));

        let mut long = token("access", "refresh", now + 449);
        long.expires_in = 900;
        assert!(should_refresh(&long, now));
        long.expires_at = now + 450;
        assert!(!should_refresh(&long, now));

        long.expires_at = 0;
        assert!(!should_refresh(&long, now));
    }

    #[test]
    fn invalid_refresh_response_never_exposes_other_token_fields() {
        let access = "response-access-must-not-leak";
        let error = token_from_response(TokenResponse {
            access_token: access.to_string(),
            refresh_token: None,
            expires_in_secs: Some(900),
            scope: Some("kimi-code".to_string()),
            token_type: Some("Bearer".to_string()),
        })
        .err()
        .expect("missing refresh token must fail");

        assert!(error.contains("missing refresh_token"), "{error}");
        assert!(!error.contains(access), "secret leaked: {error}");
    }

    #[tokio::test]
    async fn fresh_file_avoids_refresh() {
        let (root, registration) = fixture("fresh");
        save_token(
            &registration.token_file(),
            &token("cached-access", "cached-refresh", now_unix() + 900),
        )
        .unwrap();
        let fetcher = CountingFetcher::success(Duration::ZERO);
        let coordinator = KimiOauthCoordinator::new(fetcher.clone());
        coordinator.register("kimi", "coding", registration);

        let access = coordinator
            .access_token("kimi", "coding")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(access.expose(), "cached-access");
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 0);
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn concurrent_callers_share_exactly_one_refresh() {
        let (root, registration) = fixture("concurrent");
        save_token(
            &registration.token_file(),
            &token("expired-access", "refresh-before", 1),
        )
        .unwrap();
        let fetcher = CountingFetcher::success(Duration::from_millis(50));
        let coordinator = Arc::new(KimiOauthCoordinator::new(fetcher.clone()));
        coordinator.register("kimi", "coding", registration);

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let coordinator = coordinator.clone();
            tasks.push(tokio::spawn(async move {
                coordinator
                    .access_token("kimi", "coding")
                    .await
                    .unwrap()
                    .unwrap()
            }));
        }
        for task in tasks {
            assert_eq!(task.await.unwrap().expose(), "fresh-access-0");
        }
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn transient_refresh_failures_retry_twice_then_persist_success() {
        struct TransientThenSuccess {
            calls: AtomicUsize,
        }
        #[async_trait]
        impl TokenFetcher for TransientThenSuccess {
            async fn refresh(
                &self,
                _token_url: &str,
                _client_id: Option<&str>,
                _client_secret: Option<&str>,
                _refresh_token: &str,
            ) -> Result<TokenResponse, TokenFetchError> {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                if call < 2 {
                    return Err(TokenFetchError::Retryable(
                        "token endpoint returned 503".to_string(),
                    ));
                }
                Ok(TokenResponse {
                    access_token: "retried-access".to_string(),
                    refresh_token: Some("retried-refresh".to_string()),
                    expires_in_secs: Some(900),
                    scope: Some("kimi-code".to_string()),
                    token_type: Some("Bearer".to_string()),
                })
            }
        }

        let (root, registration) = fixture("retry-success");
        let token_file = registration.token_file();
        save_token(&token_file, &token("expired-access", "refresh-before", 1)).unwrap();
        let fetcher = Arc::new(TransientThenSuccess {
            calls: AtomicUsize::new(0),
        });
        let sleeper = Arc::new(RecordingSleeper::default());
        let coordinator =
            KimiOauthCoordinator::with_retry_sleeper(fetcher.clone(), sleeper.clone());
        coordinator.register("kimi", "coding", registration);

        let access = coordinator
            .access_token("kimi", "coding")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(access.expose(), "retried-access");
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            *sleeper.delays.lock().unwrap(),
            vec![Duration::from_secs(1), Duration::from_secs(2)]
        );
        let persisted = match load_state(&token_file).unwrap() {
            StoredState::Valid(token) => token,
            _ => panic!("retried token must persist"),
        };
        assert_eq!(persisted.refresh_token, "retried-refresh");
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn exhausted_transient_refresh_stops_at_three_without_tombstoning() {
        struct AlwaysTransient {
            calls: AtomicUsize,
        }
        #[async_trait]
        impl TokenFetcher for AlwaysTransient {
            async fn refresh(
                &self,
                _token_url: &str,
                _client_id: Option<&str>,
                _client_secret: Option<&str>,
                _refresh_token: &str,
            ) -> Result<TokenResponse, TokenFetchError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Err(TokenFetchError::Retryable(
                    "token endpoint returned 503".to_string(),
                ))
            }
        }

        let (root, registration) = fixture("retry-exhausted");
        let token_file = registration.token_file();
        save_token(&token_file, &token("expired-access", "refresh-before", 1)).unwrap();
        let fetcher = Arc::new(AlwaysTransient {
            calls: AtomicUsize::new(0),
        });
        let sleeper = Arc::new(RecordingSleeper::default());
        let coordinator =
            KimiOauthCoordinator::with_retry_sleeper(fetcher.clone(), sleeper.clone());
        coordinator.register("kimi", "coding", registration);

        let error = coordinator
            .access_token("kimi", "coding")
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.contains("503"), "{error}");
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            *sleeper.delays.lock().unwrap(),
            vec![Duration::from_secs(1), Duration::from_secs(2)]
        );
        let persisted = match load_state(&token_file).unwrap() {
            StoredState::Valid(token) => token,
            _ => panic!("transient failure must not tombstone credentials"),
        };
        assert_eq!(persisted.access_token, "expired-access");
        assert_eq!(persisted.refresh_token, "refresh-before");
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn fatal_refresh_failure_is_not_retried_or_tombstoned() {
        struct FatalFetcher {
            calls: AtomicUsize,
        }
        #[async_trait]
        impl TokenFetcher for FatalFetcher {
            async fn refresh(
                &self,
                _token_url: &str,
                _client_id: Option<&str>,
                _client_secret: Option<&str>,
                _refresh_token: &str,
            ) -> Result<TokenResponse, TokenFetchError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Err(TokenFetchError::Fatal(
                    "token endpoint returned 400".to_string(),
                ))
            }
        }

        let (root, registration) = fixture("fatal-no-retry");
        let token_file = registration.token_file();
        save_token(&token_file, &token("expired-access", "refresh-before", 1)).unwrap();
        let fetcher = Arc::new(FatalFetcher {
            calls: AtomicUsize::new(0),
        });
        let sleeper = Arc::new(RecordingSleeper::default());
        let coordinator =
            KimiOauthCoordinator::with_retry_sleeper(fetcher.clone(), sleeper.clone());
        coordinator.register("kimi", "coding", registration);

        let error = coordinator
            .access_token("kimi", "coding")
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.contains("400"), "{error}");
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);
        assert!(sleeper.delays.lock().unwrap().is_empty());
        let persisted = match load_state(&token_file).unwrap() {
            StoredState::Valid(token) => token,
            _ => panic!("fatal non-auth failure must not tombstone credentials"),
        };
        assert_eq!(persisted.refresh_token, "refresh-before");
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn post_lock_reread_uses_peer_rotation_without_refreshing_again() {
        let (root, registration) = fixture("peer-rotation");
        let token_file = registration.token_file();
        save_token(&token_file, &token("expired-access", "refresh-before", 1)).unwrap();
        let lock_target = registration.lock_target();
        fs::create_dir_all(lock_target.parent().unwrap()).unwrap();
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&lock_target)
            .unwrap();
        let mut lock_name = lock_target.as_os_str().to_os_string();
        lock_name.push(".lock");
        let lock_path = PathBuf::from(lock_name);
        fs::create_dir(&lock_path).unwrap();

        let fetcher = CountingFetcher::success(Duration::ZERO);
        let coordinator = Arc::new(KimiOauthCoordinator::new(fetcher.clone()));
        coordinator.register("kimi", "coding", registration);
        let caller = {
            let coordinator = coordinator.clone();
            tokio::spawn(async move {
                coordinator
                    .access_token("kimi", "coding")
                    .await
                    .unwrap()
                    .unwrap()
            })
        };

        tokio::time::sleep(Duration::from_millis(100)).await;
        save_token(
            &token_file,
            &token("peer-access", "peer-refresh", now_unix() + 900),
        )
        .unwrap();
        fs::remove_dir(&lock_path).unwrap();

        assert_eq!(caller.await.unwrap().expose(), "peer-access");
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 0);
        fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reclaimed_old_owner_release_preserves_successor_lock() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let (root, registration) = fixture("lock-ownership");
        let mut old_owner = RefreshFileLock::acquire(&registration.lock_target())
            .await
            .unwrap();
        if let Some(stop) = old_owner.stop.take() {
            let _ = stop.send(());
        }
        if let Some(heartbeat) = old_owner.heartbeat.take() {
            heartbeat.await.unwrap();
        }

        let lock_path = old_owner.lock_path.clone().unwrap();
        let path = CString::new(lock_path.as_os_str().as_bytes()).unwrap();
        let stale_at = now_unix().saturating_sub(10) as libc::time_t;
        let times = [
            libc::timespec {
                tv_sec: stale_at,
                tv_nsec: 0,
            },
            libc::timespec {
                tv_sec: stale_at,
                tv_nsec: 0,
            },
        ];
        assert_eq!(
            unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), 0) },
            0
        );

        let successor = RefreshFileLock::acquire(&registration.lock_target())
            .await
            .unwrap();
        assert!(lock_path.exists(), "successor owns the reclaimed lock");

        old_owner.release().await.unwrap();
        assert!(
            lock_path.exists(),
            "a stale prior owner must not remove its successor's lock"
        );

        successor.release().await.unwrap();
        fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn creator_stall_before_publish_cannot_adopt_or_replace_successor_lock() {
        let root = std::env::temp_dir().join(format!(
            "sb-kimi-creator-stall-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let lock_path = root.join("kimi-code.lock");
        fs::create_dir_all(&root).unwrap();
        let mut candidate = LockCandidate::create(&lock_path).unwrap();
        let candidate_identity = candidate.identity;
        let successor_identity = Arc::new(Mutex::new(None));
        let hook_identity = successor_identity.clone();
        let hook_path = lock_path.clone();
        let publish_successor = Arc::new(move || {
            fs::create_dir(&hook_path).unwrap();
            *hook_identity.lock().unwrap() = Some(LockIdentity::from_metadata(
                &fs::symlink_metadata(&hook_path).unwrap(),
            ));
        });

        let result = candidate
            .publish(&lock_path, publish_successor.as_ref())
            .unwrap();
        let opened_identity =
            LockIdentity::from_metadata(&candidate.inode.directory.metadata().unwrap());
        let expected_successor = successor_identity.lock().unwrap().unwrap();
        let successor_survived = path_has_identity(&lock_path, expected_successor).unwrap();
        let candidate_is_empty = fs::read_dir(&candidate.path).unwrap().next().is_none();
        let successor_is_empty = fs::read_dir(&lock_path).unwrap().next().is_none();
        let candidate_path = candidate.path.clone();
        drop(candidate);
        let unpublished_candidate_was_cleaned = !candidate_path.exists();
        fs::remove_dir_all(root).ok();

        assert_eq!(
            result,
            LockPublishResult::Contended,
            "a stalled creator replaced the successor lock"
        );
        assert_eq!(opened_identity, candidate_identity);
        assert!(
            successor_survived,
            "candidate publication removed successor"
        );
        assert!(candidate_is_empty, "candidate lock directory was not empty");
        assert!(successor_is_empty, "published lock directory was not empty");
        assert!(
            unpublished_candidate_was_cleaned,
            "contended candidate directory was left behind"
        );
    }

    #[cfg(unix)]
    #[test]
    fn two_open_candidates_publish_exactly_one_owned_empty_inode() {
        let root = std::env::temp_dir().join(format!(
            "sb-kimi-two-candidates-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let lock_path = root.join("kimi-code.lock");
        fs::create_dir_all(&root).unwrap();
        let mut first = LockCandidate::create(&lock_path).unwrap();
        let mut second = LockCandidate::create(&lock_path).unwrap();
        let noop = || {};

        let first_result = first.publish(&lock_path, &noop).unwrap();
        let second_result = second.publish(&lock_path, &noop).unwrap();
        let published_matches_open_fd = path_has_identity(&lock_path, first.identity).unwrap();
        let published_is_empty = fs::read_dir(&lock_path).unwrap().next().is_none();
        let second_path = second.path.clone();
        drop(second);
        let losing_candidate_was_cleaned = !second_path.exists();
        drop(first);
        fs::remove_dir_all(root).ok();

        assert_eq!(first_result, LockPublishResult::Published);
        assert_eq!(second_result, LockPublishResult::Contended);
        assert!(published_matches_open_fd);
        assert!(published_is_empty);
        assert!(losing_candidate_was_cleaned);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn replacement_after_atomic_publish_is_rejected_before_adoption() {
        let (root, registration) = fixture("post-publish-replacement");
        let lock_target = registration.lock_target();
        let mut lock_name = lock_target.as_os_str().to_os_string();
        lock_name.push(".lock");
        let lock_path = PathBuf::from(lock_name);
        let successor_identity = Arc::new(Mutex::new(None));
        let hook_identity = successor_identity.clone();
        let hook_path = lock_path.clone();
        let replace_after_publish = Arc::new(move || {
            fs::remove_dir(&hook_path).unwrap();
            fs::create_dir(&hook_path).unwrap();
            *hook_identity.lock().unwrap() = Some(LockIdentity::from_metadata(
                &fs::symlink_metadata(&hook_path).unwrap(),
            ));
        });
        let noop = || {};

        let error = match RefreshFileLock::acquire_with_hooks(
            &lock_target,
            &noop,
            replace_after_publish.as_ref(),
        )
        .await
        {
            Ok(lock) => {
                drop(lock);
                panic!("a replaced publication must not be adopted")
            }
            Err(error) => error,
        };
        let expected = successor_identity.lock().unwrap().unwrap();
        let successor_survived = path_has_identity(&lock_path, expected).unwrap();
        let successor_is_empty = fs::read_dir(&lock_path).unwrap().next().is_none();
        let successor_is_fresh = fs::symlink_metadata(&lock_path)
            .unwrap()
            .modified()
            .unwrap()
            .elapsed()
            .unwrap()
            < LOCK_STALE_AFTER;
        fs::remove_dir_all(root).ok();

        assert!(error.contains("replaced before adoption"), "{error}");
        assert!(successor_survived);
        assert!(successor_is_empty);
        assert!(successor_is_fresh);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn release_leaves_an_empty_stale_lock_for_immediate_reclaim() {
        let (root, registration) = fixture("release-reclaim");
        let owner = RefreshFileLock::acquire(&registration.lock_target())
            .await
            .unwrap();
        let lock_path = owner.lock_path.clone().unwrap();
        let old_identity = owner.ownership.as_ref().unwrap().identity;

        owner.release().await.unwrap();
        let released_is_empty = fs::read_dir(&lock_path).unwrap().next().is_none();
        let released_is_stale = fs::symlink_metadata(&lock_path)
            .unwrap()
            .modified()
            .unwrap()
            .elapsed()
            .unwrap()
            >= LOCK_STALE_AFTER;
        let successor = RefreshFileLock::acquire(&registration.lock_target())
            .await
            .unwrap();
        let successor_identity = successor.ownership.as_ref().unwrap().identity;
        let successor_is_published = path_has_identity(&lock_path, successor_identity).unwrap();
        successor.release().await.unwrap();
        fs::remove_dir_all(root).ok();

        assert!(released_is_empty, "released lock directory was not empty");
        assert!(released_is_stale, "released lock was not retired as stale");
        assert_ne!(successor_identity, old_identity);
        assert!(successor_is_published);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn release_interleaving_never_removes_or_ages_replacement_lock() {
        let (root, registration) = fixture("release-replacement");
        let mut owner = RefreshFileLock::acquire(&registration.lock_target())
            .await
            .unwrap();
        let lock_path = owner.lock_path.clone().unwrap();
        assert!(fs::read_dir(&lock_path).unwrap().next().is_none());
        let retired_inode = owner.ownership.as_ref().unwrap().inode.clone();
        let (replacement_identity, hook) = replacement_lock_hook(&lock_path);
        owner.ownership.as_mut().unwrap().before_retire = Some(hook);

        owner.release().await.unwrap();
        let expected = replacement_identity.lock().unwrap().unwrap();
        let successor_survived = path_has_identity(&lock_path, expected).unwrap();
        let successor_is_fresh = fs::symlink_metadata(&lock_path)
            .unwrap()
            .modified()
            .unwrap()
            .elapsed()
            .unwrap()
            < LOCK_STALE_AFTER;
        let owned_inode_is_stale = retired_inode
            .directory
            .metadata()
            .unwrap()
            .modified()
            .unwrap()
            .elapsed()
            .unwrap()
            >= LOCK_STALE_AFTER;
        fs::remove_dir_all(root).ok();

        assert!(successor_survived, "release removed the replacement lock");
        assert!(successor_is_fresh, "release aged the replacement lock");
        assert!(owned_inode_is_stale, "release did not age its owned inode");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn drop_interleaving_never_removes_or_ages_replacement_lock() {
        let (root, registration) = fixture("drop-replacement");
        let mut owner = RefreshFileLock::acquire(&registration.lock_target())
            .await
            .unwrap();
        let lock_path = owner.lock_path.clone().unwrap();
        assert!(fs::read_dir(&lock_path).unwrap().next().is_none());
        let retired_inode = owner.ownership.as_ref().unwrap().inode.clone();
        let (replacement_identity, hook) = replacement_lock_hook(&lock_path);
        owner.ownership.as_mut().unwrap().before_retire = Some(hook);

        drop(owner);
        let expected = replacement_identity.lock().unwrap().unwrap();
        let successor_survived = path_has_identity(&lock_path, expected).unwrap();
        let successor_is_fresh = fs::symlink_metadata(&lock_path)
            .unwrap()
            .modified()
            .unwrap()
            .elapsed()
            .unwrap()
            < LOCK_STALE_AFTER;
        let owned_inode_is_stale = retired_inode
            .directory
            .metadata()
            .unwrap()
            .modified()
            .unwrap()
            .elapsed()
            .unwrap()
            >= LOCK_STALE_AFTER;
        fs::remove_dir_all(root).ok();

        assert!(successor_survived, "drop removed the replacement lock");
        assert!(successor_is_fresh, "drop aged the replacement lock");
        assert!(owned_inode_is_stale, "drop did not age its owned inode");
    }

    #[cfg(unix)]
    #[test]
    fn vanished_contended_lock_is_retried_instead_of_failing_acquisition() {
        let path = std::env::temp_dir().join(format!(
            "sb-kimi-vanished-lock-{}",
            uuid::Uuid::new_v4().simple()
        ));

        assert!(existing_lock_metadata(&path).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn stale_reclaimer_preserves_replacement_inode_seen_before_remove() {
        let root = std::env::temp_dir().join(format!(
            "sb-kimi-stale-recheck-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let lock_path = root.join("kimi-code.lock");
        fs::create_dir_all(&lock_path).unwrap();
        let stale_identity =
            LockIdentity::from_metadata(&fs::symlink_metadata(&lock_path).unwrap());

        fs::remove_dir(&lock_path).unwrap();
        fs::create_dir(&lock_path).unwrap();
        assert!(remove_stale_lock_if_unchanged(&lock_path, stale_identity).unwrap());
        let successor_survived = lock_path.exists();
        fs::remove_dir_all(root).ok();

        assert!(
            successor_survived,
            "stale reclaimer removed a replacement lock inode"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reclaimed_old_refresh_cannot_overwrite_or_remove_successor() {
        use tokio::sync::{oneshot, Semaphore};

        struct BlockingFetcher {
            started: Mutex<Option<oneshot::Sender<()>>>,
            resume: Arc<Semaphore>,
        }
        #[async_trait]
        impl TokenFetcher for BlockingFetcher {
            async fn refresh(
                &self,
                _token_url: &str,
                _client_id: Option<&str>,
                _client_secret: Option<&str>,
                _refresh_token: &str,
            ) -> Result<TokenResponse, TokenFetchError> {
                if let Some(started) = self.started.lock().unwrap().take() {
                    let _ = started.send(());
                }
                self.resume.acquire().await.unwrap().forget();
                Ok(TokenResponse {
                    access_token: "stale-owner-access".to_string(),
                    refresh_token: Some("stale-owner-refresh".to_string()),
                    expires_in_secs: Some(900),
                    scope: Some("kimi-code".to_string()),
                    token_type: Some("Bearer".to_string()),
                })
            }
        }

        let (root, registration) = fixture("stale-refresh-fence");
        let token_file = registration.token_file();
        save_token(&token_file, &token("expired-access", "refresh-before", 1)).unwrap();
        let (started_tx, started_rx) = oneshot::channel();
        let resume = Arc::new(Semaphore::new(0));
        let fetcher = Arc::new(BlockingFetcher {
            started: Mutex::new(Some(started_tx)),
            resume: resume.clone(),
        });
        let coordinator = Arc::new(KimiOauthCoordinator::new(fetcher));
        coordinator.register("kimi", "coding", registration.clone());

        let stale_attempt = {
            let coordinator = coordinator.clone();
            tokio::spawn(async move { coordinator.access_token("kimi", "coding").await.unwrap() })
        };
        started_rx.await.unwrap();

        let lock_target = registration.lock_target();
        let mut lock_name = lock_target.as_os_str().to_os_string();
        lock_name.push(".lock");
        let lock_path = PathBuf::from(lock_name);
        fs::remove_dir(&lock_path).unwrap();
        fs::create_dir(&lock_path).unwrap();
        save_token(
            &token_file,
            &token("successor-access", "successor-refresh", now_unix() + 900),
        )
        .unwrap();

        resume.add_permits(1);
        let error = stale_attempt
            .await
            .unwrap()
            .expect_err("a reclaimed holder must fail as compromised");
        assert!(error.contains("compromised"), "{error}");

        let persisted = match load_state(&token_file).unwrap() {
            StoredState::Valid(token) => token,
            _ => panic!("successor token must remain valid"),
        };
        assert_eq!(persisted.access_token, "successor-access");
        assert_eq!(persisted.refresh_token, "successor-refresh");
        assert!(lock_path.exists(), "stale release removed successor lock");

        fs::remove_dir(&lock_path).unwrap();
        fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn takeover_after_success_file_preparation_cannot_commit_stale_rotation() {
        let (root, registration) = fixture("precommit-success-fence");
        let token_file = registration.token_file();
        save_token(&token_file, &token("expired-access", "refresh-before", 1)).unwrap();
        let (lock_path, hook_calls, hook) =
            persistence_takeover_hook(&registration, &token_file, None);
        let coordinator = KimiOauthCoordinator::with_persistence_hook(
            CountingFetcher::success(Duration::ZERO),
            hook,
        );
        coordinator.register("kimi", "coding", registration);

        let result = coordinator.access_token("kimi", "coding").await.unwrap();
        let persisted = match load_state(&token_file).unwrap() {
            StoredState::Valid(token) => Some((token.access_token, token.refresh_token)),
            _ => None,
        };
        let successor_lock_survived = lock_path.exists();
        fs::remove_dir(&lock_path).ok();
        fs::remove_dir_all(root).ok();

        let error = result.expect_err("the stale prepared rotation must be fenced out");
        assert!(error.contains("compromised"), "{error}");
        assert_eq!(hook_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            persisted,
            Some(("expired-access".to_string(), "refresh-before".to_string()))
        );
        assert!(
            successor_lock_survived,
            "stale release removed successor lock"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn takeover_after_tombstone_file_preparation_cannot_revoke_successor() {
        let (root, registration) = fixture("precommit-tombstone-fence");
        let token_file = registration.token_file();
        save_token(&token_file, &token("expired-access", "refresh-before", 1)).unwrap();
        let (lock_path, hook_calls, hook) = persistence_takeover_hook(
            &registration,
            &token_file,
            Some(token(
                "successor-access",
                "successor-refresh",
                now_unix() + 900,
            )),
        );
        let coordinator =
            KimiOauthCoordinator::with_persistence_hook(CountingFetcher::unauthorized(), hook);
        coordinator.register("kimi", "coding", registration);

        let result = coordinator.access_token("kimi", "coding").await.unwrap();
        let persisted = match load_state(&token_file).unwrap() {
            StoredState::Valid(token) => Some((token.access_token, token.refresh_token)),
            _ => None,
        };
        let successor_lock_survived = lock_path.exists();
        fs::remove_dir(&lock_path).ok();
        fs::remove_dir_all(root).ok();

        let error = result.expect_err("the stale prepared tombstone must be fenced out");
        assert!(error.contains("credential file changed"), "{error}");
        assert_eq!(hook_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            persisted,
            Some((
                "successor-access".to_string(),
                "successor-refresh".to_string()
            ))
        );
        assert!(
            successor_lock_survived,
            "stale release removed successor lock"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reclaimed_old_unauthorized_refresh_cannot_tombstone_or_remove_successor() {
        use tokio::sync::{oneshot, Semaphore};

        struct BlockingUnauthorizedFetcher {
            started: Mutex<Option<oneshot::Sender<()>>>,
            resume: Arc<Semaphore>,
        }
        #[async_trait]
        impl TokenFetcher for BlockingUnauthorizedFetcher {
            async fn refresh(
                &self,
                _token_url: &str,
                _client_id: Option<&str>,
                _client_secret: Option<&str>,
                _refresh_token: &str,
            ) -> Result<TokenResponse, TokenFetchError> {
                if let Some(started) = self.started.lock().unwrap().take() {
                    let _ = started.send(());
                }
                self.resume.acquire().await.unwrap().forget();
                Err(TokenFetchError::Unauthorized)
            }
        }

        let (root, registration) = fixture("stale-unauthorized-fence");
        let token_file = registration.token_file();
        save_token(&token_file, &token("expired-access", "refresh-before", 1)).unwrap();
        let (started_tx, started_rx) = oneshot::channel();
        let resume = Arc::new(Semaphore::new(0));
        let fetcher = Arc::new(BlockingUnauthorizedFetcher {
            started: Mutex::new(Some(started_tx)),
            resume: resume.clone(),
        });
        let coordinator = Arc::new(KimiOauthCoordinator::new(fetcher));
        coordinator.register("kimi", "coding", registration.clone());

        let stale_attempt = {
            let coordinator = coordinator.clone();
            tokio::spawn(async move { coordinator.access_token("kimi", "coding").await.unwrap() })
        };
        started_rx.await.unwrap();

        let lock_target = registration.lock_target();
        let mut lock_name = lock_target.as_os_str().to_os_string();
        lock_name.push(".lock");
        let lock_path = PathBuf::from(lock_name);
        fs::remove_dir(&lock_path).unwrap();
        fs::create_dir(&lock_path).unwrap();

        resume.add_permits(1);
        let error = stale_attempt
            .await
            .unwrap()
            .expect_err("a reclaimed holder must fail before tombstoning");
        assert!(error.contains("compromised"), "{error}");

        let persisted = match load_state(&token_file).unwrap() {
            StoredState::Valid(token) => token,
            _ => panic!("a stale unauthorized response must not tombstone credentials"),
        };
        assert_eq!(persisted.access_token, "expired-access");
        assert_eq!(persisted.refresh_token, "refresh-before");
        assert!(lock_path.exists(), "stale release removed successor lock");

        fs::remove_dir(&lock_path).unwrap();
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn unauthorized_refresh_tombstones_file_and_requires_kimi_login() {
        let (root, registration) = fixture("unauthorized");
        let token_file = registration.token_file();
        save_token(&token_file, &token("expired-access", "rejected-refresh", 1)).unwrap();
        let fetcher = CountingFetcher::unauthorized();
        let coordinator = KimiOauthCoordinator::new(fetcher.clone());
        coordinator.register("kimi", "coding", registration);

        let error = coordinator
            .access_token("kimi", "coding")
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.contains("kimi login"), "{error}");
        assert!(
            !error.contains("rejected-refresh"),
            "secret leaked: {error}"
        );
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(&token_file).unwrap()).unwrap();
        assert_eq!(persisted["access_token"], "");
        assert_eq!(persisted["refresh_token"], "");

        let second = coordinator
            .access_token("kimi", "coding")
            .await
            .unwrap()
            .unwrap_err();
        assert!(second.contains("kimi login"), "{second}");
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);
        fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn missing_and_malformed_credentials_are_actionable_and_redacted() {
        let (root, registration) = fixture("invalid-store");
        let token_file = registration.token_file();
        let fetcher = CountingFetcher::success(Duration::ZERO);
        let coordinator = KimiOauthCoordinator::new(fetcher.clone());
        coordinator.register("kimi", "coding", registration.clone());

        let missing = coordinator
            .access_token("kimi", "coding")
            .await
            .unwrap()
            .unwrap_err();
        assert!(missing.contains("kimi login"), "{missing}");

        fs::create_dir_all(token_file.parent().unwrap()).unwrap();
        let secret = "credential-body-must-not-leak";
        fs::write(&token_file, format!(r#"{{"access_token":"{secret}""#)).unwrap();
        let malformed = coordinator
            .access_token("kimi", "coding")
            .await
            .unwrap()
            .unwrap_err();
        assert!(malformed.contains("kimi login"), "{malformed}");
        assert!(!malformed.contains(secret), "secret leaked: {malformed}");

        fs::write(
            &token_file,
            format!(r#"{{"access_token":"{secret}","expires_at":1,"expires_in":900}}"#),
        )
        .unwrap();
        let missing_refresh = coordinator
            .access_token("kimi", "coding")
            .await
            .unwrap()
            .unwrap_err();
        assert!(missing_refresh.contains("kimi login"), "{missing_refresh}");
        assert!(
            !missing_refresh.contains(secret),
            "secret leaked: {missing_refresh}"
        );
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 0);
        fs::remove_dir_all(root).ok();
    }
}
