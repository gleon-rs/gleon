//! Subcommand handlers for gleon CLI.

pub mod approve;
pub mod clean;
pub mod dashboard;
pub mod diff;
pub mod gc;
pub mod init;
pub mod lint;
pub mod pull;
pub mod push;
pub mod report;
pub mod resolve;
pub mod stage;
pub mod status;
pub mod test;

use crate::exit_code::ExitCode;

/// Returns `true` if `rendered` is already formatted inside `parent` (e.g. exact match,
/// `: {rendered}`, or `({rendered})`), avoiding duplicate causes when walking an error chain.
fn is_interpolated_cause(parent: &str, rendered: &str) -> bool {
    if parent == rendered {
        return true;
    }
    if let Some(prefix) = parent.strip_suffix(rendered)
        && prefix.ends_with(": ")
    {
        return true;
    }
    if let Some(stripped_paren) = parent.strip_suffix(')')
        && let Some(prefix) = stripped_paren.strip_suffix(rendered)
        && prefix.ends_with('(')
    {
        return true;
    }
    false
}

/// Formats a command-failure error message, walking `err.source()` up to a bounded depth.
///
/// Ensures chained causes are surfaced in the final log even when an intermediate error
/// message does not interpolate its `#[source]` field (e.g. `ApproveError::ImageDecode`) would
/// otherwise report only "Image decode error for 'x.png'" and drop the actual reason.
fn format_failure(context: &str, err: &dyn std::error::Error) -> String {
    use std::fmt::Write as _;

    /// `Error::source()` may return anything, including a cycle; cap the walk so a malformed
    /// chain can never hang the CLI on its error path. Real chains are 2-3 deep.
    const MAX_CAUSE_DEPTH: usize = 8;

    let mut parent = err.to_string();
    let mut out = format!("{context}: {parent}");
    let mut source = err.source();

    for _ in 0..MAX_CAUSE_DEPTH {
        let Some(cause) = source else { break };
        let rendered = cause.to_string();
        // Skip only causes the *immediate parent* already interpolated itself
        // (`#[error("IO error: {0}")]` or `#[error("failed ({0})")]` or transparent).
        // Naive substring checking `parent.contains(&rendered)` would drop distinct causes
        // whose name happens to appear inside the parent text (e.g. cause "report").
        if !is_interpolated_cause(&parent, &rendered) {
            // Writing to a `String` via `fmt::Write` never fails.
            #[expect(
                clippy::expect_used,
                reason = "`fmt::Write` for `String` is infallible"
            )]
            write!(out, ": {rendered}").expect("write! to a String cannot fail");
        }
        source = cause.source();
        parent = rendered;
    }
    out
}

/// Logs `context` plus `err`'s full cause chain via `tracing::error!` and returns
/// [`ExitCode::Failure`], so every subcommand reports an operation failure identically
/// regardless of its own error type (a `thiserror` enum from `gleon-core`, or an
/// `anyhow::Error` chain built up locally).
pub fn report_failure(context: &str, err: &dyn std::error::Error) -> ExitCode {
    tracing::error!("{}", format_failure(context, err));
    ExitCode::Failure
}

/// The run a command reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunSource {
    /// The `.gleon/runs/latest` of the workspace, picked with `GLEON_RUN_ID` when it is set.
    Own(std::path::PathBuf),
    /// A copy of `.gleon/runs/latest` given with `--from` (a downloaded CI artifact), read as it
    /// is: the caller's `GLEON_RUN_ID` names the caller's run, not that one.
    Copy(std::path::PathBuf),
}

impl RunSource {
    /// The directory standing in for `.gleon/runs/latest`.
    #[must_use]
    pub fn dir(&self) -> &std::path::Path {
        match self {
            Self::Own(dir) | Self::Copy(dir) => dir,
        }
    }
}

/// The `GLEON_RUN_ID` of the process, if set. Only the process environment names a run (CI,
/// `gleon test`), like for the integrations: a value in `.gleon/.env` would stamp every local run
/// with one id.
///
/// # Errors
/// Returns an error if it is not a valid run id.
pub fn env_run_id() -> anyhow::Result<Option<gleon_core::case::RunId>> {
    use anyhow::Context as _;
    use gleon_core::{
        case::{RUN_ID_ENV, RunId},
        env::{EnvProvider as _, OsEnv},
    };

    RunId::from_env(OsEnv.get_var(RUN_ID_ENV).as_deref())
        .with_context(|| format!("invalid {RUN_ID_ENV}"))
}

/// The case reports of the run of `source`, with its warnings logged; an error when there are
/// none, so a run that recorded nothing never reads as a passing one.
///
/// # Errors
/// Returns an error if `GLEON_RUN_ID` is invalid, a copied run is no run directory, the reports
/// cannot be read, or there are none.
pub fn load_cases(source: &RunSource) -> anyhow::Result<gleon_core::cases::Cases> {
    use anyhow::Context as _;
    use gleon_core::{
        case::RUN_ID_ENV,
        cases::{Cases, RUN_FILE, check_run_dir},
    };

    let run_id = match source {
        RunSource::Own(_) => env_run_id()?,
        RunSource::Copy(dir) => {
            check_run_dir(dir)?;
            None
        }
    };
    let dir = source.dir();
    let cases = Cases::load(dir, run_id.as_ref())
        .with_context(|| format!("Failed to read the case reports in '{}'", dir.display()))?;
    for warning in cases.warnings() {
        tracing::warn!("{warning}");
    }
    if cases.reports().is_empty() {
        let run = match (source, cases.run_id()) {
            (RunSource::Own(_), Some(run_id)) => {
                format!(" of run '{}' ({RUN_FILE} or {RUN_ID_ENV})", run_id.as_str())
            }
            (_, Some(run_id)) => format!(" of run '{}'", run_id.as_str()),
            (_, None) => String::new(),
        };
        anyhow::bail!(
            "No case reports{run} found in '{}': run the tests with `gleon test -- <command>`, \
             or run `gleon diff`",
            dir.join("cases").display()
        );
    }
    Ok(cases)
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
    use super::*;

    #[derive(Debug)]
    struct Cause;
    impl std::fmt::Display for Cause {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("the image format could not be determined")
        }
    }
    impl std::error::Error for Cause {}

    /// Mirrors `ApproveError::ImageDecode`: carries a `#[source]` that its own `Display`
    /// deliberately does not interpolate.
    #[derive(Debug)]
    struct HidesItsSource(Cause);
    impl std::fmt::Display for HidesItsSource {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("Image decode error for 'broken.png'")
        }
    }
    impl std::error::Error for HidesItsSource {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn test_format_failure_includes_hidden_source_chain() {
        let msg = format_failure("Error approving screenshots", &HidesItsSource(Cause));
        assert!(
            msg.contains("Image decode error for 'broken.png'"),
            "top-level message missing: {msg}"
        );
        assert!(
            msg.contains("the image format could not be determined"),
            "underlying cause must not be swallowed: {msg}"
        );
    }

    /// Mirrors `CoreError::Io`: `#[error("IO error: {0}")]` already interpolates its source.
    #[derive(Debug)]
    struct InterpolatesItsSource(Cause);
    impl std::fmt::Display for InterpolatesItsSource {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "IO error: {}", self.0)
        }
    }
    impl std::error::Error for InterpolatesItsSource {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn test_format_failure_does_not_repeat_already_interpolated_source() {
        // Many `thiserror` variants render their own source (`#[error("IO error: {0}")]`).
        // Walking the chain must not echo it a second time.
        let msg = format_failure(
            "Error initializing workspace",
            &InterpolatesItsSource(Cause),
        );
        assert_eq!(
            msg,
            "Error initializing workspace: IO error: the image format could not be determined"
        );
        assert_eq!(
            msg.matches("the image format could not be determined")
                .count(),
            1,
            "cause must appear exactly once: {msg}"
        );
    }

    /// A source chain that never terminates — `Error::source()` implementations are free to
    /// return anything, and a cycle here must not hang the CLI on its error path.
    #[derive(Debug)]
    struct Cyclic;
    impl std::fmt::Display for Cyclic {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("round and round")
        }
    }
    impl std::error::Error for Cyclic {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&Cyclic)
        }
    }

    #[test]
    fn test_format_failure_terminates_on_cyclic_source_chain() {
        // Would loop forever if the walk were unbounded: the dedup check does not help here,
        // since the repeated cause is skipped but the chain still advances endlessly.
        let msg = format_failure("ctx", &Cyclic);
        assert!(msg.starts_with("ctx: round and round"));
    }

    #[test]
    fn test_format_failure_keeps_cause_that_only_the_context_mentions() {
        // The dedup must compare against the *parent error*, not the whole accumulated line:
        // otherwise a cause whose text happens to appear in the caller-supplied context
        // (here "report") would be silently dropped.
        #[derive(Debug)]
        struct Bare;
        impl std::fmt::Display for Bare {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("report")
            }
        }
        impl std::error::Error for Bare {}

        #[derive(Debug)]
        struct Outer(Bare);
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("template rendering failed")
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        assert_eq!(
            format_failure("Error generating report", &Outer(Bare)),
            "Error generating report: template rendering failed: report"
        );
    }

    #[test]
    fn test_format_failure_without_source_is_plain() {
        assert_eq!(
            format_failure("Error pushing baseline blobs", &Cause),
            "Error pushing baseline blobs: the image format could not be determined"
        );
    }

    #[test]
    fn test_format_failure_preserves_distinct_cause_matching_parent_substring() {
        #[derive(Debug)]
        struct SubstringCause;
        impl std::fmt::Display for SubstringCause {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("report")
            }
        }
        impl std::error::Error for SubstringCause {}

        #[derive(Debug)]
        struct ParentWithSubstring(SubstringCause);
        impl std::fmt::Display for ParentWithSubstring {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("failed to render report template")
            }
        }
        impl std::error::Error for ParentWithSubstring {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        let msg = format_failure("Context", &ParentWithSubstring(SubstringCause));
        assert_eq!(msg, "Context: failed to render report template: report");
    }

    /// Without reports the error names the run the reports were looked for, and a broken run
    /// file is an error with the directory.
    #[test]
    fn test_load_cases_explains_a_run_without_reports() {
        use gleon_core::{case::RunId, cases::RunInfo};

        let temp = tempfile::tempdir().unwrap();
        let latest = temp.path().join("latest");
        std::fs::create_dir_all(latest.join("cases")).unwrap();
        RunInfo {
            run_id: RunId::new("started").unwrap(),
            started_at: chrono::Utc::now(),
            command: vec!["flutter".to_owned()],
        }
        .write(&latest)
        .unwrap();
        let copy = load_cases(&RunSource::Copy(latest.clone())).unwrap_err();
        assert!(
            copy.to_string()
                .starts_with("No case reports of run 'started' found"),
            "{copy}"
        );
        if std::env::var_os(gleon_core::case::RUN_ID_ENV).is_none() {
            let own = load_cases(&RunSource::Own(latest.clone())).unwrap_err();
            assert!(
                own.to_string().contains("(run.json or GLEON_RUN_ID)"),
                "{own}"
            );
        }

        std::fs::write(latest.join("run.json"), "{").unwrap();
        let broken = load_cases(&RunSource::Copy(latest)).unwrap_err();
        assert!(
            broken
                .to_string()
                .starts_with("Failed to read the case reports in"),
            "{broken}"
        );
    }

    #[test]
    fn test_report_failure_returns_failure_exit_code() {
        assert_eq!(report_failure("ctx", &Cause), ExitCode::Failure);
        assert_eq!(i32::from(ExitCode::Failure), 1);
        assert_eq!(i32::from(ExitCode::Success), 0);
    }
}
