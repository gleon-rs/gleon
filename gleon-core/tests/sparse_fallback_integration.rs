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
//! End-to-end integration test verifying sparse multi-platform fallback baselines.
//!
//! Tests the full multi-platform lifecycle:
//! 1. Seed base screenshots on macOS (3 tests).
//! 2. Push baselines to remote storage adapter.
//! 3. Switch to Linux with `fallback_platform`: macos.
//! 4. Modify test1 on Linux, verify diff passes test2 and test3 from macOS fallback and flags test1 as Mismatch.
//! 5. Approve test1 on Linux (creates 1 sparse override on Linux).
//! 6. Verify status is clean and diff passes with 1 Linux override + 2 macOS fallbacks.
//! 7. Verify pull downloads only the 3 required blobs (1 Linux + 2 macOS) on a clean runner.
//! 8. Revert test1 on Linux to match macOS, run approve, verify Linux override is automatically pruned.

#![cfg(not(miri))]

use std::{collections::BTreeMap, path::Path};

use gleon_core::{
    config::GleonConfig,
    context::ResolvedContext,
    ops::{
        approve::approve_workspace,
        diff::{DiffOptions, run_diff},
        pull::pull_blobs,
        push::push_blobs,
        stage::stage_workspace,
        status::check_status,
    },
    platform::PlatformInfo,
    storage::StorageConfig,
};
use tempfile::tempdir;

fn make_png(width: u32, height: u32, color: [u8; 4]) -> Vec<u8> {
    let mut img = image::RgbaImage::new(width, height);
    for pixel in img.pixels_mut() {
        *pixel = image::Rgba(color);
    }
    let mut bytes = Vec::new();
    img.write_to(
        &mut std::io::Cursor::new(&mut bytes),
        image::ImageFormat::Png,
    )
    .unwrap();
    bytes
}

#[tokio::test]
async fn test_sparse_multi_platform_fallback_full_lifecycle() {
    let workspace_temp = tempdir().unwrap();
    let base_path = workspace_temp.path();
    let gleon_dir = base_path.join(".gleon");
    std::fs::create_dir_all(&gleon_dir).unwrap();

    let storage_temp = tempdir().unwrap();
    let storage_cfg = StorageConfig::new(format!("file://{}", storage_temp.path().display()));

    let macos_platform = PlatformInfo {
        os: "macos".to_string(),
        arch: Some("aarch64".to_string()),
        renderer: None,
        labels: BTreeMap::new(),
    };
    let linux_platform = PlatformInfo {
        os: "linux".to_string(),
        arch: Some("x86_64".to_string()),
        renderer: None,
        labels: BTreeMap::new(),
    };

    let macos_key = macos_platform.key().unwrap();
    let linux_key = linux_platform.key().unwrap();

    let config_yaml = r#"
required_version: ">=0.1.0"
fallback_platform:
  os: macos
  arch: aarch64
screenshots:
  - include:
      - "test/goldens/**/*.png"
    mode: pixel
"#;
    let config_file = gleon_dir.join("gleon.yaml");
    std::fs::write(&config_file, config_yaml).unwrap();
    let config = GleonConfig::load_from_file(&config_file).unwrap();

    let mut macos_ctx = ResolvedContext::default();
    macos_ctx.platform = macos_platform;
    macos_ctx.fallback_platform_key = None;
    macos_ctx.config = Some(config.clone());
    macos_ctx.base_dir = base_path.to_path_buf();

    let mut linux_ctx = ResolvedContext::default();
    linux_ctx.platform = linux_platform;
    linux_ctx.fallback_platform_key = Some(macos_key.clone());
    linux_ctx.config = Some(config);
    linux_ctx.base_dir = base_path.to_path_buf();

    // Create goldens directory with 3 screenshot files
    let goldens_dir = base_path.join("test").join("goldens");
    std::fs::create_dir_all(&goldens_dir).unwrap();

    let png_t1_macos = make_png(10, 10, [255, 0, 0, 255]); // red
    let png_t2_common = make_png(20, 20, [0, 255, 0, 255]); // green
    let png_t3_common = make_png(30, 30, [0, 0, 255, 255]); // blue

    std::fs::write(goldens_dir.join("t1.png"), &png_t1_macos).unwrap();
    std::fs::write(goldens_dir.join("t2.png"), &png_t2_common).unwrap();
    std::fs::write(goldens_dir.join("t3.png"), &png_t3_common).unwrap();

    // -------------------------------------------------------------------------
    // Step 1: Stage on macOS & push to storage
    // -------------------------------------------------------------------------
    let stage_res = stage_workspace(&macos_ctx, None).unwrap();
    assert_eq!(stage_res.total_screenshots_staged, 3);

    let push_res = push_blobs(&macos_ctx, Some(&storage_cfg), false, None)
        .await
        .unwrap();
    assert_eq!(push_res.uploaded_blobs, 3);

    let macos_manifests_dir = gleon_dir.join("manifests").join(macos_key);
    assert!(macos_manifests_dir.join("test/goldens/t1.json").exists());
    assert!(macos_manifests_dir.join("test/goldens/t2.json").exists());
    assert!(macos_manifests_dir.join("test/goldens/t3.json").exists());

    // -------------------------------------------------------------------------
    // Step 2: Switch to Linux, modify t1.png
    // -------------------------------------------------------------------------
    let png_t1_linux = make_png(10, 10, [255, 128, 0, 255]); // orange (Linux render diff)
    std::fs::write(goldens_dir.join("t1.png"), &png_t1_linux).unwrap();

    // Run diff on Linux: t2 and t3 should succeed via macOS fallback, t1 should mismatch
    let diff_1 = run_diff(&linux_ctx, &DiffOptions::default()).unwrap();
    assert_eq!(diff_1.total_tests, 3);
    assert_eq!(diff_1.failed_tests, 1);

    // -------------------------------------------------------------------------
    // Step 3: Approve t1 on Linux
    // -------------------------------------------------------------------------
    let approve_res = approve_workspace(&linux_ctx, &[], &[], None).unwrap();
    let approved: Vec<_> = approve_res
        .approved
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(approved, [format!("{linux_key}/test/goldens/t1")]);

    let linux_manifests_dir = gleon_dir.join("manifests").join(linux_key);
    // Linux directory must contain ONLY 1 override manifest (test/goldens/t1.json)
    assert!(linux_manifests_dir.join("test/goldens/t1.json").exists());
    assert!(!linux_manifests_dir.join("test/goldens/t2.json").exists());
    assert!(!linux_manifests_dir.join("test/goldens/t3.json").exists());

    // Push Linux override blob to remote storage
    let push_res_linux = push_blobs(&linux_ctx, Some(&storage_cfg), false, None)
        .await
        .unwrap();
    assert_eq!(push_res_linux.uploaded_blobs, 1);

    // -------------------------------------------------------------------------
    // Step 4: Verify status and diff on Linux are clean & passing
    // -------------------------------------------------------------------------
    let status_res = check_status(&linux_ctx).unwrap();
    assert!(status_res.is_clean());

    let diff_2 = run_diff(&linux_ctx, &DiffOptions::default()).unwrap();
    assert_eq!(diff_2.total_tests, 3);
    assert_eq!(diff_2.failed_tests, 0);

    // -------------------------------------------------------------------------
    // Step 5: Simulate clean CI runner on Linux
    // -------------------------------------------------------------------------
    let runner_temp = tempdir().unwrap();
    let runner_base = runner_temp.path();
    let runner_gleon = runner_base.join(".gleon");
    std::fs::create_dir_all(&runner_gleon).unwrap();

    // Copy gleon.yaml and manifests to runner workspace (simulate git checkout)
    std::fs::copy(&config_file, runner_gleon.join("gleon.yaml")).unwrap();
    copy_dir_recursive(
        &gleon_dir.join("manifests"),
        &runner_gleon.join("manifests"),
    );

    // Copy current workspace screenshots to runner
    copy_dir_recursive(&base_path.join("test"), &runner_base.join("test"));

    // Same platform/config as linux_ctx, but rooted at the runner's checkout directory.
    let mut runner_ctx = linux_ctx.clone();
    runner_ctx.base_dir = runner_base.to_path_buf();

    // Pull blobs on runner
    let pull_res = pull_blobs(&runner_ctx, Some(&storage_cfg), false, None)
        .await
        .unwrap();
    // Must download 3 blobs (1 Linux override + 2 macOS fallbacks)
    assert_eq!(pull_res.total_manifest_blobs, 3);
    assert_eq!(pull_res.downloaded_blobs, 3);

    // Diff on runner must pass completely
    let diff_runner = run_diff(&runner_ctx, &DiffOptions::default()).unwrap();
    assert_eq!(diff_runner.total_tests, 3);
    assert_eq!(diff_runner.failed_tests, 0);

    // -------------------------------------------------------------------------
    // Step 6: Fix t1 on Linux so it matches macOS again -> Approve prunes override
    // -------------------------------------------------------------------------
    std::fs::write(goldens_dir.join("t1.png"), &png_t1_macos).unwrap();

    // Diff fails against Linux override (because override was orange, now red)
    let diff_3 = run_diff(&linux_ctx, &DiffOptions::default()).unwrap();
    assert_eq!(diff_3.failed_tests, 1);

    // Approve on Linux: since new image matches macOS fallback baseline, override is pruned
    let approve_2 = approve_workspace(&linux_ctx, &[], &[], None).unwrap();
    assert_eq!(approve_2.approved.len(), 1);

    // Linux override manifest must now be deleted from disk
    assert!(!linux_manifests_dir.join("test/goldens/t1.json").exists());

    // Status on Linux is clean
    let status_clean = check_status(&linux_ctx).unwrap();
    assert!(status_clean.is_clean());

    // Diff on Linux passes
    let diff_4 = run_diff(&linux_ctx, &DiffOptions::default()).unwrap();
    assert_eq!(diff_4.total_tests, 3);
    assert_eq!(diff_4.failed_tests, 0);

    // -------------------------------------------------------------------------
    // Step 7: Delete t2.png on Linux -> Status reports Deleted via fallback.
    // Staging on macOS (the source-of-truth fallback platform) prunes t2.json orphan.
    // -------------------------------------------------------------------------
    std::fs::remove_file(goldens_dir.join("t2.png")).unwrap();

    let status_deleted = check_status(&linux_ctx).unwrap();
    assert_eq!(
        status_deleted.deleted,
        vec![std::path::PathBuf::from("test/goldens/t2.png")]
    );

    // Verify macOS fallback manifest for t2 currently exists
    assert!(macos_manifests_dir.join("test/goldens/t2.json").exists());

    // Stage on macOS (the fallback source of truth platform): prunes orphan manifest t2.json
    let stage_macos_orphan = stage_workspace(&macos_ctx, None).unwrap();
    // t1 and t3 are unchanged, so newly staged count is 0
    assert_eq!(stage_macos_orphan.total_screenshots_staged, 0);

    // macOS manifest for t2 is now cleaned up from disk
    assert!(!macos_manifests_dir.join("test/goldens/t2.json").exists());
    assert!(macos_manifests_dir.join("test/goldens/t1.json").exists());
    assert!(macos_manifests_dir.join("test/goldens/t3.json").exists());

    // Status on Linux is now completely clean without any deleted tests
    let status_final = check_status(&linux_ctx).unwrap();
    assert!(status_final.is_clean());
}

fn copy_dir_recursive(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let file_type = entry.file_type().unwrap();
        let target = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}
