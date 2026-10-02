//! Implementation of the `gleon dashboard` subcommand.

use std::{num::NonZeroUsize, path::Path};

use anyhow::{Context, Result};
use gleon_core::{
    context::ResolvedContext,
    dashboard::{DashboardCompiler, DashboardOptions},
    ops::common::ensure_initialized,
    storage::StorageConfig,
};

use crate::{
    commands::{RunSource, load_cases, report_failure},
    exit_code::ExitCode,
};

/// Runs the `gleon dashboard` subcommand.
///
/// Adds the run of `from` (a copy of `.gleon/runs/latest`; default: the latest run of the
/// workspace) to `history.json`, compiles `dashboard.html` from it and optionally pushes both to remote
/// storage. Returns [`ExitCode::Success`] on success or [`ExitCode::Failure`] on error.
pub async fn run_dashboard(
    ctx: &ResolvedContext,
    storage_cfg: Option<StorageConfig>,
    from: Option<&Path>,
    out: Option<&Path>,
    truncate_history: NonZeroUsize,
    push: bool,
) -> ExitCode {
    match run_dashboard_inner(ctx, storage_cfg, from, out, truncate_history, push).await {
        Ok(()) => ExitCode::Success,
        Err(e) => report_failure("Error compiling dashboard", &*e),
    }
}

async fn run_dashboard_inner(
    ctx: &ResolvedContext,
    storage_cfg: Option<StorageConfig>,
    from: Option<&Path>,
    out: Option<&Path>,
    truncate_history: NonZeroUsize,
    push: bool,
) -> Result<()> {
    tracing::info!("Compiling visual regression dashboard...");

    let paths = ensure_initialized(&ctx.base_dir)
        .context("Workspace not initialized. Run `gleon init` first.")?;
    let source = from.map_or_else(
        || RunSource::Own(paths.runs_latest()),
        |dir| RunSource::Copy(dir.to_path_buf()),
    );
    let cases = load_cases(&source)?;

    let options = DashboardOptions {
        out_html: out,
        truncate_limit: Some(truncate_history),
        push_to_storage: push,
    };

    let result = DashboardCompiler::execute(
        &paths,
        ctx,
        &cases,
        &options,
        if push { storage_cfg.as_ref() } else { None },
    )
    .await
    .context("Failed to compile history dashboard")?;

    // Output primary machine-readable artifact path to stdout
    println!("{}", result.html_path.display());

    tracing::info!(
        "Dashboard compiled successfully at {} (total runs in history: {})",
        result.html_path.display(),
        result.total_runs
    );

    if result.pushed {
        tracing::info!("Successfully uploaded history.json and dashboard.html to remote storage.");
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

    use super::*;

    const KEEP: NonZeroUsize = NonZeroUsize::new(200).unwrap();

    /// The real Flutter run of the gleon-core fixtures, copied into the workspace at `root`.
    fn record_flutter_run(root: &Path) {
        let from = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../gleon-core/tests/fixtures/cases/flutter-linux-x64/cases/test/goldens");
        let to = root.join(".gleon/runs/latest/cases/test/goldens");
        std::fs::create_dir_all(&to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
        }
    }

    #[tokio::test]
    async fn test_run_dashboard_uninitialized() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ResolvedContext::from_options(&ContextOptions::default(), temp.path()).unwrap();
        let exit_code = run_dashboard(&ctx, None, None, None, KEEP, false).await;
        assert_eq!(exit_code, ExitCode::Failure);
    }

    #[tokio::test]
    async fn test_run_dashboard_reads_the_latest_run_by_default() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ResolvedContext::from_options(&ContextOptions::default(), temp.path()).unwrap();
        gleon_core::ops::init_workspace(&ctx).unwrap();
        assert_eq!(
            run_dashboard(&ctx, None, None, None, KEEP, false).await,
            ExitCode::Failure,
            "no case reports yet"
        );

        record_flutter_run(temp.path());
        assert_eq!(
            run_dashboard(&ctx, None, None, None, KEEP, false).await,
            ExitCode::Success
        );
        assert!(temp.path().join(".gleon/dashboard.html").is_file());

        // A copy of the run, read as it is.
        let copy = temp.path().join(".gleon/runs/latest");
        assert_eq!(
            run_dashboard(&ctx, None, Some(&copy), None, KEEP, false).await,
            ExitCode::Success
        );
        let missing = temp.path().join("does_not_exist/latest");
        assert_eq!(
            run_dashboard(&ctx, None, Some(&missing), None, KEEP, false).await,
            ExitCode::Failure
        );
    }
}
