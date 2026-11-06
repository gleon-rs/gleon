//! Shared error type and helper functions used across `ops::*` modules.
//!
//! Each `ops` module keeps its own operation-specific error variants but delegates the
//! handful of concerns every operation shares (workspace initialization checks, platform key
//! resolution, manifest index loading with fallback, config-driven scanning, blob hashing, and
//! `.gitignore`/scaffold file management) to the helpers here, wrapping [`CoreError`] via
//! `#[error(transparent)] Core(#[from] CoreError)`.

use std::path::{Path, PathBuf};

use gleon_engine::phash::compute_phash;
use gleon_model::{
    case::{CaseErrorKind, Sha256Hex},
    compare,
    tolerance::Tolerance,
};

use crate::{
    config::ConfigError,
    context::{ContextError, ResolvedContext},
    manifest::{ImageHash, ManifestError, SingleTestManifest, WorkspaceIndex},
    paths::GleonPaths,
    platform::PlatformKey,
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

    /// The workspace keeps its baselines as the golden files of an integration
    /// (`baselines: goldens`), which the CLI never scans as screenshots.
    #[error(
        "the baselines of this workspace are golden files (`baselines: goldens` in \
         .gleon/gleon.yaml), compared by its tests, not manifests and blobs: `gleon status`, \
         `stage`, `diff` and `clean --screenshots` would take every golden for an unrecorded \
         screenshot, and `pull`, `push`, `gc` and `lint` have no manifests to work with. Run the \
         tests with `gleon test -- <command>` (e.g. `gleon test -- flutter test`), then \
         `gleon report <format>` and `gleon approve`"
    )]
    BaselinesAreGoldens,

    /// `--require-storage` was given but no remote storage is configured.
    #[error(
        "no remote storage is configured (GLEON_STORAGE_URL), but this run requires one: the \
         blobs of the manifests must be pulled from and pushed to it (in the approve workflow, \
         set the GLEON_STORAGE_URL secret)"
    )]
    StorageRequired,
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
pub fn platform_key(context: &ResolvedContext) -> Result<PlatformKey, CoreError> {
    context
        .platform
        .key()
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
    platform_key: &PlatformKey,
    fallback_platform_key: Option<&PlatformKey>,
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
    platform_key: &PlatformKey,
    fallback_platform_key: Option<&PlatformKey>,
) -> Result<WorkspaceIndex, CoreError> {
    let (mut workspace_index, fallback_index) =
        load_index_with_fallback(paths, platform_key, fallback_platform_key)?;

    // A fallback index exists only for a fallback platform.
    if let (Some(fb_index), Some(fallback)) = (fallback_index, fallback_platform_key)
        && !fb_index.is_empty()
    {
        tracing::info!(
            "Using fallback platform '{fallback}' for missing manifests on platform \
             '{platform_key}'."
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
            if gleon_model::platform::validate_key(&seg.to_string_lossy())
                .is_ok_and(|key| key == p) =>
        {
            Ok(manifests_root.join(p))
        }
        _ => Err(p),
    }
}

/// What a tracked screenshot is compared with, decided from its manifest and the local blob
/// before any decoding ([`baseline_of`]): one decision for `gleon diff` and `gleon status`.
#[derive(Debug, PartialEq, Eq)]
pub enum Baseline {
    /// The screenshot bytes are the baseline bytes: the manifest's sha256 names them (decided
    /// without the blob), or the blob of another hash scheme holds them.
    Identical {
        /// Whether the local blob was read (and so is usable): `false` when the manifest's
        /// sha256 decided alone, so the blob may still be missing or broken in the local cache.
        blob_read: bool,
    },
    /// The baseline's bytes, to compare the screenshot with under its rule.
    Golden {
        /// The PNG bytes of the blob.
        png: Vec<u8>,
        /// Their SHA-256 (verified against the manifest for a sha256 blob).
        sha256: Sha256Hex,
    },
    /// No comparison is possible: the identical screenshot is no valid image, or the blob is
    /// missing, no regular file, or unreadable. Never a pass.
    Unavailable {
        /// The class of the failure in a case report.
        kind: CaseErrorKind,
        /// What went wrong, for the user.
        message: String,
    },
}

/// The [`Baseline`] of the screenshot `candidate` (PNG bytes, `candidate_sha256` their SHA-256)
/// tracked by `manifest`, whose blob lies under `blobs_root`, compared under `tolerance`.
///
/// Identical bytes pass only as an image that can be compared under `tolerance` at all
/// ([`compare::check_size`]): one over the SSIM budget is unavailable with the error `compare`
/// gives where the bytes differ, so it fails on every platform alike.
///
/// A blob counts only as a regular file, opened once without following a link
/// ([`crate::storage::open_regular_file`], like for `gleon push`): a symbolic link or a directory
/// there is unavailable. A sha256 blob must hold the bytes its name says: a changed cache never
/// decides a comparison.
#[must_use]
pub fn baseline_of(
    manifest: &SingleTestManifest,
    candidate: &[u8],
    candidate_sha256: &Sha256Hex,
    tolerance: &Tolerance,
    blobs_root: &Path,
) -> Baseline {
    let identical = |blob_read| match SingleTestManifest::validate_image_bytes(candidate) {
        Ok((width, height)) => match compare::check_size(tolerance, width, height) {
            Ok(()) => Baseline::Identical { blob_read },
            Err(e) => Baseline::Unavailable {
                kind: e.kind(),
                message: e.to_string(),
            },
        },
        Err(e) => Baseline::Unavailable {
            kind: CaseErrorKind::Image,
            message: format!("candidate image: {e}"),
        },
    };
    let is_sha256 = manifest.hash.scheme() == "sha256";
    if is_sha256 && candidate_sha256.as_str() == manifest.hash.value() {
        return identical(false);
    }
    let unavailable = |message| Baseline::Unavailable {
        kind: CaseErrorKind::Io,
        message,
    };
    // One no-follow open of a regular file: the bytes read are those of the file checked.
    let read = crate::storage::open_regular_file(&crate::storage::local_blob_path(
        blobs_root,
        &manifest.hash,
    ))
    .and_then(|(mut file, _)| {
        let mut png = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut png).map(|_| png)
    });
    let png = match read {
        Ok(png) => png,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return unavailable(format!(
                "the baseline blob {} is not in .gleon/blobs: run `gleon pull` (or `gleon diff \
                 --auto-pull`)",
                manifest.hash
            ));
        }
        Err(e) if crate::storage::is_not_regular_file(&e) => {
            return unavailable(not_regular_blob(&manifest.hash));
        }
        Err(e) => return unavailable(cannot_read_blob(&manifest.hash, &e)),
    };
    let sha256 = Sha256Hex::of(&png);
    if is_sha256 && sha256.as_str() != manifest.hash.value() {
        return unavailable(format!(
            "the baseline blob {} in .gleon/blobs does not match its hash: remove it and run \
             `gleon pull`",
            manifest.hash
        ));
    }
    // A verified sha256 blob never holds the candidate's bytes (they hash differently, see
    // above): only a blob of another scheme can.
    if !is_sha256 && png == candidate {
        return identical(true);
    }
    Baseline::Golden { png, sha256 }
}

/// The state of the local blob of `hash` under `blobs_root`, from one `lstat`.
#[derive(Debug, PartialEq, Eq)]
pub enum BlobState {
    /// A regular file ([`crate::storage::is_usable_blob_metadata`]).
    Usable,
    /// Not there: `gleon pull` fetches it.
    Missing,
    /// There but no regular file (a directory, a symbolic link), or its metadata cannot be read:
    /// the reason, for the user.
    Unusable(String),
}

/// The [`BlobState`] of the local blob of `hash` under `blobs_root`, without reading it (for a
/// screenshot decided by its manifest's hash alone), worded like [`baseline_of`].
#[must_use]
pub fn local_blob_state(blobs_root: &Path, hash: &ImageHash) -> BlobState {
    match std::fs::symlink_metadata(crate::storage::local_blob_path(blobs_root, hash)) {
        Ok(meta) if crate::storage::is_usable_blob_metadata(&meta) => BlobState::Usable,
        Ok(_) => BlobState::Unusable(not_regular_blob(hash)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => BlobState::Missing,
        Err(e) => BlobState::Unusable(cannot_read_blob(hash, &e)),
    }
}

/// Why the blob of `hash` cannot be read.
fn cannot_read_blob(hash: &ImageHash, e: &std::io::Error) -> String {
    format!("cannot read the baseline blob {hash}: {e}")
}

/// Why the blob of `hash` is no blob: a directory or a symbolic link where it belongs.
fn not_regular_blob(hash: &ImageHash) -> String {
    format!(
        "the baseline blob {hash} in .gleon/blobs is no regular file: remove it and run \
         `gleon pull`"
    )
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
/// The one place the CLI takes the files its rules match for screenshots: a workspace whose
/// baselines are the golden files of an integration is refused here, before anything is read or
/// written (so `gleon diff` never replaces the integration's case reports).
///
/// # Errors
/// Returns [`CoreError::BaselinesAreGoldens`] for a `baselines: goldens` workspace, or
/// [`CoreError::Scanner`] if any include/exclude glob fails to compile or a derived test name
/// fails validation.
pub fn load_config_and_scan(context: &ResolvedContext) -> Result<Vec<TestCase>, CoreError> {
    refuse_goldens(context)?;
    let config = context.config.clone().unwrap_or_default();
    FileScanner::scan_workspace(&config, &context.base_dir).map_err(CoreError::Scanner)
}

/// Fails for a workspace whose baselines are the golden files of an integration.
///
/// [`gleon_model::config::Baselines::Goldens`]: the commands that scan screenshots check this
/// before they create or read anything.
///
/// # Errors
/// Returns [`CoreError::BaselinesAreGoldens`] for such a workspace.
pub fn refuse_goldens(context: &ResolvedContext) -> Result<(), CoreError> {
    match context.baselines() {
        gleon_model::config::Baselines::Goldens => Err(CoreError::BaselinesAreGoldens),
        gleon_model::config::Baselines::Manifests => Ok(()),
    }
}

/// Fails when remote storage is `required` but `storage` names none (missing or blank URL), so
/// a CI step that must sync blobs never passes in local mode.
///
/// # Errors
/// Returns [`CoreError::StorageRequired`] then.
pub fn require_storage(
    storage: Option<&crate::storage::StorageConfig>,
    required: bool,
) -> Result<(), CoreError> {
    let configured = storage.is_some_and(|storage| !storage.url.trim().is_empty());
    if required && !configured {
        Err(CoreError::StorageRequired)
    } else {
        Ok(())
    }
}

/// The perceptual hash a manifest records for a decoded `image` (an RGBA8 one is moved, not
/// copied).
#[must_use]
pub fn phash_of(image: image::DynamicImage) -> String {
    compute_phash(&image.into_rgba8())
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
    let hash = ImageHash::new("sha256", sha256_hex).map_err(ManifestError::from)?;
    let phash = phash_str
        .parse::<ImageHash>()
        .map_err(ManifestError::from)?;
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
    entries: &[impl AsRef<str>],
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
    for entry in entries.iter().map(AsRef::as_ref) {
        if !existing_lines.contains(entry) {
            use std::fmt::Write as _;
            // Writing to a `String` via `fmt::Write` never fails.
            #[expect(
                clippy::expect_used,
                reason = "`fmt::Write` for `String` is infallible"
            )]
            writeln!(to_append, "{entry}").expect("write! to a String cannot fail");
            added.push(entry.to_owned());
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
    use sha2::Digest as _;
    use tempfile::tempdir;

    use super::*;

    /// The bytes of the PNG fixture `name` and their SHA-256.
    fn fixture(name: &str) -> (Vec<u8>, Sha256Hex) {
        let png = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(name),
        )
        .unwrap();
        let sha256 = Sha256Hex::of(&png);
        (png, sha256)
    }

    fn manifest(scheme: &str, value: &str) -> SingleTestManifest {
        SingleTestManifest {
            schema_version: 1,
            hash: ImageHash::new(scheme, value).unwrap(),
            phash: ImageHash::new("dhash", "0000000000000000").unwrap(),
            width: 100,
            height: 100,
        }
    }

    /// Asserts that `baseline` is unavailable of `kind`, its message containing `needle`.
    fn assert_unavailable(baseline: &Baseline, kind: CaseErrorKind, needle: &str) {
        assert!(
            is_unavailable(baseline, kind, needle),
            "{baseline:?} should be {kind:?} with {needle:?}"
        );
    }

    /// Whether `baseline` is unavailable of `kind`, its message containing `needle`.
    fn is_unavailable(baseline: &Baseline, kind: CaseErrorKind, needle: &str) -> bool {
        matches!(
            baseline,
            Baseline::Unavailable { kind: found, message }
                if *found == kind && message.contains(needle)
        )
    }

    /// No size budget: the decisions of the blob alone.
    const EXACT: Tolerance = Tolerance::Exact {};

    /// Identical bytes pass only if the screenshot can be compared under `tolerance` at all: one
    /// over the SSIM budget is unavailable with the error of `compare`, on every platform, not
    /// just where the bytes happen to differ.
    #[test]
    fn test_baseline_of_checks_the_size_of_identical_screenshots() {
        let blobs = tempdir().unwrap();
        let (big, sha256) = fixture("ssim_budget_4097x4096.png");
        let tracked = manifest("sha256", sha256.as_str());
        let ssim = Tolerance::Ssim {
            min_similarity: 0.8,
            color_tolerance: 8.0,
        };
        assert_unavailable(
            &baseline_of(&tracked, &big, &sha256, &ssim, blobs.path()),
            CaseErrorKind::Image,
            "4097x4096 exceeds the SSIM analysis budget",
        );
        assert_eq!(
            baseline_of(&tracked, &big, &sha256, &EXACT, blobs.path()),
            Baseline::Identical { blob_read: false }
        );
    }

    /// The one baseline decision of `gleon diff` and `gleon status`, on real fixtures.
    #[test]
    fn test_baseline_of() {
        let blobs = tempdir().unwrap();
        let (png, sha256) = fixture("baseline_gradient_100x100.png");
        let (other, other_sha256) = fixture("diff_1px_black_center_100x100.png");
        let blob_dir = blobs.path().join("sha256");
        std::fs::create_dir_all(&blob_dir).unwrap();

        // The manifest's sha256 names the bytes: identical without any blob, which is not read.
        let tracked = manifest("sha256", sha256.as_str());
        assert_eq!(
            baseline_of(&tracked, &png, &sha256, &EXACT, blobs.path()),
            Baseline::Identical { blob_read: false }
        );
        // ... but bytes that are no valid image are never a pass.
        let (corrupt, corrupt_sha256) = fixture("corrupt.png");
        assert_unavailable(
            &baseline_of(
                &manifest("sha256", corrupt_sha256.as_str()),
                &corrupt,
                &corrupt_sha256,
                &EXACT,
                blobs.path(),
            ),
            CaseErrorKind::Image,
            "candidate image",
        );

        // Other bytes: the blob, if it is there.
        let blob = blob_dir.join(sha256.as_str());
        assert_unavailable(
            &baseline_of(&tracked, &other, &other_sha256, &EXACT, blobs.path()),
            CaseErrorKind::Io,
            "run `gleon pull`",
        );
        std::fs::create_dir(&blob).unwrap();
        assert_unavailable(
            &baseline_of(&tracked, &other, &other_sha256, &EXACT, blobs.path()),
            CaseErrorKind::Io,
            "no regular file",
        );
        std::fs::remove_dir(&blob).unwrap();
        std::fs::write(&blob, &png).unwrap();
        assert_eq!(
            baseline_of(&tracked, &other, &other_sha256, &EXACT, blobs.path()),
            Baseline::Golden {
                png: png.clone(),
                sha256: sha256.clone(),
            }
        );

        // A blob that does not hold the bytes its name says is no baseline: here it holds the
        // candidate itself, which would compare as equal.
        std::fs::write(&blob, &other).unwrap();
        assert_unavailable(
            &baseline_of(&tracked, &other, &other_sha256, &EXACT, blobs.path()),
            CaseErrorKind::Io,
            "does not match its hash",
        );

        // Another hash scheme: the blob's bytes decide identity, other bytes are compared.
        let sha512 = hex::encode(sha2::Sha512::digest(&png));
        std::fs::create_dir_all(blobs.path().join("sha512")).unwrap();
        std::fs::write(blobs.path().join("sha512").join(&sha512), &png).unwrap();
        let tracked_sha512 = manifest("sha512", &sha512);
        assert_eq!(
            baseline_of(&tracked_sha512, &png, &sha256, &EXACT, blobs.path()),
            Baseline::Identical { blob_read: true }
        );
        assert_eq!(
            baseline_of(&tracked_sha512, &other, &other_sha256, &EXACT, blobs.path()),
            Baseline::Golden {
                sha256: Sha256Hex::of(&png),
                png,
            }
        );
    }

    /// One `lstat` tells a usable blob from a missing one and from one that is there but unusable.
    #[test]
    fn test_local_blob_state() {
        let blobs = tempdir().unwrap();
        let (png, sha256) = fixture("baseline_gradient_100x100.png");
        let hash = ImageHash::new("sha256", sha256.as_str()).unwrap();
        let blob = blobs.path().join("sha256").join(sha256.as_str());
        assert_eq!(local_blob_state(blobs.path(), &hash), BlobState::Missing);
        std::fs::create_dir_all(&blob).unwrap();
        assert!(matches!(
            local_blob_state(blobs.path(), &hash),
            BlobState::Unusable(reason) if reason.contains("is no regular file")
        ));
        std::fs::remove_dir(&blob).unwrap();
        std::fs::write(&blob, &png).unwrap();
        assert_eq!(local_blob_state(blobs.path(), &hash), BlobState::Usable);

        // Metadata that cannot be read (a directory without permission) is unusable, with why.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let blob_dir = blob.parent().unwrap();
            let mode = |mode| {
                std::fs::set_permissions(blob_dir, std::fs::Permissions::from_mode(mode)).unwrap();
            };
            mode(0o000);
            let is_root = std::fs::symlink_metadata(&blob).is_ok();
            let unreadable = local_blob_state(blobs.path(), &hash);
            mode(0o755);
            // Permissions bind no superuser: then the blob stays usable.
            assert!(
                is_root
                    || matches!(&unreadable, BlobState::Unusable(reason)
                        if reason.contains("cannot read the baseline blob")),
                "{unreadable:?}"
            );
        }
    }

    /// A blob that is a symbolic link is never followed, like `gleon pull` and `gleon push` never
    /// take it for a blob.
    #[cfg(unix)]
    #[test]
    fn test_baseline_of_never_follows_a_symlinked_blob() {
        let blobs = tempdir().unwrap();
        let (png, sha256) = fixture("baseline_gradient_100x100.png");
        let (other, other_sha256) = fixture("diff_1px_black_center_100x100.png");
        let blob_dir = blobs.path().join("sha256");
        std::fs::create_dir_all(&blob_dir).unwrap();
        let elsewhere = blobs.path().join("elsewhere.png");
        std::fs::write(&elsewhere, &png).unwrap();
        std::os::unix::fs::symlink(&elsewhere, blob_dir.join(sha256.as_str())).unwrap();

        assert_unavailable(
            &baseline_of(
                &manifest("sha256", sha256.as_str()),
                &other,
                &other_sha256,
                &EXACT,
                blobs.path(),
            ),
            CaseErrorKind::Io,
            "no regular file",
        );
    }

    /// A blob that cannot be read, or whose metadata cannot be, is unavailable with the reason.
    #[cfg(unix)]
    #[test]
    fn test_baseline_of_names_why_a_blob_cannot_be_read() {
        use std::os::unix::fs::PermissionsExt;

        let blobs = tempdir().unwrap();
        let (png, sha256) = fixture("baseline_gradient_100x100.png");
        let (other, other_sha256) = fixture("diff_1px_black_center_100x100.png");
        let blob_dir = blobs.path().join("sha256");
        std::fs::create_dir_all(&blob_dir).unwrap();
        let blob = blob_dir.join(sha256.as_str());
        std::fs::write(&blob, &png).unwrap();
        let tracked = manifest("sha256", sha256.as_str());
        let mode = |path: &Path, mode| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        };

        // A regular file without read permission: its metadata is fine, reading fails.
        mode(&blob, 0o000);
        let is_root = std::fs::read(&blob).is_ok();
        let unreadable_file = baseline_of(&tracked, &other, &other_sha256, &EXACT, blobs.path());
        // A directory without permission: not even the blob's metadata can be read.
        mode(&blob_dir, 0o000);
        let unreadable_dir = baseline_of(&tracked, &other, &other_sha256, &EXACT, blobs.path());
        mode(&blob_dir, 0o755);
        mode(&blob, 0o644);

        // Permissions bind no superuser: then nothing is refused.
        for unreadable in [&unreadable_file, &unreadable_dir] {
            assert!(
                is_root
                    || is_unavailable(
                        unreadable,
                        CaseErrorKind::Io,
                        "cannot read the baseline blob"
                    ),
                "{unreadable:?}"
            );
        }
    }

    #[test]
    fn test_require_storage() {
        let blank = crate::storage::StorageConfig::new(" ");
        let memory = crate::storage::StorageConfig::new("memory://");
        assert!(require_storage(None, false).is_ok());
        assert!(require_storage(Some(&memory), true).is_ok());
        for missing in [None, Some(&blank)] {
            assert!(matches!(
                require_storage(missing, true),
                Err(CoreError::StorageRequired)
            ));
        }
    }

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
        assert_eq!(platform_key(&ctx).unwrap(), "linux");

        ctx.platform.os = "in/valid".to_string();
        assert!(matches!(platform_key(&ctx), Err(CoreError::Context(_))));
    }

    #[test]
    fn test_load_index_with_fallback_merges_only_when_requested() {
        let key = |key: &str| PlatformKey::parse(key).unwrap();
        let temp = tempdir().unwrap();
        let paths = GleonPaths::new(temp.path());
        std::fs::create_dir_all(paths.manifests_dir(&key("linux"))).unwrap();
        std::fs::create_dir_all(paths.manifests_dir(&key("macos"))).unwrap();

        let (primary, fallback) =
            load_index_with_fallback(&paths, &key("linux"), Some(&key("macos"))).unwrap();
        assert!(primary.is_empty());
        assert!(fallback.is_some());

        let (_, no_fallback) = load_index_with_fallback(&paths, &key("linux"), None).unwrap();
        assert!(no_fallback.is_none());

        // Same platform as fallback: no fallback index loaded.
        let (_, self_fallback) =
            load_index_with_fallback(&paths, &key("linux"), Some(&key("linux"))).unwrap();
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
        assert_eq!(
            resolve_platform_filter_dir(root, Some("linux-x86_64+chrome+theme=dark")).unwrap(),
            root.join("linux-x86_64+chrome+theme=dark"),
            "keys with a renderer and labels"
        );
    }

    #[test]
    fn test_resolve_platform_filter_dir_rejects_traversal_and_nesting() {
        let root = Path::new("/w/.gleon/manifests");

        // Multi-segment, parent traversal and absolute paths must never escape the root.
        for bad in ["macos/aarch64", "../etc", "/etc", ".", "", "a:b", "MacOS"] {
            assert_eq!(
                resolve_platform_filter_dir(root, Some(bad)),
                Err(bad),
                "filter {bad:?} must be rejected"
            );
        }
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
    fn test_phash_of_and_build_manifest_roundtrip() {
        let png_bytes = include_bytes!("../../tests/fixtures/baseline_100x100.png");
        let sha256 = Sha256Hex::of(png_bytes);
        let phash = phash_of(SingleTestManifest::load_image_from_bytes(png_bytes).unwrap());

        let manifest = build_manifest(sha256.as_str(), &phash, 100, 100).unwrap();
        assert_eq!(manifest.hash.value(), sha256.as_str());
        assert_eq!(
            format!("{}:{}", manifest.phash.scheme(), manifest.phash.value()),
            phash
        );
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
        let key = |key: &str| PlatformKey::parse(key).unwrap();
        let temp = tempdir().unwrap();
        let paths = GleonPaths::new(temp.path());
        let macos_manifests = paths.manifests_dir(&key("macos"));
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

        // `&File` is `io::Write`, so a shared file collects the formatted log.
        let log_path = temp.path().join("merge.log");
        let log = std::sync::Arc::new(std::fs::File::create(&log_path).unwrap());
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(log)
            .finish();
        let merged = tracing::subscriber::with_default(subscriber, || {
            load_merged_index_with_fallback(&paths, &key("linux"), Some(&key("macos"))).unwrap()
        });
        assert_eq!(merged.len(), 1);
        assert!(merged.get("login").is_some());
        let log = std::fs::read_to_string(log_path).unwrap();
        assert!(
            log.contains(
                "Using fallback platform 'macos' for missing manifests on platform 'linux'"
            ),
            "{log}"
        );
    }
}
