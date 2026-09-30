//! Implementation of the `gleon diff` subcommand.

use gleon_core::{context::ResolvedContext, storage::StorageConfig};
use tracing::info;

use crate::{
    commands::{pull, report_failure},
    exit_code::ExitCode,
};

/// Runs the `gleon diff` subcommand.
///
/// With `--auto-pull` it first pulls any baseline blobs missing locally, and aborts if that fails —
/// diffing against a half-populated blob store would report spurious missing baselines. Manifest
/// conflicts are resolved by `gleon resolve`.
pub async fn run_diff(
    ctx: &ResolvedContext,
    auto_pull: bool,
    storage_cfg: Option<StorageConfig>,
) -> ExitCode {
    if auto_pull {
        let pull_code = pull::run_pull(ctx, storage_cfg.as_ref(), false, None).await;
        if pull_code != ExitCode::Success {
            return pull_code;
        }
    }

    let report = match gleon_core::ops::run_diff(ctx) {
        Ok(report) => report,
        Err(e) => return report_failure("Error running visual diff", &e),
    };

    info!(
        "Ran {} test(s). Passed: {}, Failed: {}.",
        report.total_tests,
        report.total_tests.saturating_sub(report.failed_tests),
        report.failed_tests
    );
    info!("Report generated at {}", report.runs_dir.display());

    if report.passed {
        ExitCode::Success
    } else {
        ExitCode::Failure
    }
}
