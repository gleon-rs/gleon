//! Canonical layout of the `.gleon` workspace directory.
//!
//! Every subdirectory and file gleon reads or writes under `.gleon/` is named here once, so
//! call sites never hand-roll `base_dir.join(".gleon").join("manifests").join(key)` again.

use std::path::{Path, PathBuf};

use gleon_model::platform::PlatformKey;

/// Resolves paths within a single `.gleon` workspace rooted at a given base directory.
///
/// Construction is a pure path computation (no filesystem access); callers decide whether
/// and when to touch disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GleonPaths {
    base_dir: PathBuf,
}

impl GleonPaths {
    /// Creates a `GleonPaths` rooted at `base_dir` (the directory that contains `.gleon/`).
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    /// The workspace root directory (the parent of `.gleon/`).
    #[must_use]
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// `.gleon/`
    #[must_use]
    pub fn gleon_dir(&self) -> PathBuf {
        self.base_dir.join(".gleon")
    }

    /// `.gleon/manifests/`
    #[must_use]
    pub fn manifests_root(&self) -> PathBuf {
        self.gleon_dir().join("manifests")
    }

    /// `.gleon/manifests/<platform>/`
    #[must_use]
    pub fn manifests_dir(&self, platform: &PlatformKey) -> PathBuf {
        self.manifests_root().join(platform)
    }

    /// `.gleon/blobs/`
    #[must_use]
    pub fn blobs_root(&self) -> PathBuf {
        self.gleon_dir().join("blobs")
    }

    /// `.gleon/blobs/<scheme>/` (e.g. `.gleon/blobs/sha256/`)
    #[must_use]
    pub fn blob_scheme_dir(&self, scheme: &str) -> PathBuf {
        self.blobs_root().join(scheme)
    }

    /// `.gleon/runs/` (the whole run-history cache root, purged wholesale by `gleon clean`).
    #[must_use]
    pub fn runs_root(&self) -> PathBuf {
        self.gleon_dir().join("runs")
    }

    /// `.gleon/runs/latest/`
    #[must_use]
    pub fn runs_latest(&self) -> PathBuf {
        self.runs_root().join("latest")
    }

    /// `.gleon/gleon.yaml`
    #[must_use]
    pub fn config_file(&self) -> PathBuf {
        self.gleon_dir().join("gleon.yaml")
    }

    /// `.gleon/history.json`
    #[must_use]
    pub fn history_file(&self) -> PathBuf {
        self.gleon_dir().join("history.json")
    }

    /// `.gleon/dashboard.html`
    #[must_use]
    pub fn dashboard_file(&self) -> PathBuf {
        self.gleon_dir().join("dashboard.html")
    }

    /// `.gleon/.gitignore`
    #[must_use]
    pub fn gitignore(&self) -> PathBuf {
        self.gleon_dir().join(".gitignore")
    }

    /// Moves `base_dir` to its parent in place, mirroring [`PathBuf::pop`].
    ///
    /// Returns `false` (and leaves `base_dir` unchanged) once there is no parent left, exactly
    /// like `PathBuf::pop`. Lets [`find_workspace_root`] walk up the tree by mutating a single
    /// `GleonPaths` instead of allocating a new `PathBuf` on every ancestor.
    pub fn pop(&mut self) -> bool {
        self.base_dir.pop()
    }
}

/// Walks up from `start_dir` (inclusive) through parent directories, returning the first
/// [`GleonPaths`] for which `marker` returns `true`, or `None` if no ancestor matches.
pub fn find_workspace_root(
    start_dir: &Path,
    marker: impl Fn(&GleonPaths) -> bool,
) -> Option<GleonPaths> {
    // Walk up in place: a single `GleonPaths` is mutated via `pop`, so only one `PathBuf`
    // ever exists for the whole walk (no per-ancestor allocation).
    let mut candidate = GleonPaths::new(start_dir);
    loop {
        if marker(&candidate) {
            return Some(candidate);
        }
        if !candidate.pop() {
            return None;
        }
    }
}

#[cfg(all(test, not(miri)))]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::pedantic,
    clippy::nursery,
    reason = "test code: panics are assertions, and pedantic/nursery style lints are not enforced in tests"
)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn test_gleon_paths_layout() {
        let paths = GleonPaths::new("/workspace");
        assert_eq!(paths.gleon_dir(), Path::new("/workspace/.gleon"));
        assert_eq!(
            paths.manifests_dir(&PlatformKey::parse("macos-aarch64").unwrap()),
            Path::new("/workspace/.gleon/manifests/macos-aarch64")
        );
        assert_eq!(
            paths.blob_scheme_dir("sha256"),
            Path::new("/workspace/.gleon/blobs/sha256")
        );
        assert_eq!(
            paths.config_file(),
            Path::new("/workspace/.gleon/gleon.yaml")
        );
        assert_eq!(
            paths.history_file(),
            Path::new("/workspace/.gleon/history.json")
        );
        assert_eq!(
            paths.dashboard_file(),
            Path::new("/workspace/.gleon/dashboard.html")
        );
        assert_eq!(paths.gitignore(), Path::new("/workspace/.gleon/.gitignore"));
    }

    #[test]
    fn test_find_workspace_root_walks_up_to_marker() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let nested = root.join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(root.join(".gleon")).unwrap();

        let found = find_workspace_root(&nested, |p| p.gleon_dir().is_dir()).unwrap();
        assert_eq!(found.base_dir(), root);
    }

    #[test]
    fn test_find_workspace_root_returns_none_when_marker_never_matches() {
        let temp = tempdir().unwrap();
        assert!(find_workspace_root(temp.path(), |_| false).is_none());
    }
}
