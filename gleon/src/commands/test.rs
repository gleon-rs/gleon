//! Implementation of the `gleon test` subcommand: runs a test command as one run.

use std::{
    ffi::OsStr,
    io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use gleon_core::{
    case::RUN_ID_ENV,
    cases::{RunInfo, new_run_id},
    config::{ARTIFACTS_ENV, METRICS_ENV},
    context::ResolvedContext,
    env::EnvProvider,
    ops::common::{CoreError, ensure_initialized},
};
use tracing::{info, warn};

use crate::commands::{env_run_id, report_failure};

/// The exit code of a command that is not found, as shells give it.
const COMMAND_NOT_FOUND: i32 = 127;

/// Runs `command` (program and arguments) with a run id and metrics, after recording the run in
/// `.gleon/runs/latest/run.json`, and returns its exit code.
///
/// The run id is the process's `GLEON_RUN_ID` when the caller set it (CI), else a new one.
/// `GLEON_METRICS=1` makes integrations write a case report for every golden;
/// `GLEON_ARTIFACTS_DIR` of `.gleon/.env` reaches the command too. Outside a workspace (a monorepo
/// root) the command still runs, only the run file is missing: the reports pick the run by its
/// id. On Unix gleon becomes the command (`exec`): signals reach it directly and its exit status,
/// a signal included, is the one the caller sees. A command that is not found exits with 127, a
/// failure before it starts with [`crate::exit_code::ExitCode::Failure`].
pub fn run_test(ctx: &ResolvedContext, env: &dyn EnvProvider, command: &[String]) -> i32 {
    run_test_inner(ctx, env, command)
        .unwrap_or_else(|e| report_failure("Error running the tests", &*e).into())
}

fn run_test_inner(ctx: &ResolvedContext, env: &dyn EnvProvider, command: &[String]) -> Result<i32> {
    let (program, child) = prepare(ctx, env, command)?;
    run(child).or_else(|error| not_started(error, program))
}

/// Records the run in the run file (inside a workspace) and returns the program and the command
/// to start: with the run id, metrics and `GLEON_ARTIFACTS_DIR`.
fn prepare<'a>(
    ctx: &ResolvedContext,
    env: &dyn EnvProvider,
    command: &'a [String],
) -> Result<(&'a str, std::process::Command)> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| anyhow!("no test command given: `gleon test -- <command>`"))?;
    let started_at = chrono::Utc::now();
    let run_id = env_run_id()?.unwrap_or_else(|| new_run_id(started_at));
    match ensure_initialized(&ctx.base_dir) {
        Ok(paths) => {
            let runs_latest = paths.runs_latest();
            RunInfo {
                run_id: run_id.clone(),
                started_at,
                command: command.to_vec(),
            }
            .write(&runs_latest)
            .with_context(|| {
                format!(
                    "Failed to write the run file in '{}'",
                    runs_latest.display()
                )
            })?;
        }
        Err(CoreError::NotInitialized) => warn!(
            "No gleon workspace at '{}': the run is not recorded in a run file; the workspaces \
             of its goldens pick it by its id",
            ctx.base_dir.display()
        ),
        Err(e) => return Err(e.into()),
    }

    info!(
        "Run {}: {} (then `gleon report <format>`, `gleon dashboard` and `gleon approve` read \
         its case reports)",
        run_id.as_str(),
        command.join(" ")
    );
    let resolved = cfg!(windows)
        .then(|| {
            program_path(
                program,
                std::env::var_os("PATH").as_deref(),
                std::env::var("PATHEXT").ok().as_deref(),
            )
        })
        .flatten();
    let mut child = std::process::Command::new(
        resolved
            .as_deref()
            .map_or_else(|| program.as_ref(), Path::as_os_str),
    );
    child
        .args(args)
        .env(RUN_ID_ENV, run_id.as_str())
        .env(METRICS_ENV, "1");
    if let Some(artifacts) = env.get_var(ARTIFACTS_ENV) {
        child.env(ARTIFACTS_ENV, artifacts);
    }
    Ok((program, child))
}

/// The exit code of `program` that cannot start because it is not found (127, like a shell);
/// the error for any other reason.
fn not_started(error: io::Error, program: &str) -> Result<i32> {
    if error.kind() == io::ErrorKind::NotFound {
        tracing::error!("Error running the tests: `{program}` was not found");
        return Ok(COMMAND_NOT_FOUND);
    }
    Err(anyhow::Error::new(error).context(format!("Failed to start `{program}`")))
}

/// Becomes `command`; returns only when it cannot start.
#[cfg(unix)]
fn run(mut command: std::process::Command) -> io::Result<i32> {
    use std::os::unix::process::CommandExt as _;

    Err(command.exec())
}

/// Runs `command` and returns its exit code.
#[cfg(not(unix))]
fn run(mut command: std::process::Command) -> io::Result<i32> {
    let status = command.status()?;
    Ok(status
        .code()
        .unwrap_or_else(|| crate::exit_code::ExitCode::Failure.into()))
}

/// The file a program without an extension stands for on Windows: the first of its `PATHEXT`
/// extensions found on `PATH` for a bare name (`flutter` → `flutter.bat`), or next to a path
/// (`bin\flutter` → `bin\flutter.bat`), which `std::process::Command` does not look up (it only
/// appends `.exe`). `None` for names with an extension or when nothing is found.
fn program_path(program: &str, path: Option<&OsStr>, pathext: Option<&str>) -> Option<PathBuf> {
    let name = Path::new(program);
    if name.extension().is_some() {
        return None;
    }
    let extensions = pathext.unwrap_or(".COM;.EXE;.BAT;.CMD");
    let with_extension = |dir: &Path| {
        extensions
            .split(';')
            .map(str::trim)
            .filter(|ext| !ext.is_empty())
            .map(|ext| dir.join(format!("{}{ext}", name.display())))
            .find(|file| file.is_file())
    };
    if name.components().count() != 1 {
        return with_extension(Path::new(""));
    }
    std::env::split_paths(path?).find_map(|dir| with_extension(&dir))
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
    use super::*;
    use crate::exit_code::ExitCode;

    struct Env;
    impl EnvProvider for Env {
        fn get_var(&self, key: &str) -> Option<String> {
            (key == ARTIFACTS_ENV).then(|| ".gleon/runs/ci".to_owned())
        }
    }

    /// The env of `child` named `key`, as a string.
    fn env_of(child: &std::process::Command, key: &str) -> Option<String> {
        child
            .get_envs()
            .find(|(name, _)| *name == key)
            .and_then(|(_, value)| value)
            .map(|value| value.to_string_lossy().into_owned())
    }

    /// The command starts with the run of the run file, metrics on and the artifacts directory;
    /// outside a workspace without a run file.
    #[test]
    fn test_prepare_records_the_run_and_hands_it_to_the_command() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ResolvedContext::from_options(
            &gleon_core::context::ContextOptions::default(),
            temp.path(),
        )
        .unwrap();
        let command = ["flutter".to_owned(), "test".to_owned()];
        let (_, outside) = prepare(&ctx, &Env, &command).unwrap();
        assert!(env_of(&outside, RUN_ID_ENV).is_some());
        assert!(!temp.path().join(".gleon").exists());

        gleon_core::ops::init_workspace(&ctx).unwrap();
        let (program, child) = prepare(&ctx, &Env, &command).unwrap();
        assert_eq!(program, "flutter");
        assert_eq!(child.get_args().collect::<Vec<_>>(), ["test"]);
        let run = RunInfo::read(&temp.path().join(".gleon/runs/latest"))
            .unwrap()
            .unwrap();
        assert_eq!(run.command, command);
        assert_eq!(
            env_of(&child, RUN_ID_ENV).as_deref(),
            Some(run.run_id.as_str())
        );
        assert_eq!(env_of(&child, METRICS_ENV).as_deref(), Some("1"));
        assert_eq!(
            env_of(&child, ARTIFACTS_ENV).as_deref(),
            Some(".gleon/runs/ci")
        );
    }

    #[test]
    fn test_a_command_that_cannot_start() {
        let missing = io::Error::from(io::ErrorKind::NotFound);
        assert_eq!(not_started(missing, "flutter").unwrap(), COMMAND_NOT_FOUND);
        let denied = io::Error::from(io::ErrorKind::PermissionDenied);
        let err = not_started(denied, "flutter").unwrap_err();
        assert!(
            err.to_string().contains("Failed to start `flutter`"),
            "{err}"
        );
    }

    #[test]
    fn test_run_test_failures_before_the_command_runs() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ResolvedContext::from_options(
            &gleon_core::context::ContextOptions::default(),
            temp.path(),
        )
        .unwrap();
        let run_file = temp.path().join(".gleon/runs/latest/run.json");
        gleon_core::ops::init_workspace(&ctx).unwrap();
        assert_eq!(run_test(&ctx, &Env, &[]), i32::from(ExitCode::Failure));
        assert!(!run_file.exists());
    }

    #[test]
    fn test_program_path_finds_batch_files_like_windows() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("flutter.bat"), "").unwrap();
        let path = std::env::join_paths([temp.path().join("empty"), bin.clone()]).unwrap();

        assert_eq!(
            program_path("flutter", Some(&path), Some(".exe;.bat")),
            Some(bin.join("flutter.bat"))
        );
        assert_eq!(program_path("flutter", Some(&path), Some(".exe")), None);
        assert_eq!(
            program_path("flutter.bat", Some(&path), None),
            None,
            "named in full"
        );
        assert_eq!(
            program_path("flutter", Some(&path), Some(".com; .bat ")),
            Some(bin.join("flutter.bat")),
            "entries are trimmed"
        );
        let in_dir = bin.join("flutter");
        assert_eq!(
            program_path(in_dir.to_str().unwrap(), None, Some(".bat")),
            Some(bin.join("flutter.bat")),
            "a path gets its extension too"
        );
        assert_eq!(
            program_path(temp.path().join("nope").to_str().unwrap(), None, None),
            None
        );
        assert_eq!(program_path("flutter", None, None), None);
    }
}
