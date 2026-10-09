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

//! Goldens come from any PNG writer: 16-bit, gray, gray with alpha, palette with transparency,
//! Adam7-interlaced. Each decodes to the straight RGBA8 a reference decoder (PIL)
//! produced for it (`tests/fixtures/decode/*.rgba`, 37x23), and an integration's raw capture of
//! those pixels matches the golden exactly.

use std::path::PathBuf;

use gleon_engine::decode::decode_rgba;
use gleon_model::{
    compare::{Candidate, Compared, compare},
    tolerance::Tolerance,
};

const SIZE: (u32, u32) = (37, 23);

/// A PNG variant and the reference pixels it decodes to.
const VARIANTS: [(&str, &str); 9] = [
    ("rgba16.png", "rgba8.rgba"),
    ("rgba16_adam7.png", "rgba8.rgba"),
    ("rgba8_adam7.png", "rgba8.rgba"),
    ("rgb16.png", "rgb8.rgba"),
    ("gray16.png", "gray8.rgba"),
    ("gray8_adam7.png", "gray8.rgba"),
    ("graya16.png", "graya8.rgba"),
    ("pal_trns.png", "pal_trns.rgba"),
    ("pal_trns_adam7.png", "pal_trns.rgba"),
];

fn fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/decode")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn test_png_variants_decode_to_their_reference_pixels() {
    for (png, reference) in VARIANTS {
        let decoded = decode_rgba(&fixture(png)).unwrap();
        assert_eq!(decoded.dimensions(), SIZE, "{png}");
        assert!(
            decoded.as_raw() == &fixture(reference),
            "{png} vs {reference}"
        );
    }
}

#[test]
fn test_raw_captures_match_png_variants_exactly() {
    for (png, reference) in VARIANTS {
        let pixels = fixture(reference);
        let raw = Candidate::Rgba {
            width: SIZE.0,
            height: SIZE.1,
            pixels: &pixels,
        };
        let compared = compare(&fixture(png), raw, &Tolerance::Exact {}, &[], None)
            .unwrap()
            .compared;
        assert!(
            matches!(&compared, Compared::Match { metrics, .. } if !metrics.differs()),
            "{png}: {compared:?}"
        );
    }
}
