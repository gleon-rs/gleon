#![cfg(not(miri))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::pedantic,
    clippy::nursery,
    missing_docs,
    reason = "test code: panics are assertions, and pedantic/nursery style lints are not enforced in tests"
)]

//! Unit and integration tests for storage garbage collection.

use std::{collections::HashSet, fs, path::Path};

use chrono::{Duration, Utc};
use gleon_core::{
    context::{ContextOptions, ResolvedContext},
    manifest::{ImageHash, SingleTestManifest},
    ops::{
        gc::{
            GcError, GcMode, GcOptions, RemoteBlobEntry, collect_all_referenced_hashes,
            filter_orphans_to_delete, garbage_collect, partition_remote_blobs,
        },
        init_workspace,
    },
    storage::adapter::{ObjectStoreAdapter, StorageConfig},
};
use tempfile::tempdir;

fn file_url(path: &Path) -> String {
    let url = url::Url::from_directory_path(path)
        .unwrap_or_else(|_| url::Url::from_file_path(path).expect("valid file path"));
    url.to_string()
}

fn make_sha256(char: char) -> ImageHash {
    let hex = char.to_string().repeat(64);
    ImageHash::new("sha256", &hex).unwrap()
}

fn commit_test_tree<'a>(
    repo: &'a gix::Repository,
    reference: &str,
    message: &str,
    tree_id: impl Into<gix::ObjectId>,
    parents: impl IntoIterator<Item = impl Into<gix::ObjectId>>,
) -> gix::Id<'a> {
    let sig = gix::actor::SignatureRef::from_bytes(b"Gleon Test <test@gleon.dev> 1700000000 +0000")
        .expect("valid test signature");
    repo.commit_as(sig, sig, reference, message, tree_id, parents)
        .expect("successful commit in test")
}

#[test]
fn test_filter_orphans_to_delete_grace_period() {
    let now = Utc::now();
    let grace_period = Duration::hours(24);

    let h_ref = make_sha256('1');
    let h_old_orphan = make_sha256('2');
    let h_recent_orphan = make_sha256('3');
    let h_future_orphan = make_sha256('4');
    let h_boundary_orphan = make_sha256('5');

    let remote_blobs = vec![
        RemoteBlobEntry {
            hash: h_ref.clone(),
            last_modified: now - Duration::hours(100),
            size: 1000,
        },
        RemoteBlobEntry {
            hash: h_old_orphan.clone(),
            last_modified: now - Duration::hours(48),
            size: 2000,
        },
        RemoteBlobEntry {
            hash: h_recent_orphan.clone(),
            last_modified: now - Duration::hours(12),
            size: 3000,
        },
        RemoteBlobEntry {
            hash: h_future_orphan.clone(),
            // Clock skew: upload from a machine slightly ahead in time
            last_modified: now + Duration::hours(2),
            size: 4000,
        },
        RemoteBlobEntry {
            hash: h_boundary_orphan.clone(),
            last_modified: now - Duration::hours(24),
            size: 5000,
        },
    ];

    let mut referenced = HashSet::new();
    referenced.insert(h_ref);

    let (to_delete, protected_count) =
        filter_orphans_to_delete(&remote_blobs, &referenced, now, grace_period);

    let deleted_hashes: Vec<ImageHash> = to_delete.iter().map(|b| b.hash.clone()).collect();

    assert_eq!(protected_count, 2);
    assert_eq!(deleted_hashes.len(), 2);
    assert!(deleted_hashes.contains(&h_old_orphan));
    assert!(deleted_hashes.contains(&h_boundary_orphan));
}

#[test]
fn test_partition_remote_blobs() {
    let now = Utc::now();
    let grace_period = Duration::hours(24);

    let h_ref = make_sha256('1');
    let h_old_orphan = make_sha256('2');
    let h_recent_orphan = make_sha256('3');

    let remote_blobs = vec![
        RemoteBlobEntry {
            hash: h_ref.clone(),
            last_modified: now - Duration::hours(100),
            size: 1000,
        },
        RemoteBlobEntry {
            hash: h_old_orphan.clone(),
            last_modified: now - Duration::hours(48),
            size: 2000,
        },
        RemoteBlobEntry {
            hash: h_recent_orphan.clone(),
            last_modified: now - Duration::hours(12),
            size: 3000,
        },
    ];

    let mut referenced = HashSet::new();
    referenced.insert(h_ref);

    let (orphans, protected_count, referenced_count) =
        partition_remote_blobs(remote_blobs, &referenced, now, grace_period);

    assert_eq!(referenced_count, 1);
    assert_eq!(protected_count, 1);
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].hash, h_old_orphan);
}

#[test]
fn test_gc_options_constructors() {
    let def = GcOptions::default();
    assert!(!def.dry_run);
    assert!(!def.force);
    assert_eq!(def.grace_period, Duration::hours(24));

    let custom = GcOptions::new(true, 48, true);
    assert!(custom.dry_run);
    assert!(custom.force);
    assert_eq!(custom.grace_period, Duration::hours(48));
}

#[tokio::test]
async fn test_gc_local_mode_when_no_storage_url() {
    let temp = tempdir().unwrap();
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), temp.path()).unwrap();

    let res = garbage_collect(&ctx, None, &GcOptions::default())
        .await
        .unwrap();

    assert_eq!(res.mode, GcMode::LocalMode);
    assert_eq!(res.deleted_blobs, 0);
}

#[tokio::test]
async fn test_gc_full_lifecycle_with_storage() {
    let workspace_temp = tempdir().unwrap();
    let remote_temp = tempdir().unwrap();

    let storage_url = file_url(remote_temp.path());
    let storage_cfg = StorageConfig::new(storage_url);

    let ctx =
        ResolvedContext::from_options(&ContextOptions::default(), workspace_temp.path()).unwrap();
    init_workspace(&ctx).unwrap();

    let adapter = ObjectStoreAdapter::from_config(&storage_cfg).unwrap();

    // 1. Create a local manifest that references blob h1
    let h1 = make_sha256('a');
    let h2 = make_sha256('b');

    let manifest = SingleTestManifest {
        schema_version: 1,
        hash: h1.clone(),
        phash: ImageHash::new("dhash", "0123456789abcdef").unwrap(),
        width: 100,
        height: 100,
    };

    let plat_dir = workspace_temp
        .path()
        .join(".gleon/manifests")
        .join(ctx.platform.key().unwrap());
    fs::create_dir_all(&plat_dir).unwrap();
    let manifest_json = serde_json::to_string_pretty(&manifest).unwrap();
    fs::write(plat_dir.join("test_case.json"), manifest_json).unwrap();

    // 2. Upload both h1 (referenced) and h2 (orphan) to remote storage
    let temp_blob_file = workspace_temp.path().join("dummy.png");
    let fixture_png = include_bytes!("fixtures/baseline_100x100.png");
    fs::write(&temp_blob_file, fixture_png).unwrap();

    adapter.upload_blob(&h1, &temp_blob_file).await.unwrap();
    adapter.upload_blob(&h2, &temp_blob_file).await.unwrap();

    assert!(adapter.blob_exists(&h1).await.unwrap());
    assert!(adapter.blob_exists(&h2).await.unwrap());

    // Backdate h2 on disk so it is older than the 24-hour grace period
    let h2_file = remote_temp
        .path()
        .join("blobs")
        .join(h2.scheme())
        .join(h2.value());
    let file = fs::File::open(&h2_file).unwrap();
    let forty_eight_hours_ago =
        std::time::SystemTime::now() - std::time::Duration::from_secs(48 * 3600);
    file.set_times(fs::FileTimes::new().set_modified(forty_eight_hours_ago))
        .unwrap();

    // 3. Dry-run GC with grace_period = 24 and force = true
    let dry_run_opts = GcOptions::new(true, 24, true);
    let dry_res = garbage_collect(&ctx, Some(&storage_cfg), &dry_run_opts)
        .await
        .unwrap();

    assert_eq!(dry_res.mode, GcMode::DryRun);
    assert_eq!(dry_res.total_remote_blobs, 2);
    assert_eq!(dry_res.referenced_blobs, 1);
    assert_eq!(dry_res.deleted_blobs, 1);
    assert_eq!(dry_res.orphans.len(), 1);
    assert_eq!(dry_res.orphans[0].hash, h2);

    // Both blobs still exist after dry-run
    assert!(adapter.blob_exists(&h1).await.unwrap());
    assert!(adapter.blob_exists(&h2).await.unwrap());

    // 4. Real GC with grace_period = 24 and force = true -> deletes h2, keeps h1
    let real_opts = GcOptions::new(false, 24, true);
    let real_res = garbage_collect(&ctx, Some(&storage_cfg), &real_opts)
        .await
        .unwrap();

    assert_eq!(real_res.mode, GcMode::Executed);
    assert_eq!(real_res.total_remote_blobs, 2);
    assert_eq!(real_res.referenced_blobs, 1);
    assert_eq!(real_res.deleted_blobs, 1);
    assert_eq!(real_res.failed_blobs, 0);

    // h1 is preserved, h2 is deleted!
    assert!(
        adapter.blob_exists(&h1).await.unwrap(),
        "Referenced blob h1 must be preserved"
    );
    assert!(
        !adapter.blob_exists(&h2).await.unwrap(),
        "Orphan blob h2 must be deleted"
    );
}

#[tokio::test]
async fn test_gc_integration_real_data_mtime() {
    let workspace_temp = tempdir().unwrap();
    let remote_temp = tempdir().unwrap();

    let storage_url = file_url(remote_temp.path());
    let storage_cfg = StorageConfig::new(storage_url);

    // 1. Initialize a real Git repository with >= 2 refs to satisfy fail-closed guards
    let repo_root = workspace_temp.path();
    let repo = gix::init(repo_root).unwrap();

    let empty_tree = gix::objs::Tree::empty();
    let tree_id = repo.write_object(&empty_tree).unwrap();
    let _c1 = commit_test_tree(
        &repo,
        "refs/heads/main",
        "init main",
        tree_id,
        std::iter::empty::<gix::ObjectId>(),
    );
    let _c2 = commit_test_tree(
        &repo,
        "refs/heads/feature",
        "init feature",
        tree_id,
        std::iter::empty::<gix::ObjectId>(),
    );

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), repo_root).unwrap();
    init_workspace(&ctx).unwrap();

    let adapter = ObjectStoreAdapter::from_config(&storage_cfg).unwrap();

    // 2. Prepare 3 hashes:
    // - h_ref: referenced in workspace manifest
    // - h_recent: orphan, uploaded just now (recent mtime)
    // - h_old: orphan, uploaded and backdated to 48 hours ago
    let h_ref = make_sha256('1');
    let h_recent = make_sha256('2');
    let h_old = make_sha256('3');

    let manifest = SingleTestManifest {
        schema_version: 1,
        hash: h_ref.clone(),
        phash: ImageHash::new("dhash", "0123456789abcdef").unwrap(),
        width: 100,
        height: 100,
    };
    let plat_dir = repo_root
        .join(".gleon/manifests")
        .join(ctx.platform.key().unwrap());
    fs::create_dir_all(&plat_dir).unwrap();
    fs::write(
        plat_dir.join("test_case.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();

    // 3. Upload real PNG bytes for all three blobs
    let temp_png = repo_root.join("fixture.png");
    let fixture_png = include_bytes!("fixtures/baseline_100x100.png");
    fs::write(&temp_png, fixture_png).unwrap();

    adapter.upload_blob(&h_ref, &temp_png).await.unwrap();
    adapter.upload_blob(&h_recent, &temp_png).await.unwrap();
    adapter.upload_blob(&h_old, &temp_png).await.unwrap();

    assert!(adapter.blob_exists(&h_ref).await.unwrap());
    assert!(adapter.blob_exists(&h_recent).await.unwrap());
    assert!(adapter.blob_exists(&h_old).await.unwrap());

    // 4. Backdate h_old on disk in remote_temp
    let old_blob_file = remote_temp
        .path()
        .join("blobs")
        .join(h_old.scheme())
        .join(h_old.value());
    let file = fs::File::open(&old_blob_file).unwrap();
    let forty_eight_hours_ago =
        std::time::SystemTime::now() - std::time::Duration::from_secs(48 * 3600);
    file.set_times(fs::FileTimes::new().set_modified(forty_eight_hours_ago))
        .unwrap();

    // 5. Run GC with default 24h grace period, force = false
    let gc_opts = GcOptions::new(false, 24, false);
    let result = garbage_collect(&ctx, Some(&storage_cfg), &gc_opts)
        .await
        .unwrap();

    assert_eq!(result.mode, GcMode::Executed);
    assert_eq!(result.total_remote_blobs, 3);
    assert_eq!(result.referenced_blobs, 1);
    assert_eq!(result.protected_by_grace_period, 1);
    assert_eq!(result.deleted_blobs, 1);
    assert_eq!(result.failed_blobs, 0);
    assert_eq!(result.orphans.len(), 1);
    assert_eq!(result.orphans[0].hash, h_old);

    // Verify remote storage state:
    assert!(
        adapter.blob_exists(&h_ref).await.unwrap(),
        "h_ref must exist"
    );
    assert!(
        adapter.blob_exists(&h_recent).await.unwrap(),
        "h_recent must exist (protected by grace period)"
    );
    assert!(
        !adapter.blob_exists(&h_old).await.unwrap(),
        "h_old must be deleted"
    );
}

#[tokio::test]
async fn test_gc_grace_period_under_24h_fails_even_with_force() {
    let workspace_temp = tempdir().unwrap();
    let remote_temp = tempdir().unwrap();

    let storage_cfg = StorageConfig::new(file_url(remote_temp.path()));
    let ctx =
        ResolvedContext::from_options(&ContextOptions::default(), workspace_temp.path()).unwrap();
    init_workspace(&ctx).unwrap();

    // 0h fails
    let opts = GcOptions::new(false, 0, false);
    let res = garbage_collect(&ctx, Some(&storage_cfg), &opts).await;
    assert!(
        matches!(res, Err(GcError::GracePeriodTooShort)),
        "Must fail with GracePeriodTooShort when 0h grace period is passed without --force"
    );

    // 12h fails
    let opts_12 = GcOptions::new(false, 12, false);
    let res_12 = garbage_collect(&ctx, Some(&storage_cfg), &opts_12).await;
    assert!(
        matches!(res_12, Err(GcError::GracePeriodTooShort)),
        "Must fail with GracePeriodTooShort when 12h grace period is passed"
    );

    // 0h fails even with --force (24-hour minimum strictly enforced)
    let opts_force = GcOptions::new(false, 0, true);
    let res_force = garbage_collect(&ctx, Some(&storage_cfg), &opts_force).await;
    assert!(
        matches!(res_force, Err(GcError::GracePeriodTooShort)),
        "Must fail with GracePeriodTooShort even with --force when grace period is < 24h"
    );
}

#[tokio::test]
async fn test_gc_uninitialized_workspace_fails() {
    let temp = tempdir().unwrap();
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), temp.path()).unwrap();
    let storage_cfg = StorageConfig::new("memory://");

    let res = garbage_collect(&ctx, Some(&storage_cfg), &GcOptions::default()).await;
    assert!(
        matches!(
            res,
            Err(GcError::Core(
                gleon_core::ops::common::CoreError::NotInitialized
            ))
        ),
        "Must fail fast with NotInitialized when .gleon is missing"
    );
}

#[test]
fn test_collect_all_referenced_hashes_git_branches() {
    let temp = tempdir().unwrap();
    let base_path = temp.path();

    // 1. Initialize git repo
    let repo = gix::init(base_path).unwrap();

    // 2. Setup a manifest in a feature branch in Git
    let h_branch = make_sha256('b');
    let m_branch = SingleTestManifest {
        schema_version: 1,
        hash: h_branch.clone(),
        phash: ImageHash::new("dhash", "0123456789abcdef").unwrap(),
        width: 100,
        height: 100,
    };
    let blob_bytes = serde_json::to_vec(&m_branch).unwrap();
    let blob_id = repo.write_blob(&blob_bytes).unwrap();

    let mut manifests_tree = gix::objs::Tree::empty();
    manifests_tree.entries.push(gix::objs::tree::Entry {
        mode: gix::objs::tree::EntryKind::Blob.into(),
        filename: "feature_test.json".into(),
        oid: blob_id.detach(),
    });
    let manifests_tree_id = repo.write_object(&manifests_tree).unwrap();

    let mut gleon_tree = gix::objs::Tree::empty();
    gleon_tree.entries.push(gix::objs::tree::Entry {
        mode: gix::objs::tree::EntryKind::Tree.into(),
        filename: "manifests".into(),
        oid: manifests_tree_id.detach(),
    });
    let gleon_tree_id = repo.write_object(&gleon_tree).unwrap();

    let mut root_tree = gix::objs::Tree::empty();
    root_tree.entries.push(gix::objs::tree::Entry {
        mode: gix::objs::tree::EntryKind::Tree.into(),
        filename: ".gleon".into(),
        oid: gleon_tree_id.detach(),
    });
    let root_tree_id = repo.write_object(&root_tree).unwrap();

    let _commit_id = commit_test_tree(
        &repo,
        "refs/heads/feature-1",
        "add feature manifest",
        root_tree_id,
        std::iter::empty::<gix::ObjectId>(),
    );

    // Add a second ref (main branch) so tracked refs > 1
    let _main_commit = commit_test_tree(
        &repo,
        "refs/heads/main",
        "initial main commit",
        root_tree_id,
        std::iter::empty::<gix::ObjectId>(),
    );

    // 3. Setup a distinct manifest on local disk (not yet committed to git)
    let h_local = make_sha256('a');
    let plat_dir = base_path.join(".gleon/manifests/macos-arm64");
    fs::create_dir_all(&plat_dir).unwrap();

    let m_local = SingleTestManifest {
        schema_version: 1,
        hash: h_local.clone(),
        phash: ImageHash::new("dhash", "0123456789abcdef").unwrap(),
        width: 50,
        height: 50,
    };
    fs::write(
        plat_dir.join("local.json"),
        serde_json::to_string(&m_local).unwrap(),
    )
    .unwrap();

    // 4. Verify collection collects BOTH local disk manifest and Git branch manifest
    let collected = collect_all_referenced_hashes(base_path, &GcOptions::default()).unwrap();
    assert!(
        collected.contains(&h_local),
        "Must contain local workspace hash"
    );
    assert!(
        collected.contains(&h_branch),
        "Must contain git branch referenced hash"
    );
}

#[test]
fn test_collect_all_referenced_hashes_insufficient_refs_fails_without_force() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path();
    let repo = gix::init(repo_root).unwrap();

    // Only 1 ref
    let empty_tree = gix::objs::Tree::empty();
    let tree_id = repo.write_object(&empty_tree).unwrap();
    let _commit = commit_test_tree(
        &repo,
        "refs/heads/main",
        "init",
        tree_id,
        std::iter::empty::<gix::ObjectId>(),
    );

    let manifests_dir = repo_root.join(".gleon/manifests");
    fs::create_dir_all(&manifests_dir).unwrap();

    let err = collect_all_referenced_hashes(repo_root, &GcOptions::default()).unwrap_err();
    assert!(
        matches!(err, GcError::InsufficientCommits(1)),
        "Must fail closed when repo has only 1 ref and --force is not set"
    );

    // With force = true, it succeeds
    let force_opts = GcOptions {
        force: true,
        ..Default::default()
    };
    let ok = collect_all_referenced_hashes(repo_root, &force_opts);
    assert!(ok.is_ok(), "Must succeed with --force");
}

#[test]
fn test_collect_all_referenced_hashes_multiple_refs_same_commit_fails_without_force() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path();
    let repo = gix::init(repo_root).unwrap();

    let empty_tree = gix::objs::Tree::empty();
    let tree_id = repo.write_object(&empty_tree).unwrap();
    let c1 = commit_test_tree(
        &repo,
        "refs/heads/main",
        "init",
        tree_id,
        std::iter::empty::<gix::ObjectId>(),
    );

    // Create a second reference pointing to the exact same commit using identical in-memory signature
    let c2 = commit_test_tree(
        &repo,
        "refs/heads/feature",
        "init",
        tree_id,
        std::iter::empty::<gix::ObjectId>(),
    );
    assert_eq!(
        c1.detach(),
        c2.detach(),
        "refs must point to the identical commit ID"
    );

    let manifests_dir = repo_root.join(".gleon/manifests");
    fs::create_dir_all(&manifests_dir).unwrap();

    let err = collect_all_referenced_hashes(repo_root, &GcOptions::default()).unwrap_err();
    assert!(
        matches!(err, GcError::InsufficientCommits(1)),
        "Must fail closed when multiple refs point to only 1 unique commit"
    );
}

#[test]
fn test_collect_all_referenced_hashes_shallow_clone_fails_without_force() {
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
        "refs/heads/branch2",
        "b2",
        tree_id,
        std::iter::empty::<gix::ObjectId>(),
    );

    let manifests_dir = repo_root.join(".gleon/manifests");
    fs::create_dir_all(&manifests_dir).unwrap();

    // Simulate shallow clone by creating shallow file
    let git_dir = repo_root.join(".git");
    fs::write(git_dir.join("shallow"), b"fake_shallow_sha\n").unwrap();

    let err = collect_all_referenced_hashes(repo_root, &GcOptions::default()).unwrap_err();
    assert!(
        matches!(err, GcError::ShallowClone),
        "Must fail with ShallowClone when .git/shallow exists without --force"
    );

    let force_opts = GcOptions {
        force: true,
        ..Default::default()
    };
    assert!(
        collect_all_referenced_hashes(repo_root, &force_opts).is_ok(),
        "Must succeed when --force is specified on shallow clone"
    );
}

#[test]
fn test_collect_all_referenced_hashes_monorepo_subfolder() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path();
    let repo = gix::init(repo_root).unwrap();

    let subproject_dir = repo_root.join("packages").join("mobile");
    let sub_gleon = subproject_dir
        .join(".gleon")
        .join("manifests")
        .join("macos");
    fs::create_dir_all(&sub_gleon).unwrap();

    let h_monorepo = make_sha256('c');
    let m = SingleTestManifest {
        schema_version: 1,
        hash: h_monorepo.clone(),
        phash: ImageHash::new("dhash", "0123456789abcdef").unwrap(),
        width: 100,
        height: 100,
    };
    let blob_bytes = serde_json::to_vec(&m).unwrap();
    let blob_id = repo.write_blob(&blob_bytes).unwrap();

    // Build tree: packages -> mobile -> .gleon -> manifests -> test.json
    let mut manifests_tree = gix::objs::Tree::empty();
    manifests_tree.entries.push(gix::objs::tree::Entry {
        mode: gix::objs::tree::EntryKind::Blob.into(),
        filename: "test.json".into(),
        oid: blob_id.detach(),
    });
    let manifests_tree_id = repo.write_object(&manifests_tree).unwrap();

    let mut gleon_tree = gix::objs::Tree::empty();
    gleon_tree.entries.push(gix::objs::tree::Entry {
        mode: gix::objs::tree::EntryKind::Tree.into(),
        filename: "manifests".into(),
        oid: manifests_tree_id.detach(),
    });
    let gleon_tree_id = repo.write_object(&gleon_tree).unwrap();

    let mut dot_gleon_tree = gix::objs::Tree::empty();
    dot_gleon_tree.entries.push(gix::objs::tree::Entry {
        mode: gix::objs::tree::EntryKind::Tree.into(),
        filename: ".gleon".into(),
        oid: gleon_tree_id.detach(),
    });
    let dot_gleon_tree_id = repo.write_object(&dot_gleon_tree).unwrap();

    let mut mobile_tree = gix::objs::Tree::empty();
    mobile_tree.entries.push(gix::objs::tree::Entry {
        mode: gix::objs::tree::EntryKind::Tree.into(),
        filename: "mobile".into(),
        oid: dot_gleon_tree_id.detach(),
    });
    let mobile_tree_id = repo.write_object(&mobile_tree).unwrap();

    let mut packages_tree = gix::objs::Tree::empty();
    packages_tree.entries.push(gix::objs::tree::Entry {
        mode: gix::objs::tree::EntryKind::Tree.into(),
        filename: "packages".into(),
        oid: mobile_tree_id.detach(),
    });
    let packages_tree_id = repo.write_object(&packages_tree).unwrap();

    let _commit_id = commit_test_tree(
        &repo,
        "refs/heads/feature-monorepo",
        "monorepo commit",
        packages_tree_id,
        std::iter::empty::<gix::ObjectId>(),
    );

    let _c2 = commit_test_tree(
        &repo,
        "refs/heads/main",
        "main commit",
        packages_tree_id,
        std::iter::empty::<gix::ObjectId>(),
    );

    let collected = collect_all_referenced_hashes(&subproject_dir, &GcOptions::default()).unwrap();
    assert!(
        collected.contains(&h_monorepo),
        "Must resolve manifests when working directory is a monorepo subfolder"
    );
}

#[test]
fn test_collect_all_referenced_hashes_tags() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path();
    let repo = gix::init(repo_root).unwrap();

    let h_tag = make_sha256('d');
    let m = SingleTestManifest {
        schema_version: 1,
        hash: h_tag.clone(),
        phash: ImageHash::new("dhash", "0123456789abcdef").unwrap(),
        width: 100,
        height: 100,
    };
    let blob_bytes = serde_json::to_vec(&m).unwrap();
    let blob_id = repo.write_blob(&blob_bytes).unwrap();

    let mut manifests_tree = gix::objs::Tree::empty();
    manifests_tree.entries.push(gix::objs::tree::Entry {
        mode: gix::objs::tree::EntryKind::Blob.into(),
        filename: "tag_test.json".into(),
        oid: blob_id.detach(),
    });
    let manifests_tree_id = repo.write_object(&manifests_tree).unwrap();

    let mut gleon_tree = gix::objs::Tree::empty();
    gleon_tree.entries.push(gix::objs::tree::Entry {
        mode: gix::objs::tree::EntryKind::Tree.into(),
        filename: "manifests".into(),
        oid: manifests_tree_id.detach(),
    });
    let gleon_tree_id = repo.write_object(&gleon_tree).unwrap();

    let mut root_tree = gix::objs::Tree::empty();
    root_tree.entries.push(gix::objs::tree::Entry {
        mode: gix::objs::tree::EntryKind::Tree.into(),
        filename: ".gleon".into(),
        oid: gleon_tree_id.detach(),
    });
    let root_tree_id = repo.write_object(&root_tree).unwrap();

    let _commit_id = commit_test_tree(
        &repo,
        "refs/tags/v1.0.0",
        "release tag v1.0.0",
        root_tree_id,
        std::iter::empty::<gix::ObjectId>(),
    );

    let _main = commit_test_tree(
        &repo,
        "refs/heads/main",
        "main branch",
        root_tree_id,
        std::iter::empty::<gix::ObjectId>(),
    );

    let collected = collect_all_referenced_hashes(repo_root, &GcOptions::default()).unwrap();
    assert!(
        collected.contains(&h_tag),
        "Must collect referenced hashes from release tags (refs/tags/*)"
    );
}

#[test]
fn test_scan_commit_manifests_resilient_to_unknown_schema_version() {
    let temp = tempdir().unwrap();
    let repo_root = temp.path();
    let repo = gix::init(repo_root).unwrap();

    let h_future = make_sha256('f');
    let raw_json = format!(
        r#"{{"schema_version": 999, "hash": "sha256:{}", "custom_extra": 123}}"#,
        "f".repeat(64)
    );
    let blob_id = repo.write_blob(raw_json.as_bytes()).unwrap();

    let mut manifests_tree = gix::objs::Tree::empty();
    manifests_tree.entries.push(gix::objs::tree::Entry {
        mode: gix::objs::tree::EntryKind::Blob.into(),
        filename: "future_test.json".into(),
        oid: blob_id.detach(),
    });
    let manifests_tree_id = repo.write_object(&manifests_tree).unwrap();

    let mut gleon_tree = gix::objs::Tree::empty();
    gleon_tree.entries.push(gix::objs::tree::Entry {
        mode: gix::objs::tree::EntryKind::Tree.into(),
        filename: "manifests".into(),
        oid: manifests_tree_id.detach(),
    });
    let gleon_tree_id = repo.write_object(&gleon_tree).unwrap();

    let mut root_tree = gix::objs::Tree::empty();
    root_tree.entries.push(gix::objs::tree::Entry {
        mode: gix::objs::tree::EntryKind::Tree.into(),
        filename: ".gleon".into(),
        oid: gleon_tree_id.detach(),
    });
    let root_tree_id = repo.write_object(&root_tree).unwrap();

    let _commit_id = commit_test_tree(
        &repo,
        "refs/heads/future-branch",
        "future schema version",
        root_tree_id,
        std::iter::empty::<gix::ObjectId>(),
    );

    let _main = commit_test_tree(
        &repo,
        "refs/heads/main",
        "main branch",
        root_tree_id,
        std::iter::empty::<gix::ObjectId>(),
    );

    let collected = collect_all_referenced_hashes(repo_root, &GcOptions::default()).unwrap();
    assert!(
        collected.contains(&h_future),
        "Must extract hash even from manifests with unknown schema version or extra fields"
    );
}

#[tokio::test]
async fn test_gc_s3_prefix_isolation() {
    let temp = tempdir().unwrap();
    let root = temp.path();

    // Two project configs sharing the same underlying storage folder with distinct prefixes
    let url_a = file_url(&root.join("project_a"));
    let url_b = file_url(&root.join("project_b"));

    let cfg_a = StorageConfig::new(url_a);
    let cfg_b = StorageConfig::new(url_b);

    let adapter_a = ObjectStoreAdapter::from_config(&cfg_a).unwrap();
    let adapter_b = ObjectStoreAdapter::from_config(&cfg_b).unwrap();

    let h_a = make_sha256('a');
    let h_b = make_sha256('b');

    let dummy_file = root.join("dummy.png");
    fs::write(&dummy_file, include_bytes!("fixtures/baseline_100x100.png")).unwrap();

    adapter_a.upload_blob(&h_a, &dummy_file).await.unwrap();
    adapter_b.upload_blob(&h_b, &dummy_file).await.unwrap();

    // Both blobs exist in their respective adapters
    assert!(adapter_a.blob_exists(&h_a).await.unwrap());
    assert!(!adapter_a.blob_exists(&h_b).await.unwrap());

    assert!(!adapter_b.blob_exists(&h_a).await.unwrap());
    assert!(adapter_b.blob_exists(&h_b).await.unwrap());

    // Listing on A only returns A's blob
    let blobs_a = adapter_a.list_all_blobs().await.unwrap();
    assert_eq!(blobs_a.len(), 1);
    assert_eq!(blobs_a[0].hash, h_a);

    // Listing on B only returns B's blob
    let blobs_b = adapter_b.list_all_blobs().await.unwrap();
    assert_eq!(blobs_b.len(), 1);
    assert_eq!(blobs_b[0].hash, h_b);
}

#[tokio::test]
async fn test_delete_blobs_batch_with_not_found() {
    let temp = tempdir().unwrap();
    let cfg = StorageConfig::new(file_url(temp.path()));
    let adapter = ObjectStoreAdapter::from_config(&cfg).unwrap();

    let h1 = make_sha256('1');
    let h2 = make_sha256('2');

    let dummy_file = temp.path().join("dummy.png");
    fs::write(&dummy_file, include_bytes!("fixtures/baseline_100x100.png")).unwrap();
    adapter.upload_blob(&h1, &dummy_file).await.unwrap();

    // h1 exists, h2 does not exist (NotFound)
    let summary = adapter
        .delete_blobs(&[h1.clone(), h2.clone()])
        .await
        .unwrap();

    assert_eq!(summary.deleted, 2);
    assert_eq!(summary.failed, 0);
    assert!(summary.deleted_hashes.contains(&h1));
    assert!(summary.deleted_hashes.contains(&h2));
    assert!(!adapter.blob_exists(&h1).await.unwrap());
}
