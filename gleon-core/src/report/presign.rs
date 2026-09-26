//! Pre-signs remote storage URLs for the images referenced by failed test cases, so
//! `render_pr_comment` can link directly to signed URLs instead of falling back to
//! `base_image_url`-relative links.

use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

use tokio::sync::Semaphore;

use crate::{results::TestCaseResult, storage::ObjectStoreAdapter};

impl super::ReportGenerator {
    /// Signs remote storage URLs for the images referenced by up to
    /// [`Self::MAX_MARKDOWN_DIFF_ROWS`] failed test cases (the same cap `render_pr_comment`
    /// truncates its table to), running signing requests concurrently up to
    /// `adapter.concurrency()`.
    ///
    /// Best-effort: a path whose signing fails, panics, or is cancelled is simply absent from
    /// the returned map — callers fall back to `base_image_url`-relative linking (or `N/A`) for
    /// anything missing, so a partial result is fine.
    ///
    /// # Panics
    ///
    /// Never panics in this function's own stack: each spawned signing task acquires a
    /// semaphore permit via `.expect(...)`, which is safe because the semaphore is owned by
    /// this function's `JoinSet` and never closed while permits are outstanding. If a spawned
    /// task were to panic anyway, `join_next()` reports it as an `Err` (logged and skipped),
    /// not an unwind here.
    pub async fn sign_image_urls(
        adapter: &ObjectStoreAdapter,
        test_cases: &[TestCaseResult],
        expires_in: Duration,
    ) -> HashMap<PathBuf, String> {
        // Only baselines exist remotely, under their content-addressed key. `actual`/`diff` are
        // produced per run on the machine executing the tests and are never uploaded, so there
        // is nothing to sign for them — the report falls back to `base_image_url` or `N/A`.
        let to_sign = test_cases
            .iter()
            .filter(|tc| !tc.passed())
            .take(Self::MAX_MARKDOWN_DIFF_ROWS)
            .filter_map(|tc| tc.result.baseline_path())
            .filter_map(|path| {
                crate::storage::image_hash_from_local_blob_path(path)
                    .map(|hash| (path.to_path_buf(), hash))
            })
            .collect::<HashMap<_, _>>();

        let mut join_set = tokio::task::JoinSet::new();
        let semaphore = Arc::new(Semaphore::new(adapter.concurrency()));

        for (local_path, hash) in to_sign {
            // Address the same remote key `push`/`pull` use: `blobs/<scheme>/<value>`.
            let remote_key = format!("blobs/{}/{}", hash.scheme(), hash.value());
            let adapter = adapter.clone();
            let sem = Arc::clone(&semaphore);
            join_set.spawn(async move {
                // `sem` is owned by this task set and never closed while permits are
                // outstanding, so `acquire_owned` cannot fail here.
                #[expect(clippy::expect_used, reason = "the semaphore is owned by this task set and never closed while permits are outstanding")]
                let _permit = sem
                    .acquire_owned()
                    .await
                    .expect("semaphore never closed while permits are outstanding");
                adapter
                    .sign_blob_url(&remote_key, expires_in)
                    .await
                    .map(|signed| (local_path, signed))
            });
        }

        let mut signed_urls = HashMap::new();
        while let Some(res) = join_set.join_next().await {
            match res {
                Ok(Some((p, signed))) => {
                    let _ = signed_urls.insert(p, signed);
                }
                Ok(None) => {
                    tracing::warn!("Failed to generate pre-signed URL for a baseline blob");
                }
                Err(e) => {
                    tracing::warn!("URL signing task panicked or was cancelled: {}", e);
                }
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
    use super::*;
    use crate::{report::ReportGenerator, results::TestImageResult, storage::StorageConfig};

    #[tokio::test]
    async fn test_sign_image_urls_signs_referenced_paths_only() {
        let temp = tempfile::tempdir().unwrap();
        let cfg = StorageConfig::new(format!("file://{}", temp.path().display()));
        let adapter = ObjectStoreAdapter::from_config(&cfg).unwrap();

        let test_cases = vec![
            TestCaseResult {
                name: "passing".to_string(),
                result: TestImageResult::Success {
                    relative_path: "pass.png".into(),
                },
            },
            TestCaseResult {
                name: "mismatch".to_string(),
                result: TestImageResult::Mismatch {
                    relative_path: "rel.png".into(),
                    detail: gleon_engine::MismatchDetail::Pixel { diff_count: 1 },
                    diff_path: "diff.png".into(),
                    baseline_path: "baseline.png".into(),
                    actual_path: "actual.png".into(),
                },
            },
        ];

        // `file://` scheme adapters don't implement a signer, so every path resolves to `None`
        // and is simply absent from the result — exercises the "best-effort" `Ok(None)` path.
        let signed =
            ReportGenerator::sign_image_urls(&adapter, &test_cases, Duration::from_secs(60)).await;
        assert!(signed.is_empty());
    }

    #[tokio::test]
    async fn test_sign_image_urls_signs_the_cas_key_not_the_local_path() {
        // The signed URL must address the remote CAS object (`blobs/<scheme>/<hash>`), which is
        // where `push` actually uploads baselines. Signing the local blob path instead yields a
        // key like `.../private/var/folders/.../.gleon/blobs/...` that can only ever 404 — and
        // leaks the developer's absolute path into the PR comment.
        let mut cfg = StorageConfig::new("s3://my-bucket/gleon");
        cfg.aws_access_key_id = Some("testkey".to_string());
        cfg.aws_secret_access_key = Some("testsecret".to_string());
        cfg.aws_region = Some("us-east-1".to_string());
        let adapter = ObjectStoreAdapter::from_config(&cfg).unwrap();

        let digest = "d".repeat(64);
        let blobs_root = std::path::Path::new("/tmp/WorkSpace/.gleon/blobs");
        let hash = crate::manifest::ImageHash::new("sha256", &digest).unwrap();
        let baseline_path = crate::storage::local_blob_path(blobs_root, &hash);

        let test_cases = vec![TestCaseResult {
            name: "auth/login".to_string(),
            result: TestImageResult::Mismatch {
                relative_path: "test/Login.png".into(),
                detail: gleon_engine::MismatchDetail::Pixel { diff_count: 1 },
                diff_path: "/tmp/WorkSpace/.gleon/runs/latest/diffs/login.png".into(),
                baseline_path: baseline_path.clone(),
                actual_path: "/tmp/WorkSpace/.gleon/runs/latest/actual/login.png".into(),
            },
        }];

        let signed =
            ReportGenerator::sign_image_urls(&adapter, &test_cases, Duration::from_secs(60)).await;

        let url = signed
            .get(&baseline_path)
            .expect("baseline must be signed, keyed by its local path for the resolver");
        assert!(
            url.contains(&format!("blobs/sha256/{digest}")),
            "must sign the CAS key, got {url}"
        );
        assert!(
            !url.contains("WorkSpace") && !url.to_lowercase().contains("workspace"),
            "local filesystem path must never appear in the key: {url}"
        );

        // actual/diff live only on the runner; they are never uploaded, so they must not be
        // signed into dead links.
        assert_eq!(signed.len(), 1, "only the baseline is signable: {signed:?}");
    }

    #[tokio::test]
    async fn test_sign_image_urls_respects_max_rows_cap() {
        let temp = tempfile::tempdir().unwrap();
        let cfg = StorageConfig::new(format!("file://{}", temp.path().display()));
        let adapter = ObjectStoreAdapter::from_config(&cfg).unwrap();

        let test_cases: Vec<_> = (0..(ReportGenerator::MAX_MARKDOWN_DIFF_ROWS + 5))
            .map(|i| TestCaseResult {
                name: format!("mismatch_{i}"),
                result: TestImageResult::DecodeError {
                    relative_path: format!("{i}.png").into(),
                    error: "bad".to_string(),
                },
            })
            .collect();

        // Doesn't panic or hang when there are more failures than the row cap.
        let signed =
            ReportGenerator::sign_image_urls(&adapter, &test_cases, Duration::from_secs(60)).await;
        assert!(signed.is_empty());
    }
}
