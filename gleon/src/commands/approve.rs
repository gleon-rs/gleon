//! Handler for `gleon approve` subcommand.

use std::path::PathBuf;

use gleon_core::context::ResolvedContext;
use tracing::{info, warn};

use crate::{
    commands::{env_run_id, report_failure},
    exit_code::ExitCode,
};

/// Runs the `approve` command on the runs `from` (copies of `.gleon/runs/latest`, read as they
/// are), or on the workspace's latest run picked with `GLEON_RUN_ID`.
pub fn run_approve(ctx: &ResolvedContext, paths: &[PathBuf], from: &[PathBuf]) -> ExitCode {
    // The caller's run id names the workspace's own run, never a copied one.
    let run_id = match from.is_empty().then(env_run_id).transpose() {
        Ok(run_id) => run_id.flatten(),
        Err(e) => return report_failure("Error approving screenshots", &*e),
    };
    let res = match gleon_core::ops::approve_workspace(ctx, paths, from, run_id.as_ref()) {
        Ok(res) => res,
        Err(e) => return report_failure("Error approving screenshots", &e),
    };
    for warning in &res.warnings {
        warn!("{warning}");
    }
    info!(
        "Approved {} screenshot(s): {}.",
        res.approved_test_cases.len(),
        res.approved_test_cases.join(", ")
    );
    ExitCode::Success
}
