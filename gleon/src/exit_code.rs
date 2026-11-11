//! The process exit codes of the CLI (`gleon test` exits with the code of its test command
//! instead, and clap with 2 for a usage error).

/// The exit code of a gleon command.
///
/// An unexpected error is reported by [`crate::commands::report_failure`] and also exits with
/// [`ExitCode::Failure`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ExitCode {
    /// The operation completed and succeeded.
    Success = 0,
    /// The operation completed but determined a failure (a failed diff, a bad lint report, a
    /// non-interactive terminal, ...), or it could not run.
    Failure = 1,
    /// The license check stopped gleon before the command ran (see `gleon_core::license`).
    LicenseBlocked = 42,
}

impl From<ExitCode> for i32 {
    fn from(code: ExitCode) -> Self {
        code as Self
    }
}
