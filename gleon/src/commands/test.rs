//! Implementation of the `gleon test` subcommand: runs a test command as one run, then sums it
//! up.

use std::{
    ffi::OsStr,
    io,
    path::{Path, PathBuf},
    process::ExitStatus,
};

use anyhow::{Context, Result, anyhow};
use gleon_core::{
    case::{CaseReport, RUN_ID_ENV, RunId},
    cases::{Cases, RunInfo, RunTotals, new_run_id},
    config::{ARTIFACTS_ENV, METRICS_ENV},
    context::ResolvedContext,
    env::EnvProvider,
    ops::common::{CoreError, ensure_initialized},
    paths::GleonPaths,
    report::{CaseSummary, case_name},
};
use tracing::{info, warn};

use crate::commands::{env_run_id, report_failure};

/// The exit code of a command that is not found, as shells give it.
const COMMAND_NOT_FOUND: i32 = 127;

/// The failures the summary lists one per line; the rest are counted (the reports show all).
const MAX_LISTED_FAILURES: usize = 20;

/// Runs `command` (program and arguments) with a run id and metrics, after recording the run in
/// `.gleon/runs/latest/run.json`, then sums up the run from its case reports, and returns the
/// command's exit code.
///
/// The run id is the process's `GLEON_RUN_ID` when the caller set it (CI), else a new one.
/// `GLEON_METRICS=1` makes integrations write a case report for every golden;
/// `GLEON_ARTIFACTS_DIR` of `.gleon/.env` reaches the command too. Outside a workspace (a monorepo
/// root) the command still runs, only the run file and the summary are missing: the reports pick
/// the run by its id.
///
/// The exit code is always the command's: a command ended by a signal gives `128 + signal` like
/// a shell, 127 when it is not found, [`crate::exit_code::ExitCode::Failure`] for a failure before
/// it starts. Ctrl-C reaches the command from the terminal (its process group); gleon waits for it
/// and prints no summary of the interrupted run. `SIGINT` sent to gleon alone, `SIGTERM` and
/// `SIGHUP` are passed on to the command.
pub async fn run_test(
    ctx: &ResolvedContext,
    env: &dyn EnvProvider,
    command: &[String],
    current_dir: &Path,
) -> i32 {
    run_test_inner(ctx, env, command, current_dir)
        .await
        .unwrap_or_else(|e| report_failure("Error running the tests", &*e).into())
}

async fn run_test_inner(
    ctx: &ResolvedContext,
    env: &dyn EnvProvider,
    command: &[String],
    current_dir: &Path,
) -> Result<i32> {
    let (program, child, started) = prepare(ctx, env, command)?;
    // Listening before the command starts, so an early Ctrl-C never ends gleon instead.
    let signals = Signals::new().context("Failed to listen for interrupts")?;
    let mut child = match tokio::process::Command::from(child).spawn() {
        Ok(child) => child,
        Err(error) => return not_started(error, program),
    };
    let waited = signals.wait(&mut child).await;
    let outcome = waited.context(format!("Failed to wait for `{program}`"))?;
    if let (Outcome::Exited(_), Some(started)) = (outcome, &started) {
        summarize(started, &gleon_core::paths::resolved(current_dir));
    }
    Ok(outcome.code())
}

/// The run `gleon test` recorded in the workspaces its goldens report into, which its summary
/// reads.
#[derive(Debug)]
struct Started {
    /// The workspace the CLI found from the working directory, if any.
    own: Option<PathBuf>,
    /// Every workspace of the run, sorted: `own` and those below it, or, outside a workspace,
    /// those below the working directory.
    roots: Vec<PathBuf>,
    run_id: RunId,
}

/// The workspaces a run from `ctx` reports into, sorted: the workspace found from the working
/// directory (`own`) and those below it (goldens report into the workspace nearest above them).
/// Outside a workspace, those below the working directory when it lies in a repository (a
/// monorepo root without `.gleon/`), never below any other directory (a home directory).
fn workspaces_of_run(ctx: &ResolvedContext, own: Option<&Path>) -> Vec<PathBuf> {
    let scope = own.or_else(|| {
        gleon_core::walk::is_in_repository(&ctx.base_dir).then_some(ctx.base_dir.as_path())
    });
    let Some(scope) = scope else {
        return Vec::new();
    };
    let mut roots = gleon_core::walk::workspaces_below(scope);
    if let Some(own) = own.filter(|own| !roots.iter().any(|root| root == own)) {
        // `.gleon/` without a config (removed by hand) is still where the run is recorded.
        roots.insert(0, own.to_path_buf());
    }
    roots
}

/// Records the run in the run file of each of its workspaces and returns the program, the
/// command to start (with the run id, metrics and `GLEON_ARTIFACTS_DIR`) and the run to sum up
/// afterwards (`None` without any workspace).
///
/// Every workspace gets the run file, so `gleon report` in a nested one (without
/// `GLEON_RUN_ID`) reads this run, not older reports left there.
fn prepare<'a>(
    ctx: &ResolvedContext,
    env: &dyn EnvProvider,
    command: &'a [String],
) -> Result<(&'a str, std::process::Command, Option<Started>)> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| anyhow!("no test command given: `gleon test -- <command>`"))?;
    let started_at = chrono::Utc::now();
    let run_id = env_run_id()?.unwrap_or_else(|| new_run_id(started_at));
    let own = match ensure_initialized(&ctx.base_dir) {
        Ok(_) => Some(ctx.base_dir.clone()),
        Err(CoreError::NotInitialized) => None,
        Err(e) => return Err(e.into()),
    };
    let roots = workspaces_of_run(ctx, own.as_deref());
    let run = RunInfo {
        run_id: run_id.clone(),
        platform: gleon_core::platform::PlatformKey::host().clone(),
        started_at,
        command: command.to_vec(),
    };
    for root in &roots {
        let runs_latest = GleonPaths::new(root).runs_latest();
        run.write(&runs_latest).with_context(|| {
            format!(
                "Failed to write the run file in '{}'",
                runs_latest.display()
            )
        })?;
    }
    if own.is_none() {
        warn!(
            "No gleon workspace at '{}': the run is recorded in the {} workspace(s) below it; \
             the workspaces of its goldens pick it by its id",
            ctx.base_dir.display(),
            roots.len()
        );
    }
    let started = (!roots.is_empty()).then(|| Started {
        own,
        roots,
        run_id: run_id.clone(),
    });

    info!("Run {}: {}", run_id.as_str(), command.join(" "));
    #[cfg(windows)]
    let resolved = program_path(
        program,
        std::env::var_os("PATH").as_deref(),
        std::env::var("PATHEXT").ok().as_deref(),
    );
    #[cfg(not(windows))]
    let resolved = None::<PathBuf>;
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
    Ok((program, child, started))
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

/// How the test command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// It exited with this code.
    Exited(i32),
    /// It exited with this code after an interrupt (Ctrl-C, `SIGTERM`, `SIGHUP`): the run is
    /// incomplete.
    Interrupted(i32),
    /// A signal ended it: `128 + signal`, like a shell.
    #[cfg_attr(
        not(unix),
        expect(dead_code, reason = "only Unix ends a process by a signal")
    )]
    Signaled(i32),
}

impl Outcome {
    /// The exit code `gleon test` ends with.
    const fn code(self) -> i32 {
        match self {
            Self::Exited(code) | Self::Interrupted(code) | Self::Signaled(code) => code,
        }
    }
}

/// How a command with `status` ended; `interrupted` when an interrupt reached gleon meanwhile.
fn outcome(status: ExitStatus, interrupted: bool) -> Outcome {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;

        if let Some(signal) = status.signal() {
            return Outcome::Signaled(128 + signal);
        }
    }
    // Without a code (a signal, handled above on Unix): a failure.
    let failure = crate::exit_code::ExitCode::Failure.into();
    let code = status.code().unwrap_or(failure);
    if interrupted {
        Outcome::Interrupted(code)
    } else {
        Outcome::Exited(code)
    }
}

/// The signals gleon handles while the test command runs.
struct Signals {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(unix)]
    hangup: tokio::signal::unix::Signal,
    #[cfg(windows)]
    ctrl_c: tokio::signal::windows::CtrlC,
}

impl Signals {
    fn new() -> io::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};

            Ok(Self {
                interrupt: signal(SignalKind::interrupt())?,
                terminate: signal(SignalKind::terminate())?,
                hangup: signal(SignalKind::hangup())?,
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                ctrl_c: tokio::signal::windows::ctrl_c()?,
            })
        }
    }

    /// Waits for `child`. `SIGINT` marks the run as interrupted; it is passed on unless the
    /// process group of gleon is the foreground group of its terminal, whose Ctrl-C reaches the
    /// command directly (passing it on too would interrupt the command twice, which ends
    /// `flutter test` without its cleanup). `SIGTERM` and `SIGHUP`, usually sent to gleon alone,
    /// are passed on.
    #[cfg(unix)]
    async fn wait(mut self, child: &mut tokio::process::Child) -> io::Result<Outcome> {
        // The command is not reaped before `wait` returns, so its id stays its own meanwhile.
        let pid = child.id();
        // A Ctrl-C already received came before the command existed, so it never reached it
        // (best effort: the signal driver may not have seen one that arrived a moment ago).
        let mut interrupted = tokio::select! {
            biased;
            _ = self.interrupt.recv() => true,
            () = std::future::ready(()) => false,
        };
        if interrupted {
            forward(pid, libc::SIGINT);
        }
        loop {
            tokio::select! {
                status = child.wait() => return Ok(outcome(status?, interrupted)),
                _ = self.interrupt.recv() => {
                    interrupted = true;
                    forward(interrupt_target(pid, is_terminal_foreground()), libc::SIGINT);
                }
                // Whatever the command does with them (a trap may exit 0), the run is cut short.
                _ = self.terminate.recv() => {
                    interrupted = true;
                    forward(pid, libc::SIGTERM);
                }
                _ = self.hangup.recv() => {
                    interrupted = true;
                    forward(pid, libc::SIGHUP);
                }
            }
        }
    }

    /// Waits for `child`; Ctrl-C (which every process of the console gets) only marks the run
    /// as interrupted.
    #[cfg(windows)]
    async fn wait(mut self, child: &mut tokio::process::Child) -> io::Result<Outcome> {
        let mut interrupted = false;
        loop {
            tokio::select! {
                status = child.wait() => return Ok(outcome(status?, interrupted)),
                _ = self.ctrl_c.recv() => interrupted = true,
            }
        }
    }
}

/// The process a `SIGINT` gleon got is passed on to: the test command `pid`, unless gleon is the
/// foreground of its terminal (`is_terminal_foreground`), whose Ctrl-C reaches the command
/// directly; sent to gleon alone (`kill -INT`, `timeout -s INT`, a CI runner) it is passed on.
#[cfg(unix)]
const fn interrupt_target(pid: Option<u32>, is_terminal_foreground: bool) -> Option<u32> {
    if is_terminal_foreground { None } else { pid }
}

/// Whether the process group of gleon is the foreground process group of its controlling
/// terminal, so a Ctrl-C there reaches the test command (in the same group) without gleon. False
/// without a controlling terminal (CI) and in a background or separate process group (`timeout`
/// puts its command in one).
#[cfg(unix)]
fn is_terminal_foreground() -> bool {
    std::fs::File::open("/dev/tty").is_ok_and(|terminal| is_foreground_of(&terminal))
}

/// Whether the process group of gleon is the foreground process group of the terminal open as
/// `terminal`; false for anything else (no terminal).
#[cfg(unix)]
fn is_foreground_of(terminal: &impl std::os::fd::AsRawFd) -> bool {
    // SAFETY: `tcgetpgrp(3)` reads the foreground group of the terminal behind a descriptor that
    // stays open for the call (borrowed from `terminal`), and `getpgrp(2)` reads the process group
    // of this process; neither touches memory of ours.
    #[expect(
        unsafe_code,
        reason = "`tcgetpgrp`/`getpgrp` only read process state (see SAFETY comment)"
    )]
    let (foreground, own) = unsafe { (libc::tcgetpgrp(terminal.as_raw_fd()), libc::getpgrp()) };
    foreground != -1 && foreground == own
}

/// Sends `signal` to the test command `pid` (none: nothing to send).
#[cfg(unix)]
fn forward(pid: Option<u32>, signal: libc::c_int) {
    if let Some(pid) = pid.and_then(|pid| libc::pid_t::try_from(pid).ok()) {
        // SAFETY: `kill(2)` only asks the kernel to send `signal` to the process `pid`, the child
        // gleon started and has not reaped yet (so the id cannot belong to another process); it
        // reads or writes no memory of this process.
        #[expect(
            unsafe_code,
            reason = "`libc::kill` on the child not yet reaped passes a signal on (see SAFETY comment)"
        )]
        let sent = unsafe { libc::kill(pid, signal) };
        if sent != 0 {
            warn!(
                "Could not pass signal {signal} on to the tests: {}",
                io::Error::last_os_error()
            );
        }
    }
}

/// One line of the summary of a run, with its level.
#[derive(Debug, PartialEq, Eq)]
enum Line {
    Info(String),
    Warn(String),
}

/// The case reports of one workspace of a run.
#[derive(Debug)]
struct WorkspaceRun {
    /// The workspace root.
    root: PathBuf,
    cases: Cases,
}

/// Prints the summary of the run `started` recorded ([`summary_of_run`]).
fn summarize(started: &Started, cwd: &Path) {
    for line in summary_of_run(started, cwd) {
        match line {
            Line::Info(line) => info!("{line}"),
            Line::Warn(line) => warn!("{line}"),
        }
    }
}

/// The summary of the run `started` recorded, from the case reports of each of its workspaces;
/// paths are shown relative to `cwd` (resolved like the workspace roots).
fn summary_of_run(started: &Started, cwd: &Path) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut runs = Vec::new();
    for root in &started.roots {
        let runs_latest = GleonPaths::new(root).runs_latest();
        match Cases::load(&runs_latest, Some(&started.run_id)) {
            Ok(cases) => runs.push(WorkspaceRun {
                root: root.clone(),
                cases,
            }),
            Err(e) => lines.push(Line::Warn(format!(
                "Run {}: the case reports in '{}' could not be read: {e}",
                started.run_id.as_str(),
                shown(&runs_latest, cwd)
            ))),
        }
    }
    let unreadable = !lines.is_empty();
    lines.extend(summary(
        &started.run_id,
        &runs,
        started.own.as_deref(),
        cwd,
        unreadable,
    ));
    lines
}

/// `path` relative to `cwd` when below it, else as it is.
fn shown(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}

/// The summary of the run `run_id` from the case reports of its workspaces (`runs`; `own` is the
/// workspace the CLI finds from `cwd`, if any): the warnings of the selections, the totals, one
/// line per failure (most telling first in each workspace) with the path of its case report, and
/// what to run next in each workspace with failures. Paths are shown relative to `cwd` when below
/// it. `unreadable` when the reports of some workspace could not be read (already said): then an
/// empty run is not blamed on the rules.
fn summary(
    run_id: &RunId,
    runs: &[WorkspaceRun],
    own: Option<&Path>,
    cwd: &Path,
    unreadable: bool,
) -> Vec<Line> {
    let run = run_id.as_str();
    let mut lines: Vec<Line> = runs
        .iter()
        .flat_map(|run| run.cases.warnings())
        .map(|warning| Line::Warn(warning.clone()))
        .collect();
    let mut totals = RunTotals::default();
    for workspace in runs {
        totals += workspace.cases.totals();
    }
    if totals.total() == 0 {
        if unreadable {
            return lines;
        }
        lines.push(Line::Warn(format!(
            "Run {run}: no case reports: no golden matched a rule of a .gleon/gleon.yaml, or the \
             tests compared no goldens with gleon"
        )));
        return lines;
    }
    if totals.failed() == 0 {
        lines.push(Line::Info(format!("Run {run}: {totals}")));
        return lines;
    }
    lines.push(Line::Warn(format!("Run {run}: {totals}")));
    let mut listed = 0;
    let mut next = Vec::new();
    for workspace in runs {
        let failures = workspace.cases.failures_by_severity();
        if failures.is_empty() {
            continue;
        }
        let gleon_dir = GleonPaths::new(&workspace.root).gleon_dir();
        for (key, report) in failures
            .iter()
            .take(MAX_LISTED_FAILURES.saturating_sub(listed))
        {
            let path = CaseReport::path(&gleon_dir, key, &report.name);
            lines.push(Line::Warn(format!(
                "  {}: {} ({})",
                case_name(&workspace.cases, key, report),
                CaseSummary(report),
                shown(&path, cwd)
            )));
        }
        listed += failures.len();
        // The CLI finds `own` from the working directory; a nested workspace is named.
        let config = (own != Some(workspace.root.as_path()))
            .then(|| format!(" --config {}", shown(&gleon_dir.join("gleon.yaml"), cwd)));
        let config = config.as_deref().unwrap_or_default();
        let report_html = gleon_dir.join("runs/latest/report.html");
        next.push(Line::Info(format!(
            "Next: `gleon{config} report html --out {}` shows each failure with its golden, \
             candidate and diff; `gleon{config} approve` accepts the candidates",
            shown(&report_html, cwd)
        )));
    }
    if let Some(more) = listed.checked_sub(MAX_LISTED_FAILURES).filter(|&n| n > 0) {
        lines.push(Line::Warn(format!("  ... and {more} more")));
    }
    lines.extend(next);
    lines
}

/// The file a program without an extension stands for on Windows: the first of its `PATHEXT`
/// extensions found on `PATH` for a bare name (`flutter` → `flutter.bat`), or next to a path
/// (`bin\flutter` → `bin\flutter.bat`), which `std::process::Command` does not look up (it only
/// appends `.exe`). `None` for names with an extension or when nothing is found.
#[cfg_attr(
    all(not(windows), not(test)),
    expect(dead_code, reason = "only Windows looks programs up by `PATHEXT`")
)]
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
        let (_, outside, started) = prepare(&ctx, &Env, &command).unwrap();
        assert!(env_of(&outside, RUN_ID_ENV).is_some());
        assert!(started.is_none(), "nothing to sum up outside a workspace");
        assert!(!temp.path().join(".gleon").exists());

        gleon_core::ops::init_workspace(&ctx, None).unwrap();
        let (program, child, started) = prepare(&ctx, &Env, &command).unwrap();
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
        let started = started.unwrap();
        assert_eq!(started.run_id, run.run_id);
        assert_eq!(started.own.as_deref(), Some(temp.path()));
        assert_eq!(started.roots, [temp.path().to_path_buf()]);
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

    #[tokio::test]
    async fn test_run_test_failures_before_the_command_runs() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ResolvedContext::from_options(
            &gleon_core::context::ContextOptions::default(),
            temp.path(),
        )
        .unwrap();
        let run_file = temp.path().join(".gleon/runs/latest/run.json");
        gleon_core::ops::init_workspace(&ctx, None).unwrap();
        assert_eq!(
            run_test(&ctx, &Env, &[], temp.path()).await,
            i32::from(ExitCode::Failure)
        );
        assert!(!run_file.exists());
    }

    /// The command's code; `128 + signal` when a signal ended it; interrupted runs are told apart
    /// so they get no summary.
    #[cfg(unix)]
    #[test]
    fn test_outcome_of_exit_codes_signals_and_interrupts() {
        use std::os::unix::process::ExitStatusExt as _;

        let exited = ExitStatus::from_raw(7 << 8);
        assert_eq!(outcome(exited, false), Outcome::Exited(7));
        assert_eq!(outcome(exited, true), Outcome::Interrupted(7));
        let terminated = ExitStatus::from_raw(libc::SIGTERM);
        assert_eq!(outcome(terminated, false), Outcome::Signaled(143));
        assert_eq!(outcome(terminated, true).code(), 143);
    }

    /// A case report of `name` on linux-x86_64 with `outcome`, as an integration writes it.
    fn report(name: &str, outcome: &str) -> CaseReport {
        let golden_sha = (outcome != "missing").then(|| "1".repeat(64));
        // Identical goldens and candidates share their bytes.
        let candidate_sha = if outcome == "identical" { "1" } else { "0" }.repeat(64);
        let report = serde_json::json!({
            "schema_version": 5,
            "name": name,
            "golden": {"path": format!("{name}.png"), "sha256": golden_sha},
            "candidate": {"sha256": candidate_sha},
            "source": {"tool": "gleon_flutter", "tool_version": "0.6.0"},
            "platform": {"os": "linux", "arch": "x86_64"},
            "comparison": {"tolerance": {"kind": "exact"}, "masks": [], "policy_version": 2},
            "outcome": outcome,
            "regions": [],
            "timings_ms": {"total": 1.0},
            "run_id": "run-1",
            "recorded_at": "2026-10-20T12:00:00Z"
        });
        CaseReport::parse(report.to_string().as_bytes()).unwrap()
    }

    fn run_id() -> RunId {
        RunId::from_env(Some("run-1")).unwrap().unwrap()
    }

    fn texts(lines: &[Line]) -> Vec<&str> {
        lines
            .iter()
            .map(|line| match line {
                Line::Info(text) | Line::Warn(text) => text.as_str(),
            })
            .collect()
    }

    fn texts_contain(lines: &[Line], needle: &str) -> bool {
        texts(lines)
            .iter()
            .any(|text| text.replace('\\', "/").contains(needle))
    }

    fn run_of(root: &str, reports: Vec<CaseReport>) -> WorkspaceRun {
        WorkspaceRun {
            root: PathBuf::from(root),
            cases: Cases::new(format!("{root}/.gleon/runs/latest"), reports),
        }
    }

    /// Totals, then each failure (mismatches first) with its case report below the working
    /// directory, then what to run next; a passing run is one line.
    #[test]
    fn test_summary_lists_failures_with_their_case_reports() {
        let runs = [run_of(
            "/w",
            vec![
                report("test/goldens/new", "missing"),
                report("test/goldens/ok", "match"),
                report("test/goldens/changed", "mismatch"),
            ],
        )];
        let lines = summary(
            &run_id(),
            &runs,
            Some(Path::new("/w")),
            Path::new("/w"),
            false,
        );
        let texts = texts(&lines);
        assert_eq!(
            texts[0],
            "Run run-1: 3 case(s): 1 passed, 2 failed (1 mismatch, 1 missing)"
        );
        assert!(
            texts[1].starts_with("  test/goldens/changed: Mismatch"),
            "{texts:?}"
        );
        assert!(
            texts[1].ends_with(&format!(
                "({})",
                Path::new(".gleon/runs/latest/cases/linux-x86_64/test/goldens/changed.json")
                    .display()
            )),
            "{texts:?}"
        );
        assert!(
            texts[2].starts_with("  test/goldens/new: Missing Baseline"),
            "{texts:?}"
        );
        assert!(
            texts_contain(
                &lines[3..],
                "gleon report html --out .gleon/runs/latest/report.html"
            ),
            "{texts:?}"
        );
        assert!(matches!(lines[0], Line::Warn(_)));
        assert!(matches!(lines[3], Line::Info(_)));

        // From a directory inside the workspace, paths (the hint's too) stay whole: relative
        // ones would point below the working directory.
        let inside = summary(
            &run_id(),
            &runs,
            Some(Path::new("/w")),
            Path::new("/w/test"),
            false,
        );
        assert!(texts_contain(&inside, "(/w/.gleon/runs/latest/cases"));
        assert!(texts_contain(
            &inside,
            "--out /w/.gleon/runs/latest/report.html"
        ));

        let passing = [run_of("/w", vec![report("a", "identical")])];
        assert_eq!(
            summary(
                &run_id(),
                &passing,
                Some(Path::new("/w")),
                Path::new("/w"),
                false
            ),
            [Line::Info(
                "Run run-1: 1 case(s): 1 passed, 0 failed".to_owned()
            )]
        );
        let empty = [run_of("/w", vec![])];
        let lines = summary(
            &run_id(),
            &empty,
            Some(Path::new("/w")),
            Path::new("/w"),
            false,
        );
        assert!(texts_contain(&lines, "no case reports"));
    }

    /// Goldens of nested workspaces report there: the totals add up, and the hint names the
    /// config of the nested workspace.
    #[test]
    fn test_summary_adds_up_nested_workspaces() {
        let runs = [
            run_of("/w", vec![report("test/goldens/a", "match")]),
            run_of("/w/packages/b", vec![report("test/goldens/b", "mismatch")]),
        ];
        let lines = summary(
            &run_id(),
            &runs,
            Some(Path::new("/w")),
            Path::new("/w"),
            false,
        );
        let texts = texts(&lines);
        assert_eq!(
            texts[0],
            "Run run-1: 2 case(s): 1 passed, 1 failed (1 mismatch)"
        );
        assert!(
            texts_contain(
                &lines,
                "(packages/b/.gleon/runs/latest/cases/linux-x86_64/test/goldens/b.json)"
            ),
            "{texts:?}"
        );
        assert!(
            texts_contain(
                &lines,
                "`gleon --config packages/b/.gleon/gleon.yaml report html --out packages/b/.gleon/runs/latest/report.html`"
            ),
            "{texts:?}"
        );
        assert!(texts_contain(
            &lines,
            "`gleon --config packages/b/.gleon/gleon.yaml approve`"
        ));
        assert_eq!(lines.len(), 3, "{texts:?}");
    }

    /// Reports that could not be read were said already: an empty run is not blamed on the
    /// rules then. Outside a workspace (a monorepo root) every workspace is named by its config.
    #[test]
    fn test_summary_without_reports_or_an_own_workspace() {
        let empty = [run_of("/w", vec![])];
        let lines = summary(
            &run_id(),
            &empty,
            Some(Path::new("/w")),
            Path::new("/w"),
            true,
        );
        assert!(lines.is_empty(), "{:?}", texts(&lines));

        let runs = [run_of(
            "/w/packages/a",
            vec![report("test/goldens/a", "mismatch")],
        )];
        let lines = summary(&run_id(), &runs, None, Path::new("/w"), false);
        assert!(
            texts_contain(
                &lines,
                "`gleon --config packages/a/.gleon/gleon.yaml approve`"
            ),
            "{:?}",
            texts(&lines)
        );
    }

    /// Only the first failures are listed, over every workspace; the rest are counted.
    #[test]
    fn test_summary_caps_the_listed_failures() {
        let many = |root: &str| {
            run_of(
                root,
                (0..MAX_LISTED_FAILURES)
                    .map(|i| report(&format!("t/{i:02}"), "mismatch"))
                    .collect(),
            )
        };
        let runs = [many("/w"), many("/w/p")];
        let lines = summary(
            &run_id(),
            &runs,
            Some(Path::new("/w")),
            Path::new("/w"),
            false,
        );
        let failures = texts(&lines)
            .iter()
            .filter(|t| t.starts_with("  t/"))
            .count();
        assert_eq!(failures, MAX_LISTED_FAILURES);
        assert!(texts_contain(
            &lines,
            &format!("... and {MAX_LISTED_FAILURES} more")
        ));
        assert_eq!(lines.len(), 1 + MAX_LISTED_FAILURES + 1 + 2);
    }

    /// The warnings of the selection of reports come first.
    #[test]
    fn test_summary_starts_with_the_warnings_of_the_reports() {
        let runs = [WorkspaceRun {
            root: PathBuf::from("/w"),
            cases: Cases::new("/w/.gleon/runs/latest", vec![report("a", "match")])
                .with_warnings(vec!["mixed runs".to_owned()]),
        }];
        let lines = summary(
            &run_id(),
            &runs,
            Some(Path::new("/w")),
            Path::new("/w"),
            false,
        );
        assert_eq!(lines[0], Line::Warn("mixed runs".to_owned()));
        assert_eq!(
            lines[1],
            Line::Info("Run run-1: 1 case(s): 1 passed, 0 failed".to_owned())
        );
    }

    /// A `.gleon/` without its config (removed by hand) still records the run.
    #[test]
    fn test_a_workspace_without_its_config_records_the_run() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = ResolvedContext::default();
        ctx.base_dir = temp.path().to_path_buf();
        gleon_core::ops::init_workspace(&ctx, None).unwrap();
        std::fs::remove_file(temp.path().join(".gleon/gleon.yaml")).unwrap();

        let (_, _, started) = prepare(&ctx, &Env, &["true".to_owned()]).unwrap();
        assert_eq!(started.unwrap().roots, [temp.path().to_path_buf()]);
        assert!(temp.path().join(".gleon/runs/latest/run.json").is_file());
    }

    /// A run file that cannot be written fails before the command starts, naming where.
    #[test]
    fn test_prepare_fails_when_the_run_file_cannot_be_written() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = ResolvedContext::default();
        ctx.base_dir = temp.path().to_path_buf();
        gleon_core::ops::init_workspace(&ctx, None).unwrap();
        let runs = temp.path().join(".gleon/runs");
        std::fs::remove_dir_all(&runs).unwrap();
        std::fs::write(&runs, "not a directory").unwrap();

        let err = prepare(&ctx, &Env, &["true".to_owned()]).unwrap_err();
        assert!(
            err.to_string().contains("Failed to write the run file"),
            "{err}"
        );
    }

    /// A workspace whose `.gleon/` cannot be looked at is an error, not "no workspace".
    #[cfg(unix)]
    #[test]
    fn test_prepare_fails_on_a_workspace_it_cannot_read() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let locked = temp.path().join("locked");
        let base_dir = locked.join("workspace");
        std::fs::create_dir_all(base_dir.join(".gleon")).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        // A privileged process (root in a container) reads it anyway: nothing to test there.
        let is_readable = std::fs::metadata(base_dir.join(".gleon")).is_ok();
        let mut ctx = ResolvedContext::default();
        ctx.base_dir = base_dir;
        let prepared = prepare(&ctx, &Env, &["true".to_owned()]).map(|_| ());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(prepared.is_err(), !is_readable, "{prepared:?}");
    }

    /// Case reports that cannot be read are said once; the run is not then blamed on the rules.
    #[cfg(unix)]
    #[test]
    fn test_summary_of_run_says_which_reports_could_not_be_read() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let report_file = root.join(".gleon/runs/latest/cases/linux-x86_64/a.json");
        std::fs::create_dir_all(report_file.parent().unwrap()).unwrap();
        std::fs::write(&report_file, "{}").unwrap();
        std::fs::set_permissions(&report_file, std::fs::Permissions::from_mode(0o000)).unwrap();
        let is_readable = std::fs::read(&report_file).is_ok();
        let started = Started {
            own: Some(root.clone()),
            roots: vec![root.clone()],
            run_id: run_id(),
        };
        let lines = summary_of_run(&started, &root);
        std::fs::set_permissions(&report_file, std::fs::Permissions::from_mode(0o644)).unwrap();

        let said = texts_contain(&lines, "could not be read");
        let blamed = texts_contain(&lines, "no case reports");
        assert!(is_readable || (said && !blamed), "{:?}", texts(&lines));
    }

    /// A `SIGINT` is passed on to the command unless gleon is the foreground of its terminal.
    #[cfg(unix)]
    #[test]
    fn test_interrupt_target_and_terminal() {
        assert_eq!(interrupt_target(Some(7), false), Some(7));
        assert_eq!(interrupt_target(Some(7), true), None);
        assert_eq!(interrupt_target(None, false), None);
        // A file is no terminal.
        let file = tempfile::tempfile().unwrap();
        assert!(!is_foreground_of(&file));
    }

    /// A process that is gone (reaped) gets nothing and only draws a warning; signal 0 checks
    /// the process without sending anything.
    #[cfg(unix)]
    #[test]
    fn test_forward_to_a_process_that_is_gone() {
        let mut gone = std::process::Command::new("true").spawn().unwrap();
        let pid = gone.id();
        gone.wait().unwrap();

        // `&File` is `io::Write`, so a shared file collects the formatted log.
        let temp = tempfile::tempdir().unwrap();
        let log_path = temp.path().join("forward.log");
        let log = std::sync::Arc::new(std::fs::File::create(&log_path).unwrap());
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(log)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            forward(Some(pid), 0);
            // Nothing to send to: no warning.
            forward(None, 0);
            forward(Some(u32::MAX), 0);
        });

        let log = std::fs::read_to_string(log_path).unwrap();
        assert_eq!(
            log.matches("Could not pass signal 0 on to the tests")
                .count(),
            1,
            "{log}"
        );
    }

    /// A Ctrl-C that reached gleon before the command existed is passed on when it starts.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_an_interrupt_before_the_command_starts_reaches_it() {
        let signals = Signals::new().unwrap();
        let sent = std::process::Command::new("kill")
            .args(["-INT", &std::process::id().to_string()])
            .status()
            .unwrap();
        assert!(sent.success());
        // Lets the signal driver take it in.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let mut child = tokio::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();

        let outcome = signals.wait(&mut child).await.unwrap();
        assert_eq!(outcome, Outcome::Signaled(128 + libc::SIGINT));
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
