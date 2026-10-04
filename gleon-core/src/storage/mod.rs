pub mod adapter;

pub use adapter::{
    DeleteSummary, ObjectStoreAdapter, ObjectVersion, RemoteBlobEntry, RemoteObject, StorageConfig,
};
use object_store::path::Path as ObjPath;

pub use crate::ops::gc::{
    GcError, GcMode, GcOptions, GcResult, collect_all_referenced_hashes, filter_orphans_to_delete,
    garbage_collect,
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

/// Returns `true` only if `path` is an existing regular file and not a symlink.
#[must_use]
pub fn is_usable_blob(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| !meta.is_symlink() && meta.is_file())
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
