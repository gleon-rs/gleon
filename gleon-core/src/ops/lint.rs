//! Lint operation for validating manifest schema integrity and checking for Git merge conflict markers.

use std::{
    collections::{HashMap, hash_map::Entry},
    path::{Path, PathBuf},
};

use thiserror::Error;

use crate::{
    context::ResolvedContext,
    manifest::{index::manifest_names, single::SingleTestManifest},
    ops::{common::platform_filter_dir, sync::read_manifests_root},
    paths::GleonPaths,
};

/// Errors that can occur during manifest linting.
#[derive(Debug, Error)]
pub enum LintError {
    /// The target manifests directory was not found.
    #[error("Manifest directory does not exist at {0}")]
    ManifestDirNotFound(PathBuf),

    /// A platform filter that is no platform key.
    #[error("Invalid platform filter '{0}': must be a platform key")]
    InvalidPlatformFilter(String),

    /// Standard IO error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Error shared across `ops::*` operations (a goldens workspace has no manifests).
    #[error(transparent)]
    Core(#[from] crate::ops::common::CoreError),
}

/// Summary report returned by manifest linting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintReport {
    /// Total manifest JSON files inspected.
    pub total_files: usize,
    /// Number of schema-valid, unconflicted manifest files.
    pub valid_files: usize,
    /// Files containing Git merge conflict markers (`<<<<<<<`), stored as `(relative_path, reason)`.
    pub conflicted_files: Vec<(PathBuf, String)>,
    /// Files failing JSON schema or data validation, or named by no valid test path (one that
    /// is invalid, or that of another file once lowercase: every command refuses to load
    /// either), stored as `(relative_path, error)`.
    pub corrupted_files: Vec<(PathBuf, String)>,
    /// Entries of `.gleon/manifests/` that are no platform directory (one of an older key format,
    /// a stray file), relative to the workspace and sorted: gleon never reads them. Checked only
    /// without a platform filter; they never fail the lint.
    pub foreign_entries: Vec<PathBuf>,
    /// Overall pass/fail status (`true` if no conflicts or corruption).
    pub passed: bool,
}

/// Validates all manifest JSON files in `.gleon/manifests/`.
///
/// If `platform_filter` is provided, only manifests under `.gleon/manifests/<platform>/` are checked.
/// Otherwise, the manifests of every platform directory are validated recursively, and the other
/// entries are named ([`LintReport::foreign_entries`]).
///
/// # Errors
/// Returns [`LintError`] if reading directory fails or if the target directory is missing.
pub fn lint_workspace_manifests(
    ctx: &ResolvedContext,
    platform_filter: Option<&str>,
) -> Result<LintReport, LintError> {
    crate::ops::common::refuse_goldens(ctx)?;
    let base_dir = ctx.base_dir.as_path();
    let manifests_root = GleonPaths::new(base_dir).manifests_root();

    let (search_dirs, foreign_entries) = if let Some(filter) = platform_filter {
        let search_dir = platform_filter_dir(&manifests_root, filter)
            .map_err(|p| LintError::InvalidPlatformFilter(p.to_string()))?;
        (vec![search_dir], Vec::new())
    } else {
        let root = read_manifests_root(&manifests_root).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                LintError::ManifestDirNotFound(manifests_root.clone())
            } else {
                LintError::Io(e)
            }
        })?;
        let relative = |path: PathBuf| path.strip_prefix(base_dir).unwrap_or(&path).to_path_buf();
        (
            root.platforms.into_iter().map(|(_, dir)| dir).collect(),
            root.foreign.into_iter().map(relative).collect(),
        )
    };

    let mut total_files = 0;
    let mut valid_files = 0;
    let mut conflicted_files = Vec::new();
    let mut corrupted_files = Vec::new();
    // The first file of each test case key of each platform directory, as the index loads them.
    let mut keys = HashMap::new();

    for (search_dir, entry_res) in search_dirs.iter().flat_map(|search_dir| {
        crate::walk::manifest_walker(search_dir)
            .build()
            .map(move |entry| (search_dir, entry))
    }) {
        let entry = match entry_res {
            Ok(e) => e,
            Err(err) => {
                let err_msg = err.to_string();
                if let Some(io_err) = err.into_io_error() {
                    if io_err.kind() == std::io::ErrorKind::NotFound {
                        return Err(LintError::ManifestDirNotFound(search_dir.clone()));
                    }
                    return Err(LintError::Io(io_err));
                }
                tracing::warn!("Skipping unreadable manifest entry: {}", err_msg);
                continue;
            }
        };

        let path = entry.path();
        if entry.file_type().is_some_and(|ft| ft.is_file())
            && path.extension().is_some_and(|ext| ext == "json")
        {
            total_files += 1;
            let rel_path = path.strip_prefix(base_dir).unwrap_or(path).to_path_buf();
            // Walked inside `search_dir`, so the path is one of its entries.
            let in_platform = path.strip_prefix(search_dir).unwrap_or(path);
            if let Some(reason) = refused_by_index(&mut keys, search_dir, in_platform, &rel_path) {
                corrupted_files.push((rel_path, reason));
                continue;
            }

            match std::fs::read_to_string(path) {
                Ok(content) => {
                    if content.contains("<<<<<<<") {
                        conflicted_files.push((
                            rel_path,
                            "Git merge conflict markers (<<<<<<<) detected".to_string(),
                        ));
                    } else {
                        match serde_json::from_str::<SingleTestManifest>(&content) {
                            Ok(manifest) => match manifest.validate() {
                                Ok(()) => {
                                    valid_files += 1;
                                }
                                Err(e) => {
                                    corrupted_files.push((
                                        rel_path,
                                        format!("Manifest schema validation failed: {e}"),
                                    ));
                                }
                            },
                            Err(e) => {
                                corrupted_files
                                    .push((rel_path, format!("Invalid JSON syntax: {e}")));
                            }
                        }
                    }
                }
                Err(e) => {
                    corrupted_files.push((rel_path, format!("Failed to read file: {e}")));
                }
            }
        }
    }

    let passed = conflicted_files.is_empty() && corrupted_files.is_empty();

    Ok(LintReport {
        total_files,
        valid_files,
        conflicted_files,
        corrupted_files,
        foreign_entries,
        passed,
    })
}

/// The test case keys of the manifests seen so far, by platform directory, with the first file
/// of each.
type SeenKeys<'a> = HashMap<(&'a Path, String), PathBuf>;

/// Why no command can load the manifest at `in_platform` (relative to its platform directory
/// `search_dir`, `rel_path` in the workspace): no valid test path, or the test case of a file
/// seen before, as [`crate::manifest::WorkspaceIndex::load`] refuses them.
fn refused_by_index<'a>(
    keys: &mut SeenKeys<'a>,
    search_dir: &'a Path,
    in_platform: &Path,
    rel_path: &Path,
) -> Option<String> {
    match manifest_names(in_platform) {
        Err(e) => Some(format!("Invalid test path: {e}")),
        Ok((key, _)) => match keys.entry((search_dir, key)) {
            Entry::Occupied(first) => Some(format!(
                "Test case '{}' is also named by {}",
                first.key().1,
                first.get().display()
            )),
            Entry::Vacant(slot) => {
                slot.insert(rel_path.to_path_buf());
                None
            }
        },
    }
}

#[cfg(test)]
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
    use std::path::Path;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn test_lint_clean_manifests() {
        let temp = tempdir().unwrap();
        let manifests_dir = temp
            .path()
            .join(".gleon")
            .join("manifests")
            .join("macos-aarch64");
        std::fs::create_dir_all(&manifests_dir).unwrap();

        let manifest_content = include_str!("../../tests/fixtures/valid_manifest.json");
        std::fs::write(manifests_dir.join("login.json"), manifest_content).unwrap();

        let ctx =
            ResolvedContext::from_options(&crate::context::ContextOptions::default(), temp.path())
                .unwrap();
        let report = lint_workspace_manifests(&ctx, None).unwrap();

        assert_eq!(report.total_files, 1);
        assert_eq!(report.valid_files, 1);
        assert!(report.passed);
        assert!(report.conflicted_files.is_empty());
        assert!(report.corrupted_files.is_empty());
    }

    /// An entry of `.gleon/manifests/` that is no platform directory (one of an older key format,
    /// a stray file) is never read by gleon: lint names it instead of checking its files as
    /// manifests, and fails nothing for it; hidden entries are left alone.
    #[test]
    fn test_lint_names_entries_that_are_no_platform_directory() {
        let temp = tempdir().unwrap();
        let manifests = temp.path().join(".gleon").join("manifests");
        let valid = include_str!("../../tests/fixtures/valid_manifest.json");
        for dir in ["macos-aarch64", "macos aarch64", ".hidden"] {
            std::fs::create_dir_all(manifests.join(dir)).unwrap();
            std::fs::write(manifests.join(dir).join("login.json"), valid).unwrap();
        }
        std::fs::write(manifests.join(".DS_Store"), "x").unwrap();
        std::fs::write(manifests.join("stray.json"), valid).unwrap();

        let ctx =
            ResolvedContext::from_options(&crate::context::ContextOptions::default(), temp.path())
                .unwrap();
        let report = lint_workspace_manifests(&ctx, None).unwrap();

        let relative = Path::new(".gleon").join("manifests");
        assert_eq!(
            report.foreign_entries,
            [relative.join("macos aarch64"), relative.join("stray.json")]
        );
        assert_eq!((report.total_files, report.valid_files), (1, 1));
        assert!(report.passed);
    }

    /// A manifest named by no valid test path fails the lint, as it fails every command that loads
    /// its platform: a path with a space, and one that names the test case of another file once
    /// lowercase (`\` as `/`). `auth\login.json` is a file name on Unix only.
    #[cfg(unix)]
    #[test]
    fn test_lint_fails_manifests_no_index_loads() {
        let temp = tempdir().unwrap();
        let platform = temp.path().join(".gleon/manifests/linux-x86_64");
        let valid = include_str!("../../tests/fixtures/valid_manifest.json");
        for file in [
            "Bad Space/login.json",
            "auth/login.json",
            "auth\\login.json",
        ] {
            let path = platform.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, valid).unwrap();
        }
        let ctx =
            ResolvedContext::from_options(&crate::context::ContextOptions::default(), temp.path())
                .unwrap();
        // The index refuses to load this platform.
        assert!(crate::manifest::WorkspaceIndex::load(&platform).is_err());

        let report = lint_workspace_manifests(&ctx, None).unwrap();
        assert!(!report.passed);
        assert_eq!((report.total_files, report.valid_files), (3, 1));
        let reasons: Vec<_> = report
            .corrupted_files
            .iter()
            .map(|(_, reason)| reason.as_str())
            .collect();
        assert_eq!(reasons.len(), 2, "{reasons:?}");
        assert!(
            reasons.iter().any(|r| r.starts_with("Invalid test path:")),
            "{reasons:?}"
        );
        assert!(
            reasons
                .iter()
                .any(|r| r.starts_with("Test case 'auth/login' is also named by")),
            "{reasons:?}"
        );
    }

    /// A dangling link under `.gleon/manifests/` is named like any foreign entry; the manifests
    /// of the platform directories next to it are still checked.
    #[cfg(unix)]
    #[test]
    fn test_lint_names_a_dangling_link() {
        let temp = tempdir().unwrap();
        let manifests = temp.path().join(".gleon").join("manifests");
        std::fs::create_dir_all(manifests.join("macos-aarch64")).unwrap();
        let valid = include_str!("../../tests/fixtures/valid_manifest.json");
        std::fs::write(manifests.join("macos-aarch64").join("login.json"), valid).unwrap();
        std::os::unix::fs::symlink(temp.path().join("gone"), manifests.join("old-link")).unwrap();

        let ctx =
            ResolvedContext::from_options(&crate::context::ContextOptions::default(), temp.path())
                .unwrap();
        let report = lint_workspace_manifests(&ctx, None).unwrap();
        assert_eq!(
            report.foreign_entries,
            [Path::new(".gleon").join("manifests").join("old-link")]
        );
        assert_eq!((report.total_files, report.valid_files), (1, 1));
    }

    /// `.gleon/manifests` that cannot be listed (here a file) is an error, not an empty lint.
    #[test]
    fn test_lint_manifests_root_that_is_a_file() {
        let temp = tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".gleon")).unwrap();
        std::fs::write(temp.path().join(".gleon").join("manifests"), "").unwrap();
        let ctx =
            ResolvedContext::from_options(&crate::context::ContextOptions::default(), temp.path())
                .unwrap();
        assert!(matches!(
            lint_workspace_manifests(&ctx, None),
            Err(LintError::Io(_))
        ));
    }

    #[test]
    fn test_lint_detects_conflicts_and_corruption() {
        let temp = tempdir().unwrap();
        let manifests_dir = temp
            .path()
            .join(".gleon")
            .join("manifests")
            .join("macos-aarch64");
        std::fs::create_dir_all(&manifests_dir).unwrap();

        let conflicted = include_str!("../../tests/fixtures/conflict_2way.json");
        std::fs::write(manifests_dir.join("conflict.json"), conflicted).unwrap();

        let corrupted = include_str!("../../tests/fixtures/corrupt_manifest.json");
        std::fs::write(manifests_dir.join("corrupt.json"), corrupted).unwrap();

        let ctx =
            ResolvedContext::from_options(&crate::context::ContextOptions::default(), temp.path())
                .unwrap();
        let report = lint_workspace_manifests(&ctx, None).unwrap();

        assert_eq!(report.total_files, 2);
        assert_eq!(report.valid_files, 0);
        assert!(!report.passed);
        assert_eq!(report.conflicted_files.len(), 1);
        assert_eq!(report.corrupted_files.len(), 1);
    }

    #[test]
    fn test_lint_platform_filter_validation_and_missing_dir() {
        let temp = tempdir().unwrap();
        let ctx =
            ResolvedContext::from_options(&crate::context::ContextOptions::default(), temp.path())
                .unwrap();

        // 1. Missing directory
        let err = lint_workspace_manifests(&ctx, None);
        assert!(matches!(err, Err(LintError::ManifestDirNotFound(_))));

        // 2. Traversal platform filter
        let err_traversal = lint_workspace_manifests(&ctx, Some("../../etc"));
        assert!(matches!(
            err_traversal,
            Err(LintError::InvalidPlatformFilter(_))
        ));

        // 3. Absolute path filter
        let err_abs = lint_workspace_manifests(&ctx, Some("/tmp"));
        assert!(matches!(err_abs, Err(LintError::InvalidPlatformFilter(_))));

        // 4. Multi-segment platform filter
        let err_multi = lint_workspace_manifests(&ctx, Some("linux/x86_64"));
        assert!(matches!(
            err_multi,
            Err(LintError::InvalidPlatformFilter(_))
        ));

        // 5. Invalid characters in normal platform filter
        let err_invalid_chars = lint_workspace_manifests(&ctx, Some("linux:x86_64"));
        assert!(matches!(
            err_invalid_chars,
            Err(LintError::InvalidPlatformFilter(_))
        ));
        // A key in another case names the directory of its lowercase key, as `gleon pull` does.
        let err_upper = lint_workspace_manifests(&ctx, Some("LINUX"));
        assert!(
            matches!(&err_upper, Err(LintError::ManifestDirNotFound(dir)) if dir.ends_with("linux")),
            "{err_upper:?}"
        );

        // 6. Valid platform filter on missing directory
        let err_valid = lint_workspace_manifests(&ctx, Some("linux-x86_64"));
        assert!(matches!(err_valid, Err(LintError::ManifestDirNotFound(_))));
    }

    #[cfg(all(unix, not(miri)))]
    #[test]
    fn test_lint_unreadable_file_and_directory() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempdir().unwrap();
        let ctx =
            ResolvedContext::from_options(&crate::context::ContextOptions::default(), temp.path())
                .unwrap();
        let manifests_dir = temp
            .path()
            .join(".gleon")
            .join("manifests")
            .join("linux-x86_64");
        std::fs::create_dir_all(&manifests_dir).unwrap();

        let valid_json = "{}";
        let unreadable_file = manifests_dir.join("unreadable.json");
        std::fs::write(&unreadable_file, valid_json).unwrap();

        // Make file unreadable
        let mut perms = std::fs::metadata(&unreadable_file).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&unreadable_file, perms).unwrap();

        let can_read = std::fs::File::open(&unreadable_file).is_ok();
        let report = lint_workspace_manifests(&ctx, None).unwrap();

        // Restore permissions to allow cleanup before assertions
        let mut perms = std::fs::metadata(&unreadable_file).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&unreadable_file, perms).unwrap();

        if !can_read {
            assert!(!report.passed);
            assert_eq!(report.corrupted_files.len(), 1);
            assert!(report.corrupted_files[0].1.contains("Failed to read file:"));
        }
    }

    #[cfg(all(unix, not(miri)))]
    #[test]
    fn test_lint_unreadable_directory() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempdir().unwrap();
        let ctx =
            ResolvedContext::from_options(&crate::context::ContextOptions::default(), temp.path())
                .unwrap();
        let manifests_dir = temp.path().join(".gleon").join("manifests");
        std::fs::create_dir_all(&manifests_dir).unwrap();

        let sub_dir = manifests_dir.join("sub");
        std::fs::create_dir_all(&sub_dir).unwrap();
        let test_file = sub_dir.join("test.json");
        std::fs::write(&test_file, "{}").unwrap();

        // Make sub_dir unreadable
        let mut perms = std::fs::metadata(&sub_dir).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&sub_dir, perms).unwrap();

        let can_read_dir = std::fs::read_dir(&sub_dir).is_ok();
        let res = lint_workspace_manifests(&ctx, None);

        // Restore permissions to allow cleanup before assertions
        let mut perms = std::fs::metadata(&sub_dir).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&sub_dir, perms).unwrap();

        if !can_read_dir {
            assert!(matches!(res, Err(LintError::Io(_))));
        }
    }

    #[test]
    fn test_lint_detects_json_syntax_and_schema_errors() {
        let temp = tempdir().unwrap();
        let manifests_dir = temp
            .path()
            .join(".gleon")
            .join("manifests")
            .join("macos-aarch64");
        std::fs::create_dir_all(&manifests_dir).unwrap();

        // 1. Invalid JSON syntax
        std::fs::write(manifests_dir.join("syntax.json"), "invalid_json").unwrap();

        // 2. Valid JSON but invalid schema validation (e.g. invalid hash scheme)
        let bad_schema = "{\"schema_version\":1,\"hash\":\"invalid:123\",\"phash\":\"dhash:0000000000000000\",\"width\":10,\"height\":10}";
        std::fs::write(manifests_dir.join("schema.json"), bad_schema).unwrap();

        let ctx =
            ResolvedContext::from_options(&crate::context::ContextOptions::default(), temp.path())
                .unwrap();
        let report = lint_workspace_manifests(&ctx, None).unwrap();

        assert_eq!(report.total_files, 2);
        assert_eq!(report.valid_files, 0);
        assert!(!report.passed);
        assert_eq!(report.corrupted_files.len(), 2);
    }

    #[test]
    fn test_lint_error_display() {
        let err_missing = LintError::ManifestDirNotFound(PathBuf::from("/missing"));
        assert!(err_missing.to_string().contains("does not exist"));

        let err_filter = LintError::InvalidPlatformFilter("bad/filter".to_string());
        assert!(err_filter.to_string().contains("Invalid platform filter"));

        let io_err = std::io::Error::other("test error");
        let err_io = LintError::Io(io_err);
        assert!(err_io.to_string().contains("IO error"));
    }
}
