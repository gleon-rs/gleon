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
    context::{ContextOptions, ResolvedContext},
    ops::{
        Explained, StatusError, StatusReport, check_status, diff::DiffOptions, init_workspace,
        run_diff, stage_workspace,
    },
};

#[test]
fn test_status_uninitialized_fails() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();
    let result = check_status(&ctx);

    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        StatusError::Core(gleon_core::ops::common::CoreError::NotInitialized)
    ));
}

#[test]
fn test_status_fresh_workspace_reports_added_with_real_fixture() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    // 1. Initialize workspace
    let ctx_init = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    init_workspace(&ctx_init, None).expect("init_workspace should succeed");

    // 2. Copy real fixture file to base_path/billing/form.png
    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let real_png_bytes =
        fs::read(fixtures_dir.join("200x100.png")).expect("200x100.png fixture must exist");

    let screenshot_dir = base_path.join("billing");
    fs::create_dir_all(&screenshot_dir).unwrap();
    let screenshot_file = screenshot_dir.join("form.png");
    fs::write(&screenshot_file, real_png_bytes).unwrap();

    // 3. Write custom config targeting billing/**/*.png
    common::copy_config(base_path, "billing");

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();
    let report = check_status(&ctx).expect("check_status should succeed");

    assert!(!report.is_clean());
    assert_eq!(report.added.len(), 1);
    assert_eq!(report.added[0], Path::new("billing/form.png"));
    assert!(report.modified.is_empty());
    assert!(report.deleted.is_empty());

    let text_output = report.format_text();
    assert!(text_output.contains("Added:\n  billing/form.png"));
}

#[test]
fn test_status_from_nested_subdirectory() {
    let temp_dir = tempfile::tempdir().unwrap();
    let root_dir = temp_dir.path();

    let ctx_init = ResolvedContext::from_options(&ContextOptions::default(), root_dir).unwrap();
    init_workspace(&ctx_init, None).expect("init_workspace should succeed");

    let nested_dir = root_dir.join("src").join("billing");
    fs::create_dir_all(&nested_dir).unwrap();

    // Resolving from nested_dir discovers gleon.yaml in root_dir and sets ctx.base_dir = root_dir
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), &nested_dir).unwrap();
    assert_eq!(ctx.base_dir, gleon_model::fs::canonical(root_dir).unwrap());

    let report = check_status(&ctx).expect("check_status should succeed when using ctx.base_dir");
    assert!(report.is_clean());
}

#[test]
fn test_status_with_mask_rules_is_clean_after_staging() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let ctx_init = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();
    init_workspace(&ctx_init, None).expect("init_workspace should succeed");

    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let real_png_bytes =
        fs::read(fixtures_dir.join("200x100.png")).expect("200x100.png fixture must exist");

    let screenshot_dir = base_path.join("masked_app");
    fs::create_dir_all(&screenshot_dir).unwrap();
    fs::write(screenshot_dir.join("screen.png"), real_png_bytes).unwrap();

    common::copy_config(base_path, "masked_quarter");

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    // Stage screenshot
    stage_workspace(&ctx, None).expect("stage_workspace should succeed");

    // Check status post-staging in Phase 3.3: status is clean
    let report = check_status(&ctx).expect("check_status should succeed");
    assert!(report.is_clean());
}

#[test]
fn test_status_reports_modified() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let ctx_init = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();
    init_workspace(&ctx_init, None).expect("init_workspace should succeed");

    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let real_png_bytes =
        fs::read(fixtures_dir.join("200x100.png")).expect("200x100.png fixture must exist");

    let screenshot_dir = base_path.join("billing");
    fs::create_dir_all(&screenshot_dir).unwrap();
    let screenshot_file = screenshot_dir.join("form.png");
    fs::write(&screenshot_file, &real_png_bytes).unwrap();

    common::copy_config(base_path, "billing");

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    // Stage the baseline
    stage_workspace(&ctx, None).expect("stage_workspace should succeed");

    // Another size: a dimension mismatch fails every rule.
    let modified_png_bytes = fs::read(fixtures_dir.join("baseline_100x100.png"))
        .expect("baseline_100x100.png fixture must exist");
    fs::write(&screenshot_file, &modified_png_bytes).unwrap();

    let report = check_status(&ctx).expect("check_status should succeed");

    assert!(!report.is_clean());
    assert!(report.added.is_empty());
    assert!(report.deleted.is_empty());
    assert_eq!(report.modified.len(), 1);
    assert_eq!(report.modified[0], Path::new("billing/form.png"));
}

#[test]
fn test_status_reports_deleted() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let ctx_init = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();
    init_workspace(&ctx_init, None).expect("init_workspace should succeed");

    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let real_png_bytes =
        fs::read(fixtures_dir.join("200x100.png")).expect("200x100.png fixture must exist");

    let screenshot_dir = base_path.join("billing");
    fs::create_dir_all(&screenshot_dir).unwrap();
    let screenshot_file = screenshot_dir.join("form.png");
    fs::write(&screenshot_file, &real_png_bytes).unwrap();

    common::copy_config(base_path, "billing");

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    // Stage the baseline
    stage_workspace(&ctx, None).expect("stage_workspace should succeed");

    // Delete the screenshot
    fs::remove_file(&screenshot_file).unwrap();

    let report = check_status(&ctx).expect("check_status should succeed");

    assert!(!report.is_clean());
    assert!(report.added.is_empty());
    assert!(report.modified.is_empty());
    assert_eq!(report.deleted.len(), 1);
    assert_eq!(report.deleted[0], Path::new("billing/form.png"));
}

#[test]
fn test_status_fallback_platform_integration() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");

    // 1. Setup gleon.yaml with fallback_platform
    common::copy_config(base_path, "fallback");

    let ctx_init = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();
    init_workspace(&ctx_init, None).unwrap();

    // 2. Add real screenshot fixture
    let baseline_png_bytes = fs::read(fixtures_dir.join("200x100.png")).unwrap();

    let screenshot_dir = base_path.join("billing");
    fs::create_dir_all(&screenshot_dir).unwrap();
    fs::write(screenshot_dir.join("form.png"), &baseline_png_bytes).unwrap();

    // 3. Stage screenshot specifically on windows-x86_64 (fallback platform)
    let options_stage_windows = ContextOptions {
        os: Some("windows".to_string()),
        arch: Some("x86_64".to_string()),
        ..Default::default()
    };
    let ctx_windows = ResolvedContext::from_options(&options_stage_windows, base_path).unwrap();
    let stage_res = stage_workspace(&ctx_windows, None).unwrap();
    assert_eq!(stage_res.staged_test_cases.len(), 1);

    // 4. Run status on macos-aarch64 (current platform has NO manifests).
    let options_status_macos = ContextOptions {
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

    let ctx_macos = ResolvedContext::resolve(&options_status_macos, base_path, &EmptyEnv).unwrap();

    let status_res = check_status(&ctx_macos).unwrap();
    assert!(status_res.is_clean());
}

#[test]
fn test_status_missing_baseline_blob_is_uncomparable() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let ctx_init = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();
    init_workspace(&ctx_init, None).expect("init_workspace should succeed");

    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let baseline_bytes = fs::read(fixtures_dir.join("baseline_100x100.png")).unwrap();

    let screenshot_dir = base_path.join("masked_app");
    fs::create_dir_all(&screenshot_dir).unwrap();
    let screenshot_file = screenshot_dir.join("form.png");
    fs::write(&screenshot_file, &baseline_bytes).unwrap();

    common::copy_config(base_path, "masked_corner");

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    stage_workspace(&ctx, None).unwrap();

    // Modify the screenshot on disk so it triggers mask comparison
    let diff_bytes = fs::read(fixtures_dir.join("diff_16px_corners_100x100.png")).unwrap();
    fs::write(&screenshot_file, &diff_bytes).unwrap();

    // Remove the blob file from .gleon/blobs/sha256/
    let blobs_dir = base_path.join(".gleon").join("blobs").join("sha256");
    if let Ok(entries) = fs::read_dir(&blobs_dir) {
        for entry in entries.flatten() {
            let _ = fs::remove_file(entry.path());
        }
    }

    // check_status should NOT fail with StatusError::Io, but name the missing blob
    let res = check_status(&ctx).expect("check_status should succeed even if blob missing");
    assert!(res.modified.is_empty(), "{res:?}");
    assert!(
        matches!(
            res.uncomparable.as_slice(),
            [Explained { path, reason }]
                if path == Path::new("masked_app/form.png") && reason.contains("gleon pull")
        ),
        "{res:?}"
    );
}

#[test]
fn test_status_with_corrupt_image() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let ctx_init = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    init_workspace(&ctx_init, None).unwrap();

    let screenshot_dir = base_path.join("billing");
    fs::create_dir_all(&screenshot_dir).unwrap();
    let screenshot_file = screenshot_dir.join("form.png");

    // 1. Write a real image first so we can stage it
    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let real_png_bytes = fs::read(fixtures_dir.join("200x100.png")).unwrap();
    fs::write(&screenshot_file, &real_png_bytes).unwrap();

    common::copy_config(base_path, "billing_masked_top");

    let ctx_stage = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();
    stage_workspace(&ctx_stage, None).unwrap();

    // 2. Corrupt the image (write text instead of PNG)
    fs::write(&screenshot_file, "this is not a valid png file").unwrap();

    let ctx_status = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    // 3. The bytes changed, so status compares them with the baseline under the rule: a
    // screenshot that cannot be decoded is a failed case in `gleon diff`, so it is listed with
    // the reason, never a pass and never an error of the whole status.
    let report = check_status(&ctx_status).unwrap();
    assert!(
        matches!(
            report.uncomparable.as_slice(),
            [Explained { path, reason }]
                if path == Path::new("billing/form.png") && reason.starts_with("candidate image: ")
        ),
        "{report:?}"
    );
    assert!(report.modified.is_empty() && report.within_tolerance.is_empty());
}

/// `gleon status` judges a changed screenshot like `gleon diff`: the pixel options of its rule
/// let an anti-aliased edge drawn by another rasterizer pass (mirror of
/// `test_diff_applies_the_pixel_options_of_a_rule`).
#[test]
fn test_status_applies_the_pixel_options_of_a_rule() {
    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), root).unwrap();
    init_workspace(&ctx, None).unwrap();
    common::copy_config(root, "pixel_options");
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
    assert!(check_status(&ctx).unwrap().is_clean());
    for dir in dirs {
        fs::copy(
            fixtures.join("aa_edge_actual_10x10.png"),
            root.join(dir).join("edge.png"),
        )
        .unwrap();
    }

    let report = check_status(&ctx).unwrap();
    assert!(report.added.is_empty() && report.deleted.is_empty());
    assert_eq!(
        report.modified,
        vec![Path::new("short/edge.png"), Path::new("strict/edge.png")]
    );
    assert_eq!(
        report.within_tolerance,
        vec![
            Path::new("aa/edge.png"),
            Path::new("channel/edge.png"),
            Path::new("edges/edge.png"),
        ]
    );
}

/// A change within the rule's threshold is no modification, and `gleon diff` passes it too; it
/// is listed as within tolerance, since `gleon stage` would still record its new bytes.
#[test]
fn test_status_passes_a_change_within_the_threshold() {
    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), root).unwrap();
    init_workspace(&ctx, None).unwrap();
    // `threshold: 0.01` for `unmasked_app/**/*.png`.
    common::copy_config(root, "unmasked");
    let screenshot = root.join("unmasked_app").join("screen.png");
    fs::create_dir_all(screenshot.parent().unwrap()).unwrap();
    fs::copy(fixtures.join("baseline_gradient_100x100.png"), &screenshot).unwrap();
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), root).unwrap();
    stage_workspace(&ctx, None).unwrap();

    // One black pixel of 10,000.
    fs::copy(
        fixtures.join("diff_1px_black_center_100x100.png"),
        &screenshot,
    )
    .unwrap();
    assert_ne!(
        fs::read(&screenshot).unwrap(),
        fs::read(fixtures.join("baseline_gradient_100x100.png")).unwrap()
    );

    let report = check_status(&ctx).unwrap();
    assert!(report.modified.is_empty(), "{report:?}");
    assert_eq!(
        report.within_tolerance,
        vec![Path::new("unmasked_app/screen.png")]
    );
    let diff = run_diff(&ctx, &DiffOptions::default()).unwrap();
    assert_eq!((diff.total_tests, diff.failed_tests), (1, 0));
}

#[test]
fn test_status_refuses_a_goldens_workspace() {
    let temp_dir = tempfile::tempdir().unwrap();
    common::goldens_workspace(temp_dir.path());
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), temp_dir.path()).unwrap();
    assert!(matches!(
        check_status(&ctx).unwrap_err(),
        StatusError::Core(gleon_core::ops::common::CoreError::BaselinesAreGoldens)
    ));
}

/// An SSIM rule as `gleon diff` applies it, on real renders: an edge anti-aliased by another
/// rasterizer passes SSIM (but not an exact pixel rule), a changed dashboard fails.
#[test]
fn test_status_applies_an_ssim_rule() {
    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), root).unwrap();
    init_workspace(&ctx, None).unwrap();
    // `ssim/*.png` under SSIM, `shots/*.png` exactly (`threshold: 0`).
    common::copy_config(root, "ssim_and_exact");
    let screenshots = [
        (
            "ssim/edge.png",
            "aa_edge_baseline_10x10.png",
            "aa_edge_actual_10x10.png",
        ),
        (
            "ssim/dashboard.png",
            "dashboard_baseline.png",
            "dashboard_actual.png",
        ),
        (
            "shots/edge.png",
            "aa_edge_baseline_10x10.png",
            "aa_edge_actual_10x10.png",
        ),
    ];
    for (file, baseline, _) in screenshots {
        fs::create_dir_all(root.join(file).parent().unwrap()).unwrap();
        fs::copy(fixtures.join(baseline), root.join(file)).unwrap();
    }
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), root).unwrap();
    stage_workspace(&ctx, None).unwrap();
    for (file, _, actual) in screenshots {
        fs::copy(fixtures.join(actual), root.join(file)).unwrap();
    }

    assert_eq!(
        check_status(&ctx).unwrap(),
        StatusReport {
            modified: vec!["shots/edge.png".into(), "ssim/dashboard.png".into()],
            within_tolerance: vec!["ssim/edge.png".into()],
            ..StatusReport::default()
        }
    );
    let diff = run_diff(&ctx, &DiffOptions::default()).unwrap();
    assert_eq!((diff.total_tests, diff.failed_tests), (3, 2));
}

/// Every reason a tracked screenshot cannot be compared, on real fixtures: listed with the
/// reason, the rest of the status intact, and exactly the cases `gleon diff` fails. A blob
/// missing behind unchanged bytes is no change, only a hint.
#[test]
fn test_status_names_why_screenshots_cannot_be_compared() {
    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), root).unwrap();
    init_workspace(&ctx, None).unwrap();
    // `billing/**/*.png` with `threshold: 0.01`.
    common::copy_config(root, "billing");
    fs::create_dir_all(root.join("billing")).unwrap();
    let staged = [
        ("broken", "baseline_gradient_100x100.png"),
        ("tampered", "baseline_100x100.png"),
        ("unpulled", "diff_16px_corners_100x100.png"),
        ("uncached", "200x100.png"),
    ];
    for (name, fixture) in staged {
        fs::copy(
            fixtures.join(fixture),
            root.join(format!("billing/{name}.png")),
        )
        .unwrap();
    }
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), root).unwrap();
    stage_workspace(&ctx, None).unwrap();
    let blob_of = |fixture: &str| {
        let bytes = fs::read(fixtures.join(fixture)).unwrap();
        root.join(".gleon/blobs/sha256")
            .join(gleon_core::case::Sha256Hex::of(&bytes).as_str())
    };

    // A screenshot that is no PNG.
    fs::copy(
        fixtures.join("corrupt.png"),
        root.join("billing/broken.png"),
    )
    .unwrap();
    // A changed screenshot whose blob holds other bytes than its name says.
    fs::copy(
        fixtures.join("diff_1px_black_center_100x100.png"),
        root.join("billing/tampered.png"),
    )
    .unwrap();
    fs::copy(
        fixtures.join("diff_1px_black_center_100x100.png"),
        blob_of("baseline_100x100.png"),
    )
    .unwrap();
    // A changed screenshot whose blob was never pulled.
    fs::copy(
        fixtures.join("diff_rounded_corners_100x100.png"),
        root.join("billing/unpulled.png"),
    )
    .unwrap();
    fs::remove_file(blob_of("diff_16px_corners_100x100.png")).unwrap();
    // An unchanged screenshot whose blob was never pulled.
    fs::remove_file(blob_of("200x100.png")).unwrap();

    let report = check_status(&ctx).unwrap();
    let reasons: Vec<_> = report
        .uncomparable
        .iter()
        .map(|entry| (entry.path.clone(), entry.reason.clone()))
        .collect();
    assert_eq!(reasons.len(), 3, "{report:?}");
    for ((path, reason), (expected, needle)) in reasons.iter().zip([
        ("billing/broken.png", "candidate image: "),
        ("billing/tampered.png", "does not match its hash"),
        ("billing/unpulled.png", "run `gleon pull`"),
    ]) {
        assert_eq!(path, Path::new(expected));
        assert!(reason.contains(needle), "{expected}: {reason}");
    }
    assert_eq!(
        StatusReport {
            uncomparable: vec![],
            ..report.clone()
        },
        StatusReport {
            uncached: vec!["billing/uncached.png".into()],
            ..StatusReport::default()
        }
    );
    assert!(!report.is_clean());

    // `gleon diff` fails exactly the screenshots status could not compare.
    let diff = run_diff(&ctx, &DiffOptions::default()).unwrap();
    assert_eq!((diff.total_tests, diff.failed_tests), (4, 3));
}

/// Unchanged screenshots whose blob is broken in the local cache (real fixtures): named with the
/// reason, still clean (no manifest to change), and passed by `gleon diff`, which decides them by
/// the manifest's hash.
#[test]
fn test_status_names_unusable_blobs_of_unchanged_screenshots() {
    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), root).unwrap();
    init_workspace(&ctx, None).unwrap();
    // `billing/**/*.png` with `threshold: 0.01`.
    common::copy_config(root, "billing");
    fs::create_dir_all(root.join("billing")).unwrap();
    let staged = [
        ("directory", "baseline_gradient_100x100.png"),
        ("link", "baseline_100x100.png"),
        ("missing", "200x100.png"),
    ];
    for (name, fixture) in staged {
        fs::copy(
            fixtures.join(fixture),
            root.join(format!("billing/{name}.png")),
        )
        .unwrap();
    }
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), root).unwrap();
    stage_workspace(&ctx, None).unwrap();
    let blob_of = |fixture: &str| {
        let bytes = fs::read(fixtures.join(fixture)).unwrap();
        root.join(".gleon/blobs/sha256")
            .join(gleon_core::case::Sha256Hex::of(&bytes).as_str())
    };

    // A directory where a blob belongs: `gleon pull` could not write over it.
    let directory = blob_of("baseline_gradient_100x100.png");
    fs::remove_file(&directory).unwrap();
    fs::create_dir(&directory).unwrap();
    // A symbolic link: never taken for a blob, even to the right bytes.
    let link = blob_of("baseline_100x100.png");
    let linked = root.join("elsewhere.png");
    fs::rename(&link, &linked).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&linked, &link).unwrap();
    #[cfg(not(unix))]
    fs::create_dir(&link).unwrap();
    // A blob that is simply missing: only the hint.
    fs::remove_file(blob_of("200x100.png")).unwrap();

    let report = check_status(&ctx).unwrap();
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.uncached, vec![Path::new("billing/missing.png")]);
    let unusable: Vec<_> = report
        .unusable_blobs
        .iter()
        .map(|entry| (entry.path.as_path(), entry.reason.as_str()))
        .collect();
    assert_eq!(unusable.len(), 2, "{report:?}");
    for ((path, reason), expected) in unusable
        .iter()
        .zip(["billing/directory.png", "billing/link.png"])
    {
        assert_eq!(*path, Path::new(expected));
        assert!(reason.contains("is no regular file: remove it"), "{reason}");
    }

    let diff = run_diff(&ctx, &DiffOptions::default()).unwrap();
    assert_eq!((diff.total_tests, diff.failed_tests), (3, 0));
}
