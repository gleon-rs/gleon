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

use std::{
    fs,
    path::{Path, PathBuf},
};

use gleon_core::{
    report::ReportGenerator,
    results::{TestCaseResult, TestImageResult},
};
use gleon_engine::{
    ComparisonResult, MismatchDetail, compare_images,
    config::{DiffConfig, Mode},
};

#[test]
fn test_report_generation_with_real_images_and_durability() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixtures_dir = manifest_dir.join("tests").join("fixtures");

    // Work on copies in an isolated temp dir (never write into tracked fixtures); the report
    // goes into a subdirectory so image links stay relative (`../dashboard_baseline.png`).
    let work_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let baseline_path = work_dir.path().join("dashboard_baseline.png");
    let actual_path = work_dir.path().join("dashboard_actual.png");
    fs::copy(fixtures_dir.join("dashboard_baseline.png"), &baseline_path)
        .expect("Failed to copy dashboard_baseline.png");
    fs::copy(fixtures_dir.join("dashboard_actual.png"), &actual_path)
        .expect("Failed to copy dashboard_actual.png");

    let baseline_img = image::open(&baseline_path)
        .expect("Failed to open dashboard_baseline.png")
        .to_rgba8();
    let actual_img = image::open(&actual_path)
        .expect("Failed to open dashboard_actual.png")
        .to_rgba8();

    // 1. Perform image comparison for Pixel mode
    let diff_config = DiffConfig {
        threshold: 0.0,
        ..Default::default()
    };
    let comp_pixel_res = compare_images(&baseline_img, &actual_img, Mode::Pixel, &diff_config);
    let (pixel_detail, pixel_diff_img) = match comp_pixel_res {
        ComparisonResult::Mismatch { detail, diff_image } => (detail, diff_image),
        other => panic!("Expected ComparisonResult::Mismatch, got {other:?}"),
    };

    // 2. Perform image comparison for SSIM mode
    let ssim_config = DiffConfig {
        threshold: 1.0,
        ..Default::default()
    };
    let comp_ssim_res = compare_images(&baseline_img, &actual_img, Mode::Ssim, &ssim_config);
    let (ssim_detail, ssim_diff_img) = match comp_ssim_res {
        ComparisonResult::Mismatch { detail, diff_image } => (detail, diff_image),
        other => panic!("Expected ComparisonResult::Mismatch, got {other:?}"),
    };

    // 3. Report outputs next to the copied images.
    let report_dir = work_dir.path().join("report_output");
    fs::create_dir_all(&report_dir).expect("Failed to create report output dir");

    let diff_pixel_name = "diff_dashboard_pixel.png";
    let diff_ssim_name = "diff_dashboard_ssim.png";

    let diff_pixel_path = report_dir.join(diff_pixel_name);
    let diff_ssim_path = report_dir.join(diff_ssim_name);

    pixel_diff_img
        .save(&diff_pixel_path)
        .expect("Failed to save pixel diff image");
    ssim_diff_img
        .save(&diff_ssim_path)
        .expect("Failed to save ssim diff image");

    // 4. Construct comprehensive TestCaseResult covering all 4 failure types
    let tc_res1 = TestCaseResult {
        name: "billing_dashboard_1".to_string(),
        result: TestImageResult::Mismatch {
            relative_path: PathBuf::from("overview_metrics.png"),
            detail: pixel_detail,
            diff_path: diff_pixel_path.clone(),
            baseline_path: baseline_path.clone(),
            actual_path: actual_path.clone(),
        },
    };
    let tc_res2 = TestCaseResult {
        name: "billing_dashboard_2".to_string(),
        result: TestImageResult::Mismatch {
            relative_path: PathBuf::from("revenue_performance.png"),
            detail: ssim_detail,
            diff_path: diff_ssim_path,
            baseline_path: baseline_path.clone(),
            actual_path: actual_path.clone(),
        },
    };
    let tc_res3 = TestCaseResult {
        name: "billing_dashboard_3".to_string(),
        result: TestImageResult::Mismatch {
            relative_path: PathBuf::from("security_alert_banner.png"),
            detail: MismatchDetail::Pixel { diff_count: 14205 },
            diff_path: diff_pixel_path,
            baseline_path: baseline_path.clone(),
            actual_path: actual_path.clone(),
        },
    };
    let tc_res4 = TestCaseResult {
        name: "billing_dashboard_4".to_string(),
        result: TestImageResult::DimensionMismatch {
            relative_path: PathBuf::from("sidebar_navigation.png"),
            baseline_size: (1920, 1080),
            actual_size: (1920, 1200),
            baseline_path,
            actual_path,
        },
    };
    let tc_res5 = TestCaseResult {
        name: "billing_dashboard_5".to_string(),
        result: TestImageResult::DecodeError {
            relative_path: PathBuf::from("user_avatar.png"),
            error: "PNG header corrupted or incomplete".to_string(),
        },
    };

    let tc_res6 = TestCaseResult {
        name: "billing_dashboard_6".to_string(),
        result: TestImageResult::MissingBaseline {
            relative_path: PathBuf::from("missing_base.png"),
            reason: "Baseline missing".to_string(),
        },
    };

    let tc_res7 = TestCaseResult {
        name: "billing_dashboard_7".to_string(),
        result: TestImageResult::EncodeError {
            relative_path: PathBuf::from("encode_fail.png"),
            actual_path: PathBuf::from("/tmp/actual.png"),
            error: "Failed to write PNG chunk".to_string(),
        },
    };

    let all_tc_res = vec![
        tc_res1, tc_res2, tc_res3, tc_res4, tc_res5, tc_res6, tc_res7,
    ];

    // 5. Generate HTML report
    let html_report = ReportGenerator::generate_html(&all_tc_res, Some(&report_dir));

    let html = html_report
        .expect("HTML render should succeed")
        .expect("Expected Some(HTML), but got None");
    assert!(!html.contains("data:image/png;base64,"));
    assert!(html.contains("billing_dashboard_1 / overview_metrics.png"));
    assert!(html.contains("billing_dashboard_2 / revenue_performance.png"));
    assert!(html.contains("billing_dashboard_4 / sidebar_navigation.png"));
    assert!(html.contains("billing_dashboard_5 / user_avatar.png"));
    assert!(html.contains("billing_dashboard_6 / missing_base.png"));
    assert!(html.contains("billing_dashboard_7 / encode_fail.png"));
    assert!(html.contains("PNG header corrupted or incomplete"));
    assert!(html.contains("Baseline missing"));
    assert!(html.contains("Failed to write PNG chunk"));

    let html_path = report_dir.join("report.html");
    fs::write(&html_path, &html).expect("Failed to write HTML report");
    assert!(html_path.exists());

    // 6. Generate and write JUnit XML
    let xml = ReportGenerator::generate_junit_xml(&all_tc_res).expect("XML render should succeed");
    assert!(xml.contains("<testsuites name=\"gleon Tests\""));
    assert!(xml.contains("<failure message=\"Visual mismatch detected ("));
    assert!(xml.contains("<failure message=\"Visual mismatch detected (min local SSIM "));
    assert!(html.contains("..&#x2f;dashboard_baseline.png"));
    assert!(html.contains("..&#x2f;dashboard_actual.png"));
    assert!(html.contains("diff_dashboard_pixel.png"));
    assert!(xml.contains(
        "<failure message=\"Dimension mismatch (Baseline: 1920x1080, Actual: 1920x1200)\""
    ));
    assert!(xml.contains("<failure message=\"Decode error: PNG header corrupted or incomplete\""));
    assert!(xml.contains("<failure message=\"Missing baseline: Baseline missing\""));
    assert!(xml.contains("<failure message=\"Encode error: Failed to write PNG chunk\""));

    let xml_path = report_dir.join("junit.xml");
    fs::write(&xml_path, &xml).expect("Failed to write XML report");
    assert!(xml_path.exists());

    // 7. Generate and write Markdown
    let md = ReportGenerator::generate_markdown(&all_tc_res);
    assert!(md.contains("# gleon Visual Regression Summary"));
    assert!(md.contains("❌ Mismatch"));
    assert!(md.contains("❌ Dimension Mismatch"));
    assert!(md.contains("❌ Decode Error"));
    assert!(md.contains("❌ Missing Baseline"));
    assert!(md.contains("❌ Encode Error"));

    let md_path = report_dir.join("report.md");
    fs::write(&md_path, &md).expect("Failed to write MD report");
    assert!(md_path.exists());

    // 8. Verify fallback_demo.html fixture integrity
    // Read-only tracked fixture (not generated by this test).
    let fallback_demo_path = fixtures_dir
        .join("report_output")
        .join("fallback_demo.html");
    assert!(
        fallback_demo_path.exists(),
        "fallback_demo.html fixture must exist"
    );
    let fallback_content =
        fs::read_to_string(&fallback_demo_path).expect("Failed to read fallback_demo.html fixture");
    assert!(fallback_content.contains("document.addEventListener('error'"));
    assert!(!fallback_content.contains("onerror="));
}
