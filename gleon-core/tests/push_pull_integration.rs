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

//! Integration tests for Phase 3.5 push and pull operations.

use std::fs;

use gleon_core::{
    context::{ContextOptions, ResolvedContext},
    manifest::{ImageHash, SingleTestManifest, WorkspaceIndex},
    ops::{init_workspace, pull_blobs, push_blobs, stage_workspace},
    storage::StorageConfig,
};
use tempfile::tempdir;

#[tokio::test]
async fn test_push_pull_local_flat_mode() {
    let temp = tempdir().unwrap();
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), temp.path()).unwrap();

    init_workspace(&ctx).unwrap();

    // Push with no storage config -> local mode
    let push_res = push_blobs(&ctx, None, false, None).await.unwrap();
    assert!(push_res.local_mode);

    // Pull with no storage config -> local mode
    let pull_res = pull_blobs(&ctx, None, false, None).await.unwrap();
    assert!(pull_res.local_mode);
}

#[tokio::test]
async fn test_push_pull_file_scheme_lifecycle() {
    let workspace_temp = tempdir().unwrap();
    let remote_temp = tempdir().unwrap();

    let storage_url = format!(
        "file://{}",
        remote_temp.path().to_string_lossy().replace('\\', "/")
    );
    let storage_config = StorageConfig::new(storage_url);

    let ctx =
        ResolvedContext::from_options(&ContextOptions::default(), workspace_temp.path()).unwrap();

    init_workspace(&ctx).unwrap();

    // Copy fixture screenshot to workspace
    let screenshots_dir = workspace_temp.path().join("screenshots");
    fs::create_dir_all(&screenshots_dir).unwrap();
    let fixture_png = include_bytes!("fixtures/baseline_100x100.png");
    fs::write(screenshots_dir.join("login.png"), fixture_png).unwrap();

    // Stage the screenshot
    let stage_res = stage_workspace(&ctx, None).unwrap();
    assert_eq!(stage_res.total_screenshots_staged, 1);

    let platform_key = ctx.platform.key().unwrap();
    let manifest_dir = workspace_temp
        .path()
        .join(".gleon")
        .join("manifests")
        .join(&platform_key);
    let index = WorkspaceIndex::load(&manifest_dir).unwrap();
    assert_eq!(index.entries().len(), 1);
    let manifest = index.entries().values().next().unwrap();
    let sha256_hex = manifest.hash.value().to_string();

    let local_blob_path = workspace_temp
        .path()
        .join(".gleon")
        .join("blobs")
        .join("sha256")
        .join(&sha256_hex);
    assert!(local_blob_path.is_file());

    // 1. Initial Push -> Uploads 1 blob
    let push1 = push_blobs(&ctx, Some(&storage_config), false, None)
        .await
        .unwrap();
    assert_eq!(push1.total_manifest_blobs, 1);
    assert_eq!(push1.uploaded_blobs, 1);
    assert_eq!(push1.skipped_blobs, 0);

    // Verify blob landed in remote storage directory
    let remote_blob_path = remote_temp
        .path()
        .join("blobs")
        .join("sha256")
        .join(&sha256_hex);
    assert!(remote_blob_path.is_file());

    // 2. Idempotent Push -> Uploads 0, skips 1
    let push2 = push_blobs(&ctx, Some(&storage_config), false, None)
        .await
        .unwrap();
    assert_eq!(push2.uploaded_blobs, 0);
    assert_eq!(push2.skipped_blobs, 1);

    // 3. Delete local blob to simulate fresh environment pull
    fs::remove_file(&local_blob_path).unwrap();
    assert!(!local_blob_path.is_file());

    // 4. Pull missing blob from remote -> Downloaded 1
    let pull1 = pull_blobs(&ctx, Some(&storage_config), false, None)
        .await
        .unwrap();
    assert_eq!(pull1.downloaded_blobs, 1);
    assert_eq!(pull1.skipped_blobs, 0);
    assert!(local_blob_path.is_file());
    assert_eq!(fs::read(&local_blob_path).unwrap(), fixture_png.to_vec());

    // 5. Idempotent Pull -> Downloaded 0, skips 1
    let pull2 = pull_blobs(&ctx, Some(&storage_config), false, None)
        .await
        .unwrap();
    assert_eq!(pull2.downloaded_blobs, 0);
    assert_eq!(pull2.skipped_blobs, 1);
}

#[tokio::test]
async fn test_push_missing_local_blob_fail_fast() {
    let workspace_temp = tempdir().unwrap();
    let remote_temp = tempdir().unwrap();

    let storage_url = format!(
        "file://{}",
        remote_temp.path().to_string_lossy().replace('\\', "/")
    );
    let storage_config = StorageConfig::new(storage_url);

    let ctx =
        ResolvedContext::from_options(&ContextOptions::default(), workspace_temp.path()).unwrap();

    init_workspace(&ctx).unwrap();

    let platform_key = ctx.platform.key().unwrap();
    let manifest_dir = workspace_temp
        .path()
        .join(".gleon")
        .join("manifests")
        .join(&platform_key);

    let dummy_hash = ImageHash::new("sha256", "f".repeat(64)).unwrap();
    let dummy_phash = ImageHash::new("dhash", "0000000000000000").unwrap();
    let manifest = SingleTestManifest::new(dummy_hash, dummy_phash, 10, 10).unwrap();

    let mut index = WorkspaceIndex::new();
    index
        .save_test(&manifest_dir, "auth/login", &manifest)
        .unwrap();

    // Local blob file does not exist -> Push fails fast
    let push_res = push_blobs(&ctx, Some(&storage_config), false, None).await;
    assert!(push_res.is_err());
    let err_msg = push_res.unwrap_err().to_string();
    assert!(err_msg.contains("Missing local blob"));
    assert!(err_msg.contains(&"f".repeat(64)));
}

#[tokio::test]
async fn test_pull_missing_remote_blob_fail_fast() {
    let workspace_temp = tempdir().unwrap();
    let remote_temp = tempdir().unwrap();

    let storage_url = format!(
        "file://{}",
        remote_temp.path().to_string_lossy().replace('\\', "/")
    );
    let storage_config = StorageConfig::new(storage_url);

    let ctx =
        ResolvedContext::from_options(&ContextOptions::default(), workspace_temp.path()).unwrap();

    init_workspace(&ctx).unwrap();

    let platform_key = ctx.platform.key().unwrap();
    let manifest_dir = workspace_temp
        .path()
        .join(".gleon")
        .join("manifests")
        .join(&platform_key);

    let dummy_hash = ImageHash::new("sha256", "e".repeat(64)).unwrap();
    let dummy_phash = ImageHash::new("dhash", "0000000000000000").unwrap();
    let manifest = SingleTestManifest::new(dummy_hash, dummy_phash, 10, 10).unwrap();

    let mut index = WorkspaceIndex::new();
    index
        .save_test(&manifest_dir, "auth/login", &manifest)
        .unwrap();

    // Local blob missing AND remote blob missing -> Pull fails fast with MissingRemoteBlob
    let pull_res = pull_blobs(&ctx, Some(&storage_config), false, None).await;
    assert!(pull_res.is_err());
    let err_msg = pull_res.unwrap_err().to_string();
    assert!(err_msg.contains("Missing remote blob"));
    assert!(err_msg.contains(&"e".repeat(64)));
}

#[tokio::test]
async fn test_push_pull_all_platforms_option() {
    let workspace_temp = tempdir().unwrap();
    let remote_temp = tempdir().unwrap();

    let storage_url = format!(
        "file://{}",
        remote_temp.path().to_string_lossy().replace('\\', "/")
    );
    let storage_config = StorageConfig::new(storage_url);

    let ctx =
        ResolvedContext::from_options(&ContextOptions::default(), workspace_temp.path()).unwrap();

    init_workspace(&ctx).unwrap();

    let manifests_root = workspace_temp.path().join(".gleon").join("manifests");
    let blobs_dir = workspace_temp
        .path()
        .join(".gleon")
        .join("blobs")
        .join("sha256");
    fs::create_dir_all(&blobs_dir).unwrap();

    // Create manifest and blob for macos-aarch64
    // Content addresses of the blobs: downloads check them.
    let (sha1, sha2) = (
        "8ba0d06bc5a88966b1f681d9cab28709781ad7c450802d0e477132d8919e0cbf",
        "534d059533cc6a29b0e8747334c6af08619b1b59e6727f50a8094c90f6393282",
    );
    let hash1 = ImageHash::new("sha256", sha1).unwrap();
    let phash1 = ImageHash::new("dhash", "0000000000000000").unwrap();
    let m1 = SingleTestManifest::new(hash1, phash1, 10, 10).unwrap();

    let mut idx1 = WorkspaceIndex::new();
    idx1.save_test(manifests_root.join("macos-aarch64"), "auth/login", &m1)
        .unwrap();
    fs::write(blobs_dir.join(sha1), b"blob1").unwrap();

    // Create manifest and blob for linux-x86_64
    let hash2 = ImageHash::new("sha256", sha2).unwrap();
    let phash2 = ImageHash::new("dhash", "0000000000000000").unwrap();
    let m2 = SingleTestManifest::new(hash2, phash2, 10, 10).unwrap();

    let mut idx2 = WorkspaceIndex::new();
    idx2.save_test(manifests_root.join("linux-x86_64"), "dashboard/main", &m2)
        .unwrap();
    fs::write(blobs_dir.join(sha2), b"blob2").unwrap();

    // Push with all_platforms = true
    let push_all = push_blobs(&ctx, Some(&storage_config), true, None)
        .await
        .unwrap();
    assert_eq!(push_all.total_manifest_blobs, 2);
    assert_eq!(push_all.uploaded_blobs, 2);

    // Delete local blobs
    fs::remove_file(blobs_dir.join(sha1)).unwrap();
    fs::remove_file(blobs_dir.join(sha2)).unwrap();

    // Pull with all_platforms = true
    let pull_all = pull_blobs(&ctx, Some(&storage_config), true, None)
        .await
        .unwrap();
    assert_eq!(pull_all.total_manifest_blobs, 2);
    assert_eq!(pull_all.downloaded_blobs, 2);
    assert!(blobs_dir.join(sha1).is_file());
    assert!(blobs_dir.join(sha2).is_file());
}
