//! Push operation for uploading baseline blobs to remote storage.

use std::collections::HashSet;

use thiserror::Error;

use crate::{
    context::ResolvedContext,
    ops::{
        common::{CoreError, ensure_initialized},
        sync::{
            active_storage_config, collect_referenced_hashes, resolve_platform_dirs, short_hash,
            transfer_with_progress,
        },
    },
    storage::{ObjectStoreAdapter, StorageConfig, StorageError},
};

/// Errors that can occur during a push operation.
#[derive(Debug, Error)]
pub enum PushError {
    /// Missing local blob for a manifest hash.
    #[error(
        "Missing local blob for hash '{hash}' referenced in manifest at platform '{platform}'. Please run 'gleon stage' first."
    )]
    MissingLocalBlob {
        /// The string representation of the missing hash.
        hash: String,
        /// The platform directory key where the reference was found.
        platform: String,
    },

    /// Storage adapter error.
    #[error("Storage error: {0}")]
    Storage(#[from] StorageError),

    /// Error shared across `ops::*` operations.
    #[error(transparent)]
    Core(#[from] CoreError),
}

/// Summary of push operation results.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PushResult {
    /// Total unique baseline blobs referenced across scanned platforms.
    pub total_manifest_blobs: usize,
    /// Number of blobs uploaded to remote storage.
    pub uploaded_blobs: usize,
    /// Number of blobs already present on remote storage.
    pub skipped_blobs: usize,
    /// Indicates whether gleon executed in Local Flat Mode (no storage configured).
    pub local_mode: bool,
}

/// Executes push pipeline, uploading baseline blobs to remote storage.
///
/// # Errors
/// Returns [`PushError`] if the workspace is not initialized, local blobs are missing,
/// or remote storage operations fail.
pub async fn push_blobs(
    context: &ResolvedContext,
    storage_config: Option<&StorageConfig>,
    all_platforms: bool,
    platform_override: Option<&str>,
) -> Result<PushResult, PushError> {
    let paths = ensure_initialized(&context.base_dir)?;

    let Some(storage_cfg) = active_storage_config(storage_config) else {
        return Ok(PushResult {
            local_mode: true,
            ..PushResult::default()
        });
    };

    let manifests_root = paths.manifests_root();
    let blobs_root = paths.blobs_root();

    let platform_dirs =
        resolve_platform_dirs(context, &manifests_root, all_platforms, platform_override)?;

    // Collect all referenced unique blob hashes and their metadata (for error reporting and cloud metadata)
    let hash_to_meta = collect_referenced_hashes(&platform_dirs)?;

    let total_manifest_blobs = hash_to_meta.len();
    if total_manifest_blobs == 0 {
        return Ok(PushResult {
            total_manifest_blobs: 0,
            uploaded_blobs: 0,
            skipped_blobs: 0,
            local_mode: false,
        });
    }

    let adapter = ObjectStoreAdapter::from_config(storage_cfg)?;

    // Query remote storage in batch per unique scheme using adapter.list_blobs()
    let mut unique_schemes = HashSet::new();
    for hash in hash_to_meta.keys() {
        unique_schemes.insert(hash.scheme());
    }

    let mut existing_remote_hashes = HashSet::new();
    for scheme in unique_schemes {
        let remote_hashes = adapter
            .list_blobs(scheme)
            .await
            .map_err(PushError::Storage)?;
        for val in remote_hashes {
            if let Ok(h) = crate::manifest::ImageHash::new(scheme, val) {
                existing_remote_hashes.insert(h);
            }
        }
    }

    let mut missing_blobs = Vec::new();
    let mut skipped_blobs = 0;

    for (hash, meta) in hash_to_meta {
        if existing_remote_hashes.contains(&hash) {
            skipped_blobs += 1;
        } else {
            if !crate::storage::has_usable_local_blob(&blobs_root, &hash) {
                return Err(PushError::MissingLocalBlob {
                    hash: hash.value().to_string(),
                    platform: meta.platform,
                });
            }
            missing_blobs.push((hash, meta));
        }
    }

    let missing_count = missing_blobs.len();

    // Upload missing blobs in parallel with Fail-Fast short-circuiting
    transfer_with_progress(missing_blobs, adapter.concurrency(), |(hash, meta), pb| {
        let adapter = adapter.clone();
        let src_path = blobs_root.join(hash.scheme()).join(hash.value());
        let blob_meta = crate::storage::BlobMetadata::new(&meta.test_name, &meta.platform);
        async move {
            pb.set_message(format!("Uploading {}", short_hash(&hash)));
            let res = adapter
                .upload_blob_with_metadata(&hash, &src_path, Some(&blob_meta))
                .await
                .map_err(PushError::Storage);
            pb.inc(1);
            res
        }
    })
    .await?;

    Ok(PushResult {
        total_manifest_blobs,
        uploaded_blobs: missing_count,
        skipped_blobs,
        local_mode: false,
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
    use super::*;
    use crate::{context::ContextError, platform::PlatformError};

    #[test]
    fn test_push_error_display() {
        let err1: PushError = CoreError::NotInitialized.into();
        assert!(err1.to_string().contains("not initialized"));

        let err2 = PushError::MissingLocalBlob {
            hash: "xyz".to_string(),
            platform: "linux-x86_64".to_string(),
        };
        assert!(err2.to_string().contains("Missing local blob"));
        assert!(err2.to_string().contains("xyz"));

        let err3: PushError = CoreError::Io(std::io::Error::other("io test")).into();
        assert!(err3.to_string().contains("IO error"));

        let err4: PushError = CoreError::Context(ContextError::Platform(
            PlatformError::InvalidSegment("bad".to_string()),
        ))
        .into();
        assert!(err4.to_string().contains("Context resolution error"));

        let err5 = PushError::Storage(StorageError::BlobNotFound("hash".to_string()));
        assert!(err5.to_string().contains("Storage error"));
    }

    #[test]
    fn test_push_result_derived() {
        let res = PushResult {
            total_manifest_blobs: 5,
            uploaded_blobs: 2,
            skipped_blobs: 3,
            local_mode: false,
        };
        assert_eq!(res.clone(), res);
        assert!(!format!("{res:?}").is_empty());
        let default_res = PushResult::default();
        assert_eq!(default_res.total_manifest_blobs, 0);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn test_push_platform_override_validation() {
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();

        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            ..ResolvedContext::default()
        };
        let cfg = StorageConfig::new("memory://");

        // Invalid platform override segment
        let err = push_blobs(&ctx, Some(&cfg), false, Some("../invalid")).await;
        assert!(matches!(
            err,
            Err(PushError::Core(CoreError::Context(ContextError::Platform(
                _
            ))))
        ));

        // Empty manifest directory for valid platform override -> 0 blobs
        let res = push_blobs(&ctx, Some(&cfg), false, Some("macos-aarch64"))
            .await
            .unwrap();
        assert_eq!(res.total_manifest_blobs, 0);
    }

    #[tokio::test]
    #[cfg(unix)]
    #[cfg_attr(miri, ignore)]
    async fn test_push_unreadable_manifests_root() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let manifests = temp.path().join(".gleon").join("manifests");
        std::fs::create_dir_all(&manifests).unwrap();

        let mut perms = std::fs::metadata(&manifests).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&manifests, perms.clone()).unwrap();

        let can_read = std::fs::read_dir(&manifests).is_ok();

        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            ..ResolvedContext::default()
        };
        let cfg = StorageConfig::new("memory://");
        let res = push_blobs(&ctx, Some(&cfg), true, None).await;

        perms.set_mode(0o755);
        std::fs::set_permissions(&manifests, perms).unwrap();

        if !can_read {
            assert!(matches!(res, Err(PushError::Core(CoreError::Io(_)))));
        }
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn test_push_invalid_platform_key() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = ResolvedContext::default();
        ctx.platform.os = "invalid/os".to_string(); // Will fail key()
        ctx.base_dir = temp.path().to_path_buf();

        std::fs::create_dir_all(temp.path().join(".gleon").join("manifests")).unwrap();

        let cfg = StorageConfig::new("memory://");
        let res = push_blobs(&ctx, Some(&cfg), false, None).await;
        assert!(matches!(
            res,
            Err(PushError::Core(CoreError::Context(ContextError::Platform(
                _
            ))))
        ));
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn test_push_invalid_manifest_load() {
        let temp = tempfile::tempdir().unwrap();

        let mut ctx = ResolvedContext::default();
        ctx.platform.os = "linux".to_string();
        ctx.base_dir = temp.path().to_path_buf();
        let key = ctx.platform.key().unwrap();

        let plat_dir = temp.path().join(".gleon").join("manifests").join(&key);
        std::fs::create_dir_all(&plat_dir).unwrap();
        std::fs::write(plat_dir.join("bad.json"), "not json").unwrap();

        let cfg = StorageConfig::new("memory://");
        let res = push_blobs(&ctx, Some(&cfg), false, None).await;
        assert!(matches!(res, Err(PushError::Core(CoreError::Manifest(_)))));
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn test_push_success_with_referenced_blob() {
        let temp = tempfile::tempdir().unwrap();

        let mut ctx = ResolvedContext::default();
        ctx.platform.os = "linux".to_string();
        ctx.base_dir = temp.path().to_path_buf();
        let key = ctx.platform.key().unwrap();

        let plat_dir = temp.path().join(".gleon").join("manifests").join(&key);
        std::fs::create_dir_all(&plat_dir).unwrap();

        let hash = "1111111111111111111111111111111111111111111111111111111111111111";
        let manifest = crate::manifest::SingleTestManifest::new(
            crate::manifest::ImageHash::new("sha256", hash).unwrap(),
            crate::manifest::ImageHash::new("dhash", "0000000000000000").unwrap(),
            1,
            1,
        )
        .unwrap();
        manifest.save(plat_dir.join("test.json")).unwrap();
        manifest.save(plat_dir.join("test2.json")).unwrap();

        // Create the local blob so validation passes
        let blobs_root = temp.path().join(".gleon").join("blobs");
        std::fs::create_dir_all(blobs_root.join("sha256")).unwrap();
        std::fs::write(blobs_root.join("sha256").join(hash), b"data").unwrap();

        let cfg = StorageConfig::new("memory://");
        let res = push_blobs(&ctx, Some(&cfg), false, None).await.unwrap();

        assert_eq!(res.total_manifest_blobs, 1);
        assert_eq!(res.uploaded_blobs, 1);
        assert_eq!(res.skipped_blobs, 0);
    }

    #[tokio::test]
    #[cfg(unix)]
    #[cfg_attr(miri, ignore)]
    async fn test_push_rejects_symlink_blob() {
        let temp = tempfile::tempdir().unwrap();

        let mut ctx = ResolvedContext::default();
        ctx.platform.os = "linux".to_string();
        ctx.base_dir = temp.path().to_path_buf();
        let key = ctx.platform.key().unwrap();

        let plat_dir = temp.path().join(".gleon").join("manifests").join(&key);
        std::fs::create_dir_all(&plat_dir).unwrap();

        let hash = "2222222222222222222222222222222222222222222222222222222222222222";
        let manifest = crate::manifest::SingleTestManifest::new(
            crate::manifest::ImageHash::new("sha256", hash).unwrap(),
            crate::manifest::ImageHash::new("dhash", "0000000000000000").unwrap(),
            1,
            1,
        )
        .unwrap();
        manifest.save(plat_dir.join("symlink_test.json")).unwrap();

        let blobs_root = temp.path().join(".gleon").join("blobs");
        std::fs::create_dir_all(blobs_root.join("sha256")).unwrap();
        let target_file = temp.path().join("target.txt");
        std::fs::write(&target_file, b"secret data").unwrap();
        std::os::unix::fs::symlink(&target_file, blobs_root.join("sha256").join(hash)).unwrap();

        let cfg = StorageConfig::new("memory://");
        let res = push_blobs(&ctx, Some(&cfg), false, None).await;
        assert!(matches!(res, Err(PushError::MissingLocalBlob { .. })));
    }

    #[tokio::test]
    #[cfg(all(unix, not(miri)))]
    async fn test_push_metadata_io_error_propagation() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();

        let mut ctx = ResolvedContext::default();
        ctx.platform.os = "linux".to_string();
        ctx.base_dir = temp.path().to_path_buf();
        let key = ctx.platform.key().unwrap();

        let manifests_dir = temp.path().join(".gleon").join("manifests");
        let plat_dir = manifests_dir.join(&key);
        std::fs::create_dir_all(&plat_dir).unwrap();

        // Set parent directory permissions to 000 so stat on plat_dir fails with PermissionDenied
        let original_perms = std::fs::metadata(&manifests_dir).unwrap().permissions();
        std::fs::set_permissions(&manifests_dir, std::fs::Permissions::from_mode(0o000)).unwrap();

        let cfg = StorageConfig::new("memory://");
        let res = push_blobs(&ctx, Some(&cfg), false, None).await;

        let was_permission_denied = std::fs::metadata(&plat_dir).is_err();
        let _ = std::fs::set_permissions(&manifests_dir, original_perms);

        if was_permission_denied {
            assert!(matches!(res, Err(PushError::Core(CoreError::Io(_)))));
        } else {
            // Superuser/root runners bypass 000 directory permissions
            assert!(res.is_ok());
        }
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn test_push_skips_when_blob_already_in_remote_even_if_missing_locally() {
        use crate::storage::ObjectStoreAdapter;

        let temp = tempfile::tempdir().unwrap();

        let mut ctx = ResolvedContext::default();
        ctx.platform.os = "linux".to_string();
        ctx.base_dir = temp.path().to_path_buf();
        let key = ctx.platform.key().unwrap();

        let plat_dir = temp.path().join(".gleon").join("manifests").join(&key);
        std::fs::create_dir_all(&plat_dir).unwrap();

        let hash_str = "3333333333333333333333333333333333333333333333333333333333333333";
        let image_hash = crate::manifest::ImageHash::new("sha256", hash_str).unwrap();
        let manifest = crate::manifest::SingleTestManifest::new(
            image_hash.clone(),
            crate::manifest::ImageHash::new("dhash", "0000000000000000").unwrap(),
            1,
            1,
        )
        .unwrap();
        manifest.save(plat_dir.join("test_remote.json")).unwrap();

        let remote_dir = temp.path().join("remote_storage");
        std::fs::create_dir_all(&remote_dir).unwrap();
        let cfg = StorageConfig::new(format!("file://{}", remote_dir.display()));
        let adapter = ObjectStoreAdapter::from_config(&cfg).unwrap();

        // Pre-populate remote storage with the blob directly
        let tmp_file = temp.path().join("tmp_blob.png");
        std::fs::write(&tmp_file, b"remote blob data").unwrap();
        adapter.upload_blob(&image_hash, &tmp_file).await.unwrap();

        // Local blob directory does NOT have the blob
        let res = push_blobs(&ctx, Some(&cfg), false, None).await.unwrap();

        assert_eq!(res.total_manifest_blobs, 1);
        assert_eq!(res.uploaded_blobs, 0);
        assert_eq!(res.skipped_blobs, 1);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn test_push_fails_fast_when_blob_missing_both_remotely_and_locally() {
        let temp = tempfile::tempdir().unwrap();

        let mut ctx = ResolvedContext::default();
        ctx.platform.os = "linux".to_string();
        ctx.base_dir = temp.path().to_path_buf();
        let key = ctx.platform.key().unwrap();

        let plat_dir = temp.path().join(".gleon").join("manifests").join(&key);
        std::fs::create_dir_all(&plat_dir).unwrap();

        let hash_str = "4444444444444444444444444444444444444444444444444444444444444444";
        let image_hash = crate::manifest::ImageHash::new("sha256", hash_str).unwrap();
        let manifest = crate::manifest::SingleTestManifest::new(
            image_hash,
            crate::manifest::ImageHash::new("dhash", "0000000000000000").unwrap(),
            1,
            1,
        )
        .unwrap();
        manifest.save(plat_dir.join("test_missing.json")).unwrap();

        let cfg = StorageConfig::new("memory://");
        let res = push_blobs(&ctx, Some(&cfg), false, None).await;

        assert!(matches!(res, Err(PushError::MissingLocalBlob { .. })));
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn test_push_batch_partial_remote_and_local_upload() {
        use crate::storage::ObjectStoreAdapter;

        let temp = tempfile::tempdir().unwrap();

        let mut ctx = ResolvedContext::default();
        ctx.platform.os = "linux".to_string();
        ctx.base_dir = temp.path().to_path_buf();
        let key = ctx.platform.key().unwrap();

        let plat_dir = temp.path().join(".gleon").join("manifests").join(&key);
        std::fs::create_dir_all(&plat_dir).unwrap();

        // 1. Blob that exists remotely (should be skipped)
        let hash_remote_str = "1111111111111111111111111111111111111111111111111111111111111111";
        let image_hash_remote = crate::manifest::ImageHash::new("sha256", hash_remote_str).unwrap();
        let manifest_remote = crate::manifest::SingleTestManifest::new(
            image_hash_remote.clone(),
            crate::manifest::ImageHash::new("dhash", "0000000000000000").unwrap(),
            1,
            1,
        )
        .unwrap();
        manifest_remote
            .save(plat_dir.join("test_remote.json"))
            .unwrap();

        // 2. Blob that is missing remotely but exists locally (should be uploaded)
        let hash_local_str = "2222222222222222222222222222222222222222222222222222222222222222";
        let image_hash_local = crate::manifest::ImageHash::new("sha256", hash_local_str).unwrap();
        let manifest_local = crate::manifest::SingleTestManifest::new(
            image_hash_local.clone(),
            crate::manifest::ImageHash::new("dhash", "0000000000000000").unwrap(),
            1,
            1,
        )
        .unwrap();
        manifest_local
            .save(plat_dir.join("test_local.json"))
            .unwrap();

        let blobs_dir = temp.path().join(".gleon").join("blobs").join("sha256");
        std::fs::create_dir_all(&blobs_dir).unwrap();
        std::fs::write(blobs_dir.join(hash_local_str), b"local blob data").unwrap();

        let remote_dir = temp.path().join("remote_storage");
        std::fs::create_dir_all(&remote_dir).unwrap();
        let cfg = StorageConfig::new(format!("file://{}", remote_dir.display()));
        let adapter = ObjectStoreAdapter::from_config(&cfg).unwrap();

        // Pre-populate remote storage with the remote blob only
        let tmp_file = temp.path().join("tmp_blob.png");
        std::fs::write(&tmp_file, b"remote blob data").unwrap();
        adapter
            .upload_blob(&image_hash_remote, &tmp_file)
            .await
            .unwrap();

        let res = push_blobs(&ctx, Some(&cfg), false, None).await.unwrap();

        assert_eq!(res.total_manifest_blobs, 2);
        assert_eq!(res.uploaded_blobs, 1);
        assert_eq!(res.skipped_blobs, 1);
        assert!(!res.local_mode);
    }
}
