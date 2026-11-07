//! Shared internals of the `push`, `pull` and `gc` operations: platform-dir enumeration and
//! referenced-hash collection.
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

use futures::StreamExt as _;
use gleon_model::platform::PlatformKey;
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

/// The entries of `.gleon/manifests/` ([`read_manifests_root`]).
#[derive(Debug, Default)]
pub struct ManifestsRoot {
    /// The platform directories, named by a platform key ([`PlatformKey::parse`]), by name.
    pub platforms: Vec<(String, PathBuf)>,
    /// The other entries that are not hidden, sorted: directories of an older key format
    /// (`5:…`), stray files. gleon never reads them.
    pub foreign: Vec<PathBuf>,
}

/// Reads the entries of `manifests_root` (`.gleon/manifests/`); hidden ones (`.DS_Store`,
/// `.gitkeep`) are neither platform directories nor foreign.
///
/// # Errors
/// Returns the I/O error of reading the directory (`NotFound` only if it does not exist) or the
/// metadata of an entry; an entry whose target is missing (a dangling link) is foreign.
pub fn read_manifests_root(manifests_root: &Path) -> std::io::Result<ManifestsRoot> {
    let mut root = ManifestsRoot::default();
    for entry in std::fs::read_dir(manifests_root)? {
        let entry = entry?;
        let name = entry.file_name();
        // A name that is no UTF-8 is no platform key either.
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        // `NotFound` here is the entry's own (a dangling link, or one removed meanwhile): it
        // must not read as a missing `manifests_root`, which callers take for no platforms.
        let is_platform = match std::fs::metadata(&path) {
            Ok(metadata) => metadata.is_dir() && PlatformKey::parse(&name).is_ok(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(e),
        };
        if is_platform {
            root.platforms.push((name.into_owned(), path));
        } else {
            root.foreign.push(path);
        }
    }
    root.platforms.sort_by(|a, b| a.0.cmp(&b.0));
    root.foreign.sort();
    Ok(root)
}

/// The platform directories under `.gleon/manifests/` ([`read_manifests_root`]), by name, with a
/// warning for each entry that is none; empty (not an error) if `manifests_root` does not exist
/// yet.
///
/// # Errors
/// Returns the I/O error of reading the directory or the metadata of an entry.
pub fn list_platform_dirs(manifests_root: &Path) -> Result<Vec<(String, PathBuf)>, std::io::Error> {
    let root = match read_manifests_root(manifests_root) {
        Ok(root) => root,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    for path in &root.foreign {
        let path = path.display();
        tracing::warn!("Skipping {path}: it is no platform directory (see `gleon lint`)");
    }
    Ok(root.platforms)
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
/// `gc` uses it the same way: [`list_platform_dirs`] with `manifests_root` lists every platform,
/// then this function collects the full referenced-hash set, which `gc` checks the remote blob
/// listing against before deleting anything unreferenced.
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
/// a shared progress bar. After the first error no further task starts, and the running ones
/// finish before this returns: a dropped download would leave its blocking writer renaming a
/// blob after the caller's one directory flush.
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
    let failed = std::sync::atomic::AtomicBool::new(false);
    let (task, failed_ref, bar) = (&task, &failed, &progress_bar);

    let mut results = futures::stream::iter(items.into_iter().map(|item| async move {
        // Checked when the task would start: one that started already runs to its end.
        if failed_ref.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(());
        }
        task(item, bar.clone()).await
    }))
    .buffer_unordered(concurrency.max(1));

    let mut first_error = None;
    while let Some(result) = results.next().await {
        if let Err(e) = result {
            failed.store(true, std::sync::atomic::Ordering::Relaxed);
            first_error.get_or_insert(e);
        }
    }

    progress_bar.finish_and_clear();
    first_error.map_or(Ok(()), Err)
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
        // Key characters, but no platform key (`PlatformKey::parse`), and a hidden directory.
        for not_a_key in ["a==b", "linux.", "con", ".hidden"] {
            std::fs::create_dir_all(manifests.join(not_a_key)).unwrap();
        }
        std::fs::write(manifests.join("some_file.txt"), "hello").unwrap();

        let res = list_platform_dirs(&manifests).unwrap();
        let names: Vec<_> = res.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["linux-x86_64+chrome+theme=dark", "valid-platform"]);
    }

    /// A dangling link under `.gleon/manifests/` is a foreign entry: its `NotFound` must not read
    /// as a missing root, which would hide every platform from `push`/`pull --all` and `gc`.
    #[cfg(unix)]
    #[test]
    fn test_a_dangling_link_is_foreign_not_a_missing_root() {
        let temp = tempdir().unwrap();
        let manifests = temp.path().join("manifests");
        std::fs::create_dir_all(manifests.join("macos-aarch64")).unwrap();
        let link = manifests.join("old-link");
        std::os::unix::fs::symlink(temp.path().join("gone"), &link).unwrap();

        let root = read_manifests_root(&manifests).unwrap();
        assert_eq!(root.foreign, [link]);
        let names: Vec<_> = list_platform_dirs(&manifests)
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["macos-aarch64"]);
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

    /// After the first error no task starts, and a running one ends before the call returns
    /// (a dropped download would rename its blob after the caller's directory flush); the error
    /// returned is the first.
    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn test_transfer_with_progress_finishes_running_tasks_after_an_error() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (started, finished) = (&AtomicUsize::new(0), &AtomicUsize::new(0));
        let res = transfer_with_progress(vec![0, 1, 2, 3], 2, |item, _| async move {
            started.fetch_add(1, Ordering::Relaxed);
            // Both running tasks start before either ends.
            tokio::task::yield_now().await;
            if item == 0 {
                return Err(format!("failed on {item}"));
            }
            tokio::task::yield_now().await;
            finished.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
        .await;
        assert_eq!(res, Err("failed on 0".to_owned()));
        assert_eq!(
            (
                started.load(Ordering::Relaxed),
                finished.load(Ordering::Relaxed)
            ),
            (2, 1)
        );
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
