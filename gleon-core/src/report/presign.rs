//! Pre-signs remote storage URLs for the baselines of failed cases, so `render_pr_comment` can
//! link directly to signed URLs instead of falling back to `base_image_url` links.

use std::{
    collections::{BTreeSet, HashMap},
    time::Duration,
};

use crate::{cases::Cases, manifest::ImageHash, storage::ObjectStoreAdapter};

impl super::ReportGenerator {
    /// Signs remote storage URLs for the baselines (`golden.blob`) of the failed cases
    /// `render_pr_comment` shows (the first [`Self::MAX_MARKDOWN_DIFF_ROWS`] by severity).
    ///
    /// At most [`Self::MAX_MARKDOWN_DIFF_ROWS`] blobs, signed one by one. Best-effort: a baseline whose signing fails is absent from the returned
    /// map, and the comment falls back to `base_image_url` linking (or `N/A`).
    pub async fn sign_image_urls(
        adapter: &ObjectStoreAdapter,
        cases: &Cases,
        expires_in: Duration,
    ) -> HashMap<ImageHash, String> {
        // Only baselines exist remotely, under their content-addressed key. Candidates and diffs
        // are produced per run on the machine executing the tests and are never uploaded, so
        // there is nothing to sign for them.
        let to_sign: BTreeSet<&ImageHash> = cases
            .failures_by_severity()
            .into_iter()
            .take(Self::MAX_MARKDOWN_DIFF_ROWS)
            .filter_map(|(_, report)| report.golden.blob.as_ref())
            .collect();

        let mut signed_urls = HashMap::new();
        for hash in to_sign {
            // Address the same remote key `push`/`pull` use: `blobs/<scheme>/<value>`.
            let remote_key = format!("blobs/{}/{}", hash.scheme(), hash.value());
            if let Some(signed) = adapter.sign_blob_url(&remote_key, expires_in).await {
                let _ = signed_urls.insert(hash.clone(), signed);
            } else {
                tracing::warn!("Failed to generate pre-signed URL for a baseline blob");
            }
        }
        signed_urls
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
    use gleon_model::case::CaseOutcome;

    use super::*;
    use crate::{cases::fixtures::report, report::ReportGenerator, storage::StorageConfig};

    fn with_blob(name: &str, outcome: CaseOutcome, digest: &str) -> gleon_model::case::CaseReport {
        let mut report = report(name, outcome);
        report.golden.blob = Some(ImageHash::new("sha256", digest).unwrap());
        report
    }

    #[tokio::test]
    async fn test_sign_image_urls_without_a_signer_signs_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let cfg = StorageConfig::new(format!("file://{}", temp.path().display()));
        let adapter = ObjectStoreAdapter::from_config(&cfg).unwrap();
        let cases = Cases::new(
            "runs/latest",
            vec![with_blob("a", CaseOutcome::Mismatch, &"a".repeat(64))],
        );

        // `file://` scheme adapters don't implement a signer, so every blob resolves to `None`
        // and is simply absent from the result — exercises the "best-effort" `Ok(None)` path.
        let signed =
            ReportGenerator::sign_image_urls(&adapter, &cases, Duration::from_secs(60)).await;
        assert!(signed.is_empty());
    }

    #[tokio::test]
    async fn test_sign_image_urls_signs_the_cas_key_of_failed_cases() {
        // The signed URL must address the remote CAS object (`blobs/<scheme>/<hash>`), which is
        // where `push` actually uploads baselines.
        let mut cfg = StorageConfig::new("s3://my-bucket/gleon");
        cfg.aws_access_key_id = Some("testkey".to_string());
        cfg.aws_secret_access_key = Some("testsecret".to_string());
        cfg.aws_region = Some("us-east-1".to_string());
        let adapter = ObjectStoreAdapter::from_config(&cfg).unwrap();

        let failed = "d".repeat(64);
        let passed = "e".repeat(64);
        let cases = Cases::new(
            "runs/latest",
            vec![
                with_blob("auth/login", CaseOutcome::Mismatch, &failed),
                with_blob("auth/logout", CaseOutcome::Match, &passed),
                // A PNG golden of an integration has nothing remote.
                report("flutter", CaseOutcome::Mismatch),
            ],
        );

        let signed =
            ReportGenerator::sign_image_urls(&adapter, &cases, Duration::from_secs(60)).await;

        let hash = ImageHash::new("sha256", &failed).unwrap();
        let url = signed.get(&hash).expect("the failed baseline is signed");
        assert!(
            url.contains(&format!("blobs/sha256/{failed}")),
            "must sign the CAS key, got {url}"
        );
        assert_eq!(signed.len(), 1, "only baselines of failures: {signed:?}");
    }

    #[tokio::test]
    async fn test_sign_image_urls_respects_max_rows_cap() {
        let mut cfg = StorageConfig::new("s3://my-bucket/gleon");
        cfg.aws_access_key_id = Some("testkey".to_string());
        cfg.aws_secret_access_key = Some("testsecret".to_string());
        cfg.aws_region = Some("us-east-1".to_string());
        let adapter = ObjectStoreAdapter::from_config(&cfg).unwrap();

        let reports = (0..(ReportGenerator::MAX_MARKDOWN_DIFF_ROWS + 5))
            .map(|i| {
                with_blob(
                    &format!("test_{i:02}"),
                    CaseOutcome::Mismatch,
                    &format!("{i:064x}"),
                )
            })
            .collect();
        let signed = ReportGenerator::sign_image_urls(
            &adapter,
            &Cases::new("runs/latest", reports),
            Duration::from_secs(60),
        )
        .await;
        assert_eq!(signed.len(), ReportGenerator::MAX_MARKDOWN_DIFF_ROWS);
    }
}
