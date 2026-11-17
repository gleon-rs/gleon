//! Handler for `gleon approve` subcommand.

use std::path::PathBuf;

use anyhow::Context as _;
use gleon_core::{context::ResolvedContext, ops::ListedCases};
use tracing::{info, warn};

use crate::{commands::env_run_id, exit_code::ExitCode};

/// Runs the `approve` command on the runs `from` (copies of `.gleon/runs/latest`, read as they
/// are), or on the latest run of the workspace, picked with `GLEON_RUN_ID`.
pub fn run_approve(
    ctx: &ResolvedContext,
    paths: &[PathBuf],
    from: &[PathBuf],
) -> anyhow::Result<ExitCode> {
    // The caller's run id names a run of this workspace, never a copied one.
    let run_id = from
        .is_empty()
        .then(env_run_id)
        .transpose()
        .context("Error approving screenshots")?
        .flatten();
    let res = gleon_core::ops::approve_workspace(ctx, paths, from, run_id.as_ref())
        .context("Error approving screenshots")?;
    for warning in &res.warnings {
        warn!("{warning}");
    }
    info!(
        "Approved {} screenshot(s): {}.",
        res.approved.len(),
        ListedCases(&res.approved)
    );
    Ok(ExitCode::Success)
}
