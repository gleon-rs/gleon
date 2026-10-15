//! Implementation of the `gleon pull` subcommand.

use gleon_core::{context::ResolvedContext, ops::pull_blobs, storage::StorageConfig};
use tracing::info;

use crate::{commands::report_failure, exit_code::ExitCode};

/// Runs the `gleon pull` subcommand.
///
/// Returns [`ExitCode::Success`] on success or local mode, or [`ExitCode::Failure`] on error.
pub async fn run_pull(
    ctx: &ResolvedContext,
    storage_cfg: Option<&StorageConfig>,
    all_platforms: bool,
    platform_override: Option<&str>,
) -> ExitCode {
    info!("Running blob pull...");

    let res = match pull_blobs(ctx, storage_cfg, all_platforms, platform_override).await {
        Ok(res) => res,
        Err(e) => return report_failure("Error pulling baseline blobs", &e),
    };

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
    ExitCode::Success
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
        let exit_code_uninit = run_pull(&ctx, None, false, None).await;
        assert_eq!(exit_code_uninit, ExitCode::Failure);

        // 2. Initialized + Local Mode -> Success
        gleon_core::ops::init_workspace(&ctx, None).unwrap();
        let exit_code_local = run_pull(&ctx, None, false, None).await;
        assert_eq!(exit_code_local, ExitCode::Success);

        // 3. Initialized + Storage Configured + 0 manifest blobs -> Success
        let cfg = StorageConfig::new("memory://");
        let exit_code_zero_blobs = run_pull(&ctx, Some(&cfg), false, None).await;
        assert_eq!(exit_code_zero_blobs, ExitCode::Success);
    }
}
