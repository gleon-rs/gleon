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

use std::{path::Path, sync::Arc};

use gleon_core::{
    config::{GleonConfig, GlobPattern, ScreenshotRule},
    scanner::{FileScanner, TestCase},
};
use gleon_engine::config::{DiffConfig, Mode};

#[test]
fn test_scanner_with_real_fixture() {
    // The tests/fixtures directory contains a real 200x100.png file and a corrupt.png file.
    let base_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");

    let fixture_path = base_dir.join("200x100.png");
    let corrupt_path = base_dir.join("corrupt.png");
    assert!(fixture_path.exists(), "The 200x100.png fixture is missing!");
    assert!(corrupt_path.exists(), "The corrupt.png fixture is missing!");

    let include = vec![GlobPattern::new("**/*.png").unwrap()];
    let exclude = vec![GlobPattern::new("**/report_output/**").unwrap()];
    let rule = Arc::new(ScreenshotRule {
        include: include.clone(),
        mode: Mode::Pixel,
        diff: DiffConfig::default(),
        masks: vec![],
    });

    let config = GleonConfig {
        screenshots: vec![(*rule).clone()],
        exclude: exclude.clone(),
        ..GleonConfig::default()
    };
    let cases: Vec<TestCase> = FileScanner::scan_workspace(&config, &base_dir)
        .expect("Scanning the workspace should succeed");

    // We found all the expected PNG files (9 files total in the fixtures dir)
    assert_eq!(cases.len(), 9);

    // Verify 200x100.png
    let test_case_200 = cases
        .iter()
        .find(|c| c.name == "200x100")
        .expect("Should find test case for 200x100");

    let image_200x100 = &test_case_200.image;
    assert_eq!(image_200x100.absolute_path, fixture_path);
    let img =
        image::open(&image_200x100.absolute_path).expect("Failed to decode the real PNG fixture");
    assert_eq!(img.width(), 200);
    assert_eq!(img.height(), 100);

    // Verify baseline_100x100.png
    let test_case_100 = cases
        .iter()
        .find(|c| c.name == "baseline_100x100")
        .expect("Should find test case for baseline_100x100");

    let image_100x100 = &test_case_100.image;
    assert_eq!(
        image_100x100.absolute_path,
        base_dir.join("baseline_100x100.png")
    );
    let img_100 = image::open(&image_100x100.absolute_path)
        .expect("Failed to decode the baseline 100x100 PNG fixture");
    assert_eq!(img_100.width(), 100);
    assert_eq!(img_100.height(), 100);

    // Verify corrupt.png
    let test_case_corrupt = cases
        .iter()
        .find(|c| c.name == "corrupt")
        .expect("Should find test case for corrupt");

    let corrupt_image = &test_case_corrupt.image;
    assert_eq!(corrupt_image.absolute_path, corrupt_path);
    assert!(
        image::open(&corrupt_image.absolute_path).is_err(),
        "corrupt.png should fail to decode"
    );
}
