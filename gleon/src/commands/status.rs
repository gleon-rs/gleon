//! Implementation of the `gleon status` subcommand.

use std::io::Write;

use anyhow::Context as _;
use gleon_core::context::ResolvedContext;

use crate::exit_code::ExitCode;

/// Writes the status `report` to `writer`: JSON with `json`, else text.
///
/// # Errors
/// Returns the error of serializing or writing the report.
pub fn write_status(
    report: &gleon_core::ops::StatusReport,
    json: bool,
    writer: &mut impl Write,
) -> anyhow::Result<()> {
    if json {
        let rendered = report
            .format_json()
            .context("Error serializing status report")?;
        writeln!(writer, "{rendered}").context("Error writing status report")?;
    } else {
        write!(writer, "{}", report.format_text()).context("Error writing status report")?;
    }
    Ok(())
}

/// Runs the `gleon status` subcommand, printing the report to stdout.
pub fn run_status(ctx: &ResolvedContext, json: bool) -> anyhow::Result<ExitCode> {
    let report = gleon_core::ops::check_status(ctx).context("Error checking workspace status")?;

    write_status(&report, json, &mut std::io::stdout().lock())?;
    Ok(ExitCode::Success)
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
    use std::path::PathBuf;

    use gleon_core::ops::StatusReport;

    use super::*;

    #[test]
    fn test_write_status_text() {
        let mut report = StatusReport::default();
        report.added.push(PathBuf::from("login.png"));
        let mut buf = Vec::new();
        write_status(&report, false, &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("Added"));
        assert!(output.contains("login.png"));
    }

    #[test]
    fn test_write_status_json() {
        let mut report = StatusReport::default();
        report.added.push(PathBuf::from("login.png"));
        let mut buf = Vec::new();
        write_status(&report, true, &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("\"added\""));
        assert!(output.contains("\"login.png\""));
    }

    #[test]
    fn test_write_status_io_error_propagation() {
        struct FailingWriter;
        impl Write for FailingWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "broken pipe",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "broken pipe",
                ))
            }
        }

        let report = StatusReport::default();
        let err = write_status(&report, false, &mut FailingWriter).unwrap_err();
        assert_eq!(err.to_string(), "Error writing status report");
        assert!(err.root_cause().to_string().contains("broken pipe"));

        let err_json = write_status(&report, true, &mut FailingWriter).unwrap_err();
        assert_eq!(err_json.to_string(), "Error writing status report");
    }
}
