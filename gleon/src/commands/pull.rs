//! Implementation of the `gleon pull` subcommand.

use anyhow::Context as _;
use gleon_core::{context::ResolvedContext, ops::pull_blobs, storage::StorageConfig};
use gleon_model::platform::PlatformKey;
use tracing::info;

/// Runs the `gleon pull` subcommand; `require_storage` (`--require-storage`) fails instead of
/// working in local mode without remote storage.
///
/// # Errors
/// Returns the error of the pull. There is no other failure: local mode and a workspace of
/// golden files end in success (`gleon diff --auto-pull` stops on this error only).
pub async fn run_pull(
    ctx: &ResolvedContext,
    storage_cfg: Option<&StorageConfig>,
    all_platforms: bool,
    platform_override: Option<&PlatformKey>,
    require_storage: bool,
) -> anyhow::Result<()> {
    if crate::commands::goldens_nothing_to_do(ctx, "pull").is_some() {
        return Ok(());
    }
    info!("Running blob pull...");

    let res = pull_blobs(
        ctx,
        storage_cfg,
        all_platforms,
        platform_override,
        require_storage,
    )
    .await
    .context("Error pulling baseline blobs")?;

    if res.local_mode {
        info!("Operating in local mode. Cloud sync disabled. Please configure storage.");
    } else if res.total_manifest_blobs == 0 {
        info!("No baseline blobs found to pull.");
    } else if res.downloaded_blobs == 0 {
        info!(
            "All {} baseline blob(s) are already up to date locally.",
            res.total_manifest_blobs
        );
    } else {
        info!(
            "Downloaded {} missing baseline blob(s) from storage ({} skipped, total: {}).",
            res.downloaded_blobs, res.skipped_blobs, res.total_manifest_blobs
        );
    }
    Ok(())
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
    use gleon_core::context::ContextOptions;
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn test_run_pull_uninitialized_and_local_mode() {
        let temp = tempdir().unwrap();
        let ctx = ResolvedContext::from_options(&ContextOptions::default(), temp.path()).unwrap();

        // 1. Uninitialized -> Failure
        assert!(run_pull(&ctx, None, false, None, false).await.is_err());

        // 2. Initialized + Local Mode -> Success
        gleon_core::ops::init_workspace(&ctx, None).unwrap();
        run_pull(&ctx, None, false, None, false).await.unwrap();

        // 3. Initialized + Storage Configured + 0 manifest blobs -> Success
        let cfg = StorageConfig::new("memory://");
        run_pull(&ctx, Some(&cfg), false, None, false)
            .await
            .unwrap();

        // 4. Storage required: missing -> Failure, configured -> Success
        assert!(run_pull(&ctx, None, false, None, true).await.is_err());
        run_pull(&ctx, Some(&cfg), false, None, true).await.unwrap();
    }
}
