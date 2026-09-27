//! Shared directory-traversal helpers built on `ignore`.

use std::path::Path;

use ignore::WalkBuilder;

use crate::naming::DEFAULT_PRUNED_DIRECTORIES;

/// Returns `true` if `name` is one of [`DEFAULT_PRUNED_DIRECTORIES`].
#[must_use]
pub fn is_default_pruned_dir(name: &str) -> bool {
    DEFAULT_PRUNED_DIRECTORIES.contains(&name)
}

/// Builds a `WalkBuilder` rooted at `dir` that prunes [`DEFAULT_PRUNED_DIRECTORIES`].
///
/// `.gleon` itself is exempted so callers walking a tree rooted inside `.gleon/` can still
/// descend into it. This is the walker shape shared by manifest and manifest-adjacent
/// traversal (workspace index loading, manifest linting, conflict scanning). It is
/// intentionally narrower than [`crate::scanner::FileScanner`]'s walker, which also prunes
/// `.gleon` itself and layers in glob-based exclude matching.
#[must_use]
pub fn pruned_walker(dir: &Path) -> WalkBuilder {
    let mut builder = WalkBuilder::new(dir);
    builder.standard_filters(false).filter_entry(|entry| {
        let is_pruned_dir = entry.file_type().is_some_and(|ft| ft.is_dir())
            && matches!(entry.file_name().to_str(), Some(name) if name != ".gleon" && is_default_pruned_dir(name));
        !is_pruned_dir
    });
    builder
}

/// Builds a `WalkBuilder` rooted at `dir` for traversing manifest trees under `.gleon/manifests/`.
///
/// Unlike [`pruned_walker`], this walker does not prune directory names like `target` or `build`,
/// which are valid test path components under `.gleon/manifests/` (e.g. `target/login.json`).
/// It prunes only hidden directories (`name.starts_with('.')`) beneath the search root.
#[must_use]
pub fn manifest_walker(dir: &Path) -> WalkBuilder {
    let mut builder = WalkBuilder::new(dir);
    builder.standard_filters(false).filter_entry(|entry| {
        let is_hidden_subdir = entry.file_type().is_some_and(|ft| ft.is_dir())
            && entry.depth() > 0
            && matches!(entry.file_name().to_str(), Some(name) if name.starts_with('.'));
        !is_hidden_subdir
    });
    builder
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
    fn test_is_default_pruned_dir() {
        assert!(is_default_pruned_dir("node_modules"));
        assert!(is_default_pruned_dir(".git"));
        assert!(!is_default_pruned_dir("src"));
    }

    #[test]
    fn test_pruned_walker_skips_pruned_dirs_but_walks_into_gleon() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        std::fs::write(root.join("node_modules").join("dead.txt"), "x").unwrap();
        std::fs::create_dir_all(root.join(".gleon").join("manifests")).unwrap();
        std::fs::write(root.join(".gleon").join("manifests").join("live.txt"), "x").unwrap();

        let names: Vec<_> = pruned_walker(root)
            .build()
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .collect();

        assert!(!names.contains(&"dead.txt".to_string()));
        assert!(names.contains(&"live.txt".to_string()));
    }

    #[test]
    fn test_manifest_walker_prunes_hidden_but_retains_target_and_build() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git").join("dead.txt"), "x").unwrap();
        std::fs::create_dir_all(root.join("target").join("login")).unwrap();
        std::fs::write(root.join("target").join("login").join("live.json"), "{}").unwrap();
        std::fs::create_dir_all(root.join("build")).unwrap();
        std::fs::write(root.join("build").join("header.json"), "{}").unwrap();

        let names: Vec<_> = manifest_walker(root)
            .build()
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .collect();

        assert!(!names.contains(&"dead.txt".to_string()));
        assert!(names.contains(&"live.json".to_string()));
        assert!(names.contains(&"header.json".to_string()));
    }
}
