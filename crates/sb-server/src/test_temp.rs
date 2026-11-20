//! Test folders under the system temp dir that remove themselves.
//!
//! A helper that returns a bare `PathBuf` leaves its folder behind after every run: the
//! capture tests left hundreds of MiB per run that nothing removed. Keep the returned value
//! in scope for as long as the folder is used. Dropping it removes the folder, also when
//! the test panics.

use std::ops::Deref;
use std::path::PathBuf;

pub(crate) struct TempRoot(PathBuf);

impl TempRoot {
    /// Creates `<temp dir>/<name>` empty; an older folder with the same name is removed first.
    pub(crate) fn new(name: String) -> Self {
        let path = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Deref for TempRoot {
    type Target = PathBuf;

    fn deref(&self) -> &PathBuf {
        &self.0
    }
}

impl AsRef<std::path::Path> for TempRoot {
    fn as_ref(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::TempRoot;

    #[test]
    fn folder_lives_while_in_scope_and_is_removed_on_drop() {
        let root = TempRoot::new(format!("sb-test-temp-drop-{}", std::process::id()));
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/b/body"), vec![0u8; 4096]).unwrap();
        let path = root.to_path_buf();
        assert!(path.join("a/b/body").is_file());
        drop(root);
        assert!(
            !path.exists(),
            "temp folder left behind: {}",
            path.display()
        );
    }

    #[test]
    fn folder_is_removed_when_the_test_panics() {
        let name = format!("sb-test-temp-panic-{}", std::process::id());
        let path = std::env::temp_dir().join(&name);
        let result = std::panic::catch_unwind(|| {
            let root = TempRoot::new(name.clone());
            std::fs::write(root.join("body"), b"x").unwrap();
            panic!("test failed");
        });
        assert!(result.is_err());
        assert!(
            !path.exists(),
            "temp folder left behind: {}",
            path.display()
        );
    }
}
