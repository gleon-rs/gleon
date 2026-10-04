//! Object store storage adapter implementing baseline and blob synchronization.

use std::{
    collections::{BTreeMap, HashSet},
    fmt,
    io::Write as _,
    path::Path,
    sync::Arc,
};

use futures::StreamExt as _;
use gleon_model::fs::Durability;
use object_store::{ObjectStore, ObjectStoreExt, parse_url_opts, path::Path as ObjPath};
use tracing::{debug, instrument, warn};

use super::{BlobMetadata, StorageError, blob_key};

/// Configuration for storage initialization and authentication credentials.
#[derive(Clone, PartialEq, Eq)]
pub struct StorageConfig {
    /// Remote storage URL (e.g., `s3://my-bucket/gleon`, `file:///path/to/store`, `memory://`).
    pub url: String,

    /// AWS or S3-compatible Access Key ID.
    pub aws_access_key_id: Option<String>,

    /// AWS or S3-compatible Secret Access Key.
    pub aws_secret_access_key: Option<String>,

    /// Google Cloud Storage JSON service account key.
    pub gcp_service_account_key: Option<String>,

    /// AWS region (defaults to `auto` for Cloudflare R2).
    pub aws_region: Option<String>,

    /// Custom AWS / S3 endpoint URL.
    pub aws_endpoint: Option<String>,

    /// Cloudflare R2 Account ID (used to construct R2 endpoint if endpoint is not set).
    pub r2_account_id: Option<String>,

    /// Concurrency limit for parallel transfer operations.
    pub concurrency: usize,
}

impl StorageConfig {
    /// Constructs a `StorageConfig` with standard defaults.
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            aws_access_key_id: None,
            aws_secret_access_key: None,
            gcp_service_account_key: None,
            aws_region: None,
            aws_endpoint: None,
            r2_account_id: None,
            concurrency: 8,
        }
    }

    /// Constructs a `StorageConfig` from an environment provider.
    ///
    /// `GLEON_*` prefixed variables override standard `AWS_*` / `R2_*` variables.
    /// Returns `None` if `GLEON_STORAGE_URL` is missing or empty.
    #[must_use]
    pub fn from_env(env: &dyn crate::env::EnvProvider) -> Option<Self> {
        let url_val = env.get_var("GLEON_STORAGE_URL")?;
        let url = url_val.trim();
        if url.is_empty() {
            return None;
        }

        // Not `env::get_trimmed_var`: credentials are returned verbatim (untrimmed) on a
        // non-blank match, with a fallback key, unlike that helper's trim-and-return semantics.
        let get_var = |gleon_key: &str, std_key: &str| -> Option<String> {
            env.get_var(gleon_key)
                .filter(|v| !v.trim().is_empty())
                .or_else(|| env.get_var(std_key).filter(|v| !v.trim().is_empty()))
        };

        let concurrency = env
            .get_var("GLEON_CONCURRENCY")
            .and_then(|v| v.parse().ok())
            .unwrap_or(8);

        Some(Self {
            url: url.to_string(),
            aws_access_key_id: get_var("GLEON_AWS_ACCESS_KEY_ID", "AWS_ACCESS_KEY_ID"),
            aws_secret_access_key: get_var("GLEON_AWS_SECRET_ACCESS_KEY", "AWS_SECRET_ACCESS_KEY"),
            gcp_service_account_key: get_var(
                "GLEON_GOOGLE_SERVICE_ACCOUNT_KEY",
                "GOOGLE_SERVICE_ACCOUNT_KEY",
            ),
            aws_region: get_var("GLEON_AWS_REGION", "AWS_REGION"),
            aws_endpoint: get_var("GLEON_AWS_ENDPOINT_URL", "AWS_ENDPOINT_URL"),
            r2_account_id: get_var("GLEON_R2_ACCOUNT_ID", "R2_ACCOUNT_ID"),
            concurrency,
        })
    }
}

impl fmt::Debug for StorageConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let masked_url = url::Url::parse(&self.url).map_or_else(
            |_| self.url.clone(),
            |mut parsed| {
                if parsed.password().is_some() {
                    let _ = parsed.set_password(Some("[REDACTED]"));
                }
                parsed.to_string()
            },
        );

        f.debug_struct("StorageConfig")
            .field("url", &masked_url)
            .field(
                "aws_access_key_id",
                &self.aws_access_key_id.as_ref().map(|_| "[PRESENT]"),
            )
            .field(
                "aws_secret_access_key",
                &self.aws_secret_access_key.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "gcp_service_account_key",
                &self.gcp_service_account_key.as_ref().map(|_| "[REDACTED]"),
            )
            .field("aws_region", &self.aws_region)
            .field("aws_endpoint", &self.aws_endpoint)
            .field("r2_account_id", &self.r2_account_id)
            .field("concurrency", &self.concurrency)
            .finish()
    }
}

use chrono::{DateTime, Utc};

/// An entry describing a blob present in remote storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteBlobEntry {
    /// Parsed and validated content-addressed hash.
    pub hash: crate::manifest::ImageHash,
    /// Last modification timestamp on remote storage.
    pub last_modified: DateTime<Utc>,
    /// Size of the blob in bytes.
    pub size: u64,
}

/// Summary of a batch blob deletion operation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeleteSummary {
    /// Number of blobs successfully deleted (or already deleted / `NotFound`).
    pub deleted: usize,
    /// Number of blobs that failed to delete due to an error.
    pub failed: usize,
    /// Hashes of blobs successfully deleted (or already deleted / `NotFound`).
    pub deleted_hashes: HashSet<crate::manifest::ImageHash>,
}

/// Unified storage adapter backing baseline and blob operations via `object_store`.
#[derive(Clone)]
pub struct ObjectStoreAdapter {
    store: Arc<dyn ObjectStore>,
    signer: Option<Arc<dyn object_store::signer::Signer>>,
    concurrency: usize,
    supports_attributes: bool,
}

impl ObjectStoreAdapter {
    /// Instantiates an `ObjectStoreAdapter` from a `StorageConfig`.
    ///
    /// # Errors
    /// Returns [`StorageError::InvalidUrl`] if the URL or parameters cannot be parsed by `object_store`.
    #[instrument(skip(config), level = "debug")]
    pub fn from_config(config: &StorageConfig) -> Result<Self, StorageError> {
        let invalid_url = |e: &dyn fmt::Display| StorageError::InvalidUrl {
            url: config.url.clone(),
            reason: e.to_string(),
        };

        let parsed_url = url::Url::parse(&config.url).map_err(|e| invalid_url(&e))?;

        let url_path = parsed_url.path().trim_start_matches('/');
        let prefix = if url_path.is_empty() {
            object_store::path::Path::default()
        } else {
            object_store::path::Path::parse(url_path).map_err(|e| invalid_url(&e))?
        };

        let supports_attributes = parsed_url.scheme() != "file";

        let (store, signer): (
            Arc<dyn ObjectStore>,
            Option<Arc<dyn object_store::signer::Signer>>,
        ) = match parsed_url.scheme() {
            "s3" | "r2" => {
                let mut builder = object_store::aws::AmazonS3Builder::from_env();
                if parsed_url.scheme() == "r2" {
                    let r2_as_s3 = config.url.replace("r2://", "s3://");
                    builder = builder.with_url(&r2_as_s3);
                } else {
                    builder = builder.with_url(&config.url);
                }

                if let Some(key_id) = &config.aws_access_key_id {
                    builder = builder.with_access_key_id(key_id);
                }
                if let Some(secret) = &config.aws_secret_access_key {
                    builder = builder.with_secret_access_key(secret);
                }
                if let Some(region) = &config.aws_region {
                    builder = builder.with_region(region);
                } else if config.r2_account_id.is_some() {
                    builder = builder.with_region("auto");
                }
                if let Some(endpoint) = &config.aws_endpoint {
                    builder = builder.with_endpoint(endpoint);
                } else if let Some(account_id) = &config.r2_account_id {
                    let r2_endpoint = format!("https://{account_id}.r2.cloudflarestorage.com");
                    builder = builder.with_endpoint(r2_endpoint);
                }

                let s3 = builder.build().map_err(|e| invalid_url(&e))?;
                if prefix.as_ref().is_empty() {
                    let s3_arc = Arc::new(s3);
                    (s3_arc.clone(), Some(s3_arc))
                } else {
                    let p_store = Arc::new(object_store::prefix::PrefixStore::new(s3, prefix));
                    (p_store.clone(), Some(p_store))
                }
            }
            "gs" => {
                let mut builder = object_store::gcp::GoogleCloudStorageBuilder::from_env();
                builder = builder.with_url(&config.url);
                if let Some(sec) = &config.gcp_service_account_key {
                    builder = builder.with_service_account_key(sec);
                }
                let gcs = builder.build().map_err(|e| invalid_url(&e))?;
                if prefix.as_ref().is_empty() {
                    let gcs_arc = Arc::new(gcs);
                    (gcs_arc.clone(), Some(gcs_arc))
                } else {
                    let p_store = Arc::new(object_store::prefix::PrefixStore::new(gcs, prefix));
                    (p_store.clone(), Some(p_store))
                }
            }
            _ => {
                let mut opts = BTreeMap::new();
                if let Some(key_id) = &config.aws_access_key_id {
                    let _ = opts.insert("aws_access_key_id".to_string(), key_id.clone());
                }
                if let Some(secret) = &config.aws_secret_access_key {
                    let _ = opts.insert("aws_secret_access_key".to_string(), secret.clone());
                }
                if let Some(region) = &config.aws_region {
                    let _ = opts.insert("aws_region".to_string(), region.clone());
                }
                if let Some(endpoint) = &config.aws_endpoint {
                    let _ = opts.insert("aws_endpoint".to_string(), endpoint.clone());
                }

                let (raw_store, path) =
                    parse_url_opts(&parsed_url, opts).map_err(|e| invalid_url(&e))?;

                let store: Arc<dyn ObjectStore> = if path.as_ref().is_empty() {
                    Arc::from(raw_store)
                } else {
                    Arc::new(object_store::prefix::PrefixStore::new(raw_store, path))
                };

                return Ok(Self {
                    store,
                    signer: None,
                    concurrency: std::cmp::max(1, config.concurrency),
                    supports_attributes,
                });
            }
        };

        Ok(Self {
            store,
            signer,
            concurrency: std::cmp::max(1, config.concurrency),
            supports_attributes,
        })
    }

    /// Generates a pre-signed URL for a given remote blob path if supported by the storage backend.
    #[instrument(skip(self), level = "debug")]
    pub async fn sign_blob_url(
        &self,
        relative_path: &str,
        expires_in: std::time::Duration,
    ) -> Option<String> {
        if let Some(signer) = &self.signer {
            let path = object_store::path::Path::parse(relative_path).ok()?;
            match signer
                .signed_url(http::Method::GET, &path, expires_in)
                .await
            {
                Ok(url) => Some(url.to_string()),
                Err(e) => {
                    warn!(
                        error = %e,
                        path = %relative_path,
                        "Failed to generate pre-signed URL for remote blob"
                    );
                    None
                }
            }
        } else {
            None
        }
    }

    /// Returns the concurrency limit configured for this adapter.
    #[must_use]
    pub const fn concurrency(&self) -> usize {
        self.concurrency
    }

    /// Returns the attributes of a remote blob, if supported and available.
    ///
    /// # Errors
    /// Returns [`StorageError`] if the remote query fails for a reason other than the blob not existing.
    #[instrument(skip(self), level = "debug")]
    pub async fn get_blob_attributes(
        &self,
        hash: &crate::manifest::ImageHash,
    ) -> Result<Option<object_store::Attributes>, StorageError> {
        let key = blob_key(hash);
        match self.store.get(&key).await {
            Ok(res) => Ok(Some(res.attributes)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(source) => Err(StorageError::Store { source }),
        }
    }

    /// Uploads a single blob from disk to remote storage at `blobs/<scheme>/<hash>` with optional metadata.
    ///
    /// # Errors
    /// Returns [`StorageError`] if the local file cannot be read or remote upload fails.
    #[instrument(skip(self, src_path, metadata), level = "debug")]
    pub async fn upload_blob_with_metadata(
        &self,
        hash: &crate::manifest::ImageHash,
        src_path: &Path,
        metadata: Option<&BlobMetadata>,
    ) -> Result<(), StorageError> {
        let key = blob_key(hash);
        let mut options = std::fs::OpenOptions::new();
        options.read(true);

        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }

        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
        }

        let std_file = options.open(src_path)?;

        // On Windows, opening a reparse point with FILE_FLAG_OPEN_REPARSE_POINT succeeds.
        // We must inspect the metadata of the opened handle to reject symlinks.
        // On Unix, O_NOFOLLOW fails to open symlinks with ELOOP, but this check is a harmless safety net.
        let file_meta = std_file.metadata()?;

        #[cfg(windows)]
        let is_symlink_or_reparse = {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
            file_meta.is_symlink()
                || (file_meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
        };
        #[cfg(not(windows))]
        let is_symlink_or_reparse = file_meta.is_symlink();

        if is_symlink_or_reparse || !file_meta.is_file() {
            return Err(StorageError::Io {
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Symlink or non-regular file blobs are not allowed for security reasons",
                ),
            });
        }

        let len = file_meta.len();
        const MAX_BLOB_SIZE: u64 = 100 * 1024 * 1024;
        if len > MAX_BLOB_SIZE {
            return Err(StorageError::Io {
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "Blob size {len} exceeds maximum allowed size of {MAX_BLOB_SIZE} bytes"
                    ),
                ),
            });
        }

        let mut file = tokio::fs::File::from_std(std_file);
        // `len` was validated above to be <= MAX_BLOB_SIZE (100 MiB), which fits comfortably
        // in `usize` on all supported platforms (including 32-bit targets).
        #[expect(
            clippy::cast_possible_truncation,
            reason = "length is bounded by a size limit checked above and fits `usize` on all supported targets"
        )]
        let mut bytes = Vec::with_capacity(len as usize);
        tokio::io::AsyncReadExt::read_to_end(&mut file, &mut bytes).await?;

        let payload_bytes = bytes::Bytes::from(bytes);

        // Fast-path: backends known not to support custom metadata (such as LocalFileSystem `file://`)
        // directly upload via `put`, avoiding redundant failed `put_opts` round-trips.
        if !self.supports_attributes {
            self.store
                .put(&key, object_store::PutPayload::from(payload_bytes))
                .await
                .map_err(|source| StorageError::Store { source })?;
            debug!(hash = %hash.value(), "Successfully uploaded blob to remote storage");
            return Ok(());
        }

        let attributes = build_blob_attributes(metadata);
        let put_opts = object_store::PutOptions {
            attributes,
            ..Default::default()
        };

        let put_res = self
            .store
            .put_opts(
                &key,
                object_store::PutPayload::from(payload_bytes.clone()),
                put_opts,
            )
            .await;

        if let Err(e) = put_res {
            // S3-compatible proxies, MinIO, or custom storage backends may reject
            // custom metadata or attributes. Since metadata is non-critical DevEx information,
            // we log a warning and fall back to standard `put` without attributes.
            tracing::warn!(
                hash = %hash.value(),
                error = %e,
                "Failed to upload blob with custom metadata, falling back to standard put without metadata"
            );
            self.store
                .put(&key, object_store::PutPayload::from(payload_bytes))
                .await
                .map_err(|source| StorageError::Store { source })?;
        }

        debug!(hash = %hash.value(), "Successfully uploaded blob to remote storage");
        Ok(())
    }

    /// Uploads a single blob from disk to remote storage at `blobs/<scheme>/<hash>`.
    ///
    /// # Errors
    /// Returns [`StorageError`] if the local file cannot be read or remote upload fails.
    #[instrument(skip(self, src_path), level = "debug")]
    pub async fn upload_blob(
        &self,
        hash: &crate::manifest::ImageHash,
        src_path: &Path,
    ) -> Result<(), StorageError> {
        self.upload_blob_with_metadata(hash, src_path, None).await
    }
}

/// Checks that a metadata value is suitable for an HTTP header value (non-empty, ASCII, non-control).
#[inline]
fn is_valid_meta_value(val: &str) -> bool {
    !val.is_empty() && val.chars().all(|c| c.is_ascii() && !c.is_ascii_control())
}

/// Constructs `object_store::Attributes` for a blob upload, validating and sanitizing headers.
fn build_blob_attributes(metadata: Option<&BlobMetadata>) -> object_store::Attributes {
    let mut attributes = object_store::Attributes::new();
    let content_type = metadata
        .and_then(|m| m.content_type.as_deref())
        .unwrap_or("image/png");

    if is_valid_meta_value(content_type) {
        attributes.insert(
            object_store::Attribute::ContentType,
            object_store::AttributeValue::from(content_type.to_string()),
        );
    }

    if let Some(meta) = metadata {
        if let Some(test_name) = &meta.test_name {
            if is_valid_meta_value(test_name) {
                attributes.insert(
                    object_store::Attribute::Metadata(std::borrow::Cow::Borrowed("test")),
                    object_store::AttributeValue::from(test_name.clone()),
                );
            } else {
                tracing::warn!(
                    test = ?test_name,
                    "Skipping invalid test metadata header containing non-ASCII or control characters"
                );
            }
        }
        if let Some(platform) = &meta.platform {
            if is_valid_meta_value(platform) {
                attributes.insert(
                    object_store::Attribute::Metadata(std::borrow::Cow::Borrowed("platform")),
                    object_store::AttributeValue::from(platform.clone()),
                );
            } else {
                tracing::warn!(
                    platform = ?platform,
                    "Skipping invalid platform metadata header containing non-ASCII or control characters"
                );
            }
        }
        if let Some(path) = &meta.path {
            if is_valid_meta_value(path) {
                attributes.insert(
                    object_store::Attribute::Metadata(std::borrow::Cow::Borrowed("path")),
                    object_store::AttributeValue::from(path.clone()),
                );
            } else {
                tracing::warn!(
                    path = ?path,
                    "Skipping invalid path metadata header containing non-ASCII or control characters"
                );
            }
        }
    }

    attributes
}

/// Returned object payload with version and `ETag` from remote storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteObject {
    /// Object binary contents.
    pub bytes: bytes::Bytes,
    /// `ETag` identifier from remote storage, if supported.
    pub e_tag: Option<String>,
    /// Version identifier from remote storage, if supported.
    pub version: Option<String>,
}

/// The version of a remote object (no contents), for a conditional put over it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectVersion {
    /// `ETag` identifier from remote storage, if supported.
    pub e_tag: Option<String>,
    /// Version identifier from remote storage, if supported.
    pub version: Option<String>,
}

impl ObjectStoreAdapter {
    /// Checks if a blob exists on remote storage without downloading it.
    ///
    /// # Errors
    /// Returns [`StorageError`] if the remote existence check fails for a reason other than
    /// the object simply not being found.
    pub async fn blob_exists(
        &self,
        hash: &crate::manifest::ImageHash,
    ) -> Result<bool, StorageError> {
        let key = blob_key(hash);
        match self.store.head(&key).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(StorageError::Store { source: e }),
        }
    }

    /// Downloads a single blob from remote storage at `blob_key(hash)` to `dest_path` atomically
    /// and durably ([`gleon_model::fs::write_atomically_with`]), streamed: a large blob never sits
    /// in memory whole. The content is checked against its hash before it is kept, so a truncated
    /// or wrong download never becomes a baseline; a blob of a scheme that cannot be checked
    /// (only `sha256` can) is not downloaded.
    ///
    /// # Errors
    /// Returns [`StorageError::UnsupportedHashScheme`] for a hash that cannot be checked,
    /// [`StorageError::BlobNotFound`] if the hash does not exist on remote storage,
    /// [`StorageError::Store`] if the download fails, [`StorageError::HashMismatch`] if the
    /// content is not the blob's, or [`StorageError::Io`] if writing it fails.
    #[instrument(skip(self, dest_path), level = "debug")]
    pub async fn download_blob(
        &self,
        hash: &crate::manifest::ImageHash,
        dest_path: &Path,
    ) -> Result<(), StorageError> {
        if hash.scheme() != "sha256" {
            return Err(StorageError::UnsupportedHashScheme(
                hash.scheme().to_owned(),
            ));
        }
        let key = blob_key(hash);
        let get_output = match self.store.get(&key).await {
            Ok(output) => output,
            Err(object_store::Error::NotFound { .. }) => {
                return Err(StorageError::BlobNotFound(hash.value().to_string()));
            }
            Err(err) => return Err(StorageError::Store { source: err }),
        };

        write_verified(get_output.into_stream(), dest_path, hash.value()).await?;

        debug!(hash = %hash.value(), path = %dest_path.display(), "Successfully downloaded blob from remote storage");
        Ok(())
    }

    /// Lists all blob hashes existing under the remote `blobs/<scheme>/` prefix for the given `scheme`.
    ///
    /// # Errors
    /// Returns [`StorageError`] if remote object listing fails.
    #[instrument(skip(self), level = "debug")]
    pub async fn list_blobs(&self, scheme: &str) -> Result<Vec<String>, StorageError> {
        let prefix = ObjPath::from(format!("blobs/{scheme}"));
        let mut list_stream = self.store.list(Some(&prefix));

        let mut hashes = Vec::new();
        while let Some(meta_res) = list_stream.next().await {
            let meta = meta_res.map_err(|source| StorageError::Store { source })?;
            if let Some(filename) = meta.location.filename() {
                hashes.push(filename.to_string());
            }
        }

        Ok(hashes)
    }

    /// Lists all valid remote CAS blobs under the `blobs/` prefix.
    ///
    /// Malformed object paths or non-blob items are safely skipped.
    ///
    /// # Errors
    /// Returns [`StorageError`] if remote object listing fails.
    #[instrument(skip(self), level = "debug")]
    pub async fn list_all_blobs(&self) -> Result<Vec<RemoteBlobEntry>, StorageError> {
        let prefix = ObjPath::from("blobs");
        let mut list_stream = self.store.list(Some(&prefix));

        let mut entries = Vec::new();
        while let Some(meta_res) = list_stream.next().await {
            let meta = meta_res.map_err(|source| StorageError::Store { source })?;
            let mut parts = meta.location.parts();
            match (parts.next(), parts.next(), parts.next(), parts.next()) {
                (Some(p0), Some(p1), Some(p2), None) if p0.as_ref() == "blobs" => {
                    if let Ok(hash) = crate::manifest::ImageHash::new(p1.as_ref(), p2.as_ref()) {
                        entries.push(RemoteBlobEntry {
                            hash,
                            last_modified: meta.last_modified,
                            size: meta.size,
                        });
                    } else {
                        warn!(
                            location = %meta.location,
                            "Skipping remote object with invalid image hash"
                        );
                    }
                }
                _ => {
                    debug!(
                        location = %meta.location,
                        "Skipping non-blob remote object under blobs/"
                    );
                }
            }
        }

        Ok(entries)
    }

    /// Deletes a single blob from remote storage at `blob_key(hash)`.
    ///
    /// # Errors
    /// Returns [`StorageError`] if remote deletion fails.
    #[instrument(skip(self), level = "debug")]
    pub async fn delete_blob(&self, hash: &crate::manifest::ImageHash) -> Result<(), StorageError> {
        let key = blob_key(hash);
        match self.store.delete(&key).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(source) => Err(StorageError::Store { source }),
        }
    }

    /// Batch deletes multiple blobs from remote storage using `delete_stream`.
    ///
    /// Tolerates `NotFound` (treating already deleted blobs as successful), aggregates
    /// failures, and invokes `on_progress` on each yielded result.
    ///
    /// # Errors
    /// Returns [`StorageError`] if deletion stream fails.
    #[instrument(skip(self, hashes, on_progress), level = "debug")]
    pub async fn delete_blobs_with_progress<'a, I, F>(
        &self,
        hashes: I,
        mut on_progress: F,
    ) -> Result<DeleteSummary, StorageError>
    where
        I: IntoIterator<Item = &'a crate::manifest::ImageHash>,
        F: FnMut(&crate::manifest::ImageHash, bool) + Send,
    {
        let hash_refs: Vec<&'a crate::manifest::ImageHash> = hashes.into_iter().collect();
        if hash_refs.is_empty() {
            return Ok(DeleteSummary::default());
        }

        let store = Arc::clone(&self.store);
        let concurrency = self.concurrency.max(1);

        let mut results_stream = futures::stream::iter(hash_refs)
            .map(|hash| {
                let store = Arc::clone(&store);
                let key = blob_key(hash);
                async move {
                    let res = store.delete(&key).await;
                    (hash, res)
                }
            })
            .buffer_unordered(concurrency);

        let mut summary = DeleteSummary::default();
        while let Some((hash, res)) = results_stream.next().await {
            match res {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => {
                    summary.deleted += 1;
                    summary.deleted_hashes.insert(hash.clone());
                    on_progress(hash, true);
                }
                Err(e) => {
                    warn!(error = %e, hash = %hash, "Failed to delete remote blob");
                    summary.failed += 1;
                    on_progress(hash, false);
                }
            }
        }

        Ok(summary)
    }

    /// Batch deletes multiple blobs from remote storage without progress callbacks.
    ///
    /// # Errors
    /// Returns [`StorageError`] if deletion stream fails.
    #[instrument(skip(self, hashes), level = "debug")]
    pub async fn delete_blobs<'a, I>(&self, hashes: I) -> Result<DeleteSummary, StorageError>
    where
        I: IntoIterator<Item = &'a crate::manifest::ImageHash>,
    {
        self.delete_blobs_with_progress(hashes, |_, _| {}).await
    }

    /// Fetches the raw bytes of an object from remote storage at `relative_path`.
    ///
    /// Returns `Ok(None)` if the object does not exist on remote storage.
    ///
    /// # Errors
    /// Returns [`StorageError`] if reading from remote storage fails for reasons other than `NotFound`.
    #[instrument(skip(self), level = "debug")]
    pub async fn get_object(
        &self,
        relative_path: &str,
    ) -> Result<Option<RemoteObject>, StorageError> {
        let key = ObjPath::from(relative_path);
        match self.store.get(&key).await {
            Ok(res) => {
                let e_tag = res.meta.e_tag.clone();
                let version = res.meta.version.clone();
                let bytes = res
                    .bytes()
                    .await
                    .map_err(|source| StorageError::Store { source })?;
                Ok(Some(RemoteObject {
                    bytes,
                    e_tag,
                    version,
                }))
            }
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(source) => Err(StorageError::Store { source }),
        }
    }

    /// The version of the object at `relative_path` without downloading it; `Ok(None)` if it
    /// does not exist.
    ///
    /// # Errors
    /// Returns [`StorageError`] if the check fails for reasons other than `NotFound`.
    #[instrument(skip(self), level = "debug")]
    pub async fn head_object(
        &self,
        relative_path: &str,
    ) -> Result<Option<ObjectVersion>, StorageError> {
        match self.store.head(&ObjPath::from(relative_path)).await {
            Ok(meta) => Ok(Some(ObjectVersion {
                e_tag: meta.e_tag,
                version: meta.version,
            })),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(source) => Err(StorageError::Store { source }),
        }
    }

    /// Uploads raw bytes to remote storage at `relative_path` with optional optimistic concurrency check.
    ///
    /// If `expected_e_tag` or `expected_version` is provided and the storage adapter supports conditional put,
    /// an atomic conditional update is performed (`PutMode::Update`).
    /// If a precondition failure occurs, returns [`StorageError::PreconditionFailed`].
    /// For storage schemes that do not support conditional updates (such as `file://`), falls back to overwrite.
    ///
    /// # Errors
    /// Returns [`StorageError::PreconditionFailed`] if optimistic concurrency check fails.
    /// Returns [`StorageError::Store`] on other storage errors.
    #[instrument(skip(self, data), level = "debug")]
    pub async fn put_object_conditional(
        &self,
        relative_path: &str,
        data: bytes::Bytes,
        content_type: Option<&str>,
        expected_e_tag: Option<&str>,
        expected_version: Option<&str>,
        create_only: bool,
    ) -> Result<(), StorageError> {
        let key = ObjPath::from(relative_path);
        let payload = object_store::PutPayload::from(data);

        let mut attributes = object_store::Attributes::new();
        if self.supports_attributes
            && let Some(ct) = content_type.filter(|ct| is_valid_meta_value(ct))
        {
            attributes.insert(
                object_store::Attribute::ContentType,
                object_store::AttributeValue::from((*ct).to_string()),
            );
        }

        let is_conditional = expected_e_tag.is_some() || expected_version.is_some();

        let mode = if create_only {
            object_store::PutMode::Create
        } else if is_conditional {
            object_store::PutMode::Update(object_store::UpdateVersion {
                e_tag: expected_e_tag.map(ToString::to_string),
                version: expected_version.map(ToString::to_string),
            })
        } else {
            object_store::PutMode::Overwrite
        };

        let put_opts = object_store::PutOptions {
            mode,
            attributes,
            ..Default::default()
        };

        match self.store.put_opts(&key, payload.clone(), put_opts).await {
            Ok(_) => Ok(()),
            Err(object_store::Error::Precondition { source, .. }) => {
                Err(StorageError::PreconditionFailed {
                    path: relative_path.to_string(),
                    source: object_store::Error::Precondition {
                        path: relative_path.to_string(),
                        source,
                    },
                })
            }
            Err(object_store::Error::AlreadyExists { source, .. }) => {
                Err(StorageError::PreconditionFailed {
                    path: relative_path.to_string(),
                    source: object_store::Error::AlreadyExists {
                        path: relative_path.to_string(),
                        source,
                    },
                })
            }
            Err(
                object_store::Error::NotImplemented { .. }
                | object_store::Error::NotSupported { .. },
            ) if is_conditional => {
                tracing::warn!(
                    "Storage backend does not support conditional update for '{}'; falling back to overwrite",
                    relative_path
                );
                self.store
                    .put(&key, payload)
                    .await
                    .map_err(|source| StorageError::Store { source })?;
                Ok(())
            }
            Err(source) => Err(StorageError::Store { source }),
        }
    }

    /// Uploads raw bytes to remote storage at `relative_path`.
    ///
    /// # Errors
    /// Returns [`StorageError`] if writing to remote storage fails.
    #[instrument(skip(self, data), level = "debug")]
    pub async fn put_object(
        &self,
        relative_path: &str,
        data: bytes::Bytes,
        content_type: Option<&str>,
    ) -> Result<(), StorageError> {
        self.put_object_conditional(relative_path, data, content_type, None, None, false)
            .await
    }
}

/// Streams `chunks` into `dest` atomically and durably, kept only if the stream ends and its
/// content hashes to `expected` (sha256).
async fn write_verified(
    mut chunks: impl futures::Stream<Item = object_store::Result<bytes::Bytes>> + Unpin,
    dest: &Path,
    expected: &str,
) -> Result<(), StorageError> {
    use sha2::Digest as _;

    // Chunks go to one blocking writer; `None` marks the end of the stream, and a channel closed
    // without it (a failed stream, or this future dropped) aborts the write: nothing is left.
    let (chunks_tx, mut chunks_rx) = tokio::sync::mpsc::channel::<Option<bytes::Bytes>>(8);
    let (dest, expected) = (dest.to_path_buf(), expected.to_owned());
    let writer = tokio::task::spawn_blocking(move || {
        let mut actual = None;
        let written = gleon_model::fs::write_atomically_with(&dest, Durability::Durable, |file| {
            let mut digest = sha2::Sha256::new();
            while let Some(chunk) = chunks_rx.blocking_recv() {
                let Some(chunk) = chunk else {
                    let digest = hex::encode(digest.finalize());
                    if digest.eq_ignore_ascii_case(&expected) {
                        return Ok(());
                    }
                    actual = Some(digest);
                    return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
                };
                digest.update(&chunk);
                file.write_all(&chunk)?;
            }
            Err(std::io::Error::other("download interrupted"))
        });
        (written, actual, expected)
    });
    let mut failed = None;
    while let Some(chunk) = chunks.next().await {
        match chunk {
            Ok(chunk) => {
                // A closed channel means the writer failed: its error is reported below.
                if chunks_tx.send(Some(chunk)).await.is_err() {
                    break;
                }
            }
            Err(e) => {
                failed = Some(e);
                break;
            }
        }
    }
    if failed.is_none() {
        let _ = chunks_tx.send(None).await;
    }
    drop(chunks_tx);
    let (written, actual, expected) = writer.await.map_err(|e| StorageError::Io {
        source: std::io::Error::other(e),
    })?;
    if let Some(source) = failed {
        return Err(StorageError::Store { source });
    }
    if let Some(actual) = actual {
        return Err(StorageError::HashMismatch { expected, actual });
    }
    Ok(written?)
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
    use std::collections::HashMap;

    use super::*;

    struct MapEnv(HashMap<String, String>);
    impl crate::env::EnvProvider for MapEnv {
        fn get_var(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    #[test]
    fn test_storage_config_from_env_map_missing_or_empty_url() {
        let vars = HashMap::new();
        assert!(StorageConfig::from_env(&MapEnv(vars)).is_none());

        let mut vars_empty = HashMap::new();
        vars_empty.insert("GLEON_STORAGE_URL".to_string(), "  ".to_string());
        assert!(StorageConfig::from_env(&MapEnv(vars_empty)).is_none());
    }

    #[test]
    fn test_storage_config_from_env_map_priorities() {
        let mut vars = HashMap::new();
        vars.insert(
            "GLEON_STORAGE_URL".to_string(),
            "s3://my-bucket/gleon".to_string(),
        );

        // Standard AWS vars
        vars.insert("AWS_ACCESS_KEY_ID".to_string(), "aws_key".to_string());
        vars.insert("AWS_SECRET_ACCESS_KEY".to_string(), "aws_sec".to_string());
        vars.insert("AWS_REGION".to_string(), "us-east-1".to_string());
        vars.insert(
            "AWS_ENDPOINT_URL".to_string(),
            "https://aws.endpoint".to_string(),
        );
        vars.insert("R2_ACCOUNT_ID".to_string(), "r2_acc".to_string());

        let cfg = StorageConfig::from_env(&MapEnv(vars.clone())).unwrap();
        assert_eq!(cfg.url, "s3://my-bucket/gleon");
        assert_eq!(cfg.aws_access_key_id.as_deref(), Some("aws_key"));
        assert_eq!(cfg.aws_secret_access_key.as_deref(), Some("aws_sec"));
        assert_eq!(cfg.aws_region.as_deref(), Some("us-east-1"));
        assert_eq!(cfg.aws_endpoint.as_deref(), Some("https://aws.endpoint"));
        assert_eq!(cfg.r2_account_id.as_deref(), Some("r2_acc"));
        assert_eq!(cfg.concurrency, 8);

        // Override with GLEON_ prefixed vars
        vars.insert(
            "GLEON_AWS_ACCESS_KEY_ID".to_string(),
            "gleon_key".to_string(),
        );
        vars.insert(
            "GLEON_AWS_SECRET_ACCESS_KEY".to_string(),
            "gleon_sec".to_string(),
        );
        vars.insert("GLEON_AWS_REGION".to_string(), "gleon-region".to_string());
        vars.insert(
            "GLEON_AWS_ENDPOINT_URL".to_string(),
            "https://gleon.endpoint".to_string(),
        );
        vars.insert("GLEON_R2_ACCOUNT_ID".to_string(), "gleon_r2".to_string());
        vars.insert("GLEON_CONCURRENCY".to_string(), "16".to_string());

        let cfg_override = StorageConfig::from_env(&MapEnv(vars)).unwrap();
        assert_eq!(cfg_override.aws_access_key_id.as_deref(), Some("gleon_key"));
        assert_eq!(
            cfg_override.aws_secret_access_key.as_deref(),
            Some("gleon_sec")
        );
        assert_eq!(cfg_override.aws_region.as_deref(), Some("gleon-region"));
        assert_eq!(
            cfg_override.aws_endpoint.as_deref(),
            Some("https://gleon.endpoint")
        );
        assert_eq!(cfg_override.r2_account_id.as_deref(), Some("gleon_r2"));
        assert_eq!(cfg_override.concurrency, 16);
    }

    #[tokio::test]
    #[cfg(not(miri))]
    async fn test_sign_blob_url_memory_store_returns_none() {
        let cfg = StorageConfig::new("memory://");
        let adapter = ObjectStoreAdapter::from_config(&cfg).unwrap();
        let res = adapter
            .sign_blob_url("blobs/sha256/1234", std::time::Duration::from_mins(1))
            .await;
        assert!(res.is_none());
    }

    #[tokio::test]
    #[cfg(not(miri))]
    async fn test_sign_blob_url_s3_store() {
        // Use from_env with empty env to ensure hermetic execution
        let mut cfg = StorageConfig::from_env(&MapEnv(HashMap::new()))
            .unwrap_or_else(|| StorageConfig::new("s3://mybucket"));
        cfg.aws_access_key_id = Some("testkey".to_string());
        cfg.aws_secret_access_key = Some("testsecret".to_string());
        cfg.aws_region = Some("us-east-1".to_string());
        let adapter = ObjectStoreAdapter::from_config(&cfg).unwrap();
        let url = adapter
            .sign_blob_url("blobs/sha256/1234", std::time::Duration::from_mins(1))
            .await
            .expect("Expected Some URL for S3 signing");
        assert!(url.contains("mybucket"));
        assert!(url.contains("X-Amz-Signature"));
    }

    #[tokio::test]
    #[cfg(not(miri))]
    async fn test_sign_blob_url_s3_store_with_prefix() {
        let mut cfg = StorageConfig::from_env(&MapEnv(HashMap::new()))
            .unwrap_or_else(|| StorageConfig::new("s3://mybucket/subfolder/prefix"));
        cfg.aws_access_key_id = Some("testkey".to_string());
        cfg.aws_secret_access_key = Some("testsecret".to_string());
        cfg.aws_region = Some("us-east-1".to_string());
        let adapter = ObjectStoreAdapter::from_config(&cfg).unwrap();
        let url = adapter
            .sign_blob_url("blobs/sha256/1234", std::time::Duration::from_mins(1))
            .await
            .expect("Expected Some URL for S3 signing");
        assert!(url.contains("mybucket"));
        assert!(url.contains("subfolder/prefix/blobs/sha256/1234"));
        assert!(url.contains("X-Amz-Signature"));
    }

    #[tokio::test]
    #[cfg(not(miri))]
    async fn test_sign_blob_url_gcs_store() {
        let cfg = StorageConfig::from_env(&MapEnv(HashMap::new()))
            .unwrap_or_else(|| StorageConfig::new("gs://mybucket"));
        let adapter = ObjectStoreAdapter::from_config(&cfg).unwrap();
        let res = adapter
            .sign_blob_url("blobs/sha256/1234", std::time::Duration::from_mins(1))
            .await;
        // Unauthenticated / metadata-less GCS safely falls back to None instead of failing
        assert!(res.is_none());
    }

    #[test]
    fn test_storage_config_invalid_path_syntax() {
        let cfg = StorageConfig::new("s3://mybucket//invalid//path");
        let res = ObjectStoreAdapter::from_config(&cfg);
        assert!(matches!(
            res,
            Err(StorageError::InvalidUrl { ref url, ref reason })
            if url == "s3://mybucket//invalid//path" && !reason.is_empty()
        ));
    }

    #[test]
    fn test_storage_config_invalid_s3_builder_error() {
        let cfg = StorageConfig::new("s3://");
        let res = ObjectStoreAdapter::from_config(&cfg);
        assert!(matches!(res, Err(StorageError::InvalidUrl { .. })));
    }

    #[test]
    fn test_storage_config_invalid_gcs_builder_error() {
        let mut cfg = StorageConfig::new("gs://mybucket");
        cfg.gcp_service_account_key = Some("not valid json".to_string());
        let res = ObjectStoreAdapter::from_config(&cfg);
        assert!(matches!(res, Err(StorageError::InvalidUrl { .. })));
    }

    #[tokio::test]
    #[cfg(all(unix, not(miri)))]
    async fn test_upload_blob_rejects_symlink() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target.png");
        std::fs::write(&target, b"fake png").unwrap();
        let link = temp.path().join("link.png");
        symlink(&target, &link).unwrap();

        let cfg = StorageConfig::new(format!("file://{}", temp.path().display()));
        let adapter = ObjectStoreAdapter::from_config(&cfg).unwrap();
        let hash = crate::manifest::ImageHash::new(
            "sha256",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .unwrap();

        let result = adapter.upload_blob(&hash, &link).await;
        assert!(matches!(result, Err(StorageError::Io { .. })));
    }

    #[tokio::test]
    #[cfg(not(miri))]
    async fn test_put_opts_error_fallback_to_plain_put() {
        let temp = tempfile::tempdir().unwrap();
        let url = format!("file://{}", temp.path().display());
        let cfg = StorageConfig::new(url);
        let mut adapter = ObjectStoreAdapter::from_config(&cfg).unwrap();

        // Force supports_attributes = true on LocalFileSystem so that put_opts is invoked.
        // LocalFileSystem returns NotImplemented on put_opts with attributes,
        // exercising the warning log and fallback to standard put.
        adapter.supports_attributes = true;

        let src_file = temp.path().join("fallback_sample.png");
        std::fs::write(&src_file, b"sample_fallback_data").unwrap();

        let hash = crate::manifest::ImageHash::new(
            "sha256",
            "6666666666666666666666666666666666666666666666666666666666666666",
        )
        .unwrap();

        let meta = BlobMetadata::new("checkout/cart", "ios-arm64");
        adapter
            .upload_blob_with_metadata(&hash, &src_file, Some(&meta))
            .await
            .expect("fallback to plain put should succeed");

        assert!(adapter.blob_exists(&hash).await.unwrap());
        let attrs = adapter.get_blob_attributes(&hash).await.unwrap().unwrap();
        assert!(attrs.is_empty());
    }

    #[tokio::test]
    #[cfg(not(miri))]
    async fn test_put_opts_error_fallback_failure_propagates_error() {
        let temp = tempfile::tempdir().unwrap();
        let url = format!("file://{}", temp.path().display());
        let cfg = StorageConfig::new(url);
        let mut adapter = ObjectStoreAdapter::from_config(&cfg).unwrap();

        adapter.supports_attributes = true;

        // In LocalFileSystem, blob keys are written to blobs/<scheme>/<hash>.
        // If "blobs" is a file instead of a directory, both put_opts and fallback put fail!
        let blobs_conflict = temp.path().join("blobs");
        std::fs::write(&blobs_conflict, b"not a directory").unwrap();

        let src_file = temp.path().join("sample.png");
        std::fs::write(&src_file, b"data").unwrap();

        let hash = crate::manifest::ImageHash::new(
            "sha256",
            "1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef",
        )
        .unwrap();

        let meta = BlobMetadata::new("checkout/cart", "ios-arm64");
        let res = adapter
            .upload_blob_with_metadata(&hash, &src_file, Some(&meta))
            .await;
        assert!(matches!(res, Err(StorageError::Store { .. })));
    }

    #[tokio::test]
    #[cfg(not(miri))]
    async fn test_upload_blob_put_failure_propagates_error() {
        let temp = tempfile::tempdir().unwrap();
        let url = format!("file://{}", temp.path().display());
        let cfg = StorageConfig::new(url);
        let adapter = ObjectStoreAdapter::from_config(&cfg).unwrap();
        assert!(!adapter.supports_attributes);

        let blobs_conflict = temp.path().join("blobs");
        std::fs::write(&blobs_conflict, b"not a directory").unwrap();

        let src_file = temp.path().join("sample2.png");
        std::fs::write(&src_file, b"data2").unwrap();

        let hash = crate::manifest::ImageHash::new(
            "sha256",
            "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890",
        )
        .unwrap();

        let res = adapter.upload_blob(&hash, &src_file).await;
        assert!(matches!(res, Err(StorageError::Store { .. })));
    }

    #[tokio::test]
    #[cfg(not(miri))]
    async fn test_get_and_put_object_round_trip() {
        let cfg = StorageConfig::new("memory://");
        let adapter = ObjectStoreAdapter::from_config(&cfg).unwrap();

        // 1. Missing object returns None
        let missing = adapter.get_object("history.json").await.unwrap();
        assert!(missing.is_none());

        // 2. Put object and verify get
        let content = bytes::Bytes::from_static(b"{\"schema_version\":1,\"runs\":[]}");
        adapter
            .put_object("history.json", content.clone(), Some("application/json"))
            .await
            .unwrap();

        let retrieved = adapter.get_object("history.json").await.unwrap().unwrap();
        assert_eq!(retrieved.bytes, content);
        assert_eq!(
            adapter.head_object("history.json").await.unwrap(),
            Some(ObjectVersion {
                e_tag: retrieved.e_tag,
                version: retrieved.version,
            }),
            "the version without the contents"
        );
        assert_eq!(adapter.head_object("missing.json").await.unwrap(), None);

        // 3. Put object with empty/invalid content-type filters it out cleanly
        adapter
            .put_object("empty_ct.json", content, Some(""))
            .await
            .unwrap();
    }

    #[tokio::test]
    #[cfg(not(miri))]
    async fn test_put_object_conditional_precondition_failure() {
        let cfg = StorageConfig::new("memory://");
        let adapter = ObjectStoreAdapter::from_config(&cfg).unwrap();

        let content = bytes::Bytes::from_static(b"{\"schema_version\":1,\"runs\":[]}");
        adapter
            .put_object("history.json", content.clone(), Some("application/json"))
            .await
            .unwrap();

        // Intentionally mismatched ETag must fail with PreconditionFailed
        let err = adapter
            .put_object_conditional(
                "history.json",
                content,
                Some("application/json"),
                Some("mismatched_etag"),
                None,
                false,
            )
            .await;

        assert!(matches!(err, Err(StorageError::PreconditionFailed { .. })));
    }

    #[tokio::test]
    #[cfg(not(miri))]
    async fn test_put_object_conditional_create_only_already_exists() {
        let cfg = StorageConfig::new("memory://");
        let adapter = ObjectStoreAdapter::from_config(&cfg).unwrap();

        let content = bytes::Bytes::from_static(b"{\"schema_version\":1,\"runs\":[]}");
        adapter
            .put_object("history.json", content.clone(), Some("application/json"))
            .await
            .unwrap();

        // Calling with create_only=true when the file exists must return PreconditionFailed
        let err = adapter
            .put_object_conditional(
                "history.json",
                content,
                Some("application/json"),
                None,
                None,
                true,
            )
            .await;

        assert!(matches!(err, Err(StorageError::PreconditionFailed { .. })));
    }

    #[tokio::test]
    #[cfg(not(miri))]
    async fn test_put_object_conditional_fallback_on_unsupported_backend() {
        let temp = tempfile::tempdir().unwrap();
        let url = format!("file://{}", temp.path().display());
        let cfg = StorageConfig::new(url);
        let adapter = ObjectStoreAdapter::from_config(&cfg).unwrap();

        let content = bytes::Bytes::from_static(b"data");
        let res = adapter
            .put_object_conditional("test.json", content, None, Some("etag1"), None, false)
            .await;

        assert!(res.is_ok());
    }

    #[tokio::test]
    #[cfg(not(miri))]
    async fn test_put_object_conditional_store_error() {
        let temp = tempfile::tempdir().unwrap();
        let url = format!("file://{}", temp.path().display());
        let cfg = StorageConfig::new(url);
        let adapter = ObjectStoreAdapter::from_config(&cfg).unwrap();

        let conflict = temp.path().join("conflict_dir_file");
        std::fs::write(&conflict, b"file not dir").unwrap();

        let content = bytes::Bytes::from_static(b"test");
        let res = adapter
            .put_object_conditional(
                "conflict_dir_file/nested.json",
                content,
                None,
                None,
                None,
                false,
            )
            .await;
        assert!(matches!(res, Err(StorageError::Store { .. })));
    }

    #[tokio::test]
    #[cfg(not(miri))]
    async fn test_put_object_conditional_fallback_store_error() {
        let temp = tempfile::tempdir().unwrap();
        let url = format!("file://{}", temp.path().display());
        let cfg = StorageConfig::new(url);
        let adapter = ObjectStoreAdapter::from_config(&cfg).unwrap();

        let conflict = temp.path().join("conflict_fb_file");
        std::fs::write(&conflict, b"file not dir").unwrap();

        let content = bytes::Bytes::from_static(b"test");
        let res = adapter
            .put_object_conditional(
                "conflict_fb_file/nested.json",
                content,
                None,
                Some("etag"),
                None,
                false,
            )
            .await;
        assert!(matches!(res, Err(StorageError::Store { .. })));
    }

    /// Writes `chunks` to `dir/blob`, expecting the hash of the chunks that are not errors.
    async fn write_chunks(
        dir: &Path,
        chunks: Vec<object_store::Result<bytes::Bytes>>,
    ) -> Result<(), StorageError> {
        use sha2::Digest as _;
        let whole: Vec<u8> = chunks.iter().flatten().flat_map(|c| c.to_vec()).collect();
        let expected = hex::encode(sha2::Sha256::digest(&whole));
        write_verified(futures::stream::iter(chunks), &dir.join("blob"), &expected).await
    }

    fn files_in(dir: &Path) -> usize {
        std::fs::read_dir(dir).unwrap().count()
    }

    #[tokio::test]
    async fn test_a_download_is_kept_only_when_its_stream_ends() {
        let dir = tempfile::tempdir().unwrap();
        let chunk = |bytes: &'static [u8]| Ok(bytes::Bytes::from_static(bytes));
        write_chunks(dir.path(), vec![chunk(b"a"), chunk(b"b")])
            .await
            .unwrap();
        assert_eq!(std::fs::read(dir.path().join("blob")).unwrap(), b"ab");
        std::fs::remove_file(dir.path().join("blob")).unwrap();

        // A stream that fails after a chunk: its error, and nothing written.
        let reset = object_store::Error::Generic {
            store: "test",
            source: "connection reset".into(),
        };
        let err = write_chunks(dir.path(), vec![chunk(b"a"), Err(reset), chunk(b"b")])
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::Store { .. }), "{err:?}");
        assert_eq!(files_in(dir.path()), 0, "no file, no temporary file");

        // A writer that cannot write: its error, with the stream left unread.
        let file = dir.path().join("file");
        std::fs::write(&file, b"").unwrap();
        let many = (0..32).map(|_| chunk(b"x")).collect();
        let err = write_chunks(&file, many).await.unwrap_err();
        assert!(matches!(err, StorageError::Io { .. }), "{err:?}");
    }

    /// A download dropped halfway (e.g. a cancelled pull) never commits the chunks it got, even
    /// when they hash as expected.
    #[tokio::test]
    async fn test_an_abandoned_download_writes_nothing() {
        use sha2::Digest as _;
        let dir = tempfile::tempdir().unwrap();
        let first = futures::stream::iter([Ok(bytes::Bytes::from_static(b"a"))]);
        let stalled = first.chain(futures::stream::pending());
        let dest = dir.path().join("blob");
        let expected = hex::encode(sha2::Sha256::digest(b"a"));
        let write = write_verified(stalled, &dest, &expected);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), write)
                .await
                .is_err()
        );
        // The blocking writer finishes on its own once the channel closes.
        for _ in 0..500 {
            if files_in(dir.path()) == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(files_in(dir.path()), 0);
    }
}
