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

/// Directories [`workspaces_below`] never enters besides hidden ones and
/// [`DEFAULT_PRUNED_DIRECTORIES`]: the `Pods/` of `CocoaPods` in Flutter's `ios/` and `macos/`
/// (tens of thousands of directories, never a workspace).
const PRUNED_FOR_WORKSPACES: &[&str] = &["Pods"];

/// The workspace roots at or below `root`: the directories holding `.gleon/gleon.yaml`, sorted
/// (`root` first when it is one).
///
/// Integrations record each golden in the workspace nearest above it, so one test run from a
/// workspace (`gleon test`) may report into nested ones too (the packages of a monorepo with
/// their own `.gleon/`). The walk stays in the repository of `root`: a directory with a
/// repository marker of its own (the `.git` file of a submodule, a nested `.git`, `.hg`, `.jj` or
/// `.sl`) is another repository and not entered. Hidden directories,
/// [`DEFAULT_PRUNED_DIRECTORIES`] and `Pods/` are not entered either, symbolic links are not
/// followed, and an unreadable directory is skipped (logged at debug).
#[must_use]
pub fn workspaces_below(root: &Path) -> Vec<std::path::PathBuf> {
    let mut builder = WalkBuilder::new(root);
    builder.standard_filters(false).filter_entry(|entry| {
        if !entry.file_type().is_some_and(|ft| ft.is_dir()) {
            return false;
        }
        if entry.depth() == 0 {
            return true;
        }
        let is_pruned = matches!(
            entry.file_name().to_str(),
            Some(name) if name.starts_with('.')
                || is_default_pruned_dir(name)
                || PRUNED_FOR_WORKSPACES.contains(&name)
        );
        !is_pruned && !is_repository_root(entry.path())
    });
    let mut roots: Vec<_> = builder
        .build()
        .filter_map(|entry| {
            entry
                .map_err(|error| tracing::debug!("Skipped while looking for workspaces: {error}"))
                .ok()
        })
        .map(ignore::DirEntry::into_path)
        .filter(|dir| gleon_model::workspace::has_config(dir))
        .collect();
    roots.sort();
    roots
}

/// Whether `dir` holds a repository marker (`.git` as a directory or as the file of a
/// submodule, `.hg`, `.jj`, `.sl`).
fn is_repository_root(dir: &Path) -> bool {
    gleon_model::workspace::REPOSITORY_MARKERS
        .iter()
        .any(|marker| dir.join(marker).exists())
}

/// Whether `dir` lies in a repository: it or a directory above it holds a repository marker.
#[must_use]
pub fn is_in_repository(dir: &Path) -> bool {
    dir.ancestors().any(is_repository_root)
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
    fn test_workspaces_below_finds_nested_workspaces() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        for dir in [
            "",
            "packages/a",
            "packages/b/example",
            "build/app",
            ".dart_tool/x",
            "node_modules/pkg",
        ] {
            let gleon = root.join(dir).join(".gleon");
            std::fs::create_dir_all(&gleon).unwrap();
            std::fs::write(gleon.join("gleon.yaml"), "").unwrap();
        }
        // A `.gleon/` without a config is no workspace.
        std::fs::create_dir_all(root.join("packages/c/.gleon/runs")).unwrap();
        // Another repository (a submodule: `.git` is a file) and CocoaPods are not entered.
        for (dir, marker) in [("third_party/sub", ".git"), ("ios/Pods/Lib", "")] {
            let gleon = root.join(dir).join(".gleon");
            std::fs::create_dir_all(&gleon).unwrap();
            std::fs::write(gleon.join("gleon.yaml"), "").unwrap();
            if !marker.is_empty() {
                std::fs::write(root.join(dir).join(marker), "gitdir: ../.git/modules/sub").unwrap();
            }
        }

        let found = workspaces_below(root);
        assert_eq!(
            found,
            [
                root.to_path_buf(),
                root.join("packages/a"),
                root.join("packages/b/example"),
            ]
        );
        assert!(workspaces_below(&root.join("packages/c")).is_empty());
        // The submodule itself is a root of its own.
        assert_eq!(
            workspaces_below(&root.join("third_party/sub")),
            [root.join("third_party/sub")]
        );
    }

    #[test]
    fn test_is_in_repository() {
        let temp = tempdir().unwrap();
        let nested = temp.path().join("repo/packages/a");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(!is_in_repository(&nested));
        std::fs::create_dir_all(temp.path().join("repo/.git")).unwrap();
        assert!(is_in_repository(&nested));
        assert!(is_in_repository(&temp.path().join("repo")));
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
