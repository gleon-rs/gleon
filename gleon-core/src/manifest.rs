//! Manifest definitions for gleon.

pub mod conflict;
pub mod index;
pub mod single;

pub use conflict::{ConflictManifest, ConflictParseError, parse_conflict_manifest};
pub use gleon_model::hash::{ImageHash, InvalidImageHash};
pub use index::{WorkspaceIndex, validate_test_path};
pub use single::{SUPPORTED_SINGLE_MANIFEST_SCHEMA_VERSION, SingleTestManifest};

use crate::io::IoError;

/// Errors that can occur during manifest operations.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// IO or JSON serialization error.
    #[error("IO error: {0}")]
    Io(#[from] IoError),

    /// Standard I/O error.
    #[error("I/O error: {0}")]
    StdIo(#[source] std::io::Error),

    /// Directory traversal or walker error.
    #[error("Walker error: {0}")]
    Walker(#[source] ignore::Error),

    /// Image processing or format error.
    #[error("Image error: {0}")]
    Image(#[source] image::ImageError),

    /// Validation error in manifest schema or entry content.
    #[error("Validation error: {0}")]
    Validation(String),

    /// A malformed image hash.
    #[error("Validation error: {0}")]
    InvalidHash(#[from] InvalidImageHash),
}
