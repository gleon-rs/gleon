//! Implementation of the `gleon diff` subcommand.

use anyhow::Context as _;
use gleon_core::{
    config::{ARTIFACTS_ENV, ArtifactsDir},
    context::ResolvedContext,
    env::EnvProvider,
    ops::diff::DiffOptions,
    storage::StorageConfig,
};
use tracing::info;

use crate::{
    commands::{env_run_id, pull},
    exit_code::ExitCode,
};

/// The options of `artifacts` (the command-line flag) and the environment: `GLEON_ARTIFACTS_DIR`
/// (also of `.gleon/.env`) and the process's `GLEON_RUN_ID`.
fn options(env: &dyn EnvProvider, artifacts: Option<ArtifactsDir>) -> anyhow::Result<DiffOptions> {
    Ok(DiffOptions {
        artifacts,
        artifacts_env: ArtifactsDir::from_env(env.get_var(ARTIFACTS_ENV).as_deref())?,
        run_id: env_run_id()?,
    })
}

/// Runs the `gleon diff` subcommand.
///
/// With `--auto-pull` it first pulls any baseline blobs missing locally, and aborts if that fails —
/// diffing against a half-populated blob store would report spurious missing baselines. Manifest
/// conflicts are resolved by `gleon resolve`.
pub async fn run_diff(
    ctx: &ResolvedContext,
    env: &dyn EnvProvider,
    auto_pull: bool,
    storage_cfg: Option<StorageConfig>,
    artifacts: Option<ArtifactsDir>,
) -> anyhow::Result<ExitCode> {
    let options = options(env, artifacts).context("Error running visual diff")?;
    if auto_pull {
        // A failed pull fails the diff (a pull has no other failure).
        pull::run_pull(ctx, storage_cfg.as_ref(), false, None, false).await?;
    }

    let report = gleon_core::ops::run_diff(ctx, &options).context("Error running visual diff")?;

    info!(
        "Ran {} test(s). Passed: {}, Failed: {}.",
        report.total_tests,
        report.total_tests.saturating_sub(report.failed_tests),
        report.failed_tests
    );
    info!("Report generated at {}", report.runs_dir.display());

    if report.failed_tests == 0 {
        Ok(ExitCode::Success)
    } else {
        Ok(ExitCode::Failure)
    }
}
