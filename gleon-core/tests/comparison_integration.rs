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
    ComparisonResult, MismatchDetail, compare_images,
    config::{DiffConfig, Mode},
};
use image::{Rgba, RgbaImage};

fn load_fixture(name: &str) -> RgbaImage {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    image::open(&path)
        .unwrap_or_else(|e| panic!("failed to load fixture at {path:?}: {e}"))
        .into_rgba8()
}

#[test]
fn test_integration_1_pixel_difference() {
    let baseline = load_fixture("baseline_gradient_100x100.png");
    let actual = load_fixture("diff_1px_black_center_100x100.png");

    let config = DiffConfig {
        threshold: 0.0,
        ..Default::default()
    };

    let result = compare_images(&baseline, &actual, Mode::Pixel, &config);

    assert!(
        matches!(
            result,
            ComparisonResult::Mismatch {
                detail: MismatchDetail::Pixel { diff_count: 1 },
                ..
            }
        ),
        "Expected exactly 1 pixel mismatch, got: {result:?}"
    );

    let ComparisonResult::Mismatch { diff_image, .. } = result else {
        panic!("Expected Mismatch, but got Match");
    };
    let diff_pixel = diff_image.get_pixel(50, 50);
    assert_eq!(*diff_pixel, Rgba([255, 0, 255, 255]));
}

#[test]
fn test_integration_16_pixels_corners() {
    let baseline = load_fixture("baseline_gradient_100x100.png");
    let actual = load_fixture("diff_16px_corners_100x100.png");

    let config = DiffConfig {
        threshold: 0.0,
        ..Default::default()
    };

    let result = compare_images(&baseline, &actual, Mode::Pixel, &config);

    assert!(
        matches!(
            result,
            ComparisonResult::Mismatch {
                detail: MismatchDetail::Pixel { diff_count: 16 },
                ..
            }
        ),
        "Expected exactly 16 pixels mismatch, got: {result:?}"
    );
    let ComparisonResult::Mismatch { diff_image, .. } = result else {
        panic!("Expected Mismatch, but got Match");
    };

    // Check corner pixels in the diff image
    assert_eq!(*diff_image.get_pixel(0, 0), Rgba([255, 0, 255, 255]));
    assert_eq!(*diff_image.get_pixel(99, 99), Rgba([255, 0, 255, 255]));

    // Unchanged pixel should be darkened (divided by 2)
    let b_orig = baseline.get_pixel(50, 50);
    let d_match = diff_image.get_pixel(50, 50);
    assert_eq!(
        *d_match,
        Rgba([b_orig[0] / 2, b_orig[1] / 2, b_orig[2] / 2, b_orig[3]])
    );
}

#[test]
fn test_integration_transparency_difference() {
    let baseline = load_fixture("baseline_gradient_100x100.png");
    let actual = load_fixture("diff_rounded_corners_100x100.png");

    let config = DiffConfig {
        threshold: 0.0,
        ..Default::default()
    };

    let result = compare_images(&baseline, &actual, Mode::Pixel, &config);

    assert!(
        matches!(
            result,
            ComparisonResult::Mismatch {
                detail: MismatchDetail::Pixel { diff_count: 10 },
                ..
            }
        ),
        "Expected exactly 10 pixels mismatch due to alpha channel difference, got: {result:?}"
    );
}

#[test]
fn test_integration_dimension_mismatch_real_files() {
    let baseline = load_fixture("baseline_gradient_100x100.png");
    let actual = load_fixture("200x100.png");

    let config = DiffConfig::default();
    let result = compare_images(&baseline, &actual, Mode::Pixel, &config);

    assert!(
        matches!(
            result,
            ComparisonResult::DimensionMismatch {
                baseline_size: (100, 100),
                actual_size: (200, 100)
            }
        ),
        "Expected DimensionMismatch, got: {result:?}"
    );
}

#[test]
#[cfg(not(miri))]
fn test_integration_ssim_large_diff() {
    let baseline = load_fixture("baseline_gradient_100x100.png");
    let actual = load_fixture("diff_16px_corners_100x100.png");

    let config = DiffConfig {
        min_similarity: 0.999, // Too strict similarity
        threshold: 0.05,
        ..Default::default()
    };

    let result = compare_images(&baseline, &actual, Mode::Ssim, &config);

    assert!(
        matches!(
            result,
            ComparisonResult::Mismatch {
                detail: MismatchDetail::Ssim { .. },
                ..
            }
        ),
        "Expected SSIM mismatch, got: {result:?}"
    );
}
