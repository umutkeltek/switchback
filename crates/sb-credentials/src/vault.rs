//! Encrypted credential vault. Secrets live in an `age`-encrypted JSON file;
//! the age identity (the file key) lives in the OS keychain — so the encrypted
//! file alone is useless, and the key never sits on disk in plaintext. The
//! `SWITCHBACK_VAULT_KEY` env var overrides the keychain (CI / headless / Docker).
//!
//! The vault is ADDITIVE: env/inline auth keep working. A provider account may
//! instead reference a vault secret by name (`auth: { kind: api_key, vault: X }`),
//! which is the most-secure source and wins over env/inline.
//!
//! The crypto core (`open_with_identity` / file read+write) is fully unit-tested
//! with an explicit identity — it never touches the OS keychain. Only the thin
//! key-management wrappers (`load_identity` / `store_identity`) use `keyring`.

use std::collections::BTreeMap;
use std::path::Path;

use age::secrecy::ExposeSecret;
use sb_core::Secret;

/// Keychain "username" the age identity is stored under. The service name comes
/// from config so multiple installs don't collide.
const KEYCHAIN_USER: &str = "vault-age-identity";
/// Env override for the age identity — takes precedence over the keychain so the
/// vault works where no OS keychain exists (CI, headless Linux, containers).
pub const KEY_ENV: &str = "SWITCHBACK_VAULT_KEY";

/// A decrypted vault held in memory: secret name -> secret value.
pub struct Vault {
    secrets: BTreeMap<String, Secret>,
}

impl Vault {
    /// Look up a secret by name.
    pub fn get(&self, name: &str) -> Option<Secret> {
        self.secrets.get(name).cloned()
    }

    /// Secret names only (never values) — for `vault list` / diagnostics.
    pub fn names(&self) -> Vec<String> {
        self.secrets.keys().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.secrets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    /// Decrypt a vault file with an explicit identity. This is the crypto core —
    /// fully testable without ever touching the OS keychain.
    pub fn open_with_identity(
        path: &Path,
        identity: &age::x25519::Identity,
    ) -> Result<Self, String> {
        let map = read_map(path, identity)?;
        Ok(Vault {
            secrets: map.into_iter().map(|(k, v)| (k, Secret::new(v))).collect(),
        })
    }

    /// Open the vault using the keychain- (or env-) held identity.
    pub fn open(path: &Path, service: &str) -> Result<Self, String> {
        let identity = load_identity(service)?;
        Self::open_with_identity(path, &identity)
    }
}

/// Parse an age identity from its `AGE-SECRET-KEY-…` string.
fn parse_identity(s: &str) -> Result<age::x25519::Identity, String> {
    s.trim()
        .parse::<age::x25519::Identity>()
        .map_err(|e| format!("invalid age identity: {e}"))
}

/// Load the age identity: `SWITCHBACK_VAULT_KEY` env (preferred for CI/headless)
/// else the OS keychain entry `(service, vault-age-identity)`.
pub fn load_identity(service: &str) -> Result<age::x25519::Identity, String> {
    if let Ok(key) = std::env::var(KEY_ENV) {
        if !key.trim().is_empty() {
            return parse_identity(&key);
        }
    }
    let entry = keyring::Entry::new(service, KEYCHAIN_USER)
        .map_err(|e| format!("keychain entry ({service}): {e}"))?;
    let key = entry
        .get_password()
        .map_err(|e| format!("no vault key: set {KEY_ENV} or run `switchback vault init` ({e})"))?;
    parse_identity(&key)
}

fn store_identity(service: &str, identity: &age::x25519::Identity) -> Result<(), String> {
    let entry = keyring::Entry::new(service, KEYCHAIN_USER)
        .map_err(|e| format!("keychain entry ({service}): {e}"))?;
    entry
        .set_password(identity.to_string().expose_secret())
        .map_err(|e| format!("keychain store: {e}"))
}

fn read_map(
    path: &Path,
    identity: &age::x25519::Identity,
) -> Result<BTreeMap<String, String>, String> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let ciphertext =
        std::fs::read(path).map_err(|e| format!("read vault {}: {e}", path.display()))?;
    let plaintext = age::decrypt(identity, &ciphertext)
        .map_err(|e| format!("decrypt vault (wrong key?): {e}"))?;
    serde_json::from_slice(&plaintext).map_err(|e| format!("parse vault: {e}"))
}

pub(crate) fn write_map(
    path: &Path,
    recipient: &age::x25519::Recipient,
    map: &BTreeMap<String, String>,
) -> Result<(), String> {
    let plaintext = serde_json::to_vec(map).map_err(|e| format!("serialize vault: {e}"))?;
    let ciphertext =
        age::encrypt(recipient, &plaintext).map_err(|e| format!("encrypt vault: {e}"))?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create vault dir {}: {e}", parent.display()))?;
        }
    }
    std::fs::write(path, ciphertext).map_err(|e| format!("write vault {}: {e}", path.display()))
}

/// Initialize a vault: generate an identity, store it in the keychain, and write
/// an empty encrypted file. Refuses to clobber an existing vault file.
pub fn init(path: &Path, service: &str) -> Result<(), String> {
    if path.exists() {
        return Err(format!("vault already exists at {}", path.display()));
    }
    let identity = age::x25519::Identity::generate();
    store_identity(service, &identity)?;
    write_map(path, &identity.to_public(), &BTreeMap::new())
}

/// Add or replace a secret (loads the identity from keychain/env).
pub fn set_secret(path: &Path, service: &str, name: &str, value: &str) -> Result<(), String> {
    let identity = load_identity(service)?;
    let mut map = read_map(path, &identity)?;
    map.insert(name.to_string(), value.to_string());
    write_map(path, &identity.to_public(), &map)
}

/// Remove a secret; returns whether it existed.
pub fn remove_secret(path: &Path, service: &str, name: &str) -> Result<bool, String> {
    let identity = load_identity(service)?;
    let mut map = read_map(path, &identity)?;
    let removed = map.remove(name).is_some();
    write_map(path, &identity.to_public(), &map)?;
    Ok(removed)
}

/// List secret names (never values).
pub fn list_secrets(path: &Path, service: &str) -> Result<Vec<String>, String> {
    let identity = load_identity(service)?;
    Ok(read_map(path, &identity)?.into_keys().collect())
}

/// Generate a fresh age identity string (`AGE-SECRET-KEY-…`) for use in the
/// `SWITCHBACK_VAULT_KEY` env var — the headless / CI / container path where no
/// OS keychain exists. Print it once and store it in your secrets manager; it is
/// the key to the vault.
pub fn generate_identity_string() -> String {
    age::x25519::Identity::generate()
        .to_string()
        .expose_secret()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique temp path per test tag — no external tempfile dep, no keychain.
    fn temp_path(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("sb-vault-test-{tag}-{}.age", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn set_with_identity(path: &Path, identity: &age::x25519::Identity, name: &str, value: &str) {
        let mut map = read_map(path, identity).unwrap();
        map.insert(name.to_string(), value.to_string());
        write_map(path, &identity.to_public(), &map).unwrap();
    }

    #[test]
    fn encrypt_then_decrypt_round_trips() {
        let path = temp_path("roundtrip");
        let id = age::x25519::Identity::generate();
        set_with_identity(&path, &id, "openrouter", "key-or-123");
        set_with_identity(&path, &id, "anthropic", "key-ant-456");

        let vault = Vault::open_with_identity(&path, &id).unwrap();
        assert_eq!(vault.get("openrouter").unwrap().expose(), "key-or-123");
        assert_eq!(vault.get("anthropic").unwrap().expose(), "key-ant-456");
        assert!(vault.get("missing").is_none());
        let mut names = vault.names();
        names.sort();
        assert_eq!(names, vec!["anthropic", "openrouter"]);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn wrong_identity_cannot_decrypt() {
        let path = temp_path("wrongkey");
        let id = age::x25519::Identity::generate();
        set_with_identity(&path, &id, "k", "v");

        let other = age::x25519::Identity::generate();
        assert!(Vault::open_with_identity(&path, &other).is_err());

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn missing_file_opens_empty() {
        let path = temp_path("missing");
        let id = age::x25519::Identity::generate();
        let vault = Vault::open_with_identity(&path, &id).unwrap();
        assert!(vault.is_empty());
    }

    #[test]
    fn ciphertext_does_not_contain_plaintext() {
        let path = temp_path("opaque");
        let id = age::x25519::Identity::generate();
        set_with_identity(&path, &id, "k", "super-secret-value");
        let bytes = std::fs::read(&path).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("super-secret-value"));
        std::fs::remove_file(&path).ok();
    }
}
