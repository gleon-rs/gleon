//! Why a call failed, as a class the integration can act on plus the message to show.

#![forbid(unsafe_code)]

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
