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

use std::path::Path;

use gleon_engine::{
    config::{Dimension, Zone},
    masking,
};

#[test]
fn test_masking_integration() {
    // 1. Resolve base directory and locate the baseline_100x100.png fixture
    let base_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let baseline_path = base_dir.join("baseline_100x100.png");
    assert!(
        baseline_path.exists(),
        "Source fixture baseline_100x100.png must exist"
    );

    // 2. Load the baseline image
    let mut img = image::open(&baseline_path)
        .expect("Failed to open baseline_100x100.png fixture")
        .to_rgba8();

    assert_eq!(img.width(), 100);
    assert_eq!(img.height(), 100);

    let original_img = img.clone();

    // 3. Define an out-of-bounds mask zone (x: 90, width: 20px, height: 100%)
    // Since the image width is 100px, x + width = 110px, which is out of bounds.
    let zones = vec![Zone {
        x: 90,
        y: 0,
        width: Dimension::Pixels(20),
        height: Dimension::Percent(100.0),
    }];

    // 4. Apply masks — clamping is expected due to x:90 + width:20 > 100
    masking::apply_masks(&mut img, &zones);

    // 5. Assert that exactly the bounding pixels [90..99] are mutated to black,
    // and pixels [0..89] remain unchanged.
    for (x, y, pixel) in img.enumerate_pixels() {
        if x >= 90 {
            // Should be mutated to black
            assert_eq!(
                *pixel,
                image::Rgba([0, 0, 0, 255]),
                "Pixel at ({x}, {y}) should be black"
            );
        } else {
            // Should remain original
            let original_pixel = original_img.get_pixel(x, y);
            assert_eq!(
                *pixel, *original_pixel,
                "Pixel at ({x}, {y}) should remain unchanged"
            );
        }
    }
}
