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
//! Live cloud smoke test for Cloudflare R2 / AWS S3 compatibility.
//! Runs only when explicitly invoked via `cargo test --test r2_smoke_test -- --ignored`.

#![cfg(not(miri))]

use std::{fs, path::PathBuf};

use gleon_core::storage::{ObjectStoreAdapter, StorageConfig};
use tempfile::tempdir;

fn fixture_path(filename: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(filename)
}

#[tokio::test]
#[ignore = "requires live R2 credentials in environment"]
async fn smoke_r2_live_upload_download() {
    let bucket = std::env::var("R2_BUCKET")
        .or_else(|_| std::env::var("AWS_BUCKET"))
        .expect("R2_BUCKET or AWS_BUCKET must be set for live smoke test");

    let account_id = std::env::var("R2_ACCOUNT_ID").ok();
    let access_key = std::env::var("R2_ACCESS_KEY_ID")
        .or_else(|_| std::env::var("AWS_ACCESS_KEY_ID"))
        .expect("Access key ID must be set for live smoke test");
    let secret_key = std::env::var("R2_SECRET_ACCESS_KEY")
        .or_else(|_| std::env::var("AWS_SECRET_ACCESS_KEY"))
        .expect("Secret access key must be set for live smoke test");

    let url = format!("s3://{bucket}/smoke-test");

    let mut config = StorageConfig::new(url);
    config.r2_account_id = account_id;
    config.aws_access_key_id = Some(access_key);
    config.aws_secret_access_key = Some(secret_key);

    let adapter = ObjectStoreAdapter::from_config(&config).expect("build live R2 store adapter");

    let png_fixture = fixture_path("baseline_100x100.png");
    let hash = gleon_core::manifest::ImageHash::new(
        "sha256",
        "d9d60867c3201adfe2c83a740037e71c8daef340283cc21c8a58d3f76d34669a",
    )
    .unwrap();

    adapter
        .upload_blob(&hash, &png_fixture)
        .await
        .expect("R2 upload blob ok");

    let local_dest = tempdir().expect("tempdir");
    let downloaded_file = local_dest.path().join("r2_downloaded.png");

    adapter
        .download_blob(&hash, &downloaded_file)
        .await
        .expect("R2 download blob ok");

    let src_bytes = fs::read(&png_fixture).expect("read src fixture");
    let dest_bytes = fs::read(&downloaded_file).expect("read downloaded file");
    assert_eq!(
        src_bytes, dest_bytes,
        "R2 downloaded bytes must match original uploaded bytes"
    );
}
