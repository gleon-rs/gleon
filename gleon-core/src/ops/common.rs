//! Shared error type and helper functions used across `ops::*` modules.
//!
//! Each `ops` module keeps its own operation-specific error variants but delegates the
//! handful of concerns every operation shares (workspace initialization checks, platform key
//! resolution, manifest index loading with fallback, config-driven scanning, blob hashing, and
//! `.gitignore`/scaffold file management) to the helpers here, wrapping [`CoreError`] via
//! `#[error(transparent)] Core(#[from] CoreError)`.

use std::path::{Path, PathBuf};

use gleon_engine::phash::compute_phash;
use sha2::{Digest, Sha256};

use crate::{
    config::ConfigError,
    context::{ContextError, ResolvedContext},
    manifest::{ImageHash, ManifestError, SingleTestManifest, WorkspaceIndex},
    paths::GleonPaths,
    scanner::{FileScanner, ScannerError, TestCase},
};

/// Errors shared across `ops::*` operations.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// Workspace has not been initialized (`.gleon` missing).
    #[error("gleon workspace is not initialized. Please run 'gleon init' first.")]
    NotInitialized,

    /// Error resolving context.
    #[error("Context resolution error: {0}")]
    Context(#[from] ContextError),

    /// Error loading configuration.
    #[error("Config error: {0}")]
    Config(#[from] ConfigError),

    /// Error scanning files.
    #[error("Scanner error: {0}")]
    Scanner(#[from] ScannerError),

    /// Error loading or saving a manifest.
    #[error("Manifest error: {0}")]
    Manifest(#[from] ManifestError),

    /// IO error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<crate::io::IoError> for CoreError {
    fn from(err: crate::io::IoError) -> Self {
        match err {
            crate::io::IoError::Io(e) => Self::Io(e),
            crate::io::IoError::JsonParse(e) => Self::Io(std::io::Error::other(e)),
        }
    }
}

/// Ensures the workspace rooted at `base_dir` has been initialized (`.gleon/` exists),
/// returning its resolved paths.
///
/// # Errors
/// Returns [`CoreError::NotInitialized`] if `.gleon/` does not exist, or [`CoreError::Io`] if
/// its metadata cannot be queried for any other reason (e.g. a permission error).
pub fn ensure_initialized(base_dir: &Path) -> Result<GleonPaths, CoreError> {
    let paths = GleonPaths::new(base_dir);
    match std::fs::metadata(paths.gleon_dir()) {
        Ok(_) => Ok(paths),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(CoreError::NotInitialized),
        Err(e) => Err(CoreError::Io(e)),
    }
}

/// Resolves the active platform key from a [`ResolvedContext`].
///
/// # Errors
/// Returns [`CoreError::Context`] if the platform identity fails segment validation.
pub fn platform_key(context: &ResolvedContext) -> Result<String, CoreError> {
    context
        .platform
        .to_key()
        .map_err(|e| CoreError::Context(ContextError::Platform(e)))
}

/// Loads the `WorkspaceIndex` for `platform_key`, and separately the fallback platform's index
/// (if `fallback_platform_key` is set and differs from `platform_key`).
///
/// Callers decide how to use the fallback index: some merge it into the primary index for
/// missing entries, others compare against it without merging.
///
/// # Errors
/// Returns [`CoreError::Manifest`] if either index fails to load.
pub fn load_index_with_fallback(
    paths: &GleonPaths,
    platform_key: &str,
    fallback_platform_key: Option<&str>,
) -> Result<(WorkspaceIndex, Option<WorkspaceIndex>), CoreError> {
    let manifests_dir = paths.manifests_dir(platform_key);
    let workspace_index = WorkspaceIndex::load(&manifests_dir).map_err(CoreError::Manifest)?;

    let fallback_index = fallback_platform_key
        .filter(|&k| k != platform_key)
        .map(|fb_key| {
            WorkspaceIndex::load(paths.manifests_dir(fb_key)).map_err(CoreError::Manifest)
        })
        .transpose()?;

    Ok((workspace_index, fallback_index))
}

/// Loads the workspace index for `platform_key` and merges in fallback-platform entries for any
/// test name missing locally, logging when the fallback is actually used.
///
/// Convenience wrapper around [`load_index_with_fallback`] for callers (`run_diff`,
/// `check_status`) that always want the merged view; callers needing the fallback index kept
/// separate (e.g. `pull_blobs`, which only wants blobs for entries missing from the primary
/// index, not a merged view of both) should call [`load_index_with_fallback`] directly instead.
///
/// # Errors
/// Returns [`CoreError::Manifest`] if either index fails to load.
pub fn load_merged_index_with_fallback(
    paths: &GleonPaths,
    platform_key: &str,
    fallback_platform_key: Option<&str>,
) -> Result<WorkspaceIndex, CoreError> {
    let (mut workspace_index, fallback_index) =
        load_index_with_fallback(paths, platform_key, fallback_platform_key)?;

    if let Some(fb_index) = fallback_index
        && !fb_index.is_empty()
    {
        tracing::info!(
            "Using fallback platform '{}' for missing manifests on platform '{}'.",
            fallback_platform_key.unwrap_or_default(),
            platform_key
        );
        workspace_index.merge_fallback(fb_index);
    }

    Ok(workspace_index)
}

/// Resolves the manifest search directory for a `platform_filter` argument shared by
/// `lint_workspace_manifests` and `scan_conflicts`.
///
/// `manifests_root` itself when `platform_filter` is unset, or `manifests_root/<platform_filter>`
/// when it's exactly one valid path segment.
///
/// # Errors
/// Returns `Err(platform_filter)` (the original string, unchanged) if it isn't a single valid
/// segment, so callers can wrap it in their own `InvalidPlatformFilter` error variant.
pub fn resolve_platform_filter_dir<'a>(
    manifests_root: &Path,
    platform_filter: Option<&'a str>,
) -> Result<PathBuf, &'a str> {
    let Some(p) = platform_filter else {
        return Ok(manifests_root.to_path_buf());
    };

    let path = Path::new(p);
    let mut components = path.components();
    match (components.next(), components.next()) {
        (Some(std::path::Component::Normal(seg)), None)
            if crate::manifest::index::validate_test_path(&seg.to_string_lossy()).is_ok() =>
        {
            Ok(manifests_root.join(p))
        }
        _ => Err(p),
    }
}

/// Returns `true` if `hash`'s scheme is `sha256` and its hex value matches the SHA-256 digest.
///
/// Always `false` for any other scheme — callers fall back to their own scheme-specific
/// comparison (e.g. reading and byte-comparing the local blob) in that case.
#[must_use]
pub fn sha256_hex_matches(hash: &ImageHash, bytes: &[u8]) -> bool {
    hash.scheme() == "sha256" && hex::encode(Sha256::digest(bytes)) == hash.value()
}

/// Returns `index`'s keys with no matching entry in `present` (by exact string match).
///
/// Shared by callers that need to find staged manifests whose test case no longer exists on
/// disk — `stage_workspace` prunes these as orphans, `check_status` reports them as deletions.
pub fn index_keys_missing_from<'a, S: std::hash::BuildHasher>(
    index: &'a WorkspaceIndex,
    present: &std::collections::HashSet<&str, S>,
) -> impl Iterator<Item = &'a str> {
    index
        .entries()
        .keys()
        .map(String::as_str)
        .filter(move |k| !present.contains(k))
}

/// Loads the effective `GleonConfig` from `context` (or its default) and scans the workspace
/// rooted at `context.base_dir` for matching test cases.
///
/// # Errors
/// Returns [`CoreError::Scanner`] if any include/exclude glob fails to compile or a derived
/// test name fails validation.
pub fn load_config_and_scan(context: &ResolvedContext) -> Result<Vec<TestCase>, CoreError> {
    let config = context.config.clone().unwrap_or_default();
    FileScanner::scan_workspace(&config, &context.base_dir).map_err(CoreError::Scanner)
}

/// Decodes raw PNG bytes and computes its SHA-256 digest, perceptual hash, and dimensions.
///
/// Uses [`SingleTestManifest::load_image_from_bytes`] (not a bare decode) so callers get the
/// same dimension/format validation regardless of call site.
///
/// # Errors
/// Returns the underlying [`ManifestError`] if the bytes fail to decode or validate as a
/// supported image.
pub fn hash_and_measure(png_bytes: &[u8]) -> Result<(String, String, u32, u32), ManifestError> {
    let dynamic_img = SingleTestManifest::load_image_from_bytes(png_bytes)?;
    let width = dynamic_img.width();
    let height = dynamic_img.height();
    let rgba_img = dynamic_img.to_rgba8();

    let phash_str = compute_phash(&rgba_img);
    let sha256_hex = hex::encode(Sha256::digest(png_bytes));

    Ok((sha256_hex, phash_str, width, height))
}

/// Builds a [`SingleTestManifest`] from a sha256 hex digest, dhash string, and dimensions.
///
/// # Errors
/// Returns [`CoreError::Manifest`] if any component fails validation.
pub fn build_manifest(
    sha256_hex: &str,
    phash_str: &str,
    width: u32,
    height: u32,
) -> Result<SingleTestManifest, CoreError> {
    let hash = ImageHash::new("sha256", sha256_hex).map_err(CoreError::Manifest)?;
    let phash = phash_str
        .parse::<ImageHash>()
        .map_err(CoreError::Manifest)?;
    SingleTestManifest::new(hash, phash, width, height).map_err(CoreError::Manifest)
}

/// Appends `entries` missing (as a trimmed line) from the gitignore-style file at `path`.
///
/// Creates the file if missing, writes atomically, and returns the entries that were actually
/// appended, in order (empty if all were already present, in which case no write occurs).
///
/// # Errors
/// Returns [`CoreError::Io`] if the existing file (when present) fails to read for a reason
/// other than not existing, or if the atomic write fails.
pub fn append_missing_gitignore_lines(
    path: &Path,
    entries: &[String],
) -> Result<Vec<String>, CoreError> {
    // Read strictly: this rewrites the file wholesale below, so treating an unreadable or
    // non-UTF-8 existing file as "empty" would silently destroy the user's own ignore rules.
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(CoreError::Io(e)),
    };
    let existing_lines: std::collections::HashSet<&str> = existing.lines().map(str::trim).collect();

    let mut added = Vec::new();
    let mut to_append = String::new();
    for entry in entries {
        if !existing_lines.contains(entry.as_str()) {
            use std::fmt::Write as _;
            // Writing to a `String` via `fmt::Write` never fails.
            #[expect(
                clippy::expect_used,
                reason = "`fmt::Write` for `String` is infallible"
            )]
            writeln!(to_append, "{entry}").expect("write! to a String cannot fail");
            added.push(entry.clone());
        }
    }

    if added.is_empty() {
        return Ok(added);
    }

    let mut buffer = existing;
    if !buffer.is_empty() && !buffer.ends_with('\n') {
        buffer.push('\n');
    }
    buffer.push_str(&to_append);

    crate::io::write_file_atomically(path, |writer| {
        use std::io::Write as _;
        writer
            .write_all(buffer.as_bytes())
            .map_err(crate::io::IoError::Io)
    })?;

    Ok(added)
}

/// Creates the file at `path` with `content` if it does not already exist; a no-op otherwise.
///
/// Uses `create_new` for idempotency and durably syncs the file and `dir_to_sync` (its parent
/// directory) to disk.
///
/// # Errors
/// Returns [`CoreError::Io`] if file creation, writing, or syncing fails for a reason other
/// than the file already existing.
#[cfg_attr(
    windows,
    expect(
        unused_variables,
        reason = "`dir_to_sync` is only used for the directory fsync, which is skipped on Windows"
    )
)]
pub fn create_new_file_with_content(
    path: &Path,
    content: &[u8],
    dir_to_sync: &Path,
) -> Result<bool, CoreError> {
    use std::io::Write as _;

    let create_res = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path);
    match create_res {
        Ok(mut f) => {
            if let Err(e) = f.write_all(content).and_then(|()| f.sync_all()) {
                let _ = std::fs::remove_file(path);
                return Err(CoreError::Io(e));
            }
            #[cfg(not(windows))]
            if let Ok(d) = std::fs::File::open(dir_to_sync) {
                let _ = d.sync_all();
            }
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(CoreError::Io(e)),
    }
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
    fn test_ensure_initialized_missing_and_present() {
        let temp = tempdir().unwrap();
        assert!(matches!(
            ensure_initialized(temp.path()),
            Err(CoreError::NotInitialized)
        ));

        std::fs::create_dir_all(temp.path().join(".gleon")).unwrap();
        let paths = ensure_initialized(temp.path()).unwrap();
        assert_eq!(paths.gleon_dir(), temp.path().join(".gleon"));
    }

    #[test]
    fn test_platform_key_resolves_and_rejects_invalid() {
        let mut ctx = ResolvedContext::default();
        ctx.platform.os = "linux".to_string();
        assert_eq!(platform_key(&ctx).unwrap(), "5:linux");

        ctx.platform.os = "in/valid".to_string();
        assert!(matches!(platform_key(&ctx), Err(CoreError::Context(_))));
    }

    #[test]
    fn test_load_index_with_fallback_merges_only_when_requested() {
        let temp = tempdir().unwrap();
        let paths = GleonPaths::new(temp.path());
        std::fs::create_dir_all(paths.manifests_dir("linux")).unwrap();
        std::fs::create_dir_all(paths.manifests_dir("macos")).unwrap();

        let (primary, fallback) = load_index_with_fallback(&paths, "linux", Some("macos")).unwrap();
        assert!(primary.is_empty());
        assert!(fallback.is_some());

        let (_, no_fallback) = load_index_with_fallback(&paths, "linux", None).unwrap();
        assert!(no_fallback.is_none());

        // Same platform as fallback: no fallback index loaded.
        let (_, self_fallback) = load_index_with_fallback(&paths, "linux", Some("linux")).unwrap();
        assert!(self_fallback.is_none());
    }

    #[test]
    fn test_resolve_platform_filter_dir_accepts_single_valid_segment() {
        let root = Path::new("/w/.gleon/manifests");

        assert_eq!(
            resolve_platform_filter_dir(root, None).unwrap(),
            root.to_path_buf(),
            "no filter means the whole manifests root"
        );
        assert_eq!(
            resolve_platform_filter_dir(root, Some("macos-aarch64")).unwrap(),
            root.join("macos-aarch64")
        );
    }

    #[test]
    fn test_resolve_platform_filter_dir_rejects_traversal_and_nesting() {
        let root = Path::new("/w/.gleon/manifests");

        // Multi-segment, parent traversal and absolute paths must never escape the root.
        for bad in ["macos/aarch64", "../etc", "/etc", ".", ""] {
            assert_eq!(
                resolve_platform_filter_dir(root, Some(bad)),
                Err(bad),
                "filter {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn test_sha256_hex_matches_only_for_matching_sha256() {
        let bytes = b"hello gleon";
        let digest = hex::encode(Sha256::digest(bytes));

        let sha = ImageHash::new("sha256", &digest).unwrap();
        assert!(sha256_hex_matches(&sha, bytes));
        assert!(!sha256_hex_matches(&sha, b"different bytes"));

        // A non-sha256 scheme never claims a match, even for the identical digest text.
        let dhash = ImageHash::new("dhash", "0123456789abcdef").unwrap();
        assert!(!sha256_hex_matches(&dhash, bytes));
    }

    #[test]
    fn test_index_keys_missing_from_reports_only_absent_keys() {
        let temp = tempdir().unwrap();
        let manifest_dir = temp.path();
        let mut index = WorkspaceIndex::new();
        let manifest = SingleTestManifest::new(
            ImageHash::new("sha256", "a".repeat(64)).unwrap(),
            ImageHash::new("dhash", "0000000000000000").unwrap(),
            1,
            1,
        )
        .unwrap();
        for name in ["kept", "gone"] {
            index.save_test(manifest_dir, name, &manifest).unwrap();
        }

        let present: std::collections::HashSet<&str> = ["kept"].into_iter().collect();
        let missing: Vec<&str> = index_keys_missing_from(&index, &present).collect();
        assert_eq!(missing, vec!["gone"]);

        let all_present: std::collections::HashSet<&str> = ["kept", "gone"].into_iter().collect();
        assert_eq!(index_keys_missing_from(&index, &all_present).count(), 0);

        let none_present = std::collections::HashSet::new();
        assert_eq!(index_keys_missing_from(&index, &none_present).count(), 2);
    }

    #[test]
    fn test_hash_and_measure_and_build_manifest_roundtrip() {
        let png_bytes = include_bytes!("../../tests/fixtures/baseline_100x100.png");
        let (sha256_hex, phash_str, width, height) = hash_and_measure(png_bytes).unwrap();
        assert_eq!(width, 100);
        assert_eq!(height, 100);

        let manifest = build_manifest(&sha256_hex, &phash_str, width, height).unwrap();
        assert_eq!(manifest.hash.value(), sha256_hex);
    }

    #[test]
    fn test_hash_and_measure_rejects_corrupt_bytes() {
        assert!(hash_and_measure(b"not a png").is_err());
    }

    #[test]
    fn test_append_missing_gitignore_lines_idempotent() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(".gitignore");

        let added =
            append_missing_gitignore_lines(&path, &["blobs/".to_string(), "runs/".to_string()])
                .unwrap();
        assert_eq!(added, vec!["blobs/".to_string(), "runs/".to_string()]);

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("blobs/"));
        assert!(content.contains("runs/"));

        // Second call with an overlapping + new entry only appends the new one.
        let added_again = append_missing_gitignore_lines(
            &path,
            &["blobs/".to_string(), "credentials".to_string()],
        )
        .unwrap();
        assert_eq!(added_again, vec!["credentials".to_string()]);
    }

    #[test]
    #[cfg(unix)]
    fn test_append_missing_gitignore_lines_preserves_unreadable_file() {
        // A `.gitignore` that exists but cannot be decoded as UTF-8 must NOT be silently
        // truncated — the caller's rules are user data, so the read error has to propagate.
        let temp = tempdir().unwrap();
        let path = temp.path().join(".gitignore");
        let original: &[u8] = b"my-secret-blobs/\nlocal-caf\xe9/\n*.key\n";
        std::fs::write(&path, original).unwrap();

        let res = append_missing_gitignore_lines(&path, &["blobs/".to_string()]);

        assert!(
            matches!(res, Err(CoreError::Io(_))),
            "non-UTF-8 existing file must surface as an IO error, got {res:?}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            original,
            "existing .gitignore content must be left untouched"
        );
    }

    #[test]
    fn test_create_new_file_with_content_idempotent() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("gleon.yaml");

        assert!(create_new_file_with_content(&path, b"hello", temp.path()).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");

        // Already exists: no-op, content untouched.
        assert!(!create_new_file_with_content(&path, b"world", temp.path()).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn test_core_error_from_io_error() {
        let io_err = crate::io::IoError::Io(std::io::Error::other("test io"));
        let core_err: CoreError = io_err.into();
        assert!(matches!(core_err, CoreError::Io(_)));

        let json_err: serde_json::Error = serde_json::from_str::<String>("invalid").unwrap_err();
        let io_json_err = crate::io::IoError::JsonParse(json_err);
        let core_json_err: CoreError = io_json_err.into();
        assert!(matches!(core_json_err, CoreError::Io(_)));
    }

    #[test]
    fn test_create_new_file_with_content_error() {
        let res = create_new_file_with_content(
            Path::new("/nonexistent_dir/nested/blob.png"),
            b"test",
            Path::new("/nonexistent_dir"),
        );
        assert!(matches!(res, Err(CoreError::Io(_))));
    }

    #[test]
    fn test_load_merged_index_with_fallback_non_empty() {
        let temp = tempdir().unwrap();
        let paths = GleonPaths::new(temp.path());
        let macos_manifests = paths.manifests_dir("macos");
        std::fs::create_dir_all(&macos_manifests).unwrap();

        let mut index = WorkspaceIndex::new();
        index
            .save_test(
                &macos_manifests,
                "login",
                &SingleTestManifest {
                    schema_version: 1,
                    hash: ImageHash::new("sha256", "a".repeat(64)).unwrap(),
                    phash: ImageHash::new("dhash", "0000000000000000").unwrap(),
                    width: 10,
                    height: 10,
                },
            )
            .unwrap();

        let merged = load_merged_index_with_fallback(&paths, "linux", Some("macos")).unwrap();
        assert_eq!(merged.len(), 1);
        assert!(merged.get("login").is_some());
    }
}
