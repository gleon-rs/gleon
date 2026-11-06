pub mod adapter;

pub use adapter::{
    DeleteSummary, ObjectStoreAdapter, ObjectVersion, RemoteBlobEntry, RemoteObject, StorageConfig,
};
use object_store::path::Path as ObjPath;

pub use crate::ops::gc::{
    GcError, GcMode, GcOptions, GcResult, collect_all_referenced_hashes, garbage_collect,
};

/// Optional metadata attached to an uploaded blob in cloud storage.
///
/// When multiple test cases or platforms reference the exact same content hash, the metadata
/// attached to the remote blob represents the *first lexicographically discovered* reference
/// (deterministic traversal: sorted platform keys, then sorted test names).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlobMetadata {
    /// Test case path referencing the blob (e.g. `auth/login_screen`).
    pub test_name: Option<String>,
    /// Platform key where the blob was referenced (e.g. `macos-arm64`).
    pub platform: Option<String>,
    /// Exact relative path of the asset (e.g. `macos-arm64/auth/login_screen.png`).
    pub path: Option<String>,
    /// MIME content type of the asset (defaults to `Some("image/png")` when constructed via [`BlobMetadata::new`]).
    pub content_type: Option<String>,
}

impl BlobMetadata {
    /// Creates a new `BlobMetadata` pre-populating `test_name`, `platform`, calculated `path`,
    /// and default `content_type` (`image/png`).
    #[must_use]
    pub fn new(test_name: impl Into<String>, platform: impl Into<String>) -> Self {
        let test_name = test_name.into();
        let platform = platform.into();
        let path = format!("{platform}/{test_name}.png");
        Self {
            test_name: Some(test_name),
            platform: Some(platform),
            path: Some(path),
            content_type: Some("image/png".to_string()),
        }
    }
}

/// Storage error types.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// Error originating from the underlying `object_store` crate.
    #[error("Object store operation failed: {source}")]
    Store {
        /// Source error from `object_store`.
        #[from]
        source: object_store::Error,
    },

    /// Optimistic concurrency check failed (`ETag` or version mismatch).
    #[error("Storage precondition failed for '{path}'")]
    PreconditionFailed {
        /// Relative storage path.
        path: String,
        /// Underlying source error from `object_store`.
        #[source]
        source: object_store::Error,
    },

    /// Standard I/O error.
    #[error("I/O operation failed: {source}")]
    Io {
        /// Inner I/O error.
        #[from]
        source: std::io::Error,
    },

    /// Invalid or unparseable storage URL.
    #[error("Invalid storage URL '{url}': {reason}")]
    InvalidUrl {
        /// The raw invalid URL string.
        url: String,
        /// Reason for failure.
        reason: String,
    },

    /// Specified blob hash or object key was not found on remote.
    #[error("Object or blob not found on remote storage: {0}")]
    BlobNotFound(String),

    /// A blob's hash scheme cannot be checked on download (only `sha256` can), so it is not
    /// downloaded: an unchecked blob could silently become a baseline.
    #[error("cannot check blobs of hash scheme '{0}' on download (only sha256)")]
    UnsupportedHashScheme(String),

    /// A downloaded blob is not the content its hash names (truncated or replaced on the way).
    #[error("downloaded blob does not match its hash: expected {expected}, got {actual}")]
    HashMismatch {
        /// The blob's hash value.
        expected: String,
        /// The hash of the downloaded content.
        actual: String,
    },

    /// A local blob is not the content its hash names (changed in `.gleon/blobs`): never
    /// uploaded, since every pull of it would fail.
    #[error(
        "the local blob {expected} does not match its hash (got {actual}): remove it from \
         .gleon/blobs, then run `gleon stage` if this machine holds the screenshot, or push from \
         the machine that recorded it"
    )]
    LocalHashMismatch {
        /// The blob's hash value.
        expected: String,
        /// The hash of the local content.
        actual: String,
    },
}

/// Helper function constructing the remote object path for a CAS blob hash.
#[must_use]
pub fn blob_key(hash: &crate::manifest::ImageHash) -> ObjPath {
    ObjPath::from(format!("blobs/{}/{}", hash.scheme(), hash.value()))
}

/// Returns the local file path for a CAS blob under `blobs_root`.
#[must_use]
pub fn local_blob_path(
    blobs_root: &std::path::Path,
    hash: &crate::manifest::ImageHash,
) -> std::path::PathBuf {
    blobs_root.join(hash.scheme()).join(hash.value())
}

/// Returns `true` if `hash`'s blob exists under `blobs_root` and is usable.
///
/// A convenience for callers that only need the yes/no answer, not the path itself (which
/// they'd otherwise compute via [`local_blob_path`] just to immediately discard).
#[must_use]
pub fn has_usable_local_blob(
    blobs_root: &std::path::Path,
    hash: &crate::manifest::ImageHash,
) -> bool {
    is_usable_blob(&local_blob_path(blobs_root, hash))
}

/// Writes the blob `path` durably, never through a symbolic link.
///
/// Whatever is there (no usable blob, see [`is_usable_blob`]) is removed first, so the bytes land
/// in the cache and not at the target of a link: `gleon_model::fs::write_atomically` writes
/// through links, as goldens need.
///
/// # Errors
/// Returns the I/O error of the removal (a directory there is one) or of the write.
pub fn save_blob(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    gleon_model::fs::remove_if_exists(path).and_then(|()| {
        gleon_model::fs::write_atomically(path, bytes, gleon_model::fs::Durability::Durable)
    })
}

/// Whether `path` is a usable blob that holds the bytes `sha256` names.
///
/// The blob is opened once without following a link ([`open_regular_file`]) and read whole, like
/// `gleon push` reads it: a missing file, a link, a directory or other bytes (a changed cache)
/// are all no blob to keep.
#[must_use]
pub fn holds_blob(path: &std::path::Path, sha256: &gleon_model::case::Sha256Hex) -> bool {
    use std::io::Read as _;

    let mut bytes = Vec::new();
    open_regular_file(path)
        .and_then(|(mut file, _)| file.read_to_end(&mut bytes))
        .is_ok_and(|_| gleon_model::case::Sha256Hex::of(&bytes) == *sha256)
}

/// Returns `true` only if `path` is an existing regular file and not a symlink.
#[must_use]
pub fn is_usable_blob(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| is_usable_blob_metadata(&meta))
}

/// Whether `meta` (of [`std::fs::symlink_metadata`], which never follows a link) is that of a
/// usable blob: a regular file, not a symlink.
#[must_use]
pub fn is_usable_blob_metadata(meta: &std::fs::Metadata) -> bool {
    !meta.is_symlink() && meta.is_file()
}

/// Why [`open_regular_file`] refused a path: a symbolic link, a directory or another file that
/// is no regular file.
#[derive(Debug)]
struct NotRegularFile;

impl std::fmt::Display for NotRegularFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Symlink or non-regular file blobs are not allowed for security reasons")
    }
}

impl std::error::Error for NotRegularFile {}

/// Opens `path` for reading once, without following a symbolic link, as a regular file only.
///
/// Returns the handle with its metadata, checked on the handle itself, so the file read is the
/// file checked (no check-then-reopen race).
///
/// # Errors
/// Returns [`std::io::ErrorKind::NotFound`] for a missing file, an error [`is_not_regular_file`]
/// tells for a symbolic link, a directory or another non-regular file, and any other I/O error as
/// it is.
pub fn open_regular_file(
    path: &std::path::Path,
) -> std::io::Result<(std::fs::File, std::fs::Metadata)> {
    let not_regular = || std::io::Error::new(std::io::ErrorKind::InvalidInput, NotRegularFile);
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // No link followed, and a FIFO never blocks the open.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // The link itself opens, not its target.
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        // A directory opens too (like `std::fs::metadata` opens it), so the metadata check below
        // refuses it as no regular file instead of an access error.
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
    }
    // `O_NOFOLLOW` refuses a symbolic link with `ELOOP`.
    #[cfg(unix)]
    let refused_link = |e: &std::io::Error| e.raw_os_error() == Some(libc::ELOOP);
    #[cfg(not(unix))]
    let refused_link = |_: &std::io::Error| false;
    let file = options
        .open(path)
        .map_err(|e| if refused_link(&e) { not_regular() } else { e })?;
    // A reparse point or a directory opens on Windows: its handle's metadata tells it apart.
    let meta = file.metadata()?;
    #[cfg(windows)]
    let is_link = {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        meta.is_symlink() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    };
    #[cfg(not(windows))]
    let is_link = meta.is_symlink();
    if is_link || !meta.is_file() {
        return Err(not_regular());
    }
    Ok((file, meta))
}

/// Whether `error` is [`open_regular_file`]'s refusal of a path that is no regular file.
#[must_use]
pub fn is_not_regular_file(error: &std::io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(<dyn std::error::Error + Send + Sync>::is::<NotRegularFile>)
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
    use crate::manifest::ImageHash;

    fn sha256(value: &str) -> ImageHash {
        ImageHash::new("sha256", value).unwrap()
    }

    #[test]
    fn test_has_usable_local_blob_requires_a_real_file() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let hash = sha256(&"a".repeat(64));

        assert!(
            !has_usable_local_blob(root, &hash),
            "missing blob must not be reported as usable"
        );

        let path = local_blob_path(root, &hash);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"png bytes").unwrap();
        assert!(has_usable_local_blob(root, &hash));

        // A directory sitting where the blob should be is not a usable blob either.
        let dir_hash = sha256(&"b".repeat(64));
        std::fs::create_dir_all(local_blob_path(root, &dir_hash)).unwrap();
        assert!(!has_usable_local_blob(root, &dir_hash));
    }

    /// One open without following a link: a regular file opens with its metadata, a directory
    /// or a symbolic link is refused as no regular file, a missing path is `NotFound`.
    #[test]
    fn test_open_regular_file() {
        use std::io::Read as _;

        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("blob");
        std::fs::write(&file, b"png").unwrap();
        let (mut opened, meta) = open_regular_file(&file).unwrap();
        let mut bytes = Vec::new();
        opened.read_to_end(&mut bytes).unwrap();
        assert_eq!((bytes.as_slice(), meta.len()), (&b"png"[..], 3));

        let directory = open_regular_file(temp.path()).unwrap_err();
        assert!(is_not_regular_file(&directory), "{directory:?}");
        let missing = open_regular_file(&temp.path().join("missing")).unwrap_err();
        assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);
        assert!(!is_not_regular_file(&missing));
        #[cfg(unix)]
        {
            let link = temp.path().join("link");
            std::os::unix::fs::symlink(&file, &link).unwrap();
            let refused = open_regular_file(&link).unwrap_err();
            assert!(is_not_regular_file(&refused), "{refused:?}");
            assert!(
                refused.to_string().contains("non-regular file"),
                "{refused}"
            );
        }
    }

    /// The metadata predicate decides like the path check it backs.
    #[test]
    fn test_is_usable_blob_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("blob");
        std::fs::write(&file, b"png").unwrap();
        let meta = |path: &std::path::Path| std::fs::symlink_metadata(path).unwrap();
        assert!(is_usable_blob_metadata(&meta(&file)));
        assert!(!is_usable_blob_metadata(&meta(temp.path())));
        #[cfg(unix)]
        {
            let link = temp.path().join("link");
            std::os::unix::fs::symlink(&file, &link).unwrap();
            assert!(!is_usable_blob_metadata(&meta(&link)));
            assert!(link.is_file(), "the link resolves to a file");
        }
    }

    #[test]
    #[cfg(unix)]
    fn test_has_usable_local_blob_rejects_symlinks() {
        // Symlinked blobs are refused on purpose: a crafted manifest must not be able to make
        // gleon upload an arbitrary file from outside the blob store.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let hash = sha256(&"c".repeat(64));

        let target = temp.path().join("outside-secret.txt");
        std::fs::write(&target, b"secret").unwrap();
        let link = local_blob_path(root, &hash);
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(link.exists(), "symlink target resolves");
        assert!(!has_usable_local_blob(root, &hash));
    }
}
