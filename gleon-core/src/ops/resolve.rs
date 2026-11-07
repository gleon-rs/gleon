//! Resolution operation for handling Git merge conflicts in manifest JSON files.

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::{
    io::{IoError, save_json_atomically},
    manifest::{ConflictManifest, SingleTestManifest, parse_conflict_manifest},
    ops::{common::platform_filter_dir, sync::read_manifests_root},
    paths::GleonPaths,
};

/// Errors that can occur during conflict resolution scan or application.
#[derive(Debug, Error)]
pub enum ResolveError {
    /// Target manifest directory was not found.
    #[error("Manifest directory does not exist at {0}")]
    ManifestDirNotFound(PathBuf),

    /// A platform filter that is no platform key.
    #[error("Invalid platform filter '{0}': must be a platform key")]
    InvalidPlatformFilter(String),

    /// Standard I/O error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Failed to save resolved manifest file.
    #[error("Failed to save resolved manifest to {0}: {1}")]
    Save(PathBuf, #[source] IoError),
}

/// Represents a conflicted manifest file discovered during scanning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictedManifestItem {
    /// Relative test path (e.g. "`auth/login_screen`").
    pub test_path: String,
    /// Platform component (e.g. "macos-aarch64").
    pub platform: String,
    /// Absolute path to the conflicted manifest file.
    pub manifest_file_path: PathBuf,
    /// Parsed conflict details (`ours` and `theirs`).
    pub conflict: ConflictManifest,
}

/// Scans the platform directories of `.gleon/manifests/` for files with Git conflict markers.
///
/// Only the directory of `platform_filter` is scanned when it is given. Other entries of the root
/// (a directory of an older key format, a stray file) hold no manifests gleon reads, as
/// `gleon lint` says, so they are not scanned.
///
/// A file whose conflict markers cannot be parsed, or an entry that cannot be read while walking,
/// is skipped with a warning.
///
/// # Errors
/// Returns [`ResolveError::InvalidPlatformFilter`] for a filter that is no platform key,
/// [`ResolveError::ManifestDirNotFound`] if the directory to scan is missing, or
/// [`ResolveError::Io`] if `.gleon/manifests/` or a manifest file cannot be read.
pub fn scan_conflicts(
    base_dir: &Path,
    platform_filter: Option<&str>,
) -> Result<Vec<ConflictedManifestItem>, ResolveError> {
    let manifests_root = GleonPaths::new(base_dir).manifests_root();

    let search_dirs = if let Some(filter) = platform_filter {
        let search_dir = platform_filter_dir(&manifests_root, filter)
            .map_err(|p| ResolveError::InvalidPlatformFilter(p.to_string()))?;
        if std::fs::metadata(&search_dir).is_err() {
            return Err(ResolveError::ManifestDirNotFound(search_dir));
        }
        vec![search_dir]
    } else {
        let root = read_manifests_root(&manifests_root).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ResolveError::ManifestDirNotFound(manifests_root.clone())
            } else {
                ResolveError::Io(e)
            }
        })?;
        root.platforms.into_iter().map(|(_, dir)| dir).collect()
    };

    let mut items = Vec::new();

    for entry_res in search_dirs
        .iter()
        .flat_map(|dir| crate::walk::manifest_walker(dir).build())
    {
        let entry = match entry_res {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!("Skipping unreadable manifest entry: {}", err);
                continue;
            }
        };

        let path = entry.path();
        if entry.file_type().is_some_and(|ft| ft.is_file())
            && path.extension().is_some_and(|ext| ext == "json")
        {
            let content = std::fs::read_to_string(path)?;

            if content.contains("<<<<<<<") {
                let conflict = match parse_conflict_manifest(&content) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(
                            "Skipping unparsable conflicted manifest at {}: {}",
                            path.display(),
                            e
                        );
                        continue;
                    }
                };

                let rel = path.strip_prefix(&manifests_root).unwrap_or(path);
                let mut components = rel.components().filter_map(|c| match c {
                    std::path::Component::Normal(s) => s.to_str(),
                    _ => None,
                });

                let platform = components.next().unwrap_or_default().to_string();

                let mut test_path = String::new();
                for (i, segment) in components.enumerate() {
                    if i > 0 {
                        test_path.push('/');
                    }
                    test_path.push_str(segment);
                }
                let test_path_clean = test_path
                    .strip_suffix(".json")
                    .unwrap_or(&test_path)
                    .to_string();

                items.push(ConflictedManifestItem {
                    test_path: test_path_clean,
                    platform,
                    manifest_file_path: path.to_path_buf(),
                    conflict,
                });
            }
        }
    }

    Ok(items)
}

/// Applies a resolution choice to a conflicted manifest file, writing the chosen manifest atomically.
///
/// # Errors
/// Returns [`ResolveError`] if atomic serialization/saving fails.
pub fn apply_resolution(
    manifest_file_path: &Path,
    chosen: &SingleTestManifest,
) -> Result<(), ResolveError> {
    save_json_atomically(manifest_file_path, chosen)
        .map_err(|e| ResolveError::Save(manifest_file_path.to_path_buf(), e))
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
    fn test_scan_and_apply_resolution() {
        let temp = tempdir().unwrap();
        let manifests_dir = temp
            .path()
            .join(".gleon")
            .join("manifests")
            .join("macos-aarch64")
            .join("auth");
        std::fs::create_dir_all(&manifests_dir).unwrap();

        let conflicted = include_str!("../../tests/fixtures/conflict_2way.json");

        let login_path = manifests_dir.join("login.json");
        std::fs::write(&login_path, conflicted).unwrap();

        let conflicts = scan_conflicts(temp.path(), None).expect("Scan failed");
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].test_path, "auth/login");
        assert_eq!(conflicts[0].platform, "macos-aarch64");

        // Apply resolution choosing 'theirs'
        apply_resolution(&login_path, &conflicts[0].conflict.theirs).expect("Apply failed");

        let updated = std::fs::read_to_string(&login_path).unwrap();
        assert!(!updated.contains("<<<<<<<"));
        assert!(
            updated.contains(
                "sha256:2222222222222222222222222222222222222222222222222222222222222222"
            )
        );
    }

    #[test]
    fn test_scan_conflicts_missing_dir_and_unparseable() {
        let temp = tempdir().unwrap();

        // Missing manifest dir
        let err = scan_conflicts(temp.path(), None);
        assert!(matches!(err, Err(ResolveError::ManifestDirNotFound(_))));

        // Unparseable conflict manifest file
        let manifests_dir = temp
            .path()
            .join(".gleon")
            .join("manifests")
            .join("macos-aarch64");
        std::fs::create_dir_all(&manifests_dir).unwrap();
        let bad_conflict = "<<<<<<< HEAD\ninvalid_json\n=======\ninvalid_json\n>>>>>>> branch";
        std::fs::write(manifests_dir.join("bad.json"), bad_conflict).unwrap();

        let conflicts = scan_conflicts(temp.path(), None).unwrap();
        assert!(conflicts.is_empty());
    }

    /// Only platform directories hold manifests gleon reads: a conflicted file in a directory of
    /// an older key format or at the root of `.gleon/manifests/` is never offered (it would
    /// resolve a file gleon ignores, a stray one with an empty test path).
    #[test]
    fn test_scan_conflicts_reads_platform_directories_only() {
        let temp = tempdir().unwrap();
        let manifests = temp.path().join(".gleon").join("manifests");
        let conflicted = include_str!("../../tests/fixtures/conflict_2way.json");
        for dir in ["macos-aarch64/auth", "macos aarch64/auth"] {
            std::fs::create_dir_all(manifests.join(dir)).unwrap();
            std::fs::write(manifests.join(dir).join("login.json"), conflicted).unwrap();
        }
        std::fs::write(manifests.join("stray.json"), conflicted).unwrap();

        let conflicts = scan_conflicts(temp.path(), None).unwrap();
        let found: Vec<_> = conflicts
            .iter()
            .map(|item| (item.platform.as_str(), item.test_path.as_str()))
            .collect();
        assert_eq!(found, [("macos-aarch64", "auth/login")]);

        // `.gleon/manifests` that cannot be listed (here a file) is an error.
        std::fs::remove_dir_all(&manifests).unwrap();
        std::fs::write(&manifests, "").unwrap();
        assert!(matches!(
            scan_conflicts(temp.path(), None),
            Err(ResolveError::Io(_))
        ));
    }

    #[test]
    fn test_scan_conflicts_platform_filter_validation() {
        let temp = tempdir().unwrap();

        // 1. Traversal filter
        let err_traversal = scan_conflicts(temp.path(), Some("../../etc"));
        assert!(matches!(
            err_traversal,
            Err(ResolveError::InvalidPlatformFilter(_))
        ));

        // 2. Absolute filter
        let err_abs = scan_conflicts(temp.path(), Some("/tmp"));
        assert!(matches!(
            err_abs,
            Err(ResolveError::InvalidPlatformFilter(_))
        ));

        // 3. Multi-segment filter
        let err_multi = scan_conflicts(temp.path(), Some("macos/aarch64"));
        assert!(matches!(
            err_multi,
            Err(ResolveError::InvalidPlatformFilter(_))
        ));

        // A valid filter of a platform with no manifests directory.
        let err_missing = scan_conflicts(temp.path(), Some("linux-x86_64"));
        assert!(matches!(
            err_missing,
            Err(ResolveError::ManifestDirNotFound(_))
        ));

        // 4. Positive valid single platform filter case
        let manifests_dir = temp
            .path()
            .join(".gleon")
            .join("manifests")
            .join("macos-aarch64")
            .join("auth");
        std::fs::create_dir_all(&manifests_dir).unwrap();
        let conflicted = include_str!("../../tests/fixtures/conflict_2way.json");
        std::fs::write(manifests_dir.join("login.json"), conflicted).unwrap();

        let conflicts = scan_conflicts(temp.path(), Some("macos-aarch64")).unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].test_path, "auth/login");
        assert_eq!(conflicts[0].platform, "macos-aarch64");
    }
}
