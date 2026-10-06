//! Handler for `gleon approve` subcommand.

use std::path::PathBuf;

use gleon_core::{context::ResolvedContext, ops::ApprovedCase};
use tracing::{info, warn};

use crate::{
    commands::{env_run_id, report_failure},
    exit_code::ExitCode,
};

/// Runs the `approve` command on the runs `from` (copies of `.gleon/runs/latest`, read as they
/// are), or on the latest run of the workspace, picked with `GLEON_RUN_ID`.
pub fn run_approve(ctx: &ResolvedContext, paths: &[PathBuf], from: &[PathBuf]) -> ExitCode {
    // The caller's run id names a run of this workspace, never a copied one.
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
        res.approved.len(),
        Listed(&res.approved)
    );
    ExitCode::Success
}

/// The approved cases as `<platform>/<name>`, separated by `, `.
struct Listed<'a>(&'a [ApprovedCase]);

impl std::fmt::Display for Listed<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, case) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{case}")?;
        }
        Ok(())
    }
}
