//! Why a call failed, as a class the integration can act on plus the message to show.

#![forbid(unsafe_code)]

use gleon_model::case::CaseErrorKind;

/// Class of a failed call (`u8` across the ABI), so an integration can report errors
/// differently (a `JUnit` `error` versus a `failure`, a hint to fix the config) without parsing
/// messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ErrorKind {
    /// The call did not fail.
    None = 0,
    /// The integration broke the C contract (null or misaligned buffers, invalid UTF-8, unknown
    /// codes, invalid session strings or tolerances).
    InvalidInput = 1,
    /// `.gleon/gleon.yaml`, `GLEON_METRICS` or a golden name is invalid.
    Config = 2,
    /// A file could not be read or written.
    Io = 3,
    /// An image could not be decoded, is over the analysis budget or could not be encoded.
    Image = 4,
    /// A bug in this library (a caught panic).
    Internal = 5,
}

impl ErrorKind {
    /// The kind recorded in a case report; `None` for [`Self::None`].
    pub const fn case_kind(self) -> Option<CaseErrorKind> {
        match self {
            Self::None => None,
            Self::InvalidInput => Some(CaseErrorKind::InvalidInput),
            Self::Config => Some(CaseErrorKind::Config),
            Self::Io => Some(CaseErrorKind::Io),
            Self::Image => Some(CaseErrorKind::Image),
            Self::Internal => Some(CaseErrorKind::Internal),
        }
    }
}

impl From<CaseErrorKind> for ErrorKind {
    fn from(kind: CaseErrorKind) -> Self {
        match kind {
            CaseErrorKind::InvalidInput => Self::InvalidInput,
            CaseErrorKind::Config => Self::Config,
            CaseErrorKind::Io => Self::Io,
            CaseErrorKind::Image => Self::Image,
            CaseErrorKind::Internal => Self::Internal,
        }
    }
}

/// A failed call: its class and the complete message to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// The class.
    pub kind: ErrorKind,
    /// The message, starting with `gleon:` or naming the golden.
    pub message: String,
}

impl Failure {
    /// A failure of `kind` with `message`.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// The integration broke the C contract.
    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidInput, message)
    }

    /// The config, `GLEON_METRICS` or a golden name is invalid.
    pub fn config(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Config, message)
    }

    /// A file could not be read or written.
    pub fn io(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Io, message)
    }
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

    #[test]
    fn test_case_kinds_share_the_names() {
        assert_eq!(ErrorKind::None.case_kind(), None);
        for (kind, name) in [
            (ErrorKind::InvalidInput, "invalid_input"),
            (ErrorKind::Config, "config"),
            (ErrorKind::Io, "io"),
            (ErrorKind::Image, "image"),
            (ErrorKind::Internal, "internal"),
        ] {
            assert_eq!(kind.case_kind().unwrap().as_str(), name);
        }
    }
}
