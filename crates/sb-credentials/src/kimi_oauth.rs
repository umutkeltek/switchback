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
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sb_core::Secret;
use serde::{Deserialize, Serialize};

use crate::account::expand_path;
use crate::refresh::{TokenFetcher, TokenResponse, UNAUTHORIZED_REFRESH_ERROR};

const LOCK_RETRIES: usize = 120;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(500);
const LOCK_STALE_AFTER: Duration = Duration::from_secs(5);
const LOCK_HEARTBEAT_EVERY: Duration = Duration::from_secs(2);
const MIN_REFRESH_THRESHOLD_SECS: u64 = 300;

#[derive(Debug, Clone)]
pub struct KimiOauthRegistration {
    pub token_file: String,
    pub token_url: String,
    pub client_id: String,
    pub lock_target: String,
}

struct KimiOauthState {
    registration: KimiOauthRegistration,
}

/// De-duplicates Kimi refreshes per account and coordinates token rotation
/// with other Kimi Code/Switchback processes through Kimi's native lock.
pub struct KimiOauthCoordinator {
    states: Mutex<HashMap<String, Arc<tokio::sync::Mutex<KimiOauthState>>>>,
    fetcher: Arc<dyn TokenFetcher>,
}

impl KimiOauthCoordinator {
    pub fn new(fetcher: Arc<dyn TokenFetcher>) -> Self {
        Self {
            states: Mutex::new(HashMap::new()),
            fetcher,
        }
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
        let token_file = expand_path(&registration.token_file)?;
        let initial = require_valid_state(load_state(&token_file)?)?;
        if !should_refresh(&initial, now_unix()) {
            return Ok(Secret::new(initial.access_token));
        }

        let lock = RefreshFileLock::acquire(&registration.lock_target).await?;
        let result = self
            .refresh_under_lock(registration, &token_file, initial)
            .await;
        let release_result = lock.release().await;
        match (result, release_result) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(token), Ok(())) => Ok(token),
        }
    }

    async fn refresh_under_lock(
        &self,
        registration: &KimiOauthRegistration,
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

        let response = self
            .fetcher
            .refresh(
                &registration.token_url,
                Some(&registration.client_id),
                None,
                &active.refresh_token,
            )
            .await;

        let response = match response {
            Ok(response) => response,
            Err(error) if error == UNAUTHORIZED_REFRESH_ERROR => {
                // Match Kimi's stale-token recovery: an independently rotating
                // peer may have won despite this process's best-effort lock.
                tokio::time::sleep(Duration::from_millis(100)).await;
                if let StoredState::Valid(recovered) = load_state(token_file)? {
                    if recovered.refresh_token != active.refresh_token {
                        return Ok(Secret::new(recovered.access_token));
                    }
                }

                save_token(token_file, &KimiToken::revoked_from(&active))?;
                return Err(relogin_error("refresh token was rejected"));
            }
            Err(error) => return Err(format!("kimi_code_oauth: token refresh failed: {error}")),
        };

        let refreshed = token_from_response(response)?;
        save_token(token_file, &refreshed)?;
        Ok(Secret::new(refreshed.access_token))
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

fn save_token(path: &Path, token: &KimiToken) -> Result<(), String> {
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
}

impl RefreshFileLock {
    #[cfg(not(unix))]
    async fn acquire(_target: &str) -> Result<Self, String> {
        // Kimi Code itself disables the proper-lockfile path on Windows.
        Ok(Self {
            lock_path: None,
            stop: None,
            heartbeat: None,
        })
    }

    #[cfg(unix)]
    async fn acquire(target: &str) -> Result<Self, String> {
        let target = expand_path(target)?;
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
            .open(&target)
            .map_err(|error| {
                format!(
                    "kimi_code_oauth: prepare OAuth refresh lock `{}`: {error}",
                    target.display()
                )
            })?;

        let mut lock_name = target.as_os_str().to_os_string();
        lock_name.push(".lock");
        let lock_path = PathBuf::from(lock_name);
        let mut acquired = false;
        for attempt in 0..=LOCK_RETRIES {
            match fs::create_dir(&lock_path) {
                Ok(()) => {
                    acquired = true;
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let metadata = fs::symlink_metadata(&lock_path).map_err(|metadata_error| {
                        format!(
                            "kimi_code_oauth: inspect OAuth refresh lock `{}`: {metadata_error}",
                            lock_path.display()
                        )
                    })?;
                    if !metadata.file_type().is_dir() {
                        return Err(format!(
                            "kimi_code_oauth: OAuth refresh lock `{}` is not a directory",
                            lock_path.display()
                        ));
                    }
                    let age = metadata
                        .modified()
                        .ok()
                        .and_then(|modified| SystemTime::now().duration_since(modified).ok());
                    if age.is_some_and(|age| age >= LOCK_STALE_AFTER) {
                        match fs::remove_dir(&lock_path) {
                            Ok(()) => continue,
                            Err(remove_error)
                                if remove_error.kind() == std::io::ErrorKind::NotFound =>
                            {
                                continue
                            }
                            Err(_) => {}
                        }
                    }
                }
                Err(error) => {
                    return Err(format!(
                        "kimi_code_oauth: acquire OAuth refresh lock `{}`: {error}",
                        lock_path.display()
                    ))
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

        let (stop, mut stopped) = tokio::sync::oneshot::channel();
        let heartbeat_path = lock_path.clone();
        let heartbeat = tokio::spawn(async move {
            let mut interval = tokio::time::interval(LOCK_HEARTBEAT_EVERY);
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    _ = interval.tick() => {
                        let _ = touch_path(&heartbeat_path);
                    }
                }
            }
        });
        Ok(Self {
            lock_path: Some(lock_path),
            stop: Some(stop),
            heartbeat: Some(heartbeat),
        })
    }

    async fn release(mut self) -> Result<(), String> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(heartbeat) = self.heartbeat.take() {
            let _ = heartbeat.await;
        }
        if let Some(lock_path) = self.lock_path.take() {
            match fs::remove_dir(&lock_path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "kimi_code_oauth: release OAuth refresh lock `{}`: {error}",
                        lock_path.display()
                    ))
                }
            }
        }
        Ok(())
    }
}

impl Drop for RefreshFileLock {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(heartbeat) = self.heartbeat.take() {
            heartbeat.abort();
        }
        if let Some(lock_path) = self.lock_path.take() {
            let _ = fs::remove_dir(lock_path);
        }
    }
}

#[cfg(unix)]
fn touch_path(path: &Path) -> Result<(), String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| "kimi_code_oauth: OAuth lock path contains NUL".to_string())?;
    // A null `times` pointer sets atime and mtime to the current clock time.
    let result = unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), std::ptr::null(), 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "kimi_code_oauth: update OAuth lock heartbeat: {}",
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(not(unix))]
fn touch_path(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    #[async_trait]
    impl TokenFetcher for CountingFetcher {
        async fn refresh(
            &self,
            _token_url: &str,
            _client_id: Option<&str>,
            _client_secret: Option<&str>,
            _refresh_token: &str,
        ) -> Result<TokenResponse, String> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if self.delay > Duration::ZERO {
                tokio::time::sleep(self.delay).await;
            }
            if self.unauthorized {
                return Err(UNAUTHORIZED_REFRESH_ERROR.to_string());
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
        let token_file = root.join("credentials/kimi-code.json");
        let registration = KimiOauthRegistration {
            token_file: token_file.to_string_lossy().into_owned(),
            token_url: "https://auth.invalid/api/oauth/token".to_string(),
            client_id: "test-client".to_string(),
            lock_target: root.join("oauth/kimi-code").to_string_lossy().into_owned(),
        };
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
            Path::new(&registration.token_file),
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
            Path::new(&registration.token_file),
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
    async fn post_lock_reread_uses_peer_rotation_without_refreshing_again() {
        let (root, registration) = fixture("peer-rotation");
        let token_file = PathBuf::from(&registration.token_file);
        save_token(&token_file, &token("expired-access", "refresh-before", 1)).unwrap();
        let lock_target = PathBuf::from(&registration.lock_target);
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

    #[tokio::test]
    async fn unauthorized_refresh_tombstones_file_and_requires_kimi_login() {
        let (root, registration) = fixture("unauthorized");
        let token_file = PathBuf::from(&registration.token_file);
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
        let token_file = PathBuf::from(&registration.token_file);
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
