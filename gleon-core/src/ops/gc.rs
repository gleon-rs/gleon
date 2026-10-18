//! Remote storage garbage collector orchestration for pruning unreferenced baseline blobs.

use std::{collections::HashSet, path::Path};

use chrono::{DateTime, Duration, Utc};
use tracing::{info, instrument, warn};

pub use crate::storage::RemoteBlobEntry;
use crate::{
    context::ResolvedContext,
    manifest::ImageHash,
    ops::common::{CoreError, ensure_initialized},
    storage::{
        StorageError,
        adapter::{ObjectStoreAdapter, StorageConfig},
    },
};

/// Errors that can occur during storage garbage collection.
#[derive(Debug, thiserror::Error)]
pub enum GcError {
    /// Remote storage operation failed.
    #[error("Storage error: {0}")]
    Storage(#[from] StorageError),

    /// Manifest or workspace operation failed.
    #[error(transparent)]
    Core(#[from] CoreError),

    /// Git repository discovery failed when remote storage is configured.
    #[error(
        "Remote storage garbage collection requires a Git repository to resolve active branch manifests. Run inside a Git repository or pass --force."
    )]
    GitRequired,

    /// Repository is a shallow clone lacking full history.
    #[error(
        "Git repository is a shallow clone; commit history is incomplete. Baseline blobs of other branches would be permanently deleted. Run 'git fetch --unshallow' (or configure 'fetch-depth: 0' in CI), or pass --force."
    )]
    ShallowClone,

    /// Repository contains too few unique commits across tracked references to guarantee safe branch discovery.
    #[error(
        "Repository contains only {0} unique commit(s) across tracked references. Remote branches may not be fetched, risking baseline data loss. Fetch all branches or pass --force."
    )]
    InsufficientCommits(usize),

    /// Grace period specified is less than the 24-hour minimum.
    #[error(
        "Grace period must be at least 24 hours to prevent race conditions with concurrent uploads."
    )]
    GracePeriodTooShort,

    /// Git tree traversal encountered unrecoverable errors.
    #[error(
        "Git tree traversal encountered {failures} error(s). Halting garbage collection to prevent deleting valid baselines. Fix Git repository state or pass --force."
    )]
    UnsafeScan {
        /// Number of unreadable Git commits, trees, or test manifests.
        failures: usize,
    },

    /// Failed to resolve relative path in monorepo subfolder.
    #[error("Failed to resolve monorepo subfolder relative to repository root: '{0}'")]
    MonorepoResolutionFailed(String),
}

/// Operating mode of a completed garbage collection run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcMode {
    /// Local flat mode; remote storage is not configured so no remote operations occurred.
    LocalMode,
    /// Dry-run mode; orphan blobs were identified and reported without deletion.
    DryRun,
    /// Execution mode; orphan blobs were deleted from remote storage.
    Executed,
}

/// Options controlling storage garbage collection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcOptions {
    /// When true, reports what would be deleted without actually deleting blobs.
    pub dry_run: bool,
    /// Minimum age required before an unreferenced blob is eligible for deletion.
    pub grace_period: Duration,
    /// When true, bypasses safety checks (shallow clone, single ref, git traversal errors, non-git).
    pub force: bool,
}

impl GcOptions {
    /// Creates a new `GcOptions` with `dry_run`, `grace_period_hours`, and `force`.
    #[must_use]
    pub fn new(dry_run: bool, grace_period_hours: u32, force: bool) -> Self {
        Self {
            dry_run,
            grace_period: Duration::hours(i64::from(grace_period_hours)),
            force,
        }
    }
}

impl Default for GcOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            grace_period: Duration::hours(24),
            force: false,
        }
    }
}

/// Results of a garbage collection operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcResult {
    /// Execution mode.
    pub mode: GcMode,
    /// Total number of blobs found in remote storage.
    pub total_remote_blobs: usize,
    /// Number of remote blobs actively referenced by at least one manifest.
    pub referenced_blobs: usize,
    /// Number of unreferenced blobs protected by the grace period window.
    pub protected_by_grace_period: usize,
    /// Number of orphan blobs deleted (or identified for deletion in dry-run mode).
    pub deleted_blobs: usize,
    /// Number of orphan blobs that failed deletion.
    pub failed_blobs: usize,
    /// Total bytes freed (or identified to be freed in dry-run mode).
    pub bytes_freed: u64,
    /// List of orphan blob entries identified.
    pub orphans: Vec<RemoteBlobEntry>,
}

impl Default for GcResult {
    fn default() -> Self {
        Self {
            mode: GcMode::LocalMode,
            total_remote_blobs: 0,
            referenced_blobs: 0,
            protected_by_grace_period: 0,
            deleted_blobs: 0,
            failed_blobs: 0,
            bytes_freed: 0,
            orphans: Vec::new(),
        }
    }
}

/// Pure partitioning logic to divide remote blobs into orphans, protected, and referenced.
///
/// Every remote blob falls into exactly one category:
/// 1. Referenced: present in `referenced_hashes`.
/// 2. Protected by grace period: unreferenced, but `now - last_modified < grace_period`.
///    (Note: if `last_modified > now`, `now - last_modified` is negative, which is `< grace_period`,
///    so clock skew is naturally protected without special-casing).
/// 3. Orphan: unreferenced and older than `grace_period`.
#[must_use]
pub fn partition_remote_blobs<S: std::hash::BuildHasher>(
    remote_blobs: Vec<RemoteBlobEntry>,
    referenced_hashes: &HashSet<ImageHash, S>,
    now: DateTime<Utc>,
    grace_period: Duration,
) -> (Vec<RemoteBlobEntry>, usize, usize) {
    let mut orphans = Vec::new();
    let mut protected_by_grace_period = 0;
    let mut referenced_blobs = 0;

    for blob in remote_blobs {
        if referenced_hashes.contains(&blob.hash) {
            referenced_blobs += 1;
        } else {
            let age = now.signed_duration_since(blob.last_modified);
            if age < grace_period {
                protected_by_grace_period += 1;
            } else {
                orphans.push(blob);
            }
        }
    }

    (orphans, protected_by_grace_period, referenced_blobs)
}

/// Backward-compatible wrapper filtering borrowed slices of blobs.
#[must_use]
pub fn filter_orphans_to_delete<'a, S: std::hash::BuildHasher>(
    remote_blobs: &'a [RemoteBlobEntry],
    referenced_hashes: &HashSet<ImageHash, S>,
    now: DateTime<Utc>,
    grace_period: Duration,
) -> (Vec<&'a RemoteBlobEntry>, usize) {
    let mut to_delete = Vec::new();
    let mut protected_by_grace_period = 0;

    for blob in remote_blobs {
        if referenced_hashes.contains(&blob.hash) {
            continue;
        }

        let age = now.signed_duration_since(blob.last_modified);
        if age < grace_period {
            protected_by_grace_period += 1;
        } else {
            to_delete.push(blob);
        }
    }

    (to_delete, protected_by_grace_period)
}

#[derive(serde::Deserialize)]
struct ManifestHashOnly {
    hash: ImageHash,
}

/// Scans the manifest directory in the tree of the given commit, recording all
/// referenced `ImageHash` values.
fn scan_commit_manifests(
    repo: &gix::Repository,
    commit: &gix::Commit<'_>,
    manifests_path: &str,
    visited_trees: &mut HashSet<gix::ObjectId>,
    referenced_hashes: &mut HashSet<ImageHash>,
    scan_failures: &mut usize,
) {
    let Ok(tree) = commit.tree() else {
        *scan_failures += 1;
        return;
    };
    let Ok(entry_opt) = tree.lookup_entry_by_path(manifests_path) else {
        *scan_failures += 1;
        return;
    };
    let Some(entry) = entry_opt else {
        // Normal case: commit does not contain this manifest path (e.g. branch created before gleon)
        return;
    };
    let Ok(obj) = entry.object() else {
        *scan_failures += 1;
        return;
    };
    let Ok(manifest_tree) = obj.try_into_tree() else {
        *scan_failures += 1;
        return;
    };

    // Optimization: avoid re-scanning duplicate manifest trees across branches
    if !visited_trees.insert(manifest_tree.id) {
        return;
    }

    let mut recorder = gix::traverse::tree::Recorder::default();
    if manifest_tree
        .traverse()
        .breadthfirst(&mut recorder)
        .is_err()
    {
        *scan_failures += 1;
        return;
    }

    for record in recorder.records {
        if record.mode.is_blob() && record.filepath.ends_with(b".json") {
            let manifest_opt = repo
                .find_object(record.oid)
                .ok()
                .and_then(|blob| serde_json::from_slice::<ManifestHashOnly>(&blob.data).ok());

            if let Some(m) = manifest_opt {
                referenced_hashes.insert(m.hash);
            } else {
                *scan_failures += 1;
            }
        }
    }
}

fn resolve_manifests_git_path(base_dir: &Path, repo: &gix::Repository) -> Result<String, GcError> {
    repo.workdir().map_or_else(
        || Ok(".gleon/manifests".to_string()),
        |workdir| {
            let canonical_base = base_dir
                .canonicalize()
                .unwrap_or_else(|_| base_dir.to_path_buf());
            let canonical_work = workdir
                .canonicalize()
                .unwrap_or_else(|_| workdir.to_path_buf());
            let rel_base = match canonical_base.strip_prefix(&canonical_work) {
                Ok(p) => p,
                Err(_) => match base_dir.strip_prefix(workdir) {
                    Ok(p) => p,
                    Err(e) => {
                        return Err(GcError::MonorepoResolutionFailed(e.to_string()));
                    }
                },
            };
            let rel_str = rel_base.to_string_lossy();
            let normalized_rel = crate::naming::normalize_path_separators(&rel_str);
            if normalized_rel.is_empty() {
                Ok(".gleon/manifests".to_string())
            } else {
                Ok(format!("{normalized_rel}/.gleon/manifests"))
            }
        },
    )
}

fn scan_all_tracked_refs(
    repo: &gix::Repository,
    manifests_git_path: &str,
    visited_trees: &mut HashSet<gix::ObjectId>,
    referenced_hashes: &mut HashSet<ImageHash>,
    scan_failures: &mut usize,
) -> usize {
    let mut seen_commits = HashSet::new();
    let Ok(platform) = repo.references() else {
        *scan_failures += 1;
        return 0;
    };

    let Ok(all_refs) = platform.all() else {
        *scan_failures += 1;
        return 0;
    };

    for reference_res in all_refs {
        let Ok(reference) = reference_res else {
            *scan_failures += 1;
            continue;
        };
        let name = reference.name().as_bstr();
        let is_tracked = name.starts_with(b"refs/heads/")
            || name.starts_with(b"refs/remotes/")
            || name.starts_with(b"refs/tags/")
            || name.starts_with(b"refs/pull/");
        if !is_tracked {
            continue;
        }

        let Some(commit) = reference
            .into_fully_peeled_id()
            .ok()
            .and_then(|id| repo.find_object(id).ok())
            .and_then(|obj| obj.peel_to_commit().ok())
        else {
            *scan_failures += 1;
            continue;
        };

        if !seen_commits.insert(commit.id().detach()) {
            continue;
        }

        scan_commit_manifests(
            repo,
            &commit,
            manifests_git_path,
            visited_trees,
            referenced_hashes,
            scan_failures,
        );
    }
    seen_commits.len()
}

/// Collects all image hashes referenced across local workspace manifests
/// AND all discoverable Git branch/tag commits (refs/heads/*, refs/remotes/*, refs/tags/*, refs/pull/*, and HEAD).
///
/// # Errors
/// Returns [`GcError`] if Git repository discovery, branch iteration, or manifest parsing fails.
#[instrument(skip(base_dir, options), level = "debug")]
pub fn collect_all_referenced_hashes(
    base_dir: &Path,
    options: &GcOptions,
) -> Result<HashSet<ImageHash>, GcError> {
    let manifests_root = base_dir.join(".gleon").join("manifests");
    let local_platform_dirs =
        crate::ops::sync::list_platform_dirs(&manifests_root).map_err(CoreError::Io)?;
    let local_referenced = crate::ops::sync::collect_referenced_hashes(&local_platform_dirs)?;
    let mut referenced_hashes: HashSet<ImageHash> = local_referenced.into_keys().collect();

    let Ok(repo) = gix::discover(base_dir) else {
        if !options.force && !options.dry_run {
            return Err(GcError::GitRequired);
        }
        warn!(
            location = %base_dir.display(),
            "No Git repository discovered; operating with local workspace manifests only"
        );
        return Ok(referenced_hashes);
    };

    // An unreadable shallow-file config counts as shallow: refusing is the safe default.
    let is_shallow = repo.is_shallow().unwrap_or(true);
    if is_shallow {
        if !options.force && !options.dry_run {
            return Err(GcError::ShallowClone);
        }
        warn!(
            "Git repository is a shallow clone; commit history is incomplete. \
             Remote baseline blobs referenced only by older commits or un-fetched branches \
             may be collected as orphans. Consider configuring 'fetch-depth: 0' in CI."
        );
    }

    let manifests_git_path = resolve_manifests_git_path(base_dir, &repo)?;

    let mut visited_trees = HashSet::new();
    let mut scan_failures = 0;

    if let Ok(head_commit) = repo.head_commit() {
        scan_commit_manifests(
            &repo,
            &head_commit,
            &manifests_git_path,
            &mut visited_trees,
            &mut referenced_hashes,
            &mut scan_failures,
        );
    }

    let unique_commits_count = scan_all_tracked_refs(
        &repo,
        &manifests_git_path,
        &mut visited_trees,
        &mut referenced_hashes,
        &mut scan_failures,
    );

    if unique_commits_count <= 1 && !options.force && !options.dry_run {
        return Err(GcError::InsufficientCommits(unique_commits_count));
    }

    if scan_failures > 0 {
        if !options.force && !options.dry_run {
            return Err(GcError::UnsafeScan {
                failures: scan_failures,
            });
        }
        warn!(
            failures = scan_failures,
            "Git tree traversal encountered errors; continuing due to --force or --dry-run"
        );
    }

    let total_referenced = referenced_hashes.len();
    let scanned_trees = visited_trees.len();
    info!(
        total_referenced,
        scanned_trees,
        unique_commits = unique_commits_count,
        "Collected referenced blob hashes from workspace and Git branches"
    );

    Ok(referenced_hashes)
}

/// Orchestrates remote storage garbage collection.
///
/// 1. Verifies storage configuration (returns early with `mode = LocalMode` if not configured).
/// 2. Enforces safety guardrails (grace period >= 24h, uninitialized workspace check).
/// 3. Captures current timestamp `now` before querying remote storage.
/// 4. Lists all remote CAS blobs under `blobs/`.
/// 5. Collects referenced hashes across local workspace and Git branches.
/// 6. Partitions remote blobs into orphans, protected, and referenced.
/// 7. Batch deletes orphan blobs in parallel via `delete_stream` (unless `dry_run` is requested).
///
/// # Errors
/// Returns [`GcError`] if safety preconditions fail or remote storage operations error.
#[instrument(skip(context, storage_config), level = "info")]
pub async fn garbage_collect(
    context: &ResolvedContext,
    storage_config: Option<&StorageConfig>,
    options: &GcOptions,
) -> Result<GcResult, GcError> {
    let Some(storage_cfg) = crate::ops::sync::active_storage_config(storage_config) else {
        return Ok(GcResult {
            mode: GcMode::LocalMode,
            ..Default::default()
        });
    };

    // Fail fast if workspace is not initialized
    ensure_initialized(&context.base_dir)?;

    // Enforce grace period guardrail
    if options.grace_period < Duration::hours(24) {
        return Err(GcError::GracePeriodTooShort);
    }

    let adapter = ObjectStoreAdapter::from_config(storage_cfg).map_err(GcError::Storage)?;

    // Safe Ordering:
    // 1. Capture current timestamp before listing remote blobs
    let now = Utc::now();

    // 2. Query remote blobs
    let remote_blobs = adapter.list_all_blobs().await.map_err(GcError::Storage)?;
    let total_remote_blobs = remote_blobs.len();

    // 3. Collect all referenced hashes across local workspace & Git branches
    let referenced_hashes = collect_all_referenced_hashes(&context.base_dir, options)?;

    // 4. Partition remote blobs
    let (orphans, protected_by_grace_period, referenced_blobs) =
        partition_remote_blobs(remote_blobs, &referenced_hashes, now, options.grace_period);

    let orphan_bytes: u64 = orphans.iter().map(|b| b.size).sum();
    let orphan_count = orphans.len();

    if options.dry_run {
        info!(
            total = total_remote_blobs,
            referenced = referenced_blobs,
            protected = protected_by_grace_period,
            orphans = orphan_count,
            bytes = orphan_bytes,
            "[DRY RUN] Identified orphan blobs for deletion"
        );
        return Ok(GcResult {
            mode: GcMode::DryRun,
            total_remote_blobs,
            referenced_blobs,
            protected_by_grace_period,
            deleted_blobs: orphan_count,
            failed_blobs: 0,
            bytes_freed: orphan_bytes,
            orphans,
        });
    }

    // 5. Perform batch deletion via delete_blobs_with_progress
    let (deleted_count, failed_count, bytes_freed) = if orphans.is_empty() {
        (0, 0, 0)
    } else {
        let summary = adapter
            .delete_blobs_with_progress(orphans.iter().map(|b| &b.hash), |hash, success| {
                if !success {
                    warn!(hash = %hash, "Failed to delete orphan blob during batch deletion");
                }
            })
            .await
            .map_err(GcError::Storage)?;

        let bytes_freed: u64 = orphans
            .iter()
            .filter(|b| summary.deleted_hashes.contains(&b.hash))
            .map(|b| b.size)
            .sum();

        (summary.deleted, summary.failed, bytes_freed)
    };

    info!(
        total = total_remote_blobs,
        referenced = referenced_blobs,
        protected = protected_by_grace_period,
        deleted = deleted_count,
        failed = failed_count,
        bytes = bytes_freed,
        "Storage garbage collection completed"
    );

    Ok(GcResult {
        mode: GcMode::Executed,
        total_remote_blobs,
        referenced_blobs,
        protected_by_grace_period,
        deleted_blobs: deleted_count,
        failed_blobs: failed_count,
        bytes_freed,
        orphans,
    })
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

    fn commit_test_tree<'a>(
        repo: &'a gix::Repository,
        reference: &str,
        message: &str,
        tree_id: impl Into<gix::ObjectId>,
        parents: impl IntoIterator<Item = impl Into<gix::ObjectId>>,
    ) -> gix::Id<'a> {
        let sig =
            gix::actor::SignatureRef::from_bytes(b"Gleon Test <test@gleon.dev> 1700000000 +0000")
                .expect("valid test signature");
        repo.commit_as(sig, sig, reference, message, tree_id, parents)
            .expect("successful commit in test")
    }

    #[test]
    fn test_resolve_manifests_git_path_bare_and_outside_workdir() {
        let temp = tempdir().unwrap();
        let bare_repo = gix::init_bare(temp.path().join("bare.git")).unwrap();
        let path = resolve_manifests_git_path(temp.path(), &bare_repo).unwrap();
        assert_eq!(path, ".gleon/manifests");

        let normal_temp = tempdir().unwrap();
        let normal_repo = gix::init(normal_temp.path()).unwrap();
        let fallback_path = resolve_manifests_git_path(
            &normal_temp.path().join("non_existent_subdir"),
            &normal_repo,
        )
        .unwrap();
        assert_eq!(fallback_path, "non_existent_subdir/.gleon/manifests");

        let other_temp = tempdir().unwrap();
        let err = resolve_manifests_git_path(other_temp.path(), &normal_repo).unwrap_err();
        assert!(matches!(err, GcError::MonorepoResolutionFailed(_)));
    }

    #[test]
    fn test_scan_commit_manifests_and_refs_failure_branches() {
        let temp = tempdir().unwrap();
        let repo_root = temp.path();
        let repo = gix::init(repo_root).unwrap();

        let sig =
            gix::actor::SignatureRef::from_bytes(b"Gleon Test <test@gleon.dev> 1700000000 +0000")
                .unwrap();

        // 1. Commit with missing tree object (ODB error on commit.tree()) -> lines 223-224
        let fake_id = gix::ObjectId::from_hex(b"1111111111111111111111111111111111111111").unwrap();
        let bad_commit_obj = gix::objs::Commit {
            tree: fake_id,
            parents: vec![].into(),
            author: sig.to_owned().expect("signature"),
            committer: sig.to_owned().expect("signature"),
            encoding: None,
            message: "bad tree commit".into(),
            extra_headers: vec![],
        };
        let bad_commit_id = repo.write_object(&bad_commit_obj).unwrap();

        // 2. Tree where .gleon/manifests is a blob instead of a tree -> lines 239-240
        let blob_data = b"i am a blob, not a tree directory";
        let blob_id = repo.write_blob(blob_data).unwrap();

        let mut gleon_tree_obj = gix::objs::Tree::empty();
        gleon_tree_obj.entries.push(gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Blob.into(),
            filename: "manifests".into(),
            oid: blob_id.detach(),
        });
        let gleon_tree_id = repo.write_object(&gleon_tree_obj).unwrap();

        let mut root_tree_obj = gix::objs::Tree::empty();
        root_tree_obj.entries.push(gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Tree.into(),
            filename: ".gleon".into(),
            oid: gleon_tree_id.detach(),
        });
        let root_tree_id = repo.write_object(&root_tree_obj).unwrap();

        let blob_manifests_commit = commit_test_tree(
            &repo,
            "refs/heads/blob-manifests",
            "blob manifests",
            root_tree_id,
            std::iter::empty::<gix::ObjectId>(),
        );

        // 3. Tree where entry .object() fails because OID does not exist in ODB -> lines 235-236
        let mut missing_gleon_tree_obj = gix::objs::Tree::empty();
        missing_gleon_tree_obj.entries.push(gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Tree.into(),
            filename: "manifests".into(),
            oid: fake_id,
        });
        let missing_gleon_tree_id = repo.write_object(&missing_gleon_tree_obj).unwrap();

        let mut missing_root_tree_obj = gix::objs::Tree::empty();
        missing_root_tree_obj.entries.push(gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Tree.into(),
            filename: ".gleon".into(),
            oid: missing_gleon_tree_id.detach(),
        });
        let missing_root_tree_id = repo.write_object(&missing_root_tree_obj).unwrap();

        let missing_obj_commit = commit_test_tree(
            &repo,
            "refs/heads/missing-obj-manifests",
            "missing obj manifests",
            missing_root_tree_id,
            std::iter::empty::<gix::ObjectId>(),
        );

        // 4. Tree lookup error (corrupt sub-tree in path) -> lines 227-228
        let mut corrupt_path_root = gix::objs::Tree::empty();
        corrupt_path_root.entries.push(gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Tree.into(),
            filename: ".gleon".into(),
            oid: fake_id,
        });
        let corrupt_path_root_id = repo.write_object(&corrupt_path_root).unwrap();

        let corrupt_path_commit = commit_test_tree(
            &repo,
            "refs/heads/corrupt-path",
            "corrupt path",
            corrupt_path_root_id,
            std::iter::empty::<gix::ObjectId>(),
        );

        // 5. Corrupt JSON in manifest blob -> lines 264-265 & missing blob in tree -> lines 266-268
        let bad_json_blob_id = repo.write_blob(b"not valid json").unwrap();
        let mut manifests_sub_obj = gix::objs::Tree::empty();
        manifests_sub_obj.entries.push(gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Blob.into(),
            filename: "manifest.json".into(),
            oid: bad_json_blob_id.detach(),
        });
        manifests_sub_obj.entries.push(gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Blob.into(),
            filename: "missing.json".into(),
            oid: fake_id,
        });
        manifests_sub_obj.entries.push(gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Blob.into(),
            filename: "ignored.txt".into(),
            oid: bad_json_blob_id.detach(),
        });
        manifests_sub_obj.entries.sort();
        let manifests_sub_id = repo.write_object(&manifests_sub_obj).unwrap();

        let mut valid_gleon_obj = gix::objs::Tree::empty();
        valid_gleon_obj.entries.push(gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Tree.into(),
            filename: "manifests".into(),
            oid: manifests_sub_id.detach(),
        });
        let valid_gleon_id = repo.write_object(&valid_gleon_obj).unwrap();

        let mut valid_root_obj = gix::objs::Tree::empty();
        valid_root_obj.entries.push(gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Tree.into(),
            filename: ".gleon".into(),
            oid: valid_gleon_id.detach(),
        });
        let valid_root_id = repo.write_object(&valid_root_obj).unwrap();

        let corrupt_json_commit = commit_test_tree(
            &repo,
            "refs/heads/corrupt-json",
            "corrupt json commit",
            valid_root_id,
            std::iter::empty::<gix::ObjectId>(),
        );

        // 6. Manifest tree with corrupt sub-tree causing traverse().breadthfirst() to fail -> lines 254-255
        let mut traverse_fail_sub = gix::objs::Tree::empty();
        traverse_fail_sub.entries.push(gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Tree.into(),
            filename: "broken_subdir".into(),
            oid: fake_id,
        });
        let traverse_fail_sub_id = repo.write_object(&traverse_fail_sub).unwrap();

        let mut traverse_fail_gleon = gix::objs::Tree::empty();
        traverse_fail_gleon.entries.push(gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Tree.into(),
            filename: "manifests".into(),
            oid: traverse_fail_sub_id.detach(),
        });
        let traverse_fail_gleon_id = repo.write_object(&traverse_fail_gleon).unwrap();

        let mut traverse_fail_root = gix::objs::Tree::empty();
        traverse_fail_root.entries.push(gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Tree.into(),
            filename: ".gleon".into(),
            oid: traverse_fail_gleon_id.detach(),
        });
        let traverse_fail_root_id = repo.write_object(&traverse_fail_root).unwrap();

        let traverse_fail_commit = commit_test_tree(
            &repo,
            "refs/heads/traverse-fail",
            "traverse fail commit",
            traverse_fail_root_id,
            std::iter::empty::<gix::ObjectId>(),
        );

        // Run scan_commit_manifests directly to verify failure counters
        let mut visited_trees = HashSet::new();
        let mut referenced_hashes = HashSet::new();
        let mut scan_failures = 0;

        // Missing tree commit
        let bad_commit = repo.find_commit(bad_commit_id).unwrap();
        scan_commit_manifests(
            &repo,
            &bad_commit,
            ".gleon/manifests",
            &mut visited_trees,
            &mut referenced_hashes,
            &mut scan_failures,
        );
        assert_eq!(scan_failures, 1);

        // Corrupt path commit
        let corrupt_c = repo.find_commit(corrupt_path_commit.detach()).unwrap();
        scan_commit_manifests(
            &repo,
            &corrupt_c,
            ".gleon/manifests",
            &mut visited_trees,
            &mut referenced_hashes,
            &mut scan_failures,
        );
        assert_eq!(scan_failures, 2);

        // Missing entry obj commit
        let missing_c = repo.find_commit(missing_obj_commit.detach()).unwrap();
        scan_commit_manifests(
            &repo,
            &missing_c,
            ".gleon/manifests",
            &mut visited_trees,
            &mut referenced_hashes,
            &mut scan_failures,
        );
        assert_eq!(scan_failures, 3);

        // Blob manifests commit
        let blob_c = repo.find_commit(blob_manifests_commit.detach()).unwrap();
        scan_commit_manifests(
            &repo,
            &blob_c,
            ".gleon/manifests",
            &mut visited_trees,
            &mut referenced_hashes,
            &mut scan_failures,
        );
        assert_eq!(scan_failures, 4);

        // Traversal failure commit -> lines 254-255
        let traverse_c = repo.find_commit(traverse_fail_commit.detach()).unwrap();
        scan_commit_manifests(
            &repo,
            &traverse_c,
            ".gleon/manifests",
            &mut visited_trees,
            &mut referenced_hashes,
            &mut scan_failures,
        );
        assert_eq!(scan_failures, 5);

        // Corrupt JSON manifest commit + missing blob record -> lines 264-265 & 266-268
        let corrupt_json_c = repo.find_commit(corrupt_json_commit.detach()).unwrap();
        scan_commit_manifests(
            &repo,
            &corrupt_json_c,
            ".gleon/manifests",
            &mut visited_trees,
            &mut referenced_hashes,
            &mut scan_failures,
        );
        assert_eq!(scan_failures, 7);

        // 7. Test scan_all_tracked_refs with untracked ref, dangling ref, tree ref, and corrupt loose ref
        // Untracked ref -> lines 329, 331
        let _ = commit_test_tree(
            &repo,
            "refs/notes/my-note",
            "note",
            root_tree_id,
            std::iter::empty::<gix::ObjectId>(),
        );

        // Ref pointing directly to tree (non-commit) -> lines 344-347
        std::fs::write(
            repo.git_dir().join("refs/heads/tree-ref"),
            root_tree_id.to_string(),
        )
        .unwrap();

        // Dangling ref pointing to non-existent object -> lines 339-342
        std::fs::write(
            repo.git_dir().join("refs/heads/dangling-ref"),
            fake_id.to_string(),
        )
        .unwrap();

        // Corrupt loose ref content -> lines 322-323
        std::fs::write(
            repo.git_dir().join("refs/heads/corrupt-ref-content"),
            b"not-a-valid-oid",
        )
        .unwrap();

        let mut ref_failures = 0;
        let count = scan_all_tracked_refs(
            &repo,
            ".gleon/manifests",
            &mut visited_trees,
            &mut referenced_hashes,
            &mut ref_failures,
        );
        assert!(count > 0);
        assert!(ref_failures >= 2);

        // 8. Test collect_all_referenced_hashes with scan_failures > 0 -> lines 433-441
        let err = collect_all_referenced_hashes(repo_root, &GcOptions::default()).unwrap_err();
        assert!(matches!(err, GcError::UnsafeScan { .. }));

        let force_opts = GcOptions {
            force: true,
            ..Default::default()
        };
        assert!(collect_all_referenced_hashes(repo_root, &force_opts).is_ok());

        let dry_opts = GcOptions {
            dry_run: true,
            ..Default::default()
        };
        assert!(collect_all_referenced_hashes(repo_root, &dry_opts).is_ok());

        // 9. Packed-refs corruption -> lines 312-313
        std::fs::write(
            repo.git_dir().join("packed-refs"),
            b"garbage header that cannot be parsed as packed-refs",
        )
        .unwrap();
        let mut packed_failures = 0;
        let res_count = scan_all_tracked_refs(
            &repo,
            ".gleon/manifests",
            &mut visited_trees,
            &mut referenced_hashes,
            &mut packed_failures,
        );
        assert_eq!(res_count, 0);
        assert_eq!(packed_failures, 1);

        // 10. Platform.all() failure when packed-refs contains invalid secondary header
        let repo_refs_temp = tempdir().unwrap();
        let repo_refs = gix::init(repo_refs_temp.path()).unwrap();
        std::fs::write(
            repo_refs.git_dir().join("packed-refs"),
            b"# pack-refs with: sorted\n# invalid-header-format\n",
        )
        .unwrap();
        let mut refs_fail_count = 0;
        let c = scan_all_tracked_refs(
            &repo_refs,
            ".gleon/manifests",
            &mut visited_trees,
            &mut referenced_hashes,
            &mut refs_fail_count,
        );
        assert_eq!(c, 0);
        assert_eq!(refs_fail_count, 1);
    }

    #[tokio::test]
    async fn test_garbage_collect_failed_deletion_warning_branch() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let temp = tempdir().unwrap();
            let repo_root = temp.path();
            let repo = gix::init(repo_root).unwrap();

            let empty_tree = gix::objs::Tree::empty();
            let tree_id = repo.write_object(&empty_tree).unwrap();
            let _c1 = commit_test_tree(
                &repo,
                "refs/heads/main",
                "init",
                tree_id,
                std::iter::empty::<gix::ObjectId>(),
            );
            let _c2 = commit_test_tree(
                &repo,
                "refs/heads/feature",
                "feature commit",
                tree_id,
                std::iter::empty::<gix::ObjectId>(),
            );

            let manifests_dir = repo_root.join(".gleon/manifests");
            std::fs::create_dir_all(&manifests_dir).unwrap();

            let remote_temp = tempdir().unwrap();
            let remote_dir = remote_temp.path();
            let url = url::Url::from_directory_path(remote_dir)
                .unwrap()
                .to_string();
            let storage_cfg = StorageConfig::new(url);
            let adapter = ObjectStoreAdapter::from_config(&storage_cfg).unwrap();

            let dummy_src = repo_root.join("dummy.png");
            std::fs::write(&dummy_src, b"fake png").unwrap();

            let h_orphan = ImageHash::new(
                "sha256",
                "7777777777777777777777777777777777777777777777777777777777777777",
            )
            .unwrap();
            adapter.upload_blob(&h_orphan, &dummy_src).await.unwrap();

            let orphan_path = remote_dir
                .join("blobs")
                .join("sha256")
                .join("7777777777777777777777777777777777777777777777777777777777777777");
            let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(48 * 3600);
            let times = std::fs::FileTimes::new().set_modified(old_time);
            let f = std::fs::File::options()
                .write(true)
                .open(&orphan_path)
                .unwrap();
            f.set_times(times).unwrap();
            drop(f);

            let parent_dir = orphan_path.parent().unwrap();
            let orig_perms = std::fs::metadata(parent_dir).unwrap().permissions();
            std::fs::set_permissions(parent_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

            let ctx = ResolvedContext::from_options(
                &crate::context::ContextOptions::default(),
                repo_root,
            )
            .unwrap();
            let opts = GcOptions::default();
            let res = garbage_collect(&ctx, Some(&storage_cfg), &opts)
                .await
                .unwrap();

            std::fs::set_permissions(parent_dir, orig_perms).unwrap();

            assert_eq!(res.failed_blobs, 1);
        }
    }
}
