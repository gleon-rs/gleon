#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::pedantic,
    clippy::nursery,
    missing_docs,
    reason = "test code: panics are assertions, and pedantic/nursery style lints are not enforced in tests"
)]
//! Unit tests for `ObjectStoreAdapter` using memory:// storage backend.

#![cfg(not(miri))]

use gleon_core::storage::{BlobMetadata, ObjectStoreAdapter, StorageConfig, StorageError};
use object_store::Attribute;
use tempfile::tempdir;

#[test]
fn test_storage_config_secret_masking() {
    let mut config =
        StorageConfig::new("https://myuser:mysecretpassword@s3.amazonaws.com/mybucket");
    config.aws_access_key_id = Some("AKIAIOSFODNN7EXAMPLE".to_string());
    config.aws_secret_access_key = Some("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string());

    let debug_str = format!("{config:?}");
    assert!(!debug_str.contains("wJalrXUtnFEMI"));
    assert!(!debug_str.contains("mysecretpassword"));
    assert!(debug_str.contains("[REDACTED]"));
    assert!(debug_str.contains("[PRESENT]"));
}

#[tokio::test]
async fn test_memory_store_blob_and_manifest_lifecycle() {
    let config = StorageConfig::new("memory://");
    let adapter = ObjectStoreAdapter::from_config(&config).expect("valid memory url");

    let dir = tempdir().expect("tempdir creation");
    let src_file = dir.path().join("sample_blob.png");
    std::fs::write(&src_file, b"png_file_bytes").expect("write src file");

    let blob_hash = sha256_of(b"png_file_bytes");

    // 1. Upload Blob
    adapter
        .upload_blob(&blob_hash, &src_file)
        .await
        .expect("upload blob ok");

    // 2. List Blobs
    let list = adapter.list_blobs("sha256").await.expect("list blobs ok");
    assert_eq!(list, vec![blob_hash.value().to_string()]);

    // 3. Download Blob
    let dest_file = dir.path().join("downloaded_blob.png");
    adapter
        .download_blob(&blob_hash, &dest_file)
        .await
        .expect("download blob ok");

    let downloaded_bytes = std::fs::read(&dest_file).expect("read downloaded file");
    assert_eq!(downloaded_bytes, b"png_file_bytes");

    // 4. Download non-existent blob -> BlobNotFound
    let missing_hash = gleon_core::manifest::ImageHash::new(
        "sha256",
        "0000000000000000000000000000000000000000000000000000000000000000",
    )
    .unwrap();
    let not_found = adapter.download_blob(&missing_hash, &dest_file).await;

    assert!(matches!(not_found, Err(StorageError::BlobNotFound(_))));

    // 5. Check blob_exists
    assert!(
        adapter
            .blob_exists(&blob_hash)
            .await
            .expect("blob_exists ok")
    );
    assert!(
        !adapter
            .blob_exists(&missing_hash)
            .await
            .expect("blob_exists false ok")
    );

    // 6. Upload missing local file -> StorageError::Io
    let missing_local = dir.path().join("non_existent_file.png");
    let upload_err = adapter.upload_blob(&blob_hash, &missing_local).await;
    assert!(matches!(upload_err, Err(StorageError::Io { .. })));
}

/// The content-addressed hash of `bytes`.
fn sha256_of(bytes: &[u8]) -> gleon_core::manifest::ImageHash {
    use sha2::Digest as _;
    gleon_core::manifest::ImageHash::new("sha256", hex::encode(sha2::Sha256::digest(bytes)))
        .unwrap()
}

/// A blob whose content is not what its hash names (truncated, or replaced by a proxy) is never
/// kept: no file, no temporary file.
#[tokio::test]
async fn test_download_rejects_content_that_does_not_match_its_hash() {
    let adapter = ObjectStoreAdapter::from_config(&StorageConfig::new("memory://")).unwrap();
    let dir = tempdir().unwrap();
    let claimed = sha256_of(b"the whole blob");
    // Seeded raw: `upload_blob` refuses content that is not what its hash names.
    adapter
        .put_object(
            &format!("blobs/sha256/{}", claimed.value()),
            b"truncated".to_vec().into(),
            None,
        )
        .await
        .unwrap();

    let blobs = dir.path().join("blobs/sha256");
    let dest = blobs.join(claimed.value());
    let err = adapter.download_blob(&claimed, &dest).await.unwrap_err();
    assert!(
        matches!(&err, StorageError::HashMismatch { expected, actual }
            if expected == claimed.value() && *actual == sha256_of(b"truncated").value()),
        "{err:?}"
    );
    assert!(!dest.exists());
    assert_eq!(
        std::fs::read_dir(&blobs).unwrap().count(),
        0,
        "no temporary file left"
    );
}

/// A blob whose hash cannot be checked is not downloaded at all.
#[tokio::test]
async fn test_download_refuses_hash_schemes_it_cannot_check() {
    let adapter = ObjectStoreAdapter::from_config(&StorageConfig::new("memory://")).unwrap();
    let dir = tempdir().unwrap();
    let hash = gleon_core::manifest::ImageHash::new("blake3", "a".repeat(64)).unwrap();
    let dest = dir.path().join("blob");
    let err = adapter.download_blob(&hash, &dest).await.unwrap_err();
    assert!(
        matches!(&err, StorageError::UnsupportedHashScheme(scheme) if scheme == "blake3"),
        "{err:?}"
    );
    assert!(!dest.exists());
}

/// Downloaded blobs get the same permissions as staged ones (written by `gleon_model::fs`).
#[cfg(unix)]
#[tokio::test]
async fn test_downloaded_blobs_are_world_readable() {
    use std::os::unix::fs::PermissionsExt as _;

    let adapter = ObjectStoreAdapter::from_config(&StorageConfig::new("memory://")).unwrap();
    let dir = tempdir().unwrap();
    let src = dir.path().join("src.png");
    std::fs::write(&src, b"blob").unwrap();
    let hash = sha256_of(b"blob");
    adapter.upload_blob(&hash, &src).await.unwrap();
    let dest = dir.path().join("blob");
    adapter.download_blob(&hash, &dest).await.unwrap();
    assert_eq!(
        std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
        0o644
    );
}

/// A symbolic link where the blob goes is replaced by the download, never written through: the
/// file it points to keeps its bytes.
#[cfg(unix)]
#[tokio::test]
async fn test_download_replaces_a_linked_blob() {
    let adapter = ObjectStoreAdapter::from_config(&StorageConfig::new("memory://")).unwrap();
    let dir = tempdir().unwrap();
    let src = dir.path().join("src.png");
    std::fs::write(&src, b"blob").unwrap();
    let hash = sha256_of(b"blob");
    adapter.upload_blob(&hash, &src).await.unwrap();
    let outside = dir.path().join("outside");
    std::fs::write(&outside, b"outside").unwrap();
    let dest = dir.path().join("blob");
    std::os::unix::fs::symlink(&outside, &dest).unwrap();

    adapter.download_blob(&hash, &dest).await.unwrap();
    let meta = std::fs::symlink_metadata(&dest).unwrap();
    assert!(meta.is_file(), "{meta:?}");
    assert_eq!(std::fs::read(&dest).unwrap(), b"blob");
    assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
}

#[test]
fn test_concurrency_clamp_to_one() {
    let mut config = StorageConfig::new("memory://");
    config.concurrency = 0;
    let adapter = ObjectStoreAdapter::from_config(&config).unwrap();
    assert_eq!(adapter.concurrency(), 1);
}

#[tokio::test]
async fn test_adapter_download_io_errors() {
    let config = StorageConfig::new("memory://");
    let adapter = ObjectStoreAdapter::from_config(&config).unwrap();

    let dir = tempdir().expect("tempdir creation");
    let src_file = dir.path().join("sample_blob.png");
    std::fs::write(&src_file, b"png_file_bytes").expect("write src file");

    let blob_hash = sha256_of(b"png_file_bytes");
    adapter.upload_blob(&blob_hash, &src_file).await.unwrap();

    let file_as_dir = dir.path().join("regular_file.txt");
    std::fs::write(&file_as_dir, b"not a directory").expect("write file as dir");

    let dest_file = file_as_dir.join("downloaded.png");
    let err = adapter.download_blob(&blob_hash, &dest_file).await;

    assert!(
        matches!(err, Err(StorageError::Io { .. })),
        "Expected Io error: {err:?}"
    );
}

#[tokio::test]
async fn test_adapter_list_blobs() {
    let config = StorageConfig::new("memory://");
    let adapter = ObjectStoreAdapter::from_config(&config).unwrap();

    let dir = tempdir().expect("tempdir creation");
    let src_file = dir.path().join("blob.png");
    std::fs::write(&src_file, b"sample bytes").unwrap();

    let blob_hash = sha256_of(b"sample bytes");
    adapter.upload_blob(&blob_hash, &src_file).await.unwrap();

    let blobs = adapter.list_blobs("sha256").await.unwrap();
    assert_eq!(blobs, vec![blob_hash.value().to_string()]);
}

#[test]
fn test_aws_options_coverage() {
    let mut config = StorageConfig::new("memory://");
    config.aws_access_key_id = Some("abc".to_string());
    config.aws_secret_access_key = Some("def".to_string());
    config.aws_region = Some("us-east-1".to_string());
    config.aws_endpoint = Some("http://localhost:9000".to_string());

    // We expect this to not panic and return Ok, even though memory:// ignores AWS options.
    let adapter = ObjectStoreAdapter::from_config(&config).unwrap();
    assert_eq!(adapter.concurrency(), 8);

    // Test the R2 fallback path
    let mut config_r2 = StorageConfig::new("memory://");
    config_r2.r2_account_id = Some("123456789".to_string());
    let adapter_r2 = ObjectStoreAdapter::from_config(&config_r2).unwrap();
    assert_eq!(adapter_r2.concurrency(), 8);

    // Test invalid url parse error
    let config_bad = StorageConfig::new("http://[:::1]"); // Invalid IPv6
    let err = ObjectStoreAdapter::from_config(&config_bad);
    assert!(matches!(err, Err(StorageError::InvalidUrl { .. })));
}

#[test]
fn test_blob_metadata_creation() {
    let meta = BlobMetadata::new("auth/login", "macos-arm64");
    assert_eq!(meta.test_name.as_deref(), Some("auth/login"));
    assert_eq!(meta.platform.as_deref(), Some("macos-arm64"));
    assert_eq!(meta.path.as_deref(), Some("macos-arm64/auth/login.png"));
    assert_eq!(meta.content_type.as_deref(), Some("image/png"));

    let default_meta = BlobMetadata::default();
    assert_eq!(default_meta.test_name, None);
    assert_eq!(default_meta.platform, None);
    assert_eq!(default_meta.path, None);
    assert_eq!(default_meta.content_type, None);
}

#[tokio::test]
async fn test_upload_blob_with_metadata_attributes() {
    let config = StorageConfig::new("memory://");
    let adapter = ObjectStoreAdapter::from_config(&config).unwrap();

    let dir = tempdir().unwrap();
    let src_file = dir.path().join("blob_with_meta.png");
    std::fs::write(&src_file, b"sample_png_bytes").unwrap();

    let hash = sha256_of(b"sample_png_bytes");

    let meta = BlobMetadata::new("auth/login_screen", "macos-arm64");
    adapter
        .upload_blob_with_metadata(&hash, &src_file, Some(&meta))
        .await
        .unwrap();

    // Verify stored attributes using encapsulated get_blob_attributes
    let attributes = adapter
        .get_blob_attributes(&hash)
        .await
        .unwrap()
        .expect("attributes present");

    let content_type = attributes
        .get(&Attribute::ContentType)
        .expect("ContentType attribute present");
    assert_eq!(content_type.as_ref(), "image/png");

    let test_attr = attributes
        .get(&Attribute::Metadata(std::borrow::Cow::Borrowed("test")))
        .expect("test metadata present");
    assert_eq!(test_attr.as_ref(), "auth/login_screen");

    let platform_attr = attributes
        .get(&Attribute::Metadata(std::borrow::Cow::Borrowed("platform")))
        .expect("platform metadata present");
    assert_eq!(platform_attr.as_ref(), "macos-arm64");

    let path_attr = attributes
        .get(&Attribute::Metadata(std::borrow::Cow::Borrowed("path")))
        .expect("path metadata present");
    assert_eq!(path_attr.as_ref(), "macos-arm64/auth/login_screen.png");

    // Query non-existent hash returns None
    let missing_hash = gleon_core::manifest::ImageHash::new(
        "sha256",
        "8888888888888888888888888888888888888888888888888888888888888888",
    )
    .unwrap();
    assert!(
        adapter
            .get_blob_attributes(&missing_hash)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn test_local_file_system_skips_attributes_for_file_scheme() {
    let dir = tempdir().unwrap();
    // file:// URL creates an adapter with supports_attributes = false
    let url = format!("file://{}", dir.path().display());
    let config = StorageConfig::new(url);
    let adapter = ObjectStoreAdapter::from_config(&config).unwrap();

    let src_file = dir.path().join("local_sample.png");
    std::fs::write(&src_file, b"sample_file_data").unwrap();

    let hash = sha256_of(b"sample_file_data");

    let meta = BlobMetadata::new("home/dashboard", "linux-x86_64");
    adapter
        .upload_blob_with_metadata(&hash, &src_file, Some(&meta))
        .await
        .expect("upload on LocalFileSystem should succeed directly via put");

    assert!(adapter.blob_exists(&hash).await.unwrap());
    let attrs = adapter.get_blob_attributes(&hash).await.unwrap().unwrap();
    assert!(attrs.is_empty());
}

#[tokio::test]
async fn test_upload_blob_with_custom_content_type_and_sanitization() {
    let config = StorageConfig::new("memory://");
    let adapter = ObjectStoreAdapter::from_config(&config).unwrap();

    let dir = tempdir().unwrap();
    let src_file = dir.path().join("blob_custom_meta.png");
    std::fs::write(&src_file, b"sample_custom_bytes").unwrap();

    let hash = sha256_of(b"sample_custom_bytes");

    // Pass custom content_type and invalid (control / newline) characters in metadata
    let mut meta = BlobMetadata::new("auth/login", "macos-arm64");
    meta.content_type = Some("image/webp".to_string());
    meta.test_name = Some("auth\nmalicious".to_string()); // Invalid CRLF should be skipped
    meta.platform = Some("macos\r\ninjection".to_string()); // Invalid CRLF should be skipped
    meta.path = Some("macos-arm64/auth/login.png".to_string()); // Valid, should be kept

    adapter
        .upload_blob_with_metadata(&hash, &src_file, Some(&meta))
        .await
        .unwrap();

    let attrs = adapter
        .get_blob_attributes(&hash)
        .await
        .unwrap()
        .expect("attributes present");

    assert_eq!(
        attrs.get(&Attribute::ContentType).unwrap().as_ref(),
        "image/webp"
    );
    // Malformed headers were skipped safely
    assert!(
        attrs
            .get(&Attribute::Metadata(std::borrow::Cow::Borrowed("test")))
            .is_none()
    );
    assert!(
        attrs
            .get(&Attribute::Metadata(std::borrow::Cow::Borrowed("platform")))
            .is_none()
    );
    // Valid header was retained
    assert_eq!(
        attrs
            .get(&Attribute::Metadata(std::borrow::Cow::Borrowed("path")))
            .unwrap()
            .as_ref(),
        "macos-arm64/auth/login.png"
    );

    // Second upload (another blob): invalid path and invalid content_type are also skipped safely
    let src_file2 = dir.path().join("blob_custom_meta_2.png");
    std::fs::write(&src_file2, b"sample_custom_bytes_2").unwrap();
    let hash2 = sha256_of(b"sample_custom_bytes_2");

    let meta2 = BlobMetadata {
        content_type: Some("invalid\ncontent/type".to_string()),
        path: Some("invalid\npath/screen.png".to_string()),
        ..Default::default()
    };

    adapter
        .upload_blob_with_metadata(&hash2, &src_file2, Some(&meta2))
        .await
        .unwrap();

    let attrs2 = adapter
        .get_blob_attributes(&hash2)
        .await
        .unwrap()
        .expect("attributes present");

    assert!(attrs2.get(&Attribute::ContentType).is_none());
    assert!(
        attrs2
            .get(&Attribute::Metadata(std::borrow::Cow::Borrowed("path")))
            .is_none()
    );

    // Third upload (another blob): metadata with None fields falls back to default Content-Type and
    // sets no headers
    let src_file3 = dir.path().join("blob_custom_meta_3.png");
    std::fs::write(&src_file3, b"sample_custom_bytes_3").unwrap();
    let hash3 = sha256_of(b"sample_custom_bytes_3");

    let meta3 = BlobMetadata::default();
    adapter
        .upload_blob_with_metadata(&hash3, &src_file3, Some(&meta3))
        .await
        .unwrap();

    let attrs3 = adapter
        .get_blob_attributes(&hash3)
        .await
        .unwrap()
        .expect("attributes present");

    assert_eq!(
        attrs3.get(&Attribute::ContentType).unwrap().as_ref(),
        "image/png"
    );
    assert!(
        attrs3
            .get(&Attribute::Metadata(std::borrow::Cow::Borrowed("test")))
            .is_none()
    );
    assert!(
        attrs3
            .get(&Attribute::Metadata(std::borrow::Cow::Borrowed("platform")))
            .is_none()
    );
    assert!(
        attrs3
            .get(&Attribute::Metadata(std::borrow::Cow::Borrowed("path")))
            .is_none()
    );
}

#[tokio::test]
async fn test_adapter_delete_blob_lifecycle() {
    let config = StorageConfig::new("memory://");
    let adapter = ObjectStoreAdapter::from_config(&config).expect("valid memory url");

    let dir = tempdir().expect("tempdir creation");
    let src_file = dir.path().join("sample_blob.png");
    std::fs::write(&src_file, b"png_file_bytes").expect("write src file");

    let blob_hash = sha256_of(b"png_file_bytes");

    // 1. Upload blob
    adapter
        .upload_blob(&blob_hash, &src_file)
        .await
        .expect("upload blob ok");

    // 2. Delete existing blob
    let del_res = adapter.delete_blob(&blob_hash).await;
    assert!(del_res.is_ok(), "delete_blob must succeed on existing blob");

    // 3. Delete non-existent blob (idempotent NotFound tolerance)
    let del_missing = adapter.delete_blob(&blob_hash).await;
    assert!(
        del_missing.is_ok(),
        "delete_blob must tolerate NotFound and return Ok"
    );
}

#[test]
fn test_adapter_r2_s3_gcs_construction_and_default_concurrency() {
    let mut config_r2 = StorageConfig::new("r2://my-bucket/custom-prefix");
    config_r2.aws_access_key_id = Some("test_key".to_string());
    config_r2.aws_secret_access_key = Some("test_secret".to_string());
    config_r2.aws_endpoint = Some("https://12345.r2.cloudflarestorage.com".to_string());
    config_r2.aws_region = Some("auto".to_string());

    let adapter = ObjectStoreAdapter::from_config(&config_r2).unwrap();
    assert_eq!(adapter.concurrency(), 8);

    let mut config_s3 = StorageConfig::new("s3://my-bucket/sub/prefix");
    config_s3.aws_access_key_id = Some("test_key".to_string());
    config_s3.aws_secret_access_key = Some("test_secret".to_string());
    config_s3.aws_region = Some("us-west-2".to_string());

    let adapter_s3 = ObjectStoreAdapter::from_config(&config_s3).unwrap();
    assert_eq!(adapter_s3.concurrency(), 8);

    let config_gs = StorageConfig::new("gs://my-bucket/custom-prefix");
    let adapter_gs = ObjectStoreAdapter::from_config(&config_gs).unwrap();
    assert_eq!(adapter_gs.concurrency(), 8);
}

/// A local file that is not what its hash names is never uploaded, like a download that is not
/// is never kept (real fixtures): the bucket stays content addressed.
#[tokio::test]
async fn test_upload_refuses_content_that_does_not_match_its_hash() {
    let adapter = ObjectStoreAdapter::from_config(&StorageConfig::new("memory://")).unwrap();
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let claimed = sha256_of(&std::fs::read(fixtures.join("baseline_100x100.png")).unwrap());

    let err = adapter
        .upload_blob(&claimed, &fixtures.join("corrupt.png"))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, StorageError::LocalHashMismatch { expected, actual }
            if expected == claimed.value() && actual != expected),
        "{err:?}"
    );
    // The advice covers a blob of this machine's screenshot and one of another platform's.
    assert!(err.to_string().contains("gleon stage"), "{err}");
    assert!(
        err.to_string().contains("the machine that recorded it"),
        "{err}"
    );
    assert!(
        !adapter.blob_exists(&claimed).await.unwrap(),
        "nothing is uploaded"
    );

    adapter
        .upload_blob(&claimed, &fixtures.join("baseline_100x100.png"))
        .await
        .unwrap();
    assert!(adapter.blob_exists(&claimed).await.unwrap());
}
