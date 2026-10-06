//! Pull operation for downloading missing baseline blobs from remote storage.

use std::path::Path;

use thiserror::Error;

use crate::{
    context::ResolvedContext,
    manifest::{ImageHash, WorkspaceIndex},
    ops::{
        common::{CoreError, ensure_initialized},
        sync::{active_storage_config, resolve_platform_dirs, short_hash, transfer_with_progress},
    },
    storage::{ObjectStoreAdapter, StorageConfig, StorageError},
};

/// Errors that can occur during a pull operation.
#[derive(Debug, Error)]
pub enum PullError {
    /// Missing remote blob on Object Store.
    #[error(
        "Missing remote blob for hash '{hash}' referenced in manifest at platform '{platform}'. Blob was not found in remote storage."
    )]
    MissingRemoteBlob {
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

/// Summary of pull operation results.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PullResult {
    /// Total unique baseline blobs referenced across scanned platforms.
    pub total_manifest_blobs: usize,
    /// Number of blobs downloaded from remote storage.
    pub downloaded_blobs: usize,
    /// Number of blobs already present in local storage.
    pub skipped_blobs: usize,
    /// Indicates whether gleon executed in Local Flat Mode (no storage configured).
    pub local_mode: bool,
}

/// Registers blobs referenced by the current platform's manifest, then — if a fallback platform
/// is configured and differs from it — registers blobs for fallback entries whose test name is
/// absent from the current platform's index (sparse-platform support).
fn register_current_platform_with_fallback(
    context: &ResolvedContext,
    manifests_root: &Path,
    register_blob: &mut impl FnMut(&ImageHash, &str),
) -> Result<(), CoreError> {
    let platform_key = crate::ops::common::platform_key(context)?;
    let plat_dir = manifests_root.join(&platform_key);
    let plat_idx = WorkspaceIndex::load(&plat_dir).map_err(CoreError::Manifest)?;

    for manifest in plat_idx.entries().values() {
        register_blob(&manifest.hash, platform_key.as_str());
    }

    if let Some(fallback_key) = context
        .fallback_platform_key
        .as_ref()
        .filter(|&k| *k != platform_key)
    {
        let fb_dir = manifests_root.join(fallback_key);
        let fb_idx = WorkspaceIndex::load(&fb_dir).map_err(CoreError::Manifest)?;
        if !fb_idx.is_empty() {
            tracing::info!(
                "Using fallback platform '{}' for missing blobs on platform '{}'.",
                fallback_key,
                platform_key
            );
            for (test_name, manifest) in fb_idx.entries() {
                if !plat_idx.entries().contains_key(test_name) {
                    register_blob(&manifest.hash, fallback_key.as_str());
                }
            }
        }
    }

    Ok(())
}

/// Executes pull pipeline, downloading missing baseline blobs from remote storage.
///
/// # Errors
/// Returns [`PullError`] if the workspace is not initialized, remote blobs are missing,
/// or remote storage operations fail.
pub async fn pull_blobs(
    context: &ResolvedContext,
    storage_config: Option<&StorageConfig>,
    all_platforms: bool,
    platform_override: Option<&str>,
) -> Result<PullResult, PullError> {
    let paths = ensure_initialized(&context.base_dir)?;

    let Some(storage_cfg) = active_storage_config(storage_config) else {
        return Ok(PullResult {
            local_mode: true,
            ..PullResult::default()
        });
    };

    let manifests_root = paths.manifests_root();
    let blobs_root = paths.blobs_root();

    let mut referenced_hashes = std::collections::BTreeSet::new();
    let mut missing_blobs = Vec::new();
    let mut skipped_blobs = 0;

    let mut register_blob = |hash: &ImageHash, origin_platform: &str| {
        if referenced_hashes.insert(hash.clone()) {
            if crate::storage::has_usable_local_blob(&blobs_root, hash) {
                skipped_blobs += 1;
            } else {
                missing_blobs.push((hash.clone(), origin_platform.to_string()));
            }
        }
    };

    if all_platforms || platform_override.is_some() {
        let platform_dirs =
            resolve_platform_dirs(context, &manifests_root, all_platforms, platform_override)?;
        for (target_platform_key, target_dir) in platform_dirs {
            let index = WorkspaceIndex::load(&target_dir).map_err(CoreError::Manifest)?;
            for manifest in index.entries().values() {
                register_blob(&manifest.hash, &target_platform_key);
            }
        }
    } else {
        register_current_platform_with_fallback(context, &manifests_root, &mut register_blob)?;
    }

    let total_manifest_blobs = referenced_hashes.len();

    if missing_blobs.is_empty() {
        return Ok(PullResult {
            total_manifest_blobs,
            downloaded_blobs: 0,
            skipped_blobs,
            local_mode: false,
        });
    }

    let adapter = ObjectStoreAdapter::from_config(storage_cfg)?;

    let missing_count = missing_blobs.len();

    transfer_with_progress(
        missing_blobs,
        adapter.concurrency(),
        |(hash, origin_platform), pb| {
            let adapter = adapter.clone();
            let dest_path = blobs_root.join(hash.scheme()).join(hash.value());
            async move {
                pb.set_message(format!("Downloading {}", short_hash(&hash)));
                let res = match adapter.download_blob(&hash, &dest_path).await {
                    Ok(()) => Ok(()),
                    Err(StorageError::BlobNotFound(_)) => Err(PullError::MissingRemoteBlob {
                        hash: hash.value().to_string(),
                        platform: origin_platform,
                    }),
                    Err(e) => Err(PullError::Storage(e)),
                };
                pb.inc(1);
                res
            }
        },
    )
    .await?;

    Ok(PullResult {
        total_manifest_blobs,
        downloaded_blobs: missing_count,
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
    use gleon_model::platform::PlatformKey;

    use super::*;
    use crate::{context::ContextError, platform::PlatformError};

    /// Writes `content` to `dir/<name>` and returns the file with its content address: downloads
    /// check it.
    fn blob_file(dir: &Path, name: &str, content: &str) -> (std::path::PathBuf, ImageHash) {
        use sha2::Digest as _;
        let file = dir.join(name);
        std::fs::write(&file, content).unwrap();
        let hash = ImageHash::new("sha256", hex::encode(sha2::Sha256::digest(content))).unwrap();
        (file, hash)
    }

    #[test]
    fn test_pull_error_display() {
        let err1: PullError = CoreError::NotInitialized.into();
        assert!(err1.to_string().contains("not initialized"));

        let err2 = PullError::MissingRemoteBlob {
            hash: "xyz".to_string(),
            platform: "linux-x86_64".to_string(),
        };
        assert!(err2.to_string().contains("Missing remote blob"));
        assert!(err2.to_string().contains("xyz"));

        let err3: PullError = CoreError::Io(std::io::Error::other("io test")).into();
        assert!(err3.to_string().contains("IO error"));

        let err4: PullError = CoreError::Context(ContextError::Platform(
            PlatformError::InvalidSegment("bad".to_string()),
        ))
        .into();
        assert!(err4.to_string().contains("Context resolution error"));

        let err5 = PullError::Storage(StorageError::BlobNotFound("hash".to_string()));
        assert!(err5.to_string().contains("Storage error"));
    }

    #[test]
    fn test_pull_result_derived() {
        let res = PullResult {
            total_manifest_blobs: 10,
            downloaded_blobs: 4,
            skipped_blobs: 6,
            local_mode: false,
        };
        assert_eq!(res.clone(), res);
        assert!(!format!("{res:?}").is_empty());
        let default_res = PullResult::default();
        assert_eq!(default_res.total_manifest_blobs, 0);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn test_pull_platform_override_validation() {
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();

        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            ..ResolvedContext::default()
        };
        let cfg = StorageConfig::new("memory://");

        // Invalid platform override segment
        let res = pull_blobs(&ctx, Some(&cfg), false, Some("../invalid")).await;
        assert!(matches!(
            res,
            Err(PullError::Core(CoreError::Context(ContextError::Platform(
                _
            ))))
        ));

        // Empty manifest directory for valid platform override -> 0 blobs
        let res = pull_blobs(&ctx, Some(&cfg), false, Some("macos-aarch64"))
            .await
            .unwrap();
        assert_eq!(res.total_manifest_blobs, 0);
    }

    #[cfg(all(unix, not(miri)))]
    #[tokio::test]
    async fn test_pull_manifests_root_unreadable() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: `libc::geteuid()` is a side-effect-free POSIX syscall query that returns the process EUID.
        #[expect(
            unsafe_code,
            reason = "`libc::geteuid()` is a side-effect-free POSIX query (see SAFETY comment)"
        )]
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        let manifests_dir = gleon_dir.join("manifests");
        std::fs::create_dir_all(&manifests_dir).unwrap();

        let mut perms = std::fs::metadata(&manifests_dir).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&manifests_dir, perms.clone()).unwrap();

        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            ..ResolvedContext::default()
        };
        let cfg = StorageConfig::new("memory://");
        let res = pull_blobs(&ctx, Some(&cfg), true, None).await;

        perms.set_mode(0o755);
        let _ = std::fs::set_permissions(&manifests_dir, perms);

        assert!(matches!(res, Err(PullError::Core(CoreError::Io(_)))));
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn test_pull_invalid_platform_key() {
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();
        let mut ctx = ResolvedContext::default();
        ctx.platform.os = "invalid/os".to_string();
        ctx.base_dir = temp.path().to_path_buf();

        let cfg = StorageConfig::new("memory://");
        let res = pull_blobs(&ctx, Some(&cfg), false, None).await;
        assert!(matches!(
            res,
            Err(PullError::Core(CoreError::Context(ContextError::Platform(
                _
            ))))
        ));
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn test_pull_empty_fallback_platform() {
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");

        let plat_key = "linux-x86_64";
        let fb_key = "macos-aarch64";
        std::fs::create_dir_all(gleon_dir.join("manifests").join(plat_key)).unwrap();
        std::fs::create_dir_all(gleon_dir.join("manifests").join(fb_key)).unwrap();

        let mut ctx = ResolvedContext::default();
        ctx.platform.os = "linux".to_string();
        ctx.platform.arch = Some("x86_64".to_string());
        ctx.platform.renderer = None;
        ctx.fallback_platform_key = Some(PlatformKey::parse(fb_key).unwrap());
        ctx.base_dir = temp.path().to_path_buf();

        let cfg = StorageConfig::new("memory://");
        let res = pull_blobs(&ctx, Some(&cfg), false, None).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap().total_manifest_blobs, 0);
    }

    #[cfg(all(unix, not(miri)))]
    #[tokio::test]
    async fn test_pull_storage_io_error() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: `libc::geteuid()` is a side-effect-free POSIX syscall query that returns the process EUID.
        #[expect(
            unsafe_code,
            reason = "`libc::geteuid()` is a side-effect-free POSIX query (see SAFETY comment)"
        )]
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        std::fs::create_dir_all(gleon_dir.join("manifests")).unwrap();

        let plat_key = "linux-x86_64";
        let manifests_dir = gleon_dir.join("manifests").join(plat_key);
        std::fs::create_dir_all(&manifests_dir).unwrap();

        let hash = "1111111111111111111111111111111111111111111111111111111111111111";
        let manifest = crate::manifest::SingleTestManifest::new(
            ImageHash::new("sha256", hash).unwrap(),
            ImageHash::new("dhash", "0000000000000000").unwrap(),
            1,
            1,
        )
        .unwrap();
        manifest.save(manifests_dir.join("test.json")).unwrap();

        let mut ctx = ResolvedContext::default();
        ctx.platform.os = "linux".to_string();
        ctx.platform.arch = Some("x86_64".to_string());
        ctx.base_dir = temp.path().to_path_buf();

        let remote_dir = temp.path().join("remote_blobs");
        std::fs::create_dir_all(&remote_dir).unwrap();
        // Use a valid file storage URL
        let cfg = StorageConfig::new(format!("file://{}", remote_dir.display()));
        let adapter = ObjectStoreAdapter::from_config(&cfg).unwrap();
        // Create an empty file to upload
        let empty_file = temp.path().join("empty");
        std::fs::write(&empty_file, "").unwrap();
        // Upload the dummy blob so it exists remotely
        adapter
            .upload_blob(&ImageHash::new("sha256", hash).unwrap(), &empty_file)
            .await
            .expect("upload dummy blob");

        // Make the local blobs directory unreadable to trigger an IO error during download
        let local_blobs = temp.path().join(".gleon").join("blobs");
        std::fs::create_dir_all(&local_blobs).unwrap();

        let mut perms = std::fs::metadata(&local_blobs).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&local_blobs, perms.clone()).unwrap();

        let res = pull_blobs(&ctx, Some(&cfg), false, None).await;

        perms.set_mode(0o755);
        let _ = std::fs::set_permissions(&local_blobs, perms);

        assert!(matches!(res, Err(PullError::Storage(_))));
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn test_pull_fallback_platform_and_duplicate_hashes() {
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();

        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            fallback_platform_key: Some(PlatformKey::parse("fallback-platform").unwrap()),
            ..Default::default()
        };

        let cfg = StorageConfig::new("memory://");
        let hash = ImageHash::new(
            "sha256",
            "1111111111111111111111111111111111111111111111111111111111111111",
        )
        .unwrap();
        let phash = ImageHash::new("dhash", "0000000000000000").unwrap();

        // 1. Create manifests in fallback-platform directory
        let fallback_manifests_dir = gleon_dir.join("manifests").join("fallback-platform");
        std::fs::create_dir_all(&fallback_manifests_dir).unwrap();

        // Write two manifests with identical hash to test deduplication (`referenced_hashes.insert(...) == false`)
        let m1 =
            crate::manifest::SingleTestManifest::new(hash.clone(), phash.clone(), 10, 10).unwrap();
        let m2 = crate::manifest::SingleTestManifest::new(hash, phash, 10, 10).unwrap();
        m1.save(fallback_manifests_dir.join("test1.json")).unwrap();
        m2.save(fallback_manifests_dir.join("test2.json")).unwrap();

        let blob_path = gleon_dir
            .join("blobs")
            .join("sha256")
            .join("1111111111111111111111111111111111111111111111111111111111111111");
        if let Some(parent) = blob_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&blob_path, "blob content").unwrap();

        let res = pull_blobs(&ctx, Some(&cfg), false, None).await.unwrap();

        assert_eq!(res.total_manifest_blobs, 1);
        assert_eq!(res.skipped_blobs, 1);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn test_pull_sparse_platform_with_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();

        let linux_key = "linux-x86_64";
        let macos_key = "macos-aarch64";

        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            platform: crate::platform::PlatformInfo {
                os: "linux".to_string(),
                arch: Some("x86_64".to_string()),
                renderer: None,
                labels: std::collections::BTreeMap::new(),
            },
            fallback_platform_key: Some(PlatformKey::parse(macos_key).unwrap()),
            ..Default::default()
        };

        let remote_temp = tempfile::tempdir().unwrap();
        let cfg = StorageConfig::new(format!("file://{}", remote_temp.path().display()));
        let adapter = ObjectStoreAdapter::from_config(&cfg).unwrap();

        let (file1, hash1) = blob_file(temp.path(), "blob1", "data 1");
        let (file2, hash2) = blob_file(temp.path(), "blob2", "data 2");
        let (dummy_file, dummy_hash) = blob_file(temp.path(), "blob3", "data 3");

        let phash = ImageHash::new("dhash", "0000000000000000").unwrap();

        // Upload blobs to remote
        adapter.upload_blob(&hash1, &file1).await.unwrap();
        adapter.upload_blob(&hash2, &file2).await.unwrap();
        adapter.upload_blob(&dummy_hash, &dummy_file).await.unwrap();

        // 1. Fallback manifests (macos) contains test1 (pointing to dummy_hash) and test2 (pointing to hash2)
        let macos_manifests_dir = gleon_dir.join("manifests").join(macos_key);
        std::fs::create_dir_all(&macos_manifests_dir).unwrap();
        let m_mac1 =
            crate::manifest::SingleTestManifest::new(dummy_hash.clone(), phash.clone(), 10, 10)
                .unwrap();
        let m_mac2 =
            crate::manifest::SingleTestManifest::new(hash2.clone(), phash.clone(), 20, 20).unwrap();
        m_mac1.save(macos_manifests_dir.join("test1.json")).unwrap();
        m_mac2.save(macos_manifests_dir.join("test2.json")).unwrap();

        // 2. Linux manifests contains ONLY test1 override (pointing to hash1)
        let linux_manifests_dir = gleon_dir.join("manifests").join(linux_key);
        std::fs::create_dir_all(&linux_manifests_dir).unwrap();
        let m_lin1 =
            crate::manifest::SingleTestManifest::new(hash1.clone(), phash.clone(), 10, 10).unwrap();
        m_lin1.save(linux_manifests_dir.join("test1.json")).unwrap();

        let res = pull_blobs(&ctx, Some(&cfg), false, None).await.unwrap();

        assert_eq!(res.total_manifest_blobs, 2);
        assert_eq!(res.downloaded_blobs, 2);
        assert_eq!(res.skipped_blobs, 0);

        // Verify local blobs: hash1 and hash2 exist, dummy_hash was NOT downloaded
        let local_blobs = gleon_dir.join("blobs").join("sha256");
        assert!(local_blobs.join(hash1.value()).exists());
        assert!(local_blobs.join(hash2.value()).exists());
        assert!(!local_blobs.join(dummy_hash.value()).exists());
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn test_pull_uninitialized_workspace_fails() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            ..ResolvedContext::default()
        };
        let cfg = StorageConfig::new("memory://");
        let res = pull_blobs(&ctx, Some(&cfg), false, None).await;
        assert!(matches!(
            res,
            Err(PullError::Core(CoreError::NotInitialized))
        ));
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn test_pull_local_mode_without_storage_or_empty_url() {
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();

        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            ..ResolvedContext::default()
        };
        // 1. None storage config
        let res_none = pull_blobs(&ctx, None, false, None).await.unwrap();
        assert!(res_none.local_mode);
        assert_eq!(res_none.total_manifest_blobs, 0);

        // 2. Empty URL storage config
        let empty_cfg = StorageConfig::new("   ");
        let res_empty = pull_blobs(&ctx, Some(&empty_cfg), false, None)
            .await
            .unwrap();
        assert!(res_empty.local_mode);
        assert_eq!(res_empty.total_manifest_blobs, 0);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn test_pull_all_platforms_with_manifests() {
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();

        let remote_dir = temp.path().join("remote_blobs");
        std::fs::create_dir_all(&remote_dir).unwrap();
        let cfg = StorageConfig::new(format!("file://{}", remote_dir.display()));
        let adapter = ObjectStoreAdapter::from_config(&cfg).unwrap();

        let (dummy_file, hash_a) = blob_file(temp.path(), "dummy", "blob data");
        let phash = ImageHash::new("dhash", "0000000000000000").unwrap();
        adapter.upload_blob(&hash_a, &dummy_file).await.unwrap();

        let plat_dir = gleon_dir.join("manifests").join("linux-x86_64");
        std::fs::create_dir_all(&plat_dir).unwrap();
        let manifest =
            crate::manifest::SingleTestManifest::new(hash_a.clone(), phash, 10, 10).unwrap();
        manifest.save(plat_dir.join("test_a.json")).unwrap();

        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            ..ResolvedContext::default()
        };
        let res = pull_blobs(&ctx, Some(&cfg), true, None).await.unwrap();
        assert_eq!(res.total_manifest_blobs, 1);
        assert_eq!(res.downloaded_blobs, 1);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn test_pull_platform_override_with_manifests() {
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();

        let remote_dir = temp.path().join("remote_blobs");
        std::fs::create_dir_all(&remote_dir).unwrap();
        let cfg = StorageConfig::new(format!("file://{}", remote_dir.display()));
        let adapter = ObjectStoreAdapter::from_config(&cfg).unwrap();

        let (dummy_file, hash_b) = blob_file(temp.path(), "dummy", "blob data b");
        let phash = ImageHash::new("dhash", "0000000000000000").unwrap();
        adapter.upload_blob(&hash_b, &dummy_file).await.unwrap();

        let plat_dir = gleon_dir.join("manifests").join("macos-aarch64");
        std::fs::create_dir_all(&plat_dir).unwrap();
        let manifest =
            crate::manifest::SingleTestManifest::new(hash_b.clone(), phash, 10, 10).unwrap();
        manifest.save(plat_dir.join("test_b.json")).unwrap();

        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            ..ResolvedContext::default()
        };
        let res = pull_blobs(&ctx, Some(&cfg), false, Some("macos-aarch64"))
            .await
            .unwrap();
        assert_eq!(res.total_manifest_blobs, 1);
        assert_eq!(res.downloaded_blobs, 1);
    }
}
