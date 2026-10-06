//! Shared internals for the `push`/`pull` blob-transfer operations, and the groundwork a future
//! `gc` operation will build on (platform-dir enumeration + referenced-hash collection).
//!
//! `push_blobs` and `pull_blobs` each resolve which platform manifest directories to operate on,
//! collect the blob hashes those manifests reference, and stream blobs to/from remote storage
//! behind a progress bar. This module factors out the parts that are identical (or differ only
//! in the per-item transfer closure) so each op keeps just its direction-specific logic.

use std::{
    collections::BTreeMap,
    future::Future,
    path::{Path, PathBuf},
};

use futures::{StreamExt as _, TryStreamExt as _};
use indicatif::ProgressBar;

use crate::{
    context::{ContextError, ResolvedContext},
    manifest::{ImageHash, WorkspaceIndex},
    ops::common::CoreError,
    storage::StorageConfig,
};

/// Truncates a hash's hex value to its first 8 characters (or fewer, if shorter) for a
/// human-readable progress-bar message — not for any addressing/lookup purpose.
#[must_use]
pub fn short_hash(hash: &ImageHash) -> &str {
    let value = hash.value();
    &value[..8.min(value.len())]
}

/// Discovers valid platform directories under `.gleon/manifests/`.
///
/// A "valid" entry is a directory whose name doesn't start with `.` and contains only
/// `[a-zA-Z0-9_.:-]` characters (the same alphabet accepted by platform key segments).
/// Returns an empty list (not an error) if `manifests_root` doesn't exist yet.
pub fn list_platform_dirs(manifests_root: &Path) -> Result<Vec<(String, PathBuf)>, std::io::Error> {
    let entries = match std::fs::read_dir(manifests_root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    let mut platforms = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let is_dir = std::fs::metadata(&path)?.is_dir();
        let valid_name = entry
            .file_name()
            .to_str()
            .filter(|_| is_dir)
            .filter(|n| !n.starts_with('.'))
            .and_then(|n| {
                if n.chars().all(gleon_model::platform::is_key_char) {
                    Some(n.to_string())
                } else {
                    None
                }
            });

        if let Some(name) = valid_name {
            platforms.push((name, path));
        }
    }
    platforms.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(platforms)
}

/// Resolves the platform manifest directories a `push`/`pull` invocation should operate on:
/// every platform under `manifests_root` (`all_platforms`), one explicit platform
/// (`platform_override`), or just the current platform resolved from `context`.
///
/// Callers that need fallback-platform merging semantics (as `pull_blobs`'s default case does)
/// should handle that separately; this only covers the shared "which directories" resolution.
///
/// # Errors
/// Returns [`CoreError::Io`] if `manifests_root` fails to read, or [`CoreError::Context`] if
/// `platform_override` is no platform key or the context's platform identity fails segment
/// validation.
pub fn resolve_platform_dirs(
    context: &ResolvedContext,
    manifests_root: &Path,
    all_platforms: bool,
    platform_override: Option<&str>,
) -> Result<Vec<(String, PathBuf)>, CoreError> {
    if all_platforms {
        list_platform_dirs(manifests_root).map_err(CoreError::Io)
    } else if let Some(p) = platform_override {
        let valid_key = gleon_model::platform::validate_key(p)
            .map_err(|e| CoreError::Context(ContextError::Platform(e)))?
            .into_owned();
        Ok(vec![(valid_key.clone(), manifests_root.join(valid_key))])
    } else {
        let platform_key = super::common::platform_key(context)?;
        Ok(vec![(
            platform_key.to_string(),
            manifests_root.join(platform_key),
        )])
    }
}

/// Metadata associated with a referenced blob in the workspace index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferencedBlobMeta {
    /// Platform key where the blob was referenced.
    pub platform: String,
    /// Normalized test case path referencing the blob (e.g. `auth/login_screen`).
    pub test_name: String,
}

/// Loads each platform directory's manifest index and collects the unique blob hashes they
/// reference, mapped to their (first) referencing platform and test name metadata.
///
/// Directories that don't exist (e.g. a platform never staged) are silently skipped, matching
/// `push_blobs`'s original behavior.
///
/// If multiple tests or platforms reference the same content hash (CAS deduplication), metadata
/// selection is deterministic: it preserves the *first lexicographically visited* reference.
/// Platforms in `platform_dirs` are visited in order (typically sorted via [`list_platform_dirs`]),
/// and tests within each platform's [`WorkspaceIndex`] are visited in sorted order.
///
/// This is also the piece a future `gc` operation needs: call [`list_platform_dirs`] with
/// `manifests_root` to get every platform, then this function to get the full referenced-hash
/// set to diff against local/remote blob listings before deleting anything unreferenced.
///
/// # Errors
/// Returns [`CoreError::Io`] if a directory's metadata can't be queried, or [`CoreError::Manifest`]
/// if a manifest index fails to load.
pub fn collect_referenced_hashes(
    platform_dirs: &[(String, PathBuf)],
) -> Result<BTreeMap<ImageHash, ReferencedBlobMeta>, CoreError> {
    let mut hash_to_meta = BTreeMap::new();

    for (plat_key, plat_dir) in platform_dirs {
        match std::fs::metadata(plat_dir) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(CoreError::Io(e)),
        }
        let index = WorkspaceIndex::load(plat_dir).map_err(CoreError::Manifest)?;
        for (test_name, manifest) in index.entries() {
            hash_to_meta
                .entry(manifest.hash.clone())
                .or_insert_with(|| ReferencedBlobMeta {
                    platform: plat_key.clone(),
                    test_name: test_name.clone(),
                });
        }
    }

    Ok(hash_to_meta)
}

/// Returns `storage_config` if it's set and non-blank, or logs an informational "local mode"
/// message and returns `None` otherwise.
///
/// Callers early-return their operation's zeroed `*Result { local_mode: true, .. }` when this
/// returns `None`.
pub fn active_storage_config(storage_config: Option<&StorageConfig>) -> Option<&StorageConfig> {
    match storage_config {
        Some(cfg) if !cfg.url.trim().is_empty() => Some(cfg),
        _ => {
            tracing::info!(
                "Operating in local mode. Cloud sync disabled. Please configure storage."
            );
            None
        }
    }
}

/// Runs `task` for each item in `items` with up to `concurrency` transfers in flight, tracked by
/// a shared progress bar, short-circuiting on the first error (remaining in-flight tasks are
/// still drained by `buffer_unordered`, but no further items are started).
///
/// # Errors
/// Returns the first `E` produced by any invocation of `task`.
pub async fn transfer_with_progress<T, E, F, Fut>(
    items: Vec<T>,
    concurrency: usize,
    task: F,
) -> Result<(), E>
where
    T: Send,
    F: Fn(T, ProgressBar) -> Fut + Sync,
    Fut: Future<Output = Result<(), E>> + Send,
{
    let progress_bar = crate::ui::create_progress_bar(items.len() as u64);

    let concurrency = concurrency.max(1);
    let mut stream = futures::stream::iter(
        items
            .into_iter()
            .map(|item| task(item, progress_bar.clone())),
    )
    .buffer_unordered(concurrency);

    let result = async {
        while stream.try_next().await? == Some(()) {}
        Ok::<(), E>(())
    }
    .await;

    progress_bar.finish_and_clear();
    result
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
    fn test_short_hash_truncates_without_panicking_on_short_values() {
        let sha = ImageHash::new("sha256", "a".repeat(64)).unwrap();
        assert_eq!(short_hash(&sha), &"a".repeat(8));

        // Non-sha256 schemes accept any non-empty alphanumeric value, including ones
        // shorter than the 8-byte truncation window — slicing must clamp, not panic.
        for value in ["abc", "a", "12345678", "123456789"] {
            let hash = ImageHash::new("dhash", value).unwrap();
            let expected = &value[..8.min(value.len())];
            assert_eq!(short_hash(&hash), expected, "value {value:?}");
        }
    }

    #[test]
    fn test_list_platform_dirs_filters_invalid_entries() {
        let temp = tempdir().unwrap();
        let manifests = temp.path().join("manifests");
        std::fs::create_dir_all(manifests.join("valid-platform")).unwrap();
        // Keys with a renderer and labels (`PlatformInfo::key`).
        std::fs::create_dir_all(manifests.join("linux-x86_64+chrome+theme=dark")).unwrap();
        std::fs::create_dir_all(manifests.join("invalid platform space")).unwrap();
        std::fs::create_dir_all(manifests.join("5:linux-6:x86_64")).unwrap();
        std::fs::write(manifests.join("some_file.txt"), "hello").unwrap();

        let res = list_platform_dirs(&manifests).unwrap();
        let names: Vec<_> = res.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["linux-x86_64+chrome+theme=dark", "valid-platform"]);
    }

    #[test]
    fn test_list_platform_dirs_missing_root() {
        let temp = tempdir().unwrap();
        let manifests = temp.path().join("does_not_exist");
        let res = list_platform_dirs(&manifests).unwrap();
        assert!(res.is_empty());
    }

    #[test]
    fn test_resolve_platform_dirs_all_platforms() {
        let temp = tempdir().unwrap();
        let manifests = temp.path().join("manifests");
        std::fs::create_dir_all(manifests.join("linux")).unwrap();
        std::fs::create_dir_all(manifests.join("macos")).unwrap();

        let ctx = ResolvedContext::default();
        let res = resolve_platform_dirs(&ctx, &manifests, true, None).unwrap();
        assert_eq!(res.len(), 2);
    }

    #[test]
    fn test_resolve_platform_dirs_override_validates_segment() {
        let temp = tempdir().unwrap();
        let manifests = temp.path().join("manifests");
        let ctx = ResolvedContext::default();

        let err = resolve_platform_dirs(&ctx, &manifests, false, Some("../invalid")).unwrap_err();
        assert!(matches!(err, CoreError::Context(ContextError::Platform(_))));

        let res = resolve_platform_dirs(&ctx, &manifests, false, Some("macos-aarch64")).unwrap();
        assert_eq!(
            res,
            vec![("macos-aarch64".to_string(), manifests.join("macos-aarch64"))]
        );
    }

    #[test]
    fn test_resolve_platform_dirs_current_platform() {
        let temp = tempdir().unwrap();
        let manifests = temp.path().join("manifests");
        let mut ctx = ResolvedContext::default();
        ctx.platform.os = "linux".to_string();

        let res = resolve_platform_dirs(&ctx, &manifests, false, None).unwrap();
        assert_eq!(res, vec![("linux".to_string(), manifests.join("linux"))]);
    }

    #[test]
    fn test_collect_referenced_hashes_skips_missing_dirs_and_dedupes() {
        let temp = tempdir().unwrap();
        let plat_dir = temp.path().join("linux");
        std::fs::create_dir_all(&plat_dir).unwrap();

        let hash = "1111111111111111111111111111111111111111111111111111111111111111";
        let manifest = crate::manifest::SingleTestManifest::new(
            ImageHash::new("sha256", hash).unwrap(),
            ImageHash::new("dhash", "0000000000000000").unwrap(),
            1,
            1,
        )
        .unwrap();
        manifest.save(plat_dir.join("test.json")).unwrap();
        manifest.save(plat_dir.join("test2.json")).unwrap();

        let dirs = vec![
            ("linux".to_string(), plat_dir),
            ("missing".to_string(), temp.path().join("missing")),
        ];
        let hashes = collect_referenced_hashes(&dirs).unwrap();
        assert_eq!(hashes.len(), 1);
        let img_hash = ImageHash::new("sha256", hash).unwrap();
        let meta = hashes.get(&img_hash).unwrap();
        assert_eq!(meta.platform, "linux");
        assert_eq!(meta.test_name, "test");
    }

    #[test]
    fn test_active_storage_config_local_mode_and_active() {
        assert!(active_storage_config(None).is_none());

        let blank = StorageConfig::new("   ");
        assert!(active_storage_config(Some(&blank)).is_none());

        let cfg = StorageConfig::new("memory://");
        assert!(active_storage_config(Some(&cfg)).is_some());
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn test_transfer_with_progress_propagates_first_error() {
        let items = vec![1, 2, 3];
        let res: Result<(), String> = transfer_with_progress(items, 2, |item, pb| async move {
            pb.inc(1);
            if item == 2 {
                Err(format!("failed on {item}"))
            } else {
                Ok(())
            }
        })
        .await;
        assert!(res.is_err());
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn test_transfer_with_progress_all_succeed() {
        let items = vec![1, 2, 3];
        let res: Result<(), String> = transfer_with_progress(items, 2, |item, pb| async move {
            pb.inc(1);
            let _ = item;
            Ok(())
        })
        .await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn test_transfer_with_progress_zero_concurrency_does_not_panic() {
        let items = vec![1, 2, 3];
        let res: Result<(), String> = transfer_with_progress(items, 0, |item, pb| async move {
            pb.inc(1);
            let _ = item;
            Ok(())
        })
        .await;
        assert!(res.is_ok());
    }

    #[test]
    #[cfg(all(unix, not(miri)))]
    fn test_list_platform_dirs_propagates_metadata_error() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: `libc::geteuid()` is a side-effect-free POSIX syscall query that returns the process EUID.
        #[expect(
            unsafe_code,
            reason = "`libc::geteuid()` is a side-effect-free POSIX query (see SAFETY comment)"
        )]
        if unsafe { libc::geteuid() } == 0 {
            return;
        }

        let temp = tempdir().unwrap();
        let manifests = temp.path().join("manifests");
        let secret = temp.path().join("secret");
        let target = secret.join("platform-dir");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(&manifests).unwrap();

        let sym = manifests.join("symlink-platform");
        std::os::unix::fs::symlink(&target, &sym).unwrap();

        let mut perms = std::fs::metadata(&secret).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&secret, perms).unwrap();

        let res = list_platform_dirs(&manifests);

        let is_err = res.is_err();
        let mut restore = std::fs::metadata(&secret).unwrap().permissions();
        restore.set_mode(0o755);
        let _ = std::fs::set_permissions(&secret, restore);

        assert!(is_err);
    }
}
