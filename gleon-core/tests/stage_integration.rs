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
    manifest::ManifestError,
    ops::{StageError, check_status, common::CoreError, init_workspace, stage_workspace},
};
use gleon_model::compare::CompareError;
use sha2::Digest;

#[test]
fn test_stage_uninitialized_fails() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let options = ContextOptions {
        branch: Some("main".to_string()),
        ..Default::default()
    };

    let ctx = ResolvedContext::from_options(&options, base_path).unwrap();
    let result = stage_workspace(&ctx, None);

    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        StageError::Core(CoreError::NotInitialized)
    ));
}

#[test]
fn test_stage_real_fixture_updates_index_and_makes_workspace_clean() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let ctx_init = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    // 1. Init workspace
    init_workspace(&ctx_init, None).expect("init_workspace should succeed");

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

    common::copy_config(base_path, "billing");

    let options = ContextOptions {
        branch: Some("main".to_string()),
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
    init_workspace(&ctx_init, None).expect("init_workspace should succeed");

    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let real_png_bytes =
        fs::read(fixtures_dir.join("200x100.png")).expect("200x100.png fixture must exist");

    let screenshot_dir = base_path.join("billing");
    fs::create_dir_all(&screenshot_dir).unwrap();
    fs::write(screenshot_dir.join("form1.png"), &real_png_bytes).unwrap();
    fs::write(screenshot_dir.join("form2.png"), &real_png_bytes).unwrap();

    common::copy_config(base_path, "billing");

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
    init_workspace(&ctx_init, None).expect("init_workspace should succeed");

    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let real_png_bytes =
        fs::read(fixtures_dir.join("200x100.png")).expect("200x100.png fixture must exist");

    let screenshot_dir = base_path.join("billing");
    fs::create_dir_all(&screenshot_dir).unwrap();
    fs::write(screenshot_dir.join("form.png"), &real_png_bytes).unwrap();

    common::copy_config(base_path, "billing");

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
    init_workspace(&ctx_init, None).expect("init_workspace should succeed");

    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let real_png_bytes =
        fs::read(fixtures_dir.join("200x100.png")).expect("200x100.png fixture must exist");

    let screenshot_dir = base_path.join("billing");
    fs::create_dir_all(&screenshot_dir).unwrap();
    fs::write(screenshot_dir.join("form1.png"), &real_png_bytes).unwrap();
    fs::write(screenshot_dir.join("form2.png"), &real_png_bytes).unwrap();

    common::copy_config(base_path, "billing");

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
    init_workspace(&ctx_init, None).expect("init_workspace should succeed");

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

    common::copy_config(base_path, "billing");

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
    init_workspace(&ctx_init, None).expect("init_workspace should succeed");

    let screenshot_dir = base_path.join("billing");
    fs::create_dir_all(&screenshot_dir).unwrap();
    fs::write(screenshot_dir.join("corrupt.png"), b"not a valid png").unwrap();

    common::copy_config(base_path, "billing");

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    let result = stage_workspace(&ctx, None);
    assert!(result.is_err());
    assert!(matches!(result, Err(StageError::ImageDecode { .. })));
}

/// A screenshot over the SSIM analysis budget can never be compared under `mode: ssim` (`gleon
/// diff` and `gleon status` fail it), so stage refuses it in the words of the comparison. Exact
/// and pixel rules have no such budget (`gleon_model::compare::check_size`). Every screenshot is
/// checked before anything is written: the good one gets no blob either.
#[test]
fn test_stage_refuses_an_ssim_screenshot_over_the_analysis_budget() {
    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path();
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), root).unwrap();
    init_workspace(&ctx, None).unwrap();
    // `ssim/*.png` under SSIM, `shots/*.png` under pixel.
    common::copy_config(root, "ssim_and_exact");
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for (file, fixture) in [
        ("ssim/big.png", "ssim_budget_4097x4096.png"),
        ("shots/ok.png", "baseline_100x100.png"),
    ] {
        fs::create_dir_all(root.join(file).parent().unwrap()).unwrap();
        fs::copy(fixtures.join(fixture), root.join(file)).unwrap();
    }
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), root).unwrap();

    let err = stage_workspace(&ctx, None).unwrap_err();
    assert!(
        matches!(
            err,
            StageError::Uncomparable {
                source: CompareError::TooLarge {
                    width: 4097,
                    height: 4096,
                },
                ..
            }
        ),
        "{err}"
    );
    assert!(
        err.to_string().starts_with(
            "'ssim/big.png' cannot be compared under its rule: 4097x4096 exceeds the SSIM \
             analysis budget"
        ),
        "{err}"
    );
    let manifests = root
        .join(".gleon/manifests")
        .join(ctx.platform.key().unwrap());
    assert!(!manifests.join("ssim/big.json").exists());
    assert!(!manifests.join("shots/ok.json").exists());
    assert_eq!(
        fs::read_dir(root.join(".gleon/blobs/sha256"))
            .unwrap()
            .count(),
        0
    );
}

/// A screenshot over the decoder budget (16384x4097, more pixels than allowed although each side
/// fits) is refused from its header, before it is decoded, naming the limit.
#[test]
fn test_stage_refuses_a_screenshot_over_the_decoder_budget() {
    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path();
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), root).unwrap();
    init_workspace(&ctx, None).unwrap();
    common::copy_config(root, "billing");
    fs::create_dir_all(root.join("billing")).unwrap();
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/decoder_budget_16384x4097.png"),
        root.join("billing/wide.png"),
    )
    .unwrap();
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), root).unwrap();

    let err = stage_workspace(&ctx, None).unwrap_err();
    assert!(
        matches!(
            err,
            StageError::Core(CoreError::Manifest(ManifestError::Validation(_)))
        ),
        "{err}"
    );
    assert!(
        err.to_string()
            .contains("Total pixel count 67125248 (16384x4097) exceeds"),
        "{err}"
    );
}

/// A blob that is a symbolic link, or a file holding other bytes than its name says, is no blob
/// to keep: staging writes the bytes of the screenshot in its place.
#[cfg(unix)]
#[test]
fn test_stage_replaces_blobs_it_cannot_trust() {
    let temp_dir = tempfile::tempdir().unwrap();
    let root = temp_dir.path();
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), root).unwrap();
    init_workspace(&ctx, None).unwrap();
    common::copy_config(root, "billing");
    fs::create_dir_all(root.join("billing")).unwrap();
    let png =
        fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/200x100.png")).unwrap();
    fs::write(root.join("billing/form.png"), &png).unwrap();
    let blobs = root.join(".gleon/blobs/sha256");
    fs::create_dir_all(&blobs).unwrap();
    let elsewhere = root.join("elsewhere.png");
    fs::write(&elsewhere, &png).unwrap();
    let blob = blobs.join(hex::encode(sha2::Sha256::digest(&png)));
    std::os::unix::fs::symlink(&elsewhere, &blob).unwrap();
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), root).unwrap();

    stage_workspace(&ctx, None).unwrap();
    let meta = fs::symlink_metadata(&blob).unwrap();
    assert!(meta.is_file() && !meta.file_type().is_symlink());
    assert_eq!(fs::read(&blob).unwrap(), png);

    // A changed cache: the blob's name no longer names its bytes. Staging the unchanged
    // screenshot again writes them back, as `gleon diff` and `gleon push` refuse such a blob.
    fs::write(&blob, b"not the screenshot").unwrap();
    stage_workspace(&ctx, None).unwrap();
    assert_eq!(fs::read(&blob).unwrap(), png);
}

/// A goldens workspace never gets blobs or manifests.
#[test]
fn test_stage_refuses_a_goldens_workspace() {
    let temp_dir = tempfile::tempdir().unwrap();
    common::goldens_workspace(temp_dir.path());
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), temp_dir.path()).unwrap();
    assert!(matches!(
        stage_workspace(&ctx, None).unwrap_err(),
        StageError::Core(CoreError::BaselinesAreGoldens)
    ));
    assert!(!temp_dir.path().join(".gleon/blobs").exists());
    assert!(!temp_dir.path().join(".gleon/manifests").exists());
}
