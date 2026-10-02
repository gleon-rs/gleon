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
    ops::{
        ApproveError, approve_workspace, check_status, diff::DiffOptions, init_workspace, run_diff,
        stage_workspace,
    },
};

#[test]
fn test_approve_uninitialized_fails() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();
    let result = approve_workspace(&ctx, &[], &[], None);

    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        ApproveError::Core(gleon_core::ops::common::CoreError::NotInitialized)
    ));
}

#[test]
fn test_approve_full_flow_with_diff_failures() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let ctx_init = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();
    init_workspace(&ctx_init).expect("init_workspace should succeed");

    // 1. Set up baseline screenshot using real static fixtures
    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");

    let screenshot_dir = base_path.join("login");
    fs::create_dir_all(&screenshot_dir).unwrap();
    let screenshot_file = screenshot_dir.join("button.png");
    fs::copy(fixtures_dir.join("baseline_100x100.png"), &screenshot_file).unwrap();

    fs::copy(
        fixtures_dir.join("default_config.yaml"),
        base_path.join(".gleon").join("gleon.yaml"),
    )
    .unwrap();

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    // Stage original baseline
    stage_workspace(&ctx, None).unwrap();
    assert!(check_status(&ctx).unwrap().is_clean());

    // 2. Change the screenshot and run diff -> fails and keeps the candidate as an artifact
    fs::copy(
        fixtures_dir.join("diff_16px_corners_100x100.png"),
        &screenshot_file,
    )
    .unwrap();
    let diff_res = run_diff(&ctx, &DiffOptions::default()).unwrap();
    assert_eq!(diff_res.failed_tests, 1);
    assert!(
        base_path
            .join(".gleon/runs/latest/artifacts/login/button/candidate.png")
            .is_file()
    );

    // 3. Run approve without --from (the case reports of the latest run)
    let approve_res = approve_workspace(&ctx, &[], &[], None).unwrap();
    assert_eq!(approve_res.approved_test_cases, ["login/button"]);

    // 4. Verify status is clean and diff passes!
    assert!(check_status(&ctx).unwrap().is_clean());
    let diff_res_after = run_diff(&ctx, &DiffOptions::default()).unwrap();
    assert_eq!(diff_res_after.failed_tests, 0);
}

/// Approve files baselines under the test names `gleon diff` recorded, which are the scanner's
/// (extension stripped, separators normalized, case folded), so `diff` and `status` find them.
#[test]
fn test_approve_uses_the_test_names_of_the_scanner() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();
    let ctx_init = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();
    init_workspace(&ctx_init).unwrap();
    fs::write(
        base_path.join(".gleon/gleon.yaml"),
        "required_version: \">=0.1.0\"\nscreenshots:\n  - include: \"Auth/**\"\n",
    )
    .unwrap();
    fs::create_dir_all(base_path.join("Auth")).unwrap();
    fs::write(
        base_path.join("Auth/Login.Screen.PNG"),
        include_bytes!("fixtures/baseline_100x100.png"),
    )
    .unwrap();

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();
    let missing = run_diff(&ctx, &DiffOptions::default()).unwrap();
    assert_eq!(missing.failed_tests, 1);

    let res = approve_workspace(&ctx, &[], &[], None).unwrap();
    assert_eq!(res.approved_test_cases, ["auth/login.screen"]);
    let manifest = base_path
        .join(".gleon/manifests")
        .join(ctx.platform.to_key().unwrap())
        .join("auth/login.screen.json");
    assert!(manifest.is_file(), "expected manifest at {manifest:?}");
    assert_eq!(
        run_diff(&ctx, &DiffOptions::default())
            .unwrap()
            .failed_tests,
        0
    );
}

/// A screenshot that is no PNG keeps no candidate, so there is nothing to approve.
#[test]
fn test_approve_skips_candidates_that_are_no_png() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();
    let ctx_init = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();
    init_workspace(&ctx_init).unwrap();
    fs::write(
        base_path.join(".gleon/gleon.yaml"),
        "required_version: \">=0.1.0\"\nscreenshots:\n  - include: \"billing/**/*.png\"\n",
    )
    .unwrap();
    fs::create_dir_all(base_path.join("billing")).unwrap();
    fs::write(
        base_path.join("billing/form.png"),
        "this is not a valid png file",
    )
    .unwrap();

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();
    assert_ne!(
        run_diff(&ctx, &DiffOptions::default())
            .unwrap()
            .failed_tests,
        0
    );
    assert!(matches!(
        approve_workspace(&ctx, &[], &[], None),
        Err(ApproveError::NothingToApprove { .. })
    ));
}
