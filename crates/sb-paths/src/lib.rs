//! The shared filesystem layout for Switchback-owned runtime data.
//!
//! Source code and runtime data are deliberately separate. `SWITCHBACK_ROOT`
//! remains the legacy source-checkout variable used by the operator wrapper;
//! `SWITCHBACK_RUNTIME_ROOT` is the explicit data-root contract. The existing
//! `SB_RUNTIME_ROOT` name is accepted for compatibility.

use std::env;
use std::path::{Path, PathBuf};

pub const RUNTIME_ROOT_ENV: &str = "SWITCHBACK_RUNTIME_ROOT";
pub const LEGACY_RUNTIME_ROOT_ENV: &str = "SB_RUNTIME_ROOT";
pub const SOURCE_ROOT_ENV: &str = "SWITCHBACK_ROOT";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePaths {
    runtime_root: PathBuf,
}

impl RuntimePaths {
    /// Resolve the runtime root from the stable environment contract.
    pub fn from_env() -> Self {
        Self::new(runtime_root_from_env())
    }

    /// Build a layout rooted at an explicit directory.
    pub fn new(runtime_root: impl Into<PathBuf>) -> Self {
        Self {
            runtime_root: runtime_root.into(),
        }
    }

    pub fn runtime_root(&self) -> &Path {
        &self.runtime_root
    }

    pub fn manifest(&self) -> PathBuf {
        self.runtime_root.join("manifest.json")
    }

    pub fn config_root(&self) -> PathBuf {
        self.runtime_root.join("config")
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_root().join("switchback.yaml")
    }

    pub fn env_file(&self) -> PathBuf {
        self.config_root().join("sb.env")
    }

    pub fn lanes_root(&self) -> PathBuf {
        self.config_root().join("lanes")
    }

    pub fn codex_profiles_root(&self) -> PathBuf {
        self.config_root().join("codex")
    }

    pub fn claude_profiles_root(&self) -> PathBuf {
        self.config_root().join("claude")
    }

    pub fn launch_profiles_file(&self) -> PathBuf {
        self.config_root().join("launch-profiles.json")
    }

    pub fn auth_registry_root(&self) -> PathBuf {
        self.config_root().join("codex-auth")
    }

    pub fn state_root(&self) -> PathBuf {
        self.runtime_root.join("state")
    }

    pub fn body_root(&self) -> PathBuf {
        self.state_root().join("body")
    }

    pub fn profile_projection_root(&self) -> PathBuf {
        self.state_root().join("profile-conformance")
    }

    pub fn eval_root(&self) -> PathBuf {
        self.runtime_root.join("eval")
    }

    pub fn eval_store(&self) -> PathBuf {
        self.eval_root().join("eval.sqlite")
    }

    pub fn receipts_root(&self) -> PathBuf {
        self.runtime_root.join("receipts")
    }

    pub fn binary_root(&self) -> PathBuf {
        self.runtime_root.join("bin")
    }

    pub fn backups_root(&self) -> PathBuf {
        self.runtime_root.join("backups")
    }
}

/// Resolve the runtime root without reading process environment. This is kept
/// separate so callers can test precedence without mutating global process
/// state.
pub fn runtime_root_from_values(
    explicit: Option<&Path>,
    runtime_root: Option<&Path>,
    legacy_runtime_root: Option<&Path>,
    source_root: Option<&Path>,
    current_dir: &Path,
) -> PathBuf {
    explicit
        .or(runtime_root)
        .or(legacy_runtime_root)
        .map(Path::to_path_buf)
        .or_else(|| source_root.map(|root| root.join(".switchback")))
        .unwrap_or_else(|| current_dir.join(".switchback"))
}

fn runtime_root_from_env() -> PathBuf {
    let current_dir = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    runtime_root_from_values(
        None,
        env_path(RUNTIME_ROOT_ENV).as_deref(),
        env_path(LEGACY_RUNTIME_ROOT_ENV).as_deref(),
        env_path(SOURCE_ROOT_ENV).as_deref(),
        &current_dir,
    )
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_root_wins_over_all_environment_shapes() {
        let root = runtime_root_from_values(
            Some(Path::new("/explicit")),
            Some(Path::new("/new-env")),
            Some(Path::new("/legacy-env")),
            Some(Path::new("/checkout")),
            Path::new("/cwd"),
        );
        assert_eq!(root, PathBuf::from("/explicit"));
    }

    #[test]
    fn stable_runtime_environment_wins_over_legacy_names() {
        let root = runtime_root_from_values(
            None,
            Some(Path::new("/new-env")),
            Some(Path::new("/legacy-env")),
            Some(Path::new("/checkout")),
            Path::new("/cwd"),
        );
        assert_eq!(root, PathBuf::from("/new-env"));
    }

    #[test]
    fn source_checkout_keeps_the_repository_local_compatibility_default() {
        let root = runtime_root_from_values(
            None,
            None,
            None,
            Some(Path::new("/checkout")),
            Path::new("/cwd"),
        );
        assert_eq!(root, PathBuf::from("/checkout/.switchback"));
    }

    #[test]
    fn no_environment_falls_back_to_the_current_directory() {
        let root = runtime_root_from_values(None, None, None, None, Path::new("/cwd"));
        assert_eq!(root, PathBuf::from("/cwd/.switchback"));
    }

    #[test]
    fn layout_has_one_root_for_owned_data() {
        let paths = RuntimePaths::new("/runtime");
        assert_eq!(
            paths.config_file(),
            PathBuf::from("/runtime/config/switchback.yaml")
        );
        assert_eq!(paths.state_root(), PathBuf::from("/runtime/state"));
        assert_eq!(paths.body_root(), PathBuf::from("/runtime/state/body"));
        assert_eq!(
            paths.profile_projection_root(),
            PathBuf::from("/runtime/state/profile-conformance")
        );
        assert_eq!(paths.eval_root(), PathBuf::from("/runtime/eval"));
        assert_eq!(
            paths.eval_store(),
            PathBuf::from("/runtime/eval/eval.sqlite")
        );
        assert_eq!(paths.receipts_root(), PathBuf::from("/runtime/receipts"));
        assert_eq!(paths.lanes_root(), PathBuf::from("/runtime/config/lanes"));
        assert_eq!(
            paths.launch_profiles_file(),
            PathBuf::from("/runtime/config/launch-profiles.json")
        );
        assert_eq!(
            paths.auth_registry_root(),
            PathBuf::from("/runtime/config/codex-auth")
        );
    }
}
