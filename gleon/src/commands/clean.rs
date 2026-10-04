//! Handler for `gleon clean` subcommand.

use gleon_core::{
    context::ResolvedContext,
    ops::clean::{CleanOptions, clean_workspace},
};
use tracing::info;

use crate::{commands::report_failure, exit_code::ExitCode};

/// Runs the `clean` command.
pub fn run_clean(ctx: &ResolvedContext, options: &CleanOptions) -> ExitCode {
    let res = match clean_workspace(ctx, options) {
        Ok(r) => r,
        Err(e) => return report_failure("Error cleaning workspace", &e),
    };

    if options.dry_run {
        info!(
            "[dry-run] Found {} screenshot(s) to remove:",
            res.deleted_files.len()
        );
        for f in &res.deleted_files {
            info!("  - {}", f.display());
        }
        if !res.gitignore_entries_added.is_empty() {
            info!("[dry-run] Would add to .gitignore:");
            for entry in &res.gitignore_entries_added {
                info!("  + {}", entry);
            }
        }
        if res.cache_cleaned {
            info!("[dry-run] Would remove the .gleon/runs directory.");
        }
    } else if options.screenshots {
        info!(
            "Removed {} screenshot(s) ({} untracked from Git index).",
            res.deleted_files.len(),
            res.untracked_files.len()
        );
        if !res.gitignore_entries_added.is_empty() {
            info!(
                "Added {} entry/entries to .gitignore.",
                res.gitignore_entries_added.len()
            );
        }
    }
    if !options.dry_run && res.cache_cleaned {
        info!("Cleaned the .gleon/runs cache.");
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

    #[test]
    fn test_run_clean_dry_run_and_actual_flow() {
        let temp = tempdir().unwrap();
        let base_path = temp.path();

        let gleon_dir = base_path.join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();
        std::fs::create_dir_all(gleon_dir.join("runs")).unwrap();

        let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include:
      - "test/**/*.png"
    mode: pixel
"#;
        std::fs::write(gleon_dir.join("gleon.yaml"), config_yaml).unwrap();

        let test_dir = base_path.join("test");
        std::fs::create_dir_all(&test_dir).unwrap();
        std::fs::write(test_dir.join("login.png"), b"image").unwrap();

        let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

        // 1. Dry run
        let exit_code = run_clean(
            &ctx,
            &CleanOptions {
                dry_run: true,
                screenshots: true,
                skip_gitignore: false,
                keep_runs: false,
            },
        );
        assert_eq!(exit_code, ExitCode::Success);
        assert!(test_dir.join("login.png").exists());

        // 2. A plain run keeps the screenshots (an integration's goldens) and `.gitignore`.
        let exit_code = run_clean(&ctx, &CleanOptions::default());
        assert_eq!(exit_code, ExitCode::Success);
        assert!(test_dir.join("login.png").exists());
        assert!(!base_path.join(".gitignore").exists());

        // 3. Real run with screenshots
        let exit_code = run_clean(
            &ctx,
            &CleanOptions {
                dry_run: false,
                screenshots: true,
                skip_gitignore: false,
                keep_runs: false,
            },
        );
        assert_eq!(exit_code, ExitCode::Success);
        assert!(!test_dir.join("login.png").exists());
        assert!(base_path.join(".gitignore").exists());
        assert!(!gleon_dir.join("runs").exists());
    }

    #[test]
    fn test_run_clean_with_keep_runs_and_skip_gitignore() {
        let temp = tempdir().unwrap();
        let base_path = temp.path();

        let gleon_dir = base_path.join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();

        let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include:
      - "test/**/*.png"
    mode: pixel
"#;
        std::fs::write(gleon_dir.join("gleon.yaml"), config_yaml).unwrap();

        let test_dir = base_path.join("test");
        std::fs::create_dir_all(&test_dir).unwrap();
        std::fs::write(test_dir.join("login.png"), b"image").unwrap();

        let runs_dir = gleon_dir.join("runs");
        std::fs::create_dir_all(&runs_dir).unwrap();

        let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

        // 1. Dry run with keep_runs=true and skip_gitignore=true
        let exit_code = run_clean(
            &ctx,
            &CleanOptions {
                dry_run: true,
                screenshots: true,
                skip_gitignore: true,
                keep_runs: true,
            },
        );
        assert_eq!(exit_code, ExitCode::Success);
        assert!(test_dir.join("login.png").exists());

        // 2. Real run with keep_runs=true and skip_gitignore=true
        let exit_code = run_clean(
            &ctx,
            &CleanOptions {
                dry_run: false,
                screenshots: true,
                skip_gitignore: true,
                keep_runs: true,
            },
        );
        assert_eq!(exit_code, ExitCode::Success);
        assert!(!test_dir.join("login.png").exists());
        assert!(!base_path.join(".gitignore").exists());
        assert!(runs_dir.exists());
    }

    #[test]
    fn test_run_clean_error_propagation() {
        let temp = tempdir().unwrap();
        let base_path = temp.path();

        let gleon_dir = base_path.join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();

        let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include:
      - "test/**/*.png"
    mode: pixel
"#;
        std::fs::write(gleon_dir.join("gleon.yaml"), config_yaml).unwrap();

        // Create .gitignore as directory to force CleanError::Io
        std::fs::create_dir_all(base_path.join(".gitignore")).unwrap();

        let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

        let exit_code = run_clean(
            &ctx,
            &CleanOptions {
                dry_run: false,
                screenshots: true,
                skip_gitignore: false,
                keep_runs: false,
            },
        );
        assert_eq!(exit_code, ExitCode::Failure);
    }
}
