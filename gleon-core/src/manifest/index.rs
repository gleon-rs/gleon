//! In-memory workspace index built from per-test manifest files.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use crate::{
    manifest::{ManifestError, single::SingleTestManifest},
    naming::normalize_test_name,
};

/// Validates a relative test path (e.g. `auth/login_screen`).
/// Splits on both `/` and `\`, verifying that each segment contains only valid characters `[a-z0-9_.-]`.
///
/// # Errors
/// Returns [`ManifestError::Validation`] if `test_path` is empty, contains a segment with
/// invalid characters, or attempts parent-directory traversal.
pub fn validate_test_path(test_path: &str) -> Result<(), ManifestError> {
    crate::naming::validate_test_name(test_path)
        .map_err(|e| ManifestError::Validation(e.to_string()))
}

/// In-memory index mapping test case relative paths to their `SingleTestManifest`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceIndex {
    entries: BTreeMap<String, SingleTestManifest>,
    source_paths: BTreeMap<String, String>,
}

impl WorkspaceIndex {
    /// Creates a new empty `WorkspaceIndex`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            source_paths: BTreeMap::new(),
        }
    }

    /// Loads the `WorkspaceIndex` by scanning the given platform manifest directory.
    /// If the directory does not exist on disk, returns an empty index.
    ///
    /// The files are listed first and then parsed in parallel; errors are still reported in walk
    /// order, like a sequential load.
    ///
    /// # Errors
    /// Returns [`ManifestError::Walker`] if directory traversal fails, [`ManifestError::Validation`]
    /// if a manifest file has a non-UTF-8 path, an invalid test path, or collides with another
    /// entry after normalization, or any error from [`SingleTestManifest::load`] for a malformed
    /// manifest file.
    pub fn load<P: AsRef<Path>>(manifest_dir: P) -> Result<Self, ManifestError> {
        let Some(listing) = list_manifests(manifest_dir.as_ref()) else {
            return Ok(Self::new());
        };
        // Reading and parsing the files dominates, so it runs in parallel; the checks below go in
        // walk order, so the first error is the one a sequential load would hit.
        let loaded = load_all(&listing.files);
        let mut index = Self::new();
        for (file, manifest) in listing.files.into_iter().zip(loaded) {
            if index.entries.contains_key(&file.key) {
                return Err(ManifestError::Validation(format!(
                    "Duplicate test case key collision in manifest index: '{}'",
                    file.key
                )));
            }
            let manifest = manifest?;
            index.source_paths.insert(file.key.clone(), file.source);
            index.entries.insert(file.key, manifest);
        }
        listing.error.map_or(Ok(index), Err)
    }

    /// Returns `true` if the index contains no test cases.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the number of test cases in the index.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns a reference to the inner entries map.
    #[must_use]
    pub const fn entries(&self) -> &BTreeMap<String, SingleTestManifest> {
        &self.entries
    }

    /// Consumes the index and returns the inner entries map.
    #[must_use]
    pub fn into_entries(self) -> BTreeMap<String, SingleTestManifest> {
        self.entries
    }

    /// Gets a single test manifest by test case name.
    #[must_use]
    pub fn get(&self, test_name: &str) -> Option<&SingleTestManifest> {
        let normalized = normalize_test_name(test_name);
        self.entries.get(normalized.as_ref())
    }

    /// Inserts or updates a single test manifest in memory.
    pub fn insert(&mut self, test_name: String, manifest: SingleTestManifest) {
        let normalized = normalize_test_name(&test_name).into_owned();
        self.source_paths.insert(normalized.clone(), test_name);
        self.entries.insert(normalized, manifest);
    }

    /// Removes a test case entry from the in-memory map.
    pub fn remove(&mut self, test_name: &str) -> Option<SingleTestManifest> {
        let normalized = normalize_test_name(test_name);
        self.source_paths.remove(normalized.as_ref());
        self.entries.remove(normalized.as_ref())
    }

    /// Merges entries from a fallback `WorkspaceIndex` into `self`.
    /// Existing entries in `self` (platform overrides) take precedence and are NOT overwritten.
    pub fn merge_fallback(&mut self, fallback: Self) {
        for (key, manifest) in fallback.entries {
            self.entries.entry(key).or_insert(manifest);
        }
        for (key, path) in fallback.source_paths {
            self.source_paths.entry(key).or_insert(path);
        }
    }

    /// Saves a single test manifest to disk under `manifest_dir` and updates memory.
    ///
    /// # Errors
    /// Returns [`ManifestError::Validation`] if `test_name` is not a valid test path,
    /// or [`ManifestError::Io`]/[`ManifestError::StdIo`] if writing the manifest file or
    /// removing a stale legacy-cased file fails.
    pub fn save_test<P: AsRef<Path>>(
        &mut self,
        manifest_dir: P,
        test_name: &str,
        manifest: &SingleTestManifest,
    ) -> Result<(), ManifestError> {
        let normalized = normalize_test_name(test_name);
        validate_test_path(normalized.as_ref())?;

        let manifest_dir = manifest_dir.as_ref();
        let canonical_key = normalized.as_ref();
        let target_path = manifest_file_path(manifest_dir, canonical_key);

        manifest.save(&target_path)?;

        // Remove legacy-cased manifest file on disk if it differs from canonical path
        if let Some(old_source) = self
            .source_paths
            .get(canonical_key)
            .filter(|s| *s != canonical_key)
        {
            let old_path = manifest_file_path(manifest_dir, old_source);
            let is_same_file = match (fs::canonicalize(&old_path), fs::canonicalize(&target_path)) {
                (Ok(p1), Ok(p2)) => p1 == p2,
                _ => false,
            };
            if !is_same_file {
                remove_file_ignore_missing(&old_path)?;
            }
        }

        self.source_paths
            .insert(canonical_key.to_string(), canonical_key.to_string());
        self.entries
            .insert(normalized.into_owned(), manifest.clone());
        Ok(())
    }

    /// Removes a test case manifest file from disk and memory.
    ///
    /// # Errors
    /// Returns [`ManifestError::Validation`] if `test_name` is not a valid test path, or
    /// [`ManifestError::StdIo`] if removing the manifest file from disk fails for a reason
    /// other than the file not existing.
    pub fn remove_test<P: AsRef<Path>>(
        &mut self,
        manifest_dir: P,
        test_name: &str,
    ) -> Result<Option<SingleTestManifest>, ManifestError> {
        let normalized = normalize_test_name(test_name);
        validate_test_path(normalized.as_ref())?;

        let manifest_dir = manifest_dir.as_ref();
        let canonical_key = normalized.as_ref();

        if let Some(old_source) = self.source_paths.remove(canonical_key) {
            let old_path = manifest_file_path(manifest_dir, &old_source);
            remove_file_ignore_missing(&old_path)?;
        }

        let target_path = manifest_file_path(manifest_dir, canonical_key);
        remove_file_ignore_missing(&target_path)?;
        Ok(self.entries.remove(canonical_key))
    }
}

/// A manifest file found by [`WorkspaceIndex::load`].
struct Listed {
    path: PathBuf,
    /// The path relative to the manifest directory without `.json`, as on disk.
    source: String,
    /// The normalized test name.
    key: String,
}

/// The manifest files of a directory in walk order, up to the first path that cannot be indexed.
struct Listing {
    files: Vec<Listed>,
    /// Why the walk stopped early.
    error: Option<ManifestError>,
}

/// Most threads reading manifest files: beyond a few, file opens contend in the kernel. Loading
/// 50k manifests (`tests/manifest_scale.rs`, M3 Max, 14 cores) takes:
/// - macOS on APFS: 1.03 s on one thread, 0.57-0.62 s on three, 0.59-0.70 s on four, 0.87 s on
///   six and 1.8 s on fourteen;
/// - Linux 7.0 on ext4 (Docker VM on the same machine): 158 ms on one thread, 91 ms on three,
///   78 ms on four, 70 ms on six, 69 ms on eight and 168 ms on fourteen.
///
/// Windows is not measured yet and keeps the cautious macOS value.
#[cfg(target_os = "linux")]
const LOAD_THREADS: usize = 6;
/// See the Linux value.
#[cfg(not(target_os = "linux"))]
const LOAD_THREADS: usize = 3;

/// Fewest manifests per loader thread: starting a thread costs tens of microseconds, reading 64
/// manifests about a millisecond.
const MIN_FILES_PER_THREAD: usize = 64;

/// Loads `files`, in their order, on up to [`LOAD_THREADS`] threads (on this one for a few files).
fn load_all(files: &[Listed]) -> Vec<Result<SingleTestManifest, ManifestError>> {
    let load = |chunk: &[Listed]| -> Vec<_> {
        chunk
            .iter()
            .map(|file| SingleTestManifest::load(&file.path))
            .collect()
    };
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get().min(LOAD_THREADS))
        .min(files.len().div_ceil(MIN_FILES_PER_THREAD));
    if threads <= 1 {
        return load(files);
    }
    std::thread::scope(|scope| {
        #[expect(
            clippy::needless_collect,
            reason = "every worker must be spawned before the first is joined"
        )]
        let workers: Vec<_> = files
            .chunks(files.len().div_ceil(threads))
            .map(|chunk| scope.spawn(move || load(chunk)))
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| {
                worker
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    })
}

/// Lists the manifest files under `manifest_dir`; `None` if the directory does not exist.
fn list_manifests(manifest_dir: &Path) -> Option<Listing> {
    let mut files = Vec::new();
    for entry in crate::walk::manifest_walker(manifest_dir).build() {
        let listed = match entry {
            Ok(entry) => listed(manifest_dir, entry),
            Err(err) => {
                let is_missing_root = err.depth() == Some(0)
                    && err
                        .io_error()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound);
                if is_missing_root {
                    return None;
                }
                Err(ManifestError::Walker(err))
            }
        };
        match listed {
            Ok(Some(file)) => files.push(file),
            Ok(None) => {}
            Err(error) => {
                return Some(Listing {
                    files,
                    error: Some(error),
                });
            }
        }
    }
    Some(Listing { files, error: None })
}

/// `entry` as a manifest file of `manifest_dir`; `None` for directories and other files.
fn listed(manifest_dir: &Path, entry: ignore::DirEntry) -> Result<Option<Listed>, ManifestError> {
    let path = entry.path();
    if !entry.file_type().is_some_and(|ft| ft.is_file())
        || path.extension().and_then(|ext| ext.to_str()) != Some("json")
    {
        return Ok(None);
    }
    let Ok(rel_path) = path.strip_prefix(manifest_dir) else {
        return Ok(None);
    };
    let without_ext = rel_path.with_extension("");
    let source = without_ext.to_str().ok_or_else(|| {
        ManifestError::Validation(format!(
            "Non UTF-8 path encountered in manifest directory: {}",
            without_ext.display()
        ))
    })?;
    let key = normalize_test_name(source);
    validate_test_path(key.as_ref())?;
    Ok(Some(Listed {
        key: key.into_owned(),
        source: source.to_owned(),
        path: entry.into_path(),
    }))
}

fn manifest_file_path(dir: &Path, key: &str) -> PathBuf {
    let mut file_name = std::ffi::OsString::from(key);
    file_name.push(".json");
    dir.join(file_name)
}

/// Removes the file at `path`, treating it already being absent as success.
fn remove_file_ignore_missing(path: &Path) -> Result<(), ManifestError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(ManifestError::StdIo(e)),
    }
}

// The loads touch the file system and may run on threads, which Miri does not model.
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

    #[test]
    fn test_remove_file_ignore_missing_is_idempotent() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("manifest.json");
        fs::write(&path, b"{}").unwrap();

        // First call removes it, second is a no-op rather than an error.
        remove_file_ignore_missing(&path).unwrap();
        assert!(!path.exists());
        remove_file_ignore_missing(&path).unwrap();
    }

    #[test]
    fn test_remove_file_ignore_missing_surfaces_other_errors() {
        // A directory is not a file: removal must fail loudly instead of being swallowed.
        let temp = tempdir().unwrap();
        let dir = temp.path().join("not-a-file");
        fs::create_dir(&dir).unwrap();

        assert!(matches!(
            remove_file_ignore_missing(&dir),
            Err(ManifestError::StdIo(_))
        ));
        assert!(dir.exists());
    }
    use tempfile::tempdir;

    use crate::manifest::ImageHash;

    #[test]
    fn test_save_and_load_dotted_test_name() {
        let temp = tempdir().unwrap();
        let manifest_dir = temp.path().join("manifests");
        fs::create_dir_all(&manifest_dir).unwrap();

        let mut index = WorkspaceIndex::new();
        let hash = ImageHash::new(
            "sha256",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        let phash = ImageHash::new("dhash", "0000000000000000").unwrap();
        let manifest = SingleTestManifest::new(hash, phash, 10, 10).unwrap();

        let dotted_test_name = "billing.v2";
        index
            .save_test(&manifest_dir, dotted_test_name, &manifest)
            .unwrap();

        assert!(manifest_dir.join("billing.v2.json").is_file());

        let loaded_index = WorkspaceIndex::load(&manifest_dir).unwrap();
        let loaded_manifest = loaded_index
            .get("billing.v2")
            .expect("Should find billing.v2 manifest");
        assert_eq!(loaded_manifest.width, 10);
    }

    #[test]
    fn test_workspace_index_load_empty() {
        let temp = tempdir().unwrap();
        let non_existent = temp.path().join("does_not_exist");
        let index = WorkspaceIndex::load(&non_existent).unwrap();
        assert!(index.entries().is_empty());
    }

    #[test]
    fn test_workspace_index_save_and_load() {
        let temp = tempdir().unwrap();
        let manifest_dir = temp.path().join("macos-aarch64");

        let hash = ImageHash::new("sha256", "a".repeat(64)).unwrap();
        let phash = ImageHash::new("dhash", "0000000000000000").unwrap();
        let single = SingleTestManifest::new(hash, phash, 100, 200).unwrap();

        let mut index = WorkspaceIndex::new();
        index
            .save_test(&manifest_dir, "auth/login_screen", &single)
            .unwrap();

        assert_eq!(index.entries().len(), 1);
        assert_eq!(index.get("auth/login_screen"), Some(&single));

        let loaded = WorkspaceIndex::load(&manifest_dir).unwrap();
        assert_eq!(index, loaded);

        // Test remove_test
        let removed = index
            .remove_test(&manifest_dir, "auth/login_screen")
            .unwrap();
        assert_eq!(removed, Some(single));
        assert!(index.entries().is_empty());

        // Test remove_test on non-existent file (should be Ok(None))
        let removed_again = index
            .remove_test(&manifest_dir, "auth/login_screen")
            .unwrap();
        assert_eq!(removed_again, None);

        // Test remove_test with invalid path (parent traversal / absolute)
        assert!(index.remove_test(&manifest_dir, "../invalid").is_err());
    }

    #[test]
    fn test_workspace_index_load_filters_and_validation() {
        let temp = tempdir().unwrap();
        let manifest_dir = temp.path().join("macos-aarch64");
        fs::create_dir_all(&manifest_dir).unwrap();

        // 1. Subdirectory inside manifest_dir (should be skipped)
        fs::create_dir_all(manifest_dir.join("subfolder")).unwrap();

        // 2. Non-JSON file (should be skipped)
        fs::write(manifest_dir.join("notes.txt"), "hello").unwrap();

        // 3. Invalid test path file (e.g. contains invalid chars)
        fs::write(manifest_dir.join("bad!name.json"), "{}").unwrap();

        let index = WorkspaceIndex::load(&manifest_dir);
        assert!(index.is_err());
    }

    #[test]
    fn test_workspace_index_load_reports_a_malformed_manifest() {
        let temp = tempdir().unwrap();
        let manifest_dir = temp.path().join("macos-aarch64");
        fs::create_dir_all(&manifest_dir).unwrap();
        let hash = ImageHash::new("sha256", "a".repeat(64)).unwrap();
        let phash = ImageHash::new("dhash", "0000000000000000").unwrap();
        let manifest = SingleTestManifest::new(hash, phash, 100, 100).unwrap();
        for name in ["a", "b", "c"] {
            manifest
                .save(manifest_dir.join(format!("{name}.json")))
                .unwrap();
        }
        fs::write(manifest_dir.join("broken.json"), "{").unwrap();

        assert!(matches!(
            WorkspaceIndex::load(&manifest_dir),
            Err(ManifestError::Io(_))
        ));
    }

    /// Enough manifests for several loader threads: all load, in walk order, and a broken file
    /// in a later chunk is still reported.
    #[test]
    fn test_workspace_index_load_on_threads() {
        let temp = tempdir().unwrap();
        let manifest_dir = temp.path().join("macos-aarch64");
        let count = MIN_FILES_PER_THREAD * LOAD_THREADS + 1;
        for dir in 0..7 {
            fs::create_dir_all(manifest_dir.join(format!("dir_{dir}"))).unwrap();
        }
        for id in 0..count {
            // Plain writes: `save` syncs every file, which takes seconds for this many.
            let manifest = format!(
                r#"{{"schema_version":1,"hash":"sha256:{id:064x}","phash":"dhash:{id:016x}","width":{},"height":1}}"#,
                id + 1
            );
            let path = manifest_dir.join(format!("dir_{}/case_{id:04}.json", id % 7));
            fs::write(path, manifest).unwrap();
        }

        let index = WorkspaceIndex::load(&manifest_dir).unwrap();
        assert_eq!(index.len(), count);
        for id in 0..count {
            let manifest = index.get(&format!("dir_{}/case_{id:04}", id % 7)).unwrap();
            assert_eq!(manifest.width, u32::try_from(id + 1).unwrap());
        }

        fs::write(manifest_dir.join("dir_6/zz_broken.json"), "{").unwrap();
        assert!(matches!(
            WorkspaceIndex::load(&manifest_dir),
            Err(ManifestError::Io(_))
        ));
    }

    /// `a\b.json` is a file name on Unix; normalized, it is the test `a/b` of `a/b.json`.
    #[cfg(unix)]
    #[test]
    fn test_workspace_index_load_rejects_keys_colliding_after_normalization() {
        let temp = tempdir().unwrap();
        let manifest_dir = temp.path().join("macos-aarch64");
        fs::create_dir_all(manifest_dir.join("a")).unwrap();
        let hash = ImageHash::new("sha256", "a".repeat(64)).unwrap();
        let phash = ImageHash::new("dhash", "0000000000000000").unwrap();
        let manifest = SingleTestManifest::new(hash, phash, 100, 100).unwrap();
        manifest.save(manifest_dir.join("a/b.json")).unwrap();
        manifest.save(manifest_dir.join("a\\b.json")).unwrap();

        assert!(matches!(
            WorkspaceIndex::load(&manifest_dir),
            Err(ManifestError::Validation(message)) if message.contains("collision in manifest index: 'a/b'")
        ));
    }

    /// Linux file names are bytes; APFS rejects names that are not UTF-8.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_workspace_index_load_rejects_non_utf8_names() {
        use std::os::unix::ffi::OsStrExt as _;

        let temp = tempdir().unwrap();
        let manifest_dir = temp.path().join("linux-x86_64");
        fs::create_dir_all(&manifest_dir).unwrap();
        let name = std::ffi::OsStr::from_bytes(b"bad\xff.json");
        fs::write(manifest_dir.join(name), "{}").unwrap();

        assert!(matches!(
            WorkspaceIndex::load(&manifest_dir),
            Err(ManifestError::Validation(message)) if message.contains("Non UTF-8 path")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn test_workspace_index_load_reports_unreadable_directories() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempdir().unwrap();
        let manifest_dir = temp.path().join("macos-aarch64");
        let locked = manifest_dir.join("locked");
        fs::create_dir_all(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let is_readable = fs::read_dir(&locked).is_ok(); // Always readable as root.
        let result = WorkspaceIndex::load(&manifest_dir);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            matches!(result, Err(ManifestError::Walker(_))),
            !is_readable,
            "{result:?}"
        );
    }

    #[test]
    fn test_workspace_index_load_rejects_duplicates() {
        let temp = tempdir().unwrap();
        let manifest_dir = temp.path().join("macos-aarch64");
        fs::create_dir_all(manifest_dir.join("auth")).unwrap();

        let hash = ImageHash::new("sha256", "a".repeat(64)).unwrap();
        let phash = ImageHash::new("dhash", "0000000000000000").unwrap();
        let manifest = SingleTestManifest::new(hash, phash, 100, 100).unwrap();
        manifest.save(manifest_dir.join("auth/login.json")).unwrap();

        let loaded = WorkspaceIndex::load(&manifest_dir).unwrap();
        assert_eq!(loaded.entries.len(), 1);
        assert!(loaded.get("auth/login").is_some());
    }

    #[test]
    fn test_workspace_index_into_entries_and_validation() {
        let hash = ImageHash::new("sha256", "a".repeat(64)).unwrap();
        let phash = ImageHash::new("dhash", "0000000000000000").unwrap();
        let single = SingleTestManifest::new(hash, phash, 100, 200).unwrap();

        let mut index = WorkspaceIndex::new();
        index.insert("billing/form".to_string(), single.clone());
        assert_eq!(index.remove("billing/form"), Some(single));

        assert!(validate_test_path("").is_err());
        assert!(validate_test_path("invalid/../path").is_err());
        assert!(validate_test_path("invalid/path!").is_err());
        assert!(validate_test_path("Auth/Login").is_err());

        let mut index2 = WorkspaceIndex::new();
        index2.insert(
            "test".to_string(),
            SingleTestManifest::new(
                ImageHash::new("sha256", "b".repeat(64)).unwrap(),
                ImageHash::new("dhash", "1111111111111111").unwrap(),
                10,
                10,
            )
            .unwrap(),
        );
        let map = index2.into_entries();
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn test_legacy_cased_manifest_migration() {
        let temp = tempdir().unwrap();
        let manifest_dir = temp.path().join("macos-aarch64");
        fs::create_dir_all(manifest_dir.join("Auth")).unwrap();

        let hash = ImageHash::new("sha256", "a".repeat(64)).unwrap();
        let phash = ImageHash::new("dhash", "0000000000000000").unwrap();
        let manifest = SingleTestManifest::new(hash, phash.clone(), 100, 100).unwrap();

        // Save under legacy uppercase path Auth/Login.json
        manifest.save(manifest_dir.join("Auth/Login.json")).unwrap();

        // Load into WorkspaceIndex (canonicalizes key to auth/login)
        let mut loaded = WorkspaceIndex::load(&manifest_dir).unwrap();
        assert_eq!(loaded.entries().len(), 1);
        assert!(loaded.get("auth/login").is_some());

        // Update / save under canonical key
        let updated_hash = ImageHash::new("sha256", "b".repeat(64)).unwrap();
        let updated_manifest = SingleTestManifest::new(updated_hash, phash, 100, 100).unwrap();
        loaded
            .save_test(&manifest_dir, "auth/login", &updated_manifest)
            .unwrap();

        // Verify reloading does not find duplicates and contains updated manifest
        let reloaded = WorkspaceIndex::load(&manifest_dir).unwrap();
        assert_eq!(reloaded.entries().len(), 1);
        assert_eq!(
            reloaded.get("auth/login").unwrap().hash.value(),
            &"b".repeat(64)
        );
    }

    #[test]
    fn test_remove_test_and_duplicate_collision() {
        let temp = tempdir().unwrap();
        let manifest_dir = temp.path().join("manifests");
        fs::create_dir_all(&manifest_dir).unwrap();

        let hash = ImageHash::new("sha256", "a".repeat(64)).unwrap();
        let phash = ImageHash::new("dhash", "0000000000000000").unwrap();
        let manifest = SingleTestManifest::new(hash, phash, 100, 100).unwrap();

        manifest.save(manifest_dir.join("test_item.json")).unwrap();
        let mut index = WorkspaceIndex::load(&manifest_dir).unwrap();

        // 1. Remove existing test
        let removed = index.remove_test(&manifest_dir, "test_item").unwrap();
        assert!(removed.is_some());
        assert!(!manifest_dir.join("test_item.json").exists());

        // 2. Remove non-existent test
        let removed_none = index.remove_test(&manifest_dir, "non_existent").unwrap();
        assert!(removed_none.is_none());
    }

    #[test]
    fn test_merge_fallback_disjoint() {
        let mut target = WorkspaceIndex::new();
        target.insert(
            "test1".to_string(),
            SingleTestManifest::new(
                ImageHash::new("sha256", "1".repeat(64)).unwrap(),
                ImageHash::new("dhash", "0000000000000000").unwrap(),
                100,
                100,
            )
            .unwrap(),
        );

        let mut fallback = WorkspaceIndex::new();
        fallback.insert(
            "test2".to_string(),
            SingleTestManifest::new(
                ImageHash::new("sha256", "2".repeat(64)).unwrap(),
                ImageHash::new("dhash", "0000000000000000").unwrap(),
                200,
                200,
            )
            .unwrap(),
        );

        target.merge_fallback(fallback);

        assert_eq!(target.len(), 2);
        assert_eq!(target.get("test1").unwrap().hash.value(), &"1".repeat(64));
        assert_eq!(target.get("test2").unwrap().hash.value(), &"2".repeat(64));
    }

    #[test]
    fn test_merge_fallback_shadowing_priority() {
        let mut target = WorkspaceIndex::new();
        target.insert(
            "common_test".to_string(),
            SingleTestManifest::new(
                ImageHash::new("sha256", "a".repeat(64)).unwrap(),
                ImageHash::new("dhash", "0000000000000000").unwrap(),
                100,
                100,
            )
            .unwrap(),
        );

        let mut fallback = WorkspaceIndex::new();
        fallback.insert(
            "common_test".to_string(),
            SingleTestManifest::new(
                ImageHash::new("sha256", "b".repeat(64)).unwrap(),
                ImageHash::new("dhash", "0000000000000000").unwrap(),
                100,
                100,
            )
            .unwrap(),
        );
        fallback.insert(
            "fallback_only".to_string(),
            SingleTestManifest::new(
                ImageHash::new("sha256", "c".repeat(64)).unwrap(),
                ImageHash::new("dhash", "0000000000000000").unwrap(),
                100,
                100,
            )
            .unwrap(),
        );

        target.merge_fallback(fallback);

        assert_eq!(target.len(), 2);
        // target's own manifest takes precedence
        assert_eq!(
            target.get("common_test").unwrap().hash.value(),
            &"a".repeat(64)
        );
        // fallback test is added
        assert_eq!(
            target.get("fallback_only").unwrap().hash.value(),
            &"c".repeat(64)
        );
    }

    #[test]
    fn test_merge_fallback_empty() {
        let mut target = WorkspaceIndex::new();
        let fallback = WorkspaceIndex::new();
        target.merge_fallback(fallback);
        assert!(target.is_empty());
    }

    #[test]
    fn test_save_and_load_target_directory_manifest() {
        let temp = tempdir().unwrap();
        let manifest_dir = temp.path().join("manifests");
        fs::create_dir_all(&manifest_dir).unwrap();

        let mut index = WorkspaceIndex::new();
        let hash = ImageHash::new("sha256", "a".repeat(64)).unwrap();
        let phash = ImageHash::new("dhash", "0000000000000000").unwrap();
        let manifest = SingleTestManifest::new(hash, phash, 100, 100).unwrap();

        index
            .save_test(&manifest_dir, "target/login", &manifest)
            .unwrap();

        let loaded = WorkspaceIndex::load(&manifest_dir).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(loaded.get("target/login").is_some());
    }
}
