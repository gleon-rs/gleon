//! Workspace cleanup operation.
//!
//! Purges the temporary `.gleon/runs/` directory and, only when asked
//! ([`CleanOptions::screenshots`]), removes the screenshot PNG files matching rules in
//! `gleon.yaml`, untracks them from the Git index (using `gix`) and appends wildcard entries to
//! `.gitignore`. Screenshots are never touched by default: in an integration's workspace (the
//! Flutter package) the rules match its committed goldens.

use std::path::PathBuf;

use crate::{
    context::ResolvedContext,
    git::GitResolver,
    ops::common::{CoreError, append_missing_gitignore_lines, load_config_and_scan},
};

/// Error types that can occur during the clean operation.
#[derive(Debug, thiserror::Error)]
pub enum CleanError {
    /// Error shared across `ops::*` operations.
    #[error(transparent)]
    Core(#[from] CoreError),
}

/// Options controlling the clean workspace operation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "four independent flags of `gleon clean`, not a state machine"
)]
pub struct CleanOptions {
    /// If true, only simulate the cleanup without modifying disk or Git index.
    pub dry_run: bool,
    /// If true, also delete the screenshots the rules match, untrack them from Git and add the
    /// rules to `.gitignore` (workspaces whose baselines live in manifests). Off by default: an
    /// integration's rules match its committed goldens.
    pub screenshots: bool,
    /// If true, do not update .gitignore.
    pub skip_gitignore: bool,
    /// If true, do not delete the .gleon/runs directory.
    pub keep_runs: bool,
}

/// Result summary of the clean workspace operation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanResult {
    /// List of screenshot files deleted (or to be deleted in dry run).
    pub deleted_files: Vec<PathBuf>,
    /// List of files untracked from Git index (or to be untracked in dry run).
    pub untracked_files: Vec<PathBuf>,
    /// List of entries added to .gitignore (or to be added in dry run).
    pub gitignore_entries_added: Vec<String>,
    /// Whether the runs cache was cleaned.
    pub cache_cleaned: bool,
}

/// Purges the `.gleon/runs` cache and, with [`CleanOptions::screenshots`], cleans the
/// screenshot files: deletes them, untracks them from the Git index and updates `.gitignore`.
///
/// # Errors
///
/// Returns an error if the workspace configuration cannot be scanned, if the `.gitignore` file
/// exists but cannot be read (other than being missing) or cannot be written atomically, or if
/// the `.gleon/runs` cache directory exists but fails to be removed for a reason other than
/// already being absent.
pub fn clean_workspace(
    context: &ResolvedContext,
    options: &CleanOptions,
) -> Result<CleanResult, CleanError> {
    let base_path = context.base_dir.as_path();
    let mut result = CleanResult::default();
    if options.screenshots {
        clean_screenshots(context, options, &mut result)?;
    }

    // The cache directory (.gleon/runs).
    if !options.keep_runs {
        let runs_dir = crate::paths::GleonPaths::new(base_path).runs_root();
        if !options.dry_run {
            match std::fs::remove_dir_all(&runs_dir) {
                Ok(()) => {}
                Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(CoreError::Io(e).into()),
            }
        }
        result.cache_cleaned = true;
        tracing::debug!("Cleaned the .gleon/runs cache directory");
    }

    Ok(result)
}

/// Deletes the screenshots the rules match (pruning emptied directories), untracks them from the
/// Git index and adds the rules to `.gitignore`.
// Genuinely long from three sequential, independent steps (delete+prune, git untrack,
// .gitignore update), not from duplicated logic — see ops/common.rs for the helpers that already
// factor out what *is* shared with other operations.
#[expect(
    clippy::too_many_lines,
    reason = "long by design; see the comment above"
)]
fn clean_screenshots(
    context: &ResolvedContext,
    options: &CleanOptions,
    result: &mut CleanResult,
) -> Result<(), CleanError> {
    let base_path = context.base_dir.as_path();
    let config = context.config.clone().unwrap_or_default();

    // 1. Scan for all screenshots matched by rules in gleon.yaml
    let test_cases = load_config_and_scan(context)?;
    let mut discovered_paths: Vec<PathBuf> = test_cases
        .into_iter()
        .map(|case| case.image.relative_path)
        .collect();

    discovered_paths.sort();
    discovered_paths.dedup();

    tracing::debug!(
        "Discovered {} screenshot(s) matching configuration rules",
        discovered_paths.len()
    );

    // 2. In dry-run mode, populate preview without mutating disk or Git index
    if options.dry_run {
        result.deleted_files.clone_from(&discovered_paths);
        result.untracked_files.clear();
    } else {
        // 3. Delete files from disk and prune empty parent directories
        let mut successfully_deleted = Vec::new();
        let mut affected_dirs = std::collections::HashSet::new();

        for rel_path in &discovered_paths {
            let full_path = base_path.join(rel_path);
            match std::fs::remove_file(&full_path) {
                Ok(()) => {
                    successfully_deleted.push(rel_path.clone());
                    if let Some(parent) = full_path.parent() {
                        affected_dirs.insert(parent.to_path_buf());
                    }

                    // Walk up and prune empty parent directories up to base_path
                    let mut parent = full_path.parent();
                    while let Some(dir) = parent {
                        if dir == base_path || !dir.starts_with(base_path) {
                            break;
                        }
                        if std::fs::remove_dir(dir).is_ok() {
                            if let Some(grandparent) = dir.parent() {
                                affected_dirs.insert(grandparent.to_path_buf());
                            }
                        } else {
                            // Directory not empty or cannot remove, stop pruning upward
                            break;
                        }
                        parent = dir.parent();
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Stale entry already deleted from disk, can still be untracked from Git
                    successfully_deleted.push(rel_path.clone());
                }
                Err(e) => {
                    tracing::warn!("Failed to delete screenshot {:?}: {}", full_path, e);
                }
            }
        }

        // Commit directory entry changes to disk on POSIX systems (deduplicated per dir)
        for dir in affected_dirs {
            if let Ok(dir_file) = std::fs::File::open(dir) {
                let _ = dir_file.sync_all();
            }
        }

        result.deleted_files.clone_from(&successfully_deleted);
        tracing::debug!(
            "Deleted {} screenshot file(s) from disk and pruned empty directories",
            result.deleted_files.len()
        );

        // 4. Untrack successfully deleted paths from Git index (Graceful degradation on Git failure)
        if !successfully_deleted.is_empty() {
            match GitResolver::untrack_from_index(base_path, &successfully_deleted) {
                Ok(untracked) => {
                    tracing::debug!("Untracked {} screenshot(s) from Git index", untracked.len());
                    result.untracked_files = untracked;
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to untrack screenshots from Git index (continuing workspace clean): {e}"
                    );
                    result.untracked_files.clear();
                }
            }
        }
    }

    // 4. Update .gitignore with wildcard entries
    if !options.skip_gitignore {
        let gitignore_path = base_path.join(".gitignore");
        let existing = match std::fs::read_to_string(&gitignore_path) {
            Ok(text) => text,
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(CoreError::Io(e).into()),
        };

        let existing_set: std::collections::HashSet<&str> =
            existing.lines().map(str::trim).collect();
        let mut new_entries = Vec::new();
        let mut seen_new = std::collections::HashSet::new();

        for rule in &config.screenshots {
            for pattern in &rule.include {
                let pat_str = pattern.as_str();
                let entry: std::borrow::Cow<'_, str> = if pat_str.starts_with("**/")
                    || pat_str.contains('/')
                    || pat_str.contains('\\')
                {
                    std::borrow::Cow::Borrowed(pat_str)
                } else {
                    let mut s = String::with_capacity(3 + pat_str.len());
                    s.push_str("**/");
                    s.push_str(pat_str);
                    std::borrow::Cow::Owned(s)
                };

                if !existing_set.contains(entry.as_ref())
                    && !existing_set.contains(pat_str)
                    && seen_new.insert(entry.to_string())
                {
                    new_entries.push(entry.into_owned());
                }
            }
        }

        if !new_entries.is_empty() {
            result.gitignore_entries_added.clone_from(&new_entries);
            if !options.dry_run {
                append_missing_gitignore_lines(&gitignore_path, &new_entries)?;
                tracing::debug!(
                    "Added {} new rule(s) to .gitignore",
                    result.gitignore_entries_added.len()
                );
            }
        }
    }

    Ok(())
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
    use crate::{config::ConfigError, context::ContextError, git::GitError, scanner::ScannerError};

    #[test]
    fn test_clean_workspace_dry_run_and_execution() {
        let temp = tempdir().unwrap();
        let base_path = temp.path();

        // 1. Setup gleon.yaml
        let gleon_dir = base_path.join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();
        std::fs::create_dir_all(gleon_dir.join("runs")).unwrap();

        let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include:
      - "test/goldens/**/*.png"
    mode: pixel
"#;
        std::fs::write(gleon_dir.join("gleon.yaml"), config_yaml).unwrap();

        // Create sample screenshot
        let golden_dir = base_path.join("test").join("goldens");
        std::fs::create_dir_all(&golden_dir).unwrap();
        let golden_file = golden_dir.join("login.png");
        std::fs::write(&golden_file, b"sample png").unwrap();

        let ctx =
            ResolvedContext::from_options(&crate::context::ContextOptions::default(), base_path)
                .unwrap();

        // 2. Test dry-run
        let dry_opts = CleanOptions {
            screenshots: true,
            dry_run: true,
            skip_gitignore: false,
            keep_runs: false,
        };
        let dry_res = clean_workspace(&ctx, &dry_opts).unwrap();
        assert_eq!(dry_res.deleted_files.len(), 1);
        assert_eq!(
            dry_res.deleted_files[0],
            PathBuf::from("test/goldens/login.png")
        );
        assert!(golden_file.exists()); // File still exists after dry run

        // 3. Test actual execution
        let exec_opts = CleanOptions {
            screenshots: true,
            ..CleanOptions::default()
        };
        let exec_res = clean_workspace(&ctx, &exec_opts).unwrap();
        assert_eq!(exec_res.deleted_files.len(), 1);
        assert!(!golden_file.exists()); // File removed
        assert!(!golden_dir.exists()); // Empty parent dir pruned
        assert!(!gleon_dir.join("runs").exists()); // Runs dir removed

        // 4. Verify .gitignore content
        let gitignore = std::fs::read_to_string(base_path.join(".gitignore")).unwrap();
        assert!(gitignore.contains("test/goldens/**/*.png"));
    }

    #[test]
    fn test_clean_workspace_skip_gitignore_and_keep_runs() {
        let temp = tempdir().unwrap();
        let base_path = temp.path();

        let gleon_dir = base_path.join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();
        let runs_dir = gleon_dir.join("runs");
        std::fs::create_dir_all(&runs_dir).unwrap();

        let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include:
      - "test/goldens/**/*.png"
    mode: pixel
"#;
        std::fs::write(gleon_dir.join("gleon.yaml"), config_yaml).unwrap();

        let golden_dir = base_path.join("test").join("goldens");
        std::fs::create_dir_all(&golden_dir).unwrap();
        let golden_file = golden_dir.join("app.png");
        std::fs::write(&golden_file, b"sample png").unwrap();

        let ctx =
            ResolvedContext::from_options(&crate::context::ContextOptions::default(), base_path)
                .unwrap();

        let opts = CleanOptions {
            screenshots: true,
            dry_run: false,
            skip_gitignore: true,
            keep_runs: true,
        };
        let res = clean_workspace(&ctx, &opts).unwrap();
        assert_eq!(res.deleted_files.len(), 1);
        assert!(!golden_file.exists());
        assert!(runs_dir.exists()); // runs preserved
        assert!(!base_path.join(".gitignore").exists()); // .gitignore not created
    }

    #[test]
    fn test_clean_workspace_appends_to_existing_gitignore_without_trailing_newline() {
        let temp = tempdir().unwrap();
        let base_path = temp.path();

        let gleon_dir = base_path.join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();

        let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include:
      - "test/goldens/**/*.png"
    mode: pixel
"#;
        std::fs::write(gleon_dir.join("gleon.yaml"), config_yaml).unwrap();

        // Write existing .gitignore WITHOUT trailing newline
        std::fs::write(base_path.join(".gitignore"), b"target/").unwrap();

        let golden_dir = base_path.join("test").join("goldens");
        std::fs::create_dir_all(&golden_dir).unwrap();
        let golden_file = golden_dir.join("app.png");
        std::fs::write(&golden_file, b"sample png").unwrap();

        let ctx =
            ResolvedContext::from_options(&crate::context::ContextOptions::default(), base_path)
                .unwrap();

        let opts = CleanOptions {
            screenshots: true,
            ..CleanOptions::default()
        };
        let res = clean_workspace(&ctx, &opts).unwrap();
        assert_eq!(res.deleted_files.len(), 1);

        let gitignore = std::fs::read_to_string(base_path.join(".gitignore")).unwrap();
        assert_eq!(gitignore, "target/\ntest/goldens/**/*.png\n");
    }

    #[test]
    fn test_clean_workspace_fails_fast_on_unreadable_gitignore() {
        let temp = tempdir().unwrap();
        let base_path = temp.path();

        let gleon_dir = base_path.join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();

        let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include:
      - "test/goldens/**/*.png"
    mode: pixel
"#;
        std::fs::write(gleon_dir.join("gleon.yaml"), config_yaml).unwrap();

        // Create .gitignore AS A DIRECTORY to force a non-NotFound read error (IsADirectory)
        std::fs::create_dir_all(base_path.join(".gitignore")).unwrap();

        let golden_dir = base_path.join("test").join("goldens");
        std::fs::create_dir_all(&golden_dir).unwrap();
        let golden_file = golden_dir.join("app.png");
        std::fs::write(&golden_file, b"sample png").unwrap();

        let ctx =
            ResolvedContext::from_options(&crate::context::ContextOptions::default(), base_path)
                .unwrap();

        let opts = CleanOptions {
            screenshots: true,
            ..CleanOptions::default()
        };
        let err = clean_workspace(&ctx, &opts).unwrap_err();
        assert!(matches!(err, CleanError::Core(CoreError::Io(_))));
    }

    #[test]
    fn test_clean_error_display() {
        let io_err: CleanError = CoreError::Io(std::io::Error::other("disk full")).into();
        assert!(io_err.to_string().contains("IO error: disk full"));

        let ctx_err: CleanError =
            CoreError::Context(ContextError::Git(GitError::DetachedHead)).into();
        assert!(ctx_err.to_string().contains("Context resolution error:"));

        let cfg_err: CleanError =
            CoreError::Config(ConfigError::NotFound(PathBuf::from("foo"))).into();
        assert!(cfg_err.to_string().contains("Config error:"));

        let scan_err: CleanError = CoreError::Scanner(ScannerError::InvalidTestName {
            name: "bad".to_string(),
            reason: "invalid".to_string(),
        })
        .into();
        assert!(scan_err.to_string().contains("Scanner error:"));

        let res = CleanResult::default();
        let cloned_res = res.clone();
        assert_eq!(res, cloned_res);
        assert_eq!(format!("{res:?}"), format!("{cloned_res:?}"));
    }

    #[test]
    fn test_clean_workspace_bare_vs_scoped_patterns_and_duplicates() {
        let temp = tempdir().unwrap();
        let base_path = temp.path();

        let gleon_dir = base_path.join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();

        let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include:
      - "*.png"
      - "scoped/**/*.png"
      - "**/already_globbed/*.png"
    mode: pixel
"#;
        std::fs::write(gleon_dir.join("gleon.yaml"), config_yaml).unwrap();

        // Write existing .gitignore already containing scoped/**/*.png
        std::fs::write(base_path.join(".gitignore"), "scoped/**/*.png\n").unwrap();

        let root_png = base_path.join("root.png");
        std::fs::write(&root_png, b"png").unwrap();

        let ctx =
            ResolvedContext::from_options(&crate::context::ContextOptions::default(), base_path)
                .unwrap();

        let opts = CleanOptions {
            screenshots: true,
            ..CleanOptions::default()
        };
        let res = clean_workspace(&ctx, &opts).unwrap();
        assert_eq!(res.deleted_files.len(), 1);

        let gitignore = std::fs::read_to_string(base_path.join(".gitignore")).unwrap();
        // Bare pattern "*.png" gets "**/" prepended
        assert!(gitignore.contains("**/*.png"));
        // Already globbed pattern stays as is
        assert!(gitignore.contains("**/already_globbed/*.png"));
        // Existing "scoped/**/*.png" was already in .gitignore and not duplicated
        assert_eq!(gitignore.matches("scoped/**/*.png").count(), 1);
    }

    #[test]
    #[cfg(unix)]
    fn test_clean_workspace_deletion_failure_handled_gracefully() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempdir().unwrap();
        let base_path = temp.path();

        let gleon_dir = base_path.join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();

        let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include: "locked/**/*.png"
    mode: pixel
"#;
        std::fs::write(gleon_dir.join("gleon.yaml"), config_yaml).unwrap();

        let locked_dir = base_path.join("locked");
        std::fs::create_dir_all(&locked_dir).unwrap();
        let locked_file = locked_dir.join("image.png");
        std::fs::write(&locked_file, b"sample png").unwrap();

        // Make parent directory read-only so removing file fails with PermissionDenied
        let orig_perms = std::fs::metadata(&locked_dir).unwrap().permissions();
        let mut read_only = orig_perms.clone();
        read_only.set_mode(0o555);
        std::fs::set_permissions(&locked_dir, read_only).unwrap();

        let ctx =
            ResolvedContext::from_options(&crate::context::ContextOptions::default(), base_path)
                .unwrap();

        let opts = CleanOptions {
            screenshots: true,
            ..CleanOptions::default()
        };
        let res = clean_workspace(&ctx, &opts);

        // Restore permissions before assertions
        std::fs::set_permissions(&locked_dir, orig_perms).unwrap();

        let clean_res = res.unwrap();
        // File could not be deleted, so deleted_files is empty
        assert_eq!(clean_res.deleted_files.len(), 0);
        assert!(locked_file.exists());
    }

    #[test]
    fn test_clean_workspace_prune_non_empty_and_keep_runs() {
        let temp = tempdir().unwrap();
        let base_path = temp.path();

        let gleon_dir = base_path.join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();

        let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include: "nested/**/*.png"
    mode: pixel
"#;
        std::fs::write(gleon_dir.join("gleon.yaml"), config_yaml).unwrap();

        let nested_dir = base_path.join("nested").join("deep");
        std::fs::create_dir_all(&nested_dir).unwrap();
        let screenshot = nested_dir.join("test.png");
        std::fs::write(&screenshot, b"png").unwrap();

        // Other file in nested/deep so directory is not empty after screenshot deletion
        let other_file = nested_dir.join("keep.txt");
        std::fs::write(&other_file, b"keep me").unwrap();

        let ctx =
            ResolvedContext::from_options(&crate::context::ContextOptions::default(), base_path)
                .unwrap();

        let opts = CleanOptions {
            screenshots: true,
            dry_run: false,
            skip_gitignore: false,
            keep_runs: true,
        };
        let res = clean_workspace(&ctx, &opts).unwrap();
        assert_eq!(res.deleted_files.len(), 1);
        assert!(!screenshot.exists());
        assert!(other_file.exists());
        assert!(nested_dir.exists());
        assert!(!res.cache_cleaned);
    }

    #[test]
    #[cfg(unix)]
    fn test_clean_workspace_runs_dir_removal_failure() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempdir().unwrap();
        let base_path = temp.path();

        let gleon_dir = base_path.join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();

        let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include: "*.png"
    mode: pixel
"#;
        std::fs::write(gleon_dir.join("gleon.yaml"), config_yaml).unwrap();

        let runs_dir = gleon_dir.join("runs");
        std::fs::create_dir_all(&runs_dir).unwrap();
        let inner_file = runs_dir.join("artifact.txt");
        std::fs::write(&inner_file, b"content").unwrap();

        // Make runs_dir read-only so remove_dir_all fails
        let orig_perms = std::fs::metadata(&runs_dir).unwrap().permissions();
        let mut read_only = orig_perms.clone();
        read_only.set_mode(0o555);
        std::fs::set_permissions(&runs_dir, read_only).unwrap();

        let probe = runs_dir.join(".probe");
        if std::fs::write(&probe, b"").is_ok() {
            let _ = std::fs::remove_file(&probe);
            std::fs::set_permissions(&runs_dir, orig_perms).unwrap();
            return;
        }

        let ctx =
            ResolvedContext::from_options(&crate::context::ContextOptions::default(), base_path)
                .unwrap();

        let opts = CleanOptions {
            screenshots: true,
            ..CleanOptions::default()
        };
        let res = clean_workspace(&ctx, &opts);

        // Restore permissions before assertions
        std::fs::set_permissions(&runs_dir, orig_perms).unwrap();

        assert!(res.is_err());
        assert!(matches!(
            res.unwrap_err(),
            CleanError::Core(CoreError::Io(_))
        ));
    }

    #[test]
    #[cfg(all(unix, not(miri)))]
    fn test_clean_workspace_git_untrack_failure() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempdir().unwrap();
        let base_path = temp.path();
        let _repo = gix::init(base_path).unwrap();

        let gleon_dir = base_path.join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();

        let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include: "tracked.png"
    mode: pixel
"#;
        std::fs::write(gleon_dir.join("gleon.yaml"), config_yaml).unwrap();

        let screenshot_file = base_path.join("tracked.png");
        std::fs::write(&screenshot_file, b"content").unwrap();

        let git_index_path = base_path.join(".git").join("index");
        let state = gix::index::State::new(gix::hash::Kind::Sha1);
        let mut index = gix::index::File::from_state(state, &git_index_path);
        let rel_path_bstr = "tracked.png";
        index.dangerously_push_entry(
            gix::index::entry::Stat::default(),
            gix::hash::ObjectId::empty_tree(gix::hash::Kind::Sha1),
            gix::index::entry::Flags::empty(),
            gix::index::entry::Mode::FILE,
            rel_path_bstr.as_bytes().into(),
        );
        index.write(gix::index::write::Options::default()).unwrap();

        // Make .git directory read-only so untrack_from_index fails to write lockfile
        let git_dir = base_path.join(".git");
        let orig_perms = std::fs::metadata(&git_dir).unwrap().permissions();
        let mut read_only = orig_perms.clone();
        read_only.set_mode(0o555);
        std::fs::set_permissions(&git_dir, read_only).unwrap();

        let probe = git_dir.join(".probe");
        if std::fs::write(&probe, b"").is_ok() {
            let _ = std::fs::remove_file(&probe);
            std::fs::set_permissions(&git_dir, orig_perms).unwrap();
            return;
        }

        let ctx =
            ResolvedContext::from_options(&crate::context::ContextOptions::default(), base_path)
                .unwrap();

        let opts = CleanOptions {
            screenshots: true,
            dry_run: false,
            skip_gitignore: true,
            keep_runs: true,
        };
        let res = clean_workspace(&ctx, &opts);

        // Restore permissions before assertions
        std::fs::set_permissions(&git_dir, orig_perms).unwrap();

        let clean_res = res.unwrap();
        assert_eq!(clean_res.deleted_files.len(), 1);
        assert!(clean_res.untracked_files.is_empty());
    }
}
