//! Whether a build of gleon is official, decided at build time; shared by `build.rs` and its test
//! (`tests/official_build.rs`), since a build script has no tests of its own.

use sha2::{Digest, Sha256};

/// SHA-256 of the secret of official builds.
const OFFICIAL_SECRET_HASH: [u8; 32] = [
    0xa4, 0x9c, 0x93, 0x96, 0x4b, 0x0c, 0x70, 0x7c, 0x82, 0x8a, 0xa3, 0xf4, 0x09, 0x11, 0xb8, 0xf5,
    0x17, 0x99, 0xcc, 0xd9, 0x91, 0x80, 0x30, 0xa4, 0xb3, 0x7e, 0x23, 0xb0, 0x0c, 0x93, 0xe6, 0xfc,
];

/// Whether the SHA-256 of `secret` is `hash`.
pub fn hashes_to(secret: &str, hash: &[u8; 32]) -> bool {
    Sha256::digest(secret.as_bytes()).as_slice() == hash
}

/// Whether the build with `secret` (`GLEON_OFFICIAL_SECRET`) is official: `Ok(true)` for the
/// secret of official builds, `Ok(false)` otherwise, and an error instead of `Ok(false)` when an
/// official build is `required` (`GLEON_REQUIRE_OFFICIAL`).
///
/// # Errors
/// Returns the reason when an official build is required and `secret` is missing or wrong.
pub fn decide(secret: Option<&str>, required: bool) -> Result<bool, &'static str> {
    let official = secret.is_some_and(|secret| hashes_to(secret, &OFFICIAL_SECRET_HASH));
    if official || !required {
        Ok(official)
    } else {
        Err(
            "GLEON_REQUIRE_OFFICIAL is set, but GLEON_OFFICIAL_SECRET is missing or not the secret of official builds",
        )
    }
}
