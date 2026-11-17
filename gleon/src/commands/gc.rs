use anyhow::Context as _;
use gleon_core::{
    context::ResolvedContext,
    ops::gc::{GcMode, GcOptions, garbage_collect},
    storage::StorageConfig,
};
use indicatif::BinaryBytes;
use tracing::info;

use crate::exit_code::ExitCode;

/// Runs the `gleon gc` subcommand.
///
/// Returns [`ExitCode::Success`] on success or local mode, or [`ExitCode::Failure`] on error.
pub async fn run_gc(
    ctx: &ResolvedContext,
    storage_cfg: Option<&StorageConfig>,
    dry_run: bool,
    grace_period_hours: u32,
    force: bool,
) -> anyhow::Result<ExitCode> {
    if let Some(code) = crate::commands::goldens_nothing_to_do(ctx, "collect") {
        return Ok(code);
    }
    info!("Running storage garbage collection...");

    let options = GcOptions::new(dry_run, grace_period_hours, force);

    let res = garbage_collect(ctx, storage_cfg, &options)
        .await
        .context("Error during storage garbage collection")?;

    match res.mode {
        GcMode::LocalMode => {
            info!("Operating in local mode. Cloud sync disabled. Please configure storage.");
        }
        GcMode::DryRun => {
            info!(
                "[DRY RUN] Would delete {} orphan blob(s) ({} freed). {} referenced, {} protected by {}-hour grace period.",
                res.deleted_blobs,
                BinaryBytes(res.bytes_freed),
                res.referenced_blobs,
                res.protected_by_grace_period,
                grace_period_hours
            );

            let now = chrono::Utc::now();
            for blob in &res.orphans {
                let age = now.signed_duration_since(blob.last_modified);
                let age_hours = age.num_hours();
                let size_str = BinaryBytes(blob.size);
                let age_str = format!("{age_hours}h");
                info!(
                    hash = %blob.hash,
                    size = %size_str,
                    age = %age_str,
                    "[DRY RUN] Orphan candidate"
                );
            }
        }
        GcMode::Executed => {
            if res.deleted_blobs == 0 && res.failed_blobs == 0 {
                info!(
                    "No orphan blobs eligible for deletion. {} referenced blob(s), {} protected by {}-hour grace period.",
                    res.referenced_blobs, res.protected_by_grace_period, grace_period_hours
                );
            } else {
                let freed_str = BinaryBytes(res.bytes_freed);
                info!(
                    "Successfully deleted {} orphan blob(s) ({} freed). {} failed. {} referenced, {} protected by {}-hour grace period.",
                    res.deleted_blobs,
                    freed_str,
                    res.failed_blobs,
                    res.referenced_blobs,
                    res.protected_by_grace_period,
                    grace_period_hours
                );
                if res.failed_blobs > 0 {
                    return Ok(ExitCode::Failure);
                }
            }
        }
    }
    Ok(ExitCode::Success)
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
    use gleon_core::{context::ContextOptions, storage::ObjectStoreAdapter};
    use tempfile::tempdir;

    use super::*;
    use crate::commands::exit_code;

    #[tokio::test]
    async fn test_run_gc_uninitialized_and_local_mode() {
        let temp = tempdir().unwrap();
        let ctx = ResolvedContext::from_options(&ContextOptions::default(), temp.path()).unwrap();

        // 1. Uninitialized with storage -> Failure
        let cfg = StorageConfig::new("memory://");
        let exit_code_uninit = exit_code(run_gc(&ctx, Some(&cfg), false, 24, false).await);
        assert_eq!(exit_code_uninit, ExitCode::Failure);

        // 2. Initialized + Local Mode (no storage) -> Success
        gleon_core::ops::init_workspace(&ctx, None).unwrap();
        let exit_code_local = exit_code(run_gc(&ctx, None, false, 24, false).await);
        assert_eq!(exit_code_local, ExitCode::Success);

        // 3. Initialized + Storage Configured in non-git directory without force -> Failure (GitRequired)
        let exit_code_non_git = exit_code(run_gc(&ctx, Some(&cfg), false, 24, false).await);
        assert_eq!(exit_code_non_git, ExitCode::Failure);

        // 4. Initialized + Storage Configured + force -> Success
        let exit_code_force = exit_code(run_gc(&ctx, Some(&cfg), false, 24, true).await);
        assert_eq!(exit_code_force, ExitCode::Success);

        // 5. Initialized + Storage Configured + Dry Run (allowed without force) -> Success
        let exit_code_dry = exit_code(run_gc(&ctx, Some(&cfg), true, 24, false).await);
        assert_eq!(exit_code_dry, ExitCode::Success);

        // 6. Initialized + Storage Configured + Grace Period < 24h fails even with force
        let exit_code_short = exit_code(run_gc(&ctx, Some(&cfg), false, 12, true).await);
        assert_eq!(exit_code_short, ExitCode::Failure);
    }

    #[tokio::test]
    async fn test_run_gc_with_orphan_blobs_dry_run_and_execution() {
        let temp = tempdir().unwrap();
        let ctx = ResolvedContext::from_options(&ContextOptions::default(), temp.path()).unwrap();
        gleon_core::ops::init_workspace(&ctx, None).unwrap();

        let remote_temp = tempdir().unwrap();
        let remote_store_path = remote_temp.path();
        let url_str = url::Url::from_directory_path(remote_store_path)
            .unwrap()
            .to_string();
        let cfg = StorageConfig::new(url_str);

        let adapter = ObjectStoreAdapter::from_config(&cfg).unwrap();
        let dummy_src = temp.path().join("dummy.png");
        std::fs::write(&dummy_src, b"fake png").unwrap();

        // The content address of the dummy blob: uploads are checked.
        let h_orphan = gleon_core::manifest::ImageHash::new(
            "sha256",
            gleon_core::case::Sha256Hex::of(b"fake png").as_str(),
        )
        .unwrap();
        adapter.upload_blob(&h_orphan, &dummy_src).await.unwrap();

        // Artificially backdate the blob to 48 hours ago
        let blob_path = remote_store_path
            .join("blobs")
            .join("sha256")
            .join(h_orphan.value());
        let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(48 * 3600);
        let times = std::fs::FileTimes::new().set_modified(old_time);
        let f = std::fs::File::options()
            .write(true)
            .open(&blob_path)
            .unwrap();
        f.set_times(times).unwrap();
        drop(f);

        // 1. Dry Run with orphan -> outputs orphan candidate log (lines 62-71)
        let exit_code_dry = exit_code(run_gc(&ctx, Some(&cfg), true, 24, true).await);
        assert_eq!(exit_code_dry, ExitCode::Success);

        // 2. Executed mode with orphan -> deletes blob (lines 80-88)
        let exit_code_exec = exit_code(run_gc(&ctx, Some(&cfg), false, 24, true).await);
        assert_eq!(exit_code_exec, ExitCode::Success);

        // 3. Failed blob deletion (lines 89-91)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Another blob, under the SHA-256 of its bytes: uploads are checked.
            let fail_src = temp.path().join("fail.png");
            std::fs::write(&fail_src, b"another fake png").unwrap();
            let h_fail = gleon_core::manifest::ImageHash::new(
                "sha256",
                gleon_core::case::Sha256Hex::of(b"another fake png").as_str(),
            )
            .unwrap();
            adapter.upload_blob(&h_fail, &fail_src).await.unwrap();
            let fail_path = remote_store_path
                .join("blobs")
                .join("sha256")
                .join(h_fail.value());
            let f = std::fs::File::options()
                .write(true)
                .open(&fail_path)
                .unwrap();
            f.set_times(times).unwrap();
            drop(f);

            let parent_dir = fail_path.parent().unwrap();
            let orig_perm = std::fs::metadata(parent_dir).unwrap().permissions();
            std::fs::set_permissions(parent_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

            let exit_code_fail = exit_code(run_gc(&ctx, Some(&cfg), false, 24, true).await);

            std::fs::set_permissions(parent_dir, orig_perm).unwrap();
            assert_eq!(exit_code_fail, ExitCode::Failure);
        }
    }

    #[test]
    /// The sizes `gleon gc` prints (binary units of indicatif).
    fn test_sizes_in_binary_units() {
        let size = |bytes: u64| BinaryBytes(bytes).to_string();
        assert_eq!(size(500), "500 B");
        assert_eq!(size(1024), "1.00 KiB");
        assert_eq!(size(1536), "1.50 KiB");
        assert_eq!(size(10 * 1024 * 1024), "10.00 MiB");
        assert_eq!(size(2 * 1024 * 1024 * 1024), "2.00 GiB");
    }
}
