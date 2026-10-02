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

use std::{fs, path::Path};

use gleon_core::{
    context::{ContextOptions, ResolvedContext},
    ops::{StatusError, check_status, init_workspace, stage_workspace},
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

    init_workspace(&ctx_init).expect("init_workspace should succeed");

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
    let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include: "billing/**/*.png"
"#;
    fs::create_dir_all(base_path.join(".gleon")).unwrap();
    fs::write(base_path.join(".gleon").join("gleon.yaml"), config_yaml).unwrap();

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
    init_workspace(&ctx_init).expect("init_workspace should succeed");

    let nested_dir = root_dir.join("src").join("billing");
    fs::create_dir_all(&nested_dir).unwrap();

    // Resolving from nested_dir discovers gleon.yaml in root_dir and sets ctx.base_dir = root_dir
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), &nested_dir).unwrap();
    assert_eq!(ctx.base_dir, root_dir);

    let report = check_status(&ctx).expect("check_status should succeed when using ctx.base_dir");
    assert!(report.is_clean());
}

#[test]
fn test_status_with_mask_rules_is_clean_after_staging() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let ctx_init = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();
    init_workspace(&ctx_init).expect("init_workspace should succeed");

    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let real_png_bytes =
        fs::read(fixtures_dir.join("200x100.png")).expect("200x100.png fixture must exist");

    let screenshot_dir = base_path.join("masked_app");
    fs::create_dir_all(&screenshot_dir).unwrap();
    fs::write(screenshot_dir.join("screen.png"), real_png_bytes).unwrap();

    let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include: "masked_app/**/*.png"
    masks:
      - path: "**/*.png"
        zones:
          - x: 0
            y: 0
            width: 50
            height: 50
"#;
    fs::create_dir_all(base_path.join(".gleon")).unwrap();
    fs::write(base_path.join(".gleon").join("gleon.yaml"), config_yaml).unwrap();

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
    init_workspace(&ctx_init).expect("init_workspace should succeed");

    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let real_png_bytes =
        fs::read(fixtures_dir.join("200x100.png")).expect("200x100.png fixture must exist");

    let screenshot_dir = base_path.join("billing");
    fs::create_dir_all(&screenshot_dir).unwrap();
    let screenshot_file = screenshot_dir.join("form.png");
    fs::write(&screenshot_file, &real_png_bytes).unwrap();

    let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include: "billing/**/*.png"
"#;
    fs::create_dir_all(base_path.join(".gleon")).unwrap();
    fs::write(base_path.join(".gleon").join("gleon.yaml"), config_yaml).unwrap();

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    // Stage the baseline
    stage_workspace(&ctx, None).expect("stage_workspace should succeed");

    // Modify the screenshot
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
    init_workspace(&ctx_init).expect("init_workspace should succeed");

    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let real_png_bytes =
        fs::read(fixtures_dir.join("200x100.png")).expect("200x100.png fixture must exist");

    let screenshot_dir = base_path.join("billing");
    fs::create_dir_all(&screenshot_dir).unwrap();
    let screenshot_file = screenshot_dir.join("form.png");
    fs::write(&screenshot_file, &real_png_bytes).unwrap();

    let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include: "billing/**/*.png"
"#;
    fs::create_dir_all(base_path.join(".gleon")).unwrap();
    fs::write(base_path.join(".gleon").join("gleon.yaml"), config_yaml).unwrap();

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
    fs::create_dir_all(base_path.join(".gleon")).unwrap();
    fs::copy(
        fixtures_dir.join("fallback_config.yaml"),
        base_path.join(".gleon").join("gleon.yaml"),
    )
    .unwrap();

    let ctx_init = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();
    init_workspace(&ctx_init).unwrap();

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
fn test_status_missing_baseline_blob_returns_modified() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let ctx_init = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();
    init_workspace(&ctx_init).expect("init_workspace should succeed");

    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let baseline_bytes = fs::read(fixtures_dir.join("baseline_100x100.png")).unwrap();

    let screenshot_dir = base_path.join("masked_app");
    fs::create_dir_all(&screenshot_dir).unwrap();
    let screenshot_file = screenshot_dir.join("form.png");
    fs::write(&screenshot_file, &baseline_bytes).unwrap();

    let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include: "masked_app/*.png"
    masks:
      - path: "**/*.png"
        zones:
          - x: 0
            y: 0
            width: 10
            height: 10
"#;
    fs::write(base_path.join(".gleon").join("gleon.yaml"), config_yaml).unwrap();

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

    // check_status should NOT fail with StatusError::Io, but return modified
    let res = check_status(&ctx).expect("check_status should succeed even if blob missing");
    assert_eq!(res.modified.len(), 1);
}

#[test]
fn test_status_with_corrupt_image() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let ctx_init = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    init_workspace(&ctx_init).unwrap();

    let screenshot_dir = base_path.join("billing");
    fs::create_dir_all(&screenshot_dir).unwrap();
    let screenshot_file = screenshot_dir.join("form.png");

    // 1. Write a real image first so we can stage it
    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let real_png_bytes = fs::read(fixtures_dir.join("200x100.png")).unwrap();
    fs::write(&screenshot_file, &real_png_bytes).unwrap();

    let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include: "billing/**/*.png"
    masks:
      - path: "**/*.png"
        zones:
          - x: 0
            y: 0
            width: 100
            height: 50
"#;
    fs::create_dir_all(base_path.join(".gleon")).unwrap();
    fs::write(base_path.join(".gleon").join("gleon.yaml"), config_yaml).unwrap();

    let ctx_stage = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();
    stage_workspace(&ctx_stage, None).unwrap();

    // 2. Corrupt the image (write text instead of PNG)
    fs::write(&screenshot_file, "this is not a valid png file").unwrap();

    let ctx_status = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    // 3. Status should fail when trying to decode the corrupted image to apply masks
    // Wait, check_status first checks bytes, and since bytes changed, it reads the baseline to apply masks (even if no masks are defined, it loads it to do a pixel comparison in case it's visually identical).
    let result = check_status(&ctx_status);
    assert!(result.is_err());

    assert!(matches!(
        result,
        Err(StatusError::Core(gleon_core::ops::common::CoreError::Io(ref err)))
            if err.kind() == std::io::ErrorKind::InvalidData
    ));
}
