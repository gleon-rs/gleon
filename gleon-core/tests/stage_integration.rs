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
    ops::{StageError, check_status, init_workspace, stage_workspace},
};
use sha2::Digest;

#[test]
fn test_stage_uninitialized_fails() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let options = ContextOptions {
        branch: Some("main".to_string()),
        target_branch: "main".to_string(),
        ..Default::default()
    };

    let ctx = ResolvedContext::from_options(&options, base_path).unwrap();
    let result = stage_workspace(&ctx, None);

    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        StageError::Core(gleon_core::ops::common::CoreError::NotInitialized)
    ));
}

#[test]
fn test_stage_real_fixture_updates_index_and_makes_workspace_clean() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let ctx_init = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    // 1. Init workspace
    init_workspace(&ctx_init).expect("init_workspace should succeed");

    // 2. Copy real PNG fixture
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

    let options = ContextOptions {
        branch: Some("main".to_string()),
        target_branch: "main".to_string(),
        ..Default::default()
    };

    let ctx = ResolvedContext::from_options(&options, base_path).unwrap();

    // 3. Before staging: status reports 1 Added
    let status_before = check_status(&ctx).unwrap();
    assert_eq!(status_before.added.len(), 1);

    // 4. Stage workspace
    let stage_res = stage_workspace(&ctx, None).expect("stage_workspace should succeed");
    assert_eq!(stage_res.staged_test_cases.len(), 1);
    assert_eq!(stage_res.total_screenshots_staged, 1);

    // 5. Verify exact expected SHA-256 blob file exists under .gleon/blobs/sha256/
    let sha256_hex = hex::encode(sha2::Sha256::digest(&real_png_bytes));
    let expected_blob_path = base_path.join(".gleon/blobs/sha256").join(&sha256_hex);
    assert!(
        expected_blob_path.is_file(),
        "Expected blob file {expected_blob_path:?} does not exist"
    );

    // 6. Verify per-test manifest JSON file was written
    let platform_key = ctx.platform.key().unwrap();
    let expected_manifest_file = base_path
        .join(".gleon/manifests")
        .join(platform_key)
        .join("billing")
        .join("form.json");
    assert!(
        expected_manifest_file.is_file(),
        "Expected per-test manifest {expected_manifest_file:?} does not exist"
    );

    // 7. After staging: status is clean
    let status_after = check_status(&ctx).unwrap();
    assert!(status_after.is_clean());
}

#[test]
fn test_stage_partial_path_filter_preserves_existing_entries() {
    use std::path::PathBuf;

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
    fs::write(screenshot_dir.join("form1.png"), &real_png_bytes).unwrap();
    fs::write(screenshot_dir.join("form2.png"), &real_png_bytes).unwrap();

    let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include: "billing/**/*.png"
"#;
    fs::create_dir_all(base_path.join(".gleon")).unwrap();
    fs::write(base_path.join(".gleon").join("gleon.yaml"), config_yaml).unwrap();

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    // 1. Initial stage: stages form1.png and form2.png
    stage_workspace(&ctx, None).expect("initial stage should succeed");

    // 2. Modify form1.png so that restaging it counts as modified
    let fixtures_dir_100 = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let alt_png_bytes = fs::read(fixtures_dir_100.join("diff_16px_corners_100x100.png"))
        .expect("diff_16px_corners_100x100.png fixture must exist");
    fs::write(screenshot_dir.join("form1.png"), &alt_png_bytes).unwrap();

    // 3. Filtered stage: stage ONLY form1.png
    let filter = vec![PathBuf::from("billing/form1.png")];
    let stage_res = stage_workspace(&ctx, Some(&filter)).expect("filtered stage should succeed");
    assert_eq!(
        stage_res.total_screenshots_staged, 1,
        "Filtered stage should only process matching screenshot paths"
    );
    // Verify CAS blob output invariant: blobs directory retains stored blobs from both form1 and form2
    assert!(base_path.join(".gleon/blobs/sha256").is_dir());
    let blob_count = fs::read_dir(base_path.join(".gleon/blobs/sha256"))
        .unwrap()
        .count();
    assert!(
        blob_count >= 2,
        "CAS blob directory must retain blobs from both form1 and form2"
    );
}

#[test]
fn test_stage_idempotent_unchanged_screenshots() {
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
    fs::write(screenshot_dir.join("form.png"), &real_png_bytes).unwrap();

    let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include: "billing/**/*.png"
"#;
    fs::create_dir_all(base_path.join(".gleon")).unwrap();
    fs::write(base_path.join(".gleon").join("gleon.yaml"), config_yaml).unwrap();

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    // First stage: 1 screenshot staged
    let stage1 = stage_workspace(&ctx, None).unwrap();
    assert_eq!(stage1.total_screenshots_staged, 1);

    // Second stage on unchanged file: 0 screenshots staged
    let stage2 = stage_workspace(&ctx, None).unwrap();
    assert_eq!(stage2.total_screenshots_staged, 0);
}

#[test]
fn test_stage_orphan_manifest_cleanup() {
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
    fs::write(screenshot_dir.join("form1.png"), &real_png_bytes).unwrap();
    fs::write(screenshot_dir.join("form2.png"), &real_png_bytes).unwrap();

    let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include: "billing/**/*.png"
"#;
    fs::create_dir_all(base_path.join(".gleon")).unwrap();
    fs::write(base_path.join(".gleon").join("gleon.yaml"), config_yaml).unwrap();

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    // Initial stage: form1 and form2
    stage_workspace(&ctx, None).expect("initial stage should succeed");

    let platform_key = ctx.platform.key().unwrap();
    let manifest2 = base_path
        .join(".gleon/manifests")
        .join(&platform_key)
        .join("billing")
        .join("form2.json");
    assert!(manifest2.is_file());

    // Delete form2.png from disk
    fs::remove_file(screenshot_dir.join("form2.png")).unwrap();

    // Restage full workspace: form2.json should be removed as an orphan manifest
    stage_workspace(&ctx, None).expect("second stage should succeed");
    assert!(
        !manifest2.exists(),
        "Orphan manifest form2.json should be deleted"
    );
}

#[test]
fn test_stage_absolute_path_filter() {
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
    let form1_abs = screenshot_dir.join("form1.png");
    fs::write(&form1_abs, &real_png_bytes).unwrap();
    fs::write(screenshot_dir.join("form2.png"), &real_png_bytes).unwrap();

    let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include: "billing/**/*.png"
"#;
    fs::create_dir_all(base_path.join(".gleon")).unwrap();
    fs::write(base_path.join(".gleon").join("gleon.yaml"), config_yaml).unwrap();

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    // Stage using absolute path filter for form1.png
    let filter = vec![form1_abs];
    let stage_res = stage_workspace(&ctx, Some(&filter)).unwrap();
    assert_eq!(stage_res.total_screenshots_staged, 1);
    assert_eq!(stage_res.staged_test_cases, vec!["billing/form1"]);
}

#[test]
fn test_stage_corrupt_screenshot_returns_error() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let ctx_init = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();
    init_workspace(&ctx_init).expect("init_workspace should succeed");

    let screenshot_dir = base_path.join("billing");
    fs::create_dir_all(&screenshot_dir).unwrap();
    fs::write(screenshot_dir.join("corrupt.png"), b"not a valid png").unwrap();

    let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include: "billing/**/*.png"
"#;
    fs::create_dir_all(base_path.join(".gleon")).unwrap();
    fs::write(base_path.join(".gleon").join("gleon.yaml"), config_yaml).unwrap();

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    let result = stage_workspace(&ctx, None);
    assert!(result.is_err());
    assert!(matches!(result, Err(StageError::ImageDecode { .. })));
}
