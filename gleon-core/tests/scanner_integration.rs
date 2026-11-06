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
use gleon_model::tolerance::Tolerance;

/// The scanner finds the PNG files of a workspace (decodable or not) and skips excluded
/// directories; the fixtures are copied into a workspace of their own, so new fixtures never
/// change what this test sees.
#[test]
fn test_scanner_with_real_fixture() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let temp = tempfile::tempdir().unwrap();
    // Canonical, as the scanner reports it (macOS: `/var` is a link to `/private/var`).
    let base_dir = std::fs::canonicalize(temp.path()).unwrap();
    for (file, fixture) in [
        ("200x100.png", "200x100.png"),
        ("shots/baseline_100x100.png", "baseline_100x100.png"),
        ("corrupt.png", "corrupt.png"),
        ("report_output/ignored.png", "200x100.png"),
    ] {
        let path = base_dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::copy(fixtures.join(fixture), path).unwrap();
    }
    std::fs::write(base_dir.join("notes.txt"), "not a screenshot").unwrap();
    let fixture_path = base_dir.join("200x100.png");
    let corrupt_path = base_dir.join("corrupt.png");

    let include = vec![GlobPattern::new("**/*.png").unwrap()];
    let exclude = vec![GlobPattern::new("**/report_output/**").unwrap()];
    let rule = Arc::new(ScreenshotRule {
        include: include.clone(),
        tolerance: Tolerance::pixel(0.01),
        masks: vec![],
        text_tolerance: None,
    });

    let config = GleonConfig {
        screenshots: vec![(*rule).clone()],
        exclude: exclude.clone(),
        ..GleonConfig::default()
    };
    let cases: Vec<TestCase> = FileScanner::scan_workspace(&config, &base_dir)
        .expect("Scanning the workspace should succeed");

    // The three PNG files outside `report_output/`.
    assert_eq!(cases.len(), 3);

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
        .find(|c| c.name == "shots/baseline_100x100")
        .expect("Should find test case for shots/baseline_100x100");

    let image_100x100 = &test_case_100.image;
    assert_eq!(
        image_100x100.absolute_path,
        base_dir.join("shots/baseline_100x100.png")
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
