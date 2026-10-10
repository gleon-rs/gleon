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

mod common;

use std::{fs, path::Path};

use gleon_core::{
    case::{CaseErrorKind, CaseOutcome, CaseReport},
    context::{ContextOptions, ResolvedContext},
    ops::{DiffOpError, diff::DiffOptions, init_workspace, run_diff, stage_workspace},
    platform::PlatformKey,
};

/// The case report `gleon diff` wrote for `name` in the workspace at `root`.
fn case_report(root: &Path, name: &str) -> CaseReport {
    let bytes = fs::read(CaseReport::path(
        &root.join(".gleon"),
        PlatformKey::host(),
        name,
    ))
    .unwrap();
    CaseReport::parse(&bytes).unwrap()
}

#[test]
fn test_diff_uninitialized_fails() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let options = ContextOptions {
        branch: Some("main".to_string()),
        target_branch: "main".to_string(),
        ..Default::default()
    };

    let ctx = ResolvedContext::from_options(&options, base_path).unwrap();
    let result = run_diff(&ctx, &DiffOptions::default());

    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        DiffOpError::Core(gleon_core::ops::common::CoreError::NotInitialized)
    ));
}

#[test]
fn test_diff_full_flow_with_real_fixtures() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let ctx_init = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    // 1. Init workspace
    init_workspace(&ctx_init).expect("init_workspace should succeed");

    // 2. Copy real PNG fixture (baseline_100x100.png)
    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let baseline_png_bytes = fs::read(fixtures_dir.join("baseline_100x100.png"))
        .expect("baseline_100x100.png fixture must exist");

    let screenshot_dir = base_path.join("billing");
    fs::create_dir_all(&screenshot_dir).unwrap();
    let screenshot_file = screenshot_dir.join("form.png");
    fs::write(&screenshot_file, &baseline_png_bytes).unwrap();

    common::copy_config(base_path, "billing");

    let options = ContextOptions {
        branch: Some("main".to_string()),
        target_branch: "main".to_string(),
        ..Default::default()
    };

    let ctx = ResolvedContext::from_options(&options, base_path).unwrap();

    // 3. Stage initial baseline
    stage_workspace(&ctx, None).expect("stage_workspace should succeed");

    // 4. Run diff against identical baseline -> should pass
    let report_match = run_diff(&ctx, &DiffOptions::default()).expect("run_diff should succeed");
    assert_eq!(report_match.failed_tests, 0);
    assert_eq!(report_match.total_tests, 1);

    // 5. Replace form.png with a modified PNG fixture (diff_16px_corners_100x100.png)
    let modified_png_bytes = fs::read(fixtures_dir.join("diff_16px_corners_100x100.png"))
        .expect("diff_16px_corners_100x100.png fixture must exist");
    fs::write(&screenshot_file, &modified_png_bytes).unwrap();

    // 6. Run diff against modified image -> should report failure
    let report_mismatch = run_diff(&ctx, &DiffOptions::default()).expect("run_diff should succeed");
    assert_ne!(report_mismatch.failed_tests, 0);
    assert_eq!(report_mismatch.failed_tests, 1);

    // 7. Verify generated report artifacts on disk
    let runs_dir = base_path.join(".gleon/runs/latest");
    assert!(runs_dir.join("report.md").is_file());
    assert!(runs_dir.join("junit.xml").is_file());
}

#[test]
fn test_diff_from_nested_subdirectory() {
    let temp_dir = tempfile::tempdir().unwrap();
    let root_dir = temp_dir.path();

    let ctx_init = ResolvedContext::from_options(&ContextOptions::default(), root_dir).unwrap();
    init_workspace(&ctx_init).expect("init_workspace should succeed");

    let nested_dir = root_dir.join("src").join("billing");
    fs::create_dir_all(&nested_dir).unwrap();

    // A screenshot at the root, found from the nested directory (a new one: no baseline yet).
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/200x100.png");
    fs::copy(fixture, root_dir.join("shot.png")).unwrap();

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), &nested_dir).unwrap();
    assert_eq!(ctx.base_dir, root_dir);

    let report = run_diff(&ctx, &DiffOptions::default())
        .expect("run_diff should succeed when using ctx.base_dir");
    assert_eq!(report.total_tests, 1);
}

#[test]
fn test_diff_cross_platform_backslash_manifest_keys() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let ctx_init = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();
    init_workspace(&ctx_init).expect("init_workspace should succeed");

    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let baseline_png_bytes = fs::read(fixtures_dir.join("baseline_100x100.png"))
        .expect("baseline_100x100.png fixture must exist");

    let screenshot_dir = base_path.join("billing");
    fs::create_dir_all(&screenshot_dir).unwrap();
    fs::write(screenshot_dir.join("form.png"), &baseline_png_bytes).unwrap();

    common::copy_config(base_path, "billing");

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    // Stage baseline
    stage_workspace(&ctx, None).expect("stage_workspace should succeed");

    // Explicitly verify backslash-to-forward-slash path key normalization
    let backslash_path = Path::new("billing\\form.png");
    let normalized = gleon_core::scanner::FileScanner::normalize_path_str(backslash_path);
    assert_eq!(normalized, "billing/form.png");

    // Run diff -> should handle backslash manifest keys cross-platform!
    let report = run_diff(&ctx, &DiffOptions::default())
        .expect("run_diff should handle backslash manifest keys");
    assert_eq!(report.failed_tests, 0);
    assert_eq!(report.total_tests, 1);
}

#[test]
fn test_diff_missing_baseline_returns_missing_baseline() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let ctx_init = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();
    init_workspace(&ctx_init).expect("init_workspace should succeed");

    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let baseline_png_bytes = fs::read(fixtures_dir.join("200x100.png")).unwrap();

    let screenshot_dir = base_path.join("billing");
    fs::create_dir_all(&screenshot_dir).unwrap();
    fs::write(screenshot_dir.join("unstaged.png"), &baseline_png_bytes).unwrap();

    common::copy_config(base_path, "billing");

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    // Do NOT stage unstaged.png
    let report = run_diff(&ctx, &DiffOptions::default()).expect("run_diff should run");
    assert_ne!(report.failed_tests, 0);
    assert_eq!(report.total_tests, 1);
    assert_eq!(report.failed_tests, 1);

    let md = fs::read_to_string(report.runs_dir.join("report.md")).unwrap();
    assert!(md.contains("Missing Baseline"));

    // The candidate is kept for `gleon approve`.
    let candidate = report
        .runs_dir
        .join("artifacts")
        .join(PlatformKey::host())
        .join("billing/unstaged/candidate.png");
    assert_eq!(fs::read(&candidate).unwrap(), baseline_png_bytes);

    let ctx_approve = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();
    let approve_res = gleon_core::ops::approve_workspace(&ctx_approve, &[], &[], None)
        .expect("approve_workspace should succeed");
    assert_eq!(
        approve_res.approved.len(),
        1,
        "Should approve 1 missing baseline image"
    );

    // Verify it was actually baselined
    let manifest_path = base_path
        .join(".gleon/manifests")
        .join(ctx_approve.platform.key().unwrap())
        .join("billing/unstaged.json");
    assert!(
        manifest_path.exists(),
        "Approved manifest should be created"
    );
}

#[test]
fn test_diff_missing_blob_file_and_corrupt_images() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let ctx_init = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();
    init_workspace(&ctx_init).expect("init_workspace should succeed");

    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let baseline_png_bytes = fs::read(fixtures_dir.join("baseline_100x100.png")).unwrap();

    let screenshot_dir = base_path.join("billing");
    fs::create_dir_all(&screenshot_dir).unwrap();
    let screenshot_file = screenshot_dir.join("form.png");
    fs::write(&screenshot_file, &baseline_png_bytes).unwrap();

    common::copy_config(base_path, "billing");

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    // Stage baseline
    stage_workspace(&ctx, None).expect("stage_workspace should succeed");

    // 1. Remove blob file manually from .gleon/blobs/sha256
    let blobs_dir = base_path.join(".gleon/blobs/sha256");
    for entry in fs::read_dir(&blobs_dir).unwrap() {
        let path = entry.unwrap().path();
        let _ = fs::remove_file(path);
    }

    // Modify actual screenshot so it doesn't match the manifest hash, forcing diff engine to load the missing blob
    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    fs::copy(
        fixtures_dir.join("diff_16px_corners_100x100.png"),
        screenshot_dir.join("form.png"),
    )
    .unwrap();

    let report_missing_blob = run_diff(&ctx, &DiffOptions::default()).unwrap();
    assert_ne!(report_missing_blob.failed_tests, 0);
    let missing_blob = case_report(base_path, "billing/form");
    assert_eq!(missing_blob.error_kind, Some(CaseErrorKind::Io));
    assert!(missing_blob.message.unwrap().contains("gleon pull"));

    // 2. Write corrupt baseline blob file back
    let mut blob_digest = String::new();
    for entry in fs::read_dir(base_path.join(".gleon/manifests")).unwrap() {
        // Find platform dir
        let p_dir = entry.unwrap().path();
        if p_dir.is_dir() {
            let manifest_path = p_dir.join("billing/form.json");
            if manifest_path.is_file() {
                let manifest_json = fs::read_to_string(&manifest_path).unwrap();
                if let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&manifest_json) {
                    let hash_str = manifest["hash"].as_str().unwrap();
                    blob_digest = hash_str.split_once(':').unwrap().1.to_string();
                    fs::write(blobs_dir.join(&blob_digest), b"not a png").unwrap();
                }
            }
        }
    }

    let report_corrupt_blob = run_diff(&ctx, &DiffOptions::default()).unwrap();
    assert_ne!(report_corrupt_blob.failed_tests, 0);
    let corrupt_blob = case_report(base_path, "billing/form");
    assert_eq!(corrupt_blob.error_kind, Some(CaseErrorKind::Image));
    assert!(corrupt_blob.message.unwrap().starts_with("golden image"));

    // Restore valid baseline blob so run_diff decodes the baseline and tests corrupt actual screenshot
    fs::write(blobs_dir.join(&blob_digest), &baseline_png_bytes).unwrap();

    // 3. Write corrupt actual screenshot file
    fs::write(&screenshot_file, b"not a png").unwrap();
    let report_corrupt_actual = run_diff(&ctx, &DiffOptions::default()).unwrap();
    assert_ne!(report_corrupt_actual.failed_tests, 0);
    let corrupt_actual = case_report(base_path, "billing/form");
    assert_eq!(corrupt_actual.outcome, CaseOutcome::Error);
    assert!(
        corrupt_actual
            .message
            .unwrap()
            .starts_with("candidate image")
    );
    let md = fs::read_to_string(report_corrupt_actual.runs_dir.join("report.md")).unwrap();
    assert!(
        md.contains("| billing/form | billing/form.png | ❌ Error |"),
        "{md}"
    );
}

#[test]
fn test_diff_with_mask_rules_ignores_masked_differences() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let ctx_init = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();
    init_workspace(&ctx_init).expect("init_workspace should succeed");

    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let baseline_png_bytes = fs::read(fixtures_dir.join("baseline_gradient_100x100.png")).unwrap();

    let screenshot_dir = base_path.join("masked_app");
    fs::create_dir_all(&screenshot_dir).unwrap();
    let screenshot_file = screenshot_dir.join("screen.png");
    fs::write(&screenshot_file, &baseline_png_bytes).unwrap();

    // Config mask covering pixel (50, 50)
    common::copy_config(base_path, "masked_center");

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    // 1. Stage baseline (applies mask to baseline blob and saves it)
    stage_workspace(&ctx, None).expect("stage_workspace should succeed");

    // 2. Replace actual screenshot with image modified ONLY at (50, 50)
    let modified_png_bytes =
        fs::read(fixtures_dir.join("diff_1px_black_center_100x100.png")).unwrap();
    fs::write(&screenshot_file, &modified_png_bytes).unwrap();

    // 3. Run diff -> Mask on actual screenshot masks out the modified pixel (50, 50),
    // baseline is already masked. Comparison must PASS!
    let report = run_diff(&ctx, &DiffOptions::default()).expect("run_diff should succeed");
    assert_eq!(report.failed_tests, 0);
    assert_eq!(report.total_tests, 1);
}

#[test]
fn test_diff_baseline_staged_before_mask_configuration() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let ctx_init = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();
    init_workspace(&ctx_init).expect("init_workspace should succeed");

    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let baseline_png_bytes = fs::read(fixtures_dir.join("baseline_gradient_100x100.png")).unwrap();

    let screenshot_dir = base_path.join("unmasked_app");
    fs::create_dir_all(&screenshot_dir).unwrap();
    let screenshot_file = screenshot_dir.join("screen.png");
    fs::write(&screenshot_file, &baseline_png_bytes).unwrap();

    // 1. Initial config WITHOUT masks
    common::copy_config(base_path, "unmasked");

    let options = ContextOptions::default();
    let ctx = ResolvedContext::from_options(&options, base_path).unwrap();

    // 2. Stage baseline BEFORE configuring mask (blob on disk is UNMASKED)
    stage_workspace(&ctx, None).expect("stage_workspace should succeed");

    // 3. Update config AFTER staging to ADD mask covering pixel (50, 50)
    common::copy_config(base_path, "unmasked_then_masked");

    // 4. Modify actual screenshot at (50, 50)
    let modified_png_bytes =
        fs::read(fixtures_dir.join("diff_1px_black_center_100x100.png")).unwrap();
    fs::write(&screenshot_file, &modified_png_bytes).unwrap();

    // Re-resolve context with updated config
    let ctx_masked = ResolvedContext::from_options(&options, base_path).unwrap();

    // 5. Run diff -> baseline blob on disk was unmasked, but run_diff applies the new mask
    // to BOTH baseline_rgba and actual_rgba on the fly. Comparison MUST PASS!
    let report = run_diff(&ctx_masked, &DiffOptions::default()).expect("run_diff should succeed");
    assert_eq!(report.failed_tests, 0);
    assert_eq!(report.total_tests, 1);
}

#[test]
fn test_diff_fallback_platform_integration() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");

    // 1. Setup gleon.yaml with fallback_platform
    common::copy_config(base_path, "fallback");

    let ctx_init = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();
    init_workspace(&ctx_init).unwrap();

    // 2. Add real screenshot fixture
    let baseline_png_bytes = fs::read(fixtures_dir.join("baseline_100x100.png")).unwrap();

    let screenshot_dir = base_path.join("billing");
    fs::create_dir_all(&screenshot_dir).unwrap();
    fs::write(screenshot_dir.join("form.png"), &baseline_png_bytes).unwrap();

    // 3. Stage screenshot specifically on windows-x86_64 (fallback platform)
    let options_windows = ContextOptions {
        os: Some("windows".to_string()),
        arch: Some("x86_64".to_string()),
        ..Default::default()
    };
    let ctx_windows = ResolvedContext::from_options(&options_windows, base_path).unwrap();
    let stage_res = stage_workspace(&ctx_windows, None).unwrap();
    assert_eq!(stage_res.staged_test_cases.len(), 1);

    // 4. Run diff on macos-aarch64 (current platform has NO manifests).
    let options_macos = ContextOptions {
        os: Some("macos".to_string()),
        arch: Some("aarch64".to_string()),
        ..Default::default()
    };
    struct EmptyEnv;
    impl gleon_core::env::EnvProvider for EmptyEnv {
        fn get_var(&self, _key: &str) -> Option<String> {
            None
        }
    }

    let ctx_macos = ResolvedContext::resolve(&options_macos, base_path, &EmptyEnv).unwrap();
    assert_eq!(
        ctx_macos.fallback_platform_key.clone().unwrap(),
        "windows-x86_64"
    );

    let diff_res = run_diff(&ctx_macos, &DiffOptions::default()).unwrap();
    assert_eq!(diff_res.failed_tests, 0);
    assert_eq!(diff_res.total_tests, 1);
}

#[test]
fn test_diff_nested_test_name_directory_creation() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let ctx_init = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();
    init_workspace(&ctx_init).expect("init_workspace should succeed");

    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let baseline_bytes = fs::read(fixtures_dir.join("baseline_100x100.png")).unwrap();
    let diff_bytes = fs::read(fixtures_dir.join("diff_16px_corners_100x100.png")).unwrap();

    let nested_dir = base_path.join("auth").join("login");
    fs::create_dir_all(&nested_dir).unwrap();
    let screenshot_file = nested_dir.join("form.png");
    fs::write(&screenshot_file, &baseline_bytes).unwrap();

    common::copy_config(base_path, "auth_login");

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    // Stage baseline
    stage_workspace(&ctx, None).unwrap();

    // Replace with diff image
    fs::write(&screenshot_file, &diff_bytes).unwrap();

    // Run diff -> keeps the images under the nested test name
    let report = run_diff(&ctx, &DiffOptions::default()).unwrap();
    assert_ne!(report.failed_tests, 0);
    assert_eq!(report.failed_tests, 1);

    let expected_diff_file = report
        .runs_dir
        .join("artifacts")
        .join(PlatformKey::host())
        .join("auth/login/form/diff.png");
    assert!(
        expected_diff_file.is_file(),
        "Diff image must be created at {expected_diff_file:?}"
    );
}

#[test]
fn test_diff_missing_baseline_saved_and_approved() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let ctx_init = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    // 1. Init workspace
    init_workspace(&ctx_init).expect("init_workspace should succeed");

    // 2. Add an actual image WITHOUT running stage_workspace first, meaning no baseline
    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let actual_png_bytes = fs::read(fixtures_dir.join("baseline_100x100.png"))
        .expect("baseline_100x100.png fixture must exist");

    let screenshot_dir = base_path.join("billing");
    fs::create_dir_all(&screenshot_dir).unwrap();
    let screenshot_file = screenshot_dir.join("form.png");
    fs::write(&screenshot_file, &actual_png_bytes).unwrap();

    common::copy_config(base_path, "billing");

    let options = ContextOptions {
        branch: Some("main".to_string()),
        target_branch: "main".to_string(),
        ..Default::default()
    };

    let ctx = ResolvedContext::from_options(&options, base_path).unwrap();

    // 3. Run diff -> should fail as missing, BUT should keep the candidate
    let report = run_diff(&ctx, &DiffOptions::default()).expect("run_diff should succeed");
    assert_ne!(report.failed_tests, 0);
    assert_eq!(report.total_tests, 1);
    assert_eq!(report.failed_tests, 1);

    // Verify the candidate was kept in the artifacts directory
    let host = PlatformKey::host();
    let expected_actual_file = report
        .runs_dir
        .join("artifacts")
        .join(host)
        .join("billing/form/candidate.png");
    assert!(
        expected_actual_file.is_file(),
        "The candidate must be kept for a missing baseline at {expected_actual_file:?}"
    );
    let case = case_report(base_path, "billing/form");
    assert_eq!(case.outcome, CaseOutcome::Missing);
    assert_eq!(
        case.artifacts.unwrap().candidate,
        Some(format!(
            ".gleon/runs/latest/artifacts/{host}/billing/form/candidate.png"
        ))
    );

    // Verify the content is exactly the same as the original PNG
    let saved_actual_bytes = fs::read(&expected_actual_file).unwrap();
    assert_eq!(saved_actual_bytes, actual_png_bytes);
}

/// The pixel options of a rule reach the engine through `gleon diff`: real PNGs of an edge whose
/// anti-aliasing another rasterizer drew differently fail exactly and pass with `anti_alias`,
/// and the case report counts the tolerated pixels.
#[test]
fn test_diff_applies_the_pixel_options_of_a_rule() {
    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), root).unwrap();
    init_workspace(&ctx).unwrap();
    common::copy_config(root, "pixel_options");
    // One anti-aliased column moves from 128 to 100 on a black-to-white edge.
    let dirs = ["strict", "aa", "channel", "short", "edges"];
    for dir in dirs {
        fs::create_dir_all(root.join(dir)).unwrap();
        fs::copy(
            fixtures.join("aa_edge_baseline_10x10.png"),
            root.join(dir).join("edge.png"),
        )
        .unwrap();
    }
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), root).unwrap();
    stage_workspace(&ctx, None).unwrap();
    for dir in dirs {
        fs::copy(
            fixtures.join("aa_edge_actual_10x10.png"),
            root.join(dir).join("edge.png"),
        )
        .unwrap();
    }

    let result = run_diff(&ctx, &DiffOptions::default()).unwrap();
    assert_eq!((result.total_tests, result.failed_tests), (5, 2));

    for failing in ["strict", "short"] {
        let report = case_report(root, &format!("{failing}/edge"));
        assert_eq!(report.outcome, CaseOutcome::Mismatch, "{failing}");
    }
    for (passing, tolerated, edges) in [("aa", 10, 0), ("channel", 10, 0), ("edges", 0, 10)] {
        let report = case_report(root, &format!("{passing}/edge"));
        assert_eq!(report.outcome, CaseOutcome::Match, "{passing}");
        assert!(
            matches!(
                report.metrics,
                Some(gleon_core::case::Metrics::Pixel {
                    diff_pixels: 0,
                    tolerated_pixels,
                    edge_pixels,
                    ..
                }) if (tolerated_pixels, edge_pixels) == (tolerated, edges)
            ),
            "{passing}: {:?}",
            report.metrics
        );
    }
    assert!(matches!(
        case_report(root, "edges/edge").comparison.tolerance,
        gleon_model::tolerance::Tolerance::Pixel {
            edge_threshold: 64,
            ..
        }
    ));
}

/// A screenshot of another size than its baseline keeps a diff of both sizes beside the two
/// images.
#[test]
fn test_diff_keeps_a_diff_of_both_sizes_for_a_dimension_mismatch() {
    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), root).unwrap();
    init_workspace(&ctx).unwrap();
    common::copy_config(root, "pixel_options");
    fs::create_dir_all(root.join("size")).unwrap();
    fs::copy(
        fixtures.join("diff_1px_black_center_100x100.png"),
        root.join("size/size.png"),
    )
    .unwrap();
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), root).unwrap();
    stage_workspace(&ctx, None).unwrap();
    fs::copy(fixtures.join("200x100.png"), root.join("size/size.png")).unwrap();

    run_diff(&ctx, &DiffOptions::default()).unwrap();
    let report = case_report(root, "size/size");
    assert_eq!(report.outcome, CaseOutcome::DimensionMismatch);
    let diff = report
        .artifacts
        .and_then(|artifacts| artifacts.diff)
        .expect("a diff of both sizes");
    let diff = image::open(root.join(diff)).unwrap().to_rgba8();
    assert_eq!(diff.dimensions(), (200, 100));
    // The overlap differs everywhere but under the mask, which keeps the darkened golden.
    let golden = image::open(fixtures.join("diff_1px_black_center_100x100.png"))
        .unwrap()
        .to_rgba8();
    let darkened = |[r, g, b, a]: [u8; 4]| [r / 2, g / 2, b / 2, a];
    assert_eq!(diff.get_pixel(5, 5).0, darkened(golden.get_pixel(5, 5).0));
    assert_eq!(diff.get_pixel(50, 50).0, [255, 0, 255, 255]);
}
