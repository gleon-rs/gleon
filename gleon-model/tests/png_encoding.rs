#![cfg(not(miri))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::pedantic,
    clippy::nursery,
    missing_docs,
    reason = "test code: panics are assertions, and pedantic/nursery style lints are not enforced in tests"
)]

//! The bytes the comparisons encode are part of what users commit: `gleon approve` turns a kept
//! candidate into a golden, and its hash is in the case report. The `image` crate does not count
//! its encoder output as API, so an upgrade that changes it shows here first.

use std::path::PathBuf;

use gleon_engine::decode::decode_rgba;
use gleon_model::compare::{Candidate, encode_png};
use sha2::{Digest, Sha256};

/// SHA-256 of [`encode_png`] of `tests/fixtures/text/golden.png` (a real render, 240x160).
const ENCODED: &str = "9cbc17854d53cb98ccf38d2c12915d6b6b92a7ac1c99acc033463a879acb72de";

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn test_the_png_encoding_is_stable() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/text/golden.png");
    let image = decode_rgba(&std::fs::read(path).unwrap()).unwrap();
    let encoded = encode_png(&image).unwrap();
    assert_eq!(hex(&encoded), ENCODED, "the encoder output changed");
    // A raw candidate kept for `gleon approve` is the same bytes.
    let raw = Candidate::Rgba {
        width: image.width(),
        height: image.height(),
        pixels: image.as_raw(),
    };
    assert_eq!(raw.to_png().unwrap().as_ref(), encoded.as_slice());
    // And it decodes back to the same pixels.
    assert_eq!(decode_rgba(&encoded).unwrap(), image);
}
