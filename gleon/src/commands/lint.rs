//! Implementation of the `gleon lint-manifests` subcommand.

use gleon_core::{context::ResolvedContext, ops::lint_workspace_manifests};
use tracing::{error, info};

use crate::{commands::report_failure, exit_code::ExitCode};

/// Runs manifest linting across the workspace.
///
/// Returns [`ExitCode::Success`] if all manifests are valid and unconflicted, or
/// [`ExitCode::Failure`] if any file contains conflict markers or schema corruption.
pub fn run_lint(ctx: &ResolvedContext, platform_filter: Option<&str>) -> ExitCode {
    if let Some(code) = crate::commands::goldens_nothing_to_do(ctx, "lint") {
        return code;
    }
    info!("Running manifest linting...");

    let report = match lint_workspace_manifests(ctx, platform_filter) {
        Ok(rep) => rep,
        Err(e) => return report_failure("Error during manifest linting", &e),
    };

    info!(
        "Inspected {} manifest file(s): {} valid.",
        report.total_files, report.valid_files
    );

    if !report.conflicted_files.is_empty() {
        error!("Git merge conflict markers (<<<<<<<) found in:");
        for (path, msg) in &report.conflicted_files {
            error!("  - {}: {}", path.display(), msg);
        }
    }

    if !report.corrupted_files.is_empty() {
        error!("Schema or syntax errors found in:");
        for (path, msg) in &report.corrupted_files {
            error!("  - {}: {}", path.display(), msg);
        }
    }

    if report.passed {
        info!("All manifest files passed linting.");
        return ExitCode::Success;
    }

    if report.conflicted_files.is_empty() {
        error!(
            "Lint check failed due to schema/JSON errors! Please repair reported manifest files."
        );
    } else {
        error!("Lint check failed due to Git conflicts! Run 'gleon resolve' to resolve conflicts.");
    }
    ExitCode::Failure
}

#[cfg(test)]
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

    #[test]
    fn test_run_lint_branches() {
        let temp = tempdir().unwrap();
        let ctx = ResolvedContext::from_options(&ContextOptions::default(), temp.path()).unwrap();

        // 1. Missing directory -> Err -> Failure
        assert_eq!(run_lint(&ctx, None), ExitCode::Failure);

        // 2. Clean manifest -> Success
        let manifests_dir = temp
            .path()
            .join(".gleon")
            .join("manifests")
            .join("macos-aarch64");
        std::fs::create_dir_all(&manifests_dir).unwrap();
        let valid_json = include_str!("../../../gleon-core/tests/fixtures/valid_manifest.json");
        std::fs::write(manifests_dir.join("valid.json"), valid_json).unwrap();
        assert_eq!(run_lint(&ctx, None), ExitCode::Success);

        // 3. Conflicted manifest -> Failure with conflict advice
        let conflicted_json = include_str!("../../../gleon-core/tests/fixtures/conflict_2way.json");
        std::fs::write(manifests_dir.join("conflict.json"), conflicted_json).unwrap();
        assert_eq!(run_lint(&ctx, None), ExitCode::Failure);

        // 4. Corrupted manifest -> Failure with schema advice
        std::fs::remove_file(manifests_dir.join("conflict.json")).unwrap();
        let corrupt_json = include_str!("../../../gleon-core/tests/fixtures/corrupt_manifest.json");
        std::fs::write(manifests_dir.join("corrupt.json"), corrupt_json).unwrap();
        assert_eq!(run_lint(&ctx, None), ExitCode::Failure);
    }
}
