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
}

/// Whether `dir` has a `.gleon/` directory: the root of a workspace whose config may be named
/// explicitly (`--config`), or live elsewhere.
#[must_use]
pub fn has_gleon_dir(dir: &Path) -> bool {
    dir.join(".gleon").is_dir()
}

/// The workspace of `start_dir`: the nearest directory above it (inclusive) that `marker` accepts.
///
/// The walk is the one of every integration ([`gleon_model::workspace::find_root`]): within the
/// repository of `start_dir`, and never the home directory `home` or one of its parents unless
/// that is a repository root. `marker` is [`gleon_model::workspace::has_config`] to discover a
/// workspace by its config, or [`has_gleon_dir`]. The root is returned resolved
/// ([`gleon_model::fs::canonical`]).
pub fn find_workspace_root(
    start_dir: &Path,
    home: Option<&Path>,
    marker: impl Fn(&Path) -> bool,
) -> Option<GleonPaths> {
    // Walked resolved, like integrations walk their goldens, so `home` (resolved too) is
    // recognized behind a symbolic link, a junction or a `subst` drive; the root found is
    // resolved as well.
    gleon_model::workspace::find_root(&resolved(start_dir), home, marker).map(GleonPaths::new)
}

/// `dir` with symbolic links resolved ([`gleon_model::fs::canonical`]), as the roots of
/// [`find_workspace_root`] are; as it is when it cannot be resolved (it may not exist).
#[must_use]
pub fn resolved(dir: &Path) -> PathBuf {
    gleon_model::fs::canonical(dir).unwrap_or_else(|_| dir.to_path_buf())
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

        let found = find_workspace_root(&nested, None, has_gleon_dir).unwrap();
        // Resolved, like the walks of integrations.
        let resolved = gleon_model::fs::canonical(root).unwrap();
        assert_eq!(found.base_dir(), resolved);
        // The home directory is never a workspace root.
        assert_eq!(
            find_workspace_root(&nested, Some(&resolved), has_gleon_dir),
            None
        );
    }

    #[test]
    fn test_find_workspace_root_returns_none_when_marker_never_matches() {
        let temp = tempdir().unwrap();
        assert!(find_workspace_root(temp.path(), None, |_| false).is_none());
    }
}
