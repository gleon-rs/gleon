//! Implementation of the `gleon stage` subcommand.

use std::path::PathBuf;

use anyhow::Context as _;
use gleon_core::context::ResolvedContext;
use tracing::info;

use crate::exit_code::ExitCode;

/// Runs the `gleon stage` subcommand, optionally restricted to `paths`.
pub fn run_stage(ctx: &ResolvedContext, paths: &[PathBuf]) -> anyhow::Result<ExitCode> {
    let filter = if paths.is_empty() { None } else { Some(paths) };

    let res = gleon_core::ops::stage_workspace(ctx, filter).context("Error staging screenshots")?;

    if res.total_screenshots_staged == 0 {
        info!("Already up to date.");
    } else {
        info!(
            "Staged {} screenshot(s) across {} test case(s).",
            res.total_screenshots_staged,
            res.staged_test_cases.len()
        );
    }
    Ok(ExitCode::Success)
}
