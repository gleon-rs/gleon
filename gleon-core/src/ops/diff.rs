//! Diff operation for running visual comparison tests against baseline snapshots.

use std::path::{Path, PathBuf};

use gleon_engine::{ComparisonResult, compare_images, decode::decode_rgba, masking::apply_masks};
use thiserror::Error;

use crate::{
    context::ResolvedContext,
    manifest::WorkspaceIndex,
    ops::common::{
        CoreError, ensure_initialized, load_config_and_scan, load_merged_index_with_fallback,
        platform_key, sha256_hex_matches,
    },
    report::{ReportError, ReportGenerator},
    results::{TestCaseResult, TestImageResult},
};

/// Errors that can occur during diff execution.
#[derive(Debug, Error)]
pub enum DiffOpError {
    /// Error generating report files.
    #[error("Report error: {0}")]
    Report(#[from] ReportError),

    /// Image processing error.
    #[error("Image error: {0}")]
    Image(#[from] image::ImageError),

    /// Error shared across `ops::*` operations.
    #[error(transparent)]
    Core(#[from] CoreError),
}

/// Result summary of executing `gleon diff`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffReportResult {
    /// Total number of test cases evaluated.
    pub total_tests: usize,
    /// Number of test cases that failed comparison.
    pub failed_tests: usize,
    /// Whether every test case passed.
    pub passed: bool,
    /// Directory containing this run's output (`report.json`, diffs, etc.).
    pub runs_dir: PathBuf,
}

// Genuinely long from the sheer number of distinct byte-identical/missing-baseline/decode/mask/
// compare branches, each returning a different `TestImageResult` variant with its own message —
// not from duplicated logic.
#[expect(
    clippy::too_many_lines,
    reason = "long by design; see the comment above"
)]
pub(crate) fn process_diff_case(
    case: &crate::scanner::TestCase,
    workspace_index: &WorkspaceIndex,
    actual_dir: &Path,
    diffs_dir: &Path,
    blobs_root: &Path,
) -> TestImageResult {
    let test_name = case.name.clone();
    let actual_bytes = match std::fs::read(&case.image.absolute_path) {
        Ok(b) => b,
        Err(e) => {
            return TestImageResult::IoError {
                relative_path: case.image.relative_path.clone(),
                error: format!("Failed to read actual screenshot file: {e}"),
            };
        }
    };

    let single_manifest_opt = workspace_index.get(&test_name);

    if let Some(baseline_entry) = single_manifest_opt {
        let is_byte_identical = sha256_hex_matches(&baseline_entry.hash, &actual_bytes);
        if is_byte_identical {
            if !crate::storage::has_usable_local_blob(blobs_root, &baseline_entry.hash) {
                return TestImageResult::MissingBaseline {
                    relative_path: case.image.relative_path.clone(),
                    reason: format!("Baseline blob not found: {}", baseline_entry.hash.value()),
                };
            }
            if let Err(e) = crate::manifest::SingleTestManifest::validate_image_bytes(&actual_bytes)
            {
                return TestImageResult::DecodeError {
                    relative_path: case.image.relative_path.clone(),
                    error: format!("Invalid actual image dimensions/format: {e}"),
                };
            }
            return TestImageResult::Success {
                relative_path: case.image.relative_path.clone(),
            };
        }
    }

    let actual_dest_path = actual_dir.join(&case.image.relative_path);

    let parent = actual_dest_path.parent().unwrap_or_else(|| Path::new("."));
    if let Err(e) = std::fs::create_dir_all(parent) {
        return TestImageResult::IoError {
            relative_path: case.image.relative_path.clone(),
            error: format!("Failed to create directory for actual screenshot: {e}"),
        };
    }
    if let Err(e) = crate::io::save_file_atomically(&actual_dest_path, &actual_bytes) {
        return TestImageResult::IoError {
            relative_path: case.image.relative_path.clone(),
            error: format!("Failed to save actual screenshot: {e}"),
        };
    }

    let Some(baseline_entry) = single_manifest_opt else {
        return TestImageResult::MissingBaseline {
            relative_path: case.image.relative_path.clone(),
            reason: format!("No staged baseline manifest for test '{test_name}'"),
        };
    };

    let baseline_blob_path = crate::storage::local_blob_path(blobs_root, &baseline_entry.hash);

    let baseline_bytes = match std::fs::read(&baseline_blob_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return TestImageResult::MissingBaseline {
                relative_path: case.image.relative_path.clone(),
                reason: format!("Baseline blob not found: {}", baseline_entry.hash.value()),
            };
        }
        Err(e) => {
            return TestImageResult::IoError {
                relative_path: case.image.relative_path.clone(),
                error: format!("Failed to read baseline blob file: {e}"),
            };
        }
    };

    if let Err(e) = crate::manifest::SingleTestManifest::validate_image_bytes(&baseline_bytes) {
        return TestImageResult::DecodeError {
            relative_path: case.image.relative_path.clone(),
            error: format!("Invalid baseline dimensions/format: {e}"),
        };
    }

    // Same resource-limited decoder as the Flutter package, so both entry points see the same
    // pixels for the same bytes.
    let mut baseline_rgba = match decode_rgba(&baseline_bytes) {
        Ok(img) => img,
        Err(e) => {
            return TestImageResult::DecodeError {
                relative_path: case.image.relative_path.clone(),
                error: format!("Failed to decode baseline blob: {e}"),
            };
        }
    };

    if let Err(e) = crate::manifest::SingleTestManifest::validate_image_bytes(&actual_bytes) {
        return TestImageResult::DecodeError {
            relative_path: case.image.relative_path.clone(),
            error: format!("Invalid actual image dimensions/format: {e}"),
        };
    }

    let mut actual_rgba = match decode_rgba(&actual_bytes) {
        Ok(img) => img,
        Err(e) => {
            return TestImageResult::DecodeError {
                relative_path: case.image.relative_path.clone(),
                error: format!("Failed to decode actual screenshot: {e}"),
            };
        }
    };

    let matched_zones = case.rule.matched_mask_zones(&case.image.relative_path);
    if !matched_zones.is_empty() {
        apply_masks(&mut baseline_rgba, &matched_zones);
        apply_masks(&mut actual_rgba, &matched_zones);
    }

    let comp_result = compare_images(
        &baseline_rgba,
        &actual_rgba,
        case.rule.mode,
        &case.rule.diff,
    );

    let raw_file_name = case
        .image
        .relative_path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("screenshot.png"))
        .to_string_lossy();

    match comp_result {
        ComparisonResult::Match => TestImageResult::Success {
            relative_path: case.image.relative_path.clone(),
        },
        ComparisonResult::TooLarge {
            size: (width, height),
        } => TestImageResult::DecodeError {
            relative_path: case.image.relative_path.clone(),
            error: format!(
                "{width}x{height} exceeds the SSIM analysis budget of {} pixels; use pixel mode or a smaller capture",
                gleon_engine::ssim::MAX_ANALYSIS_PIXELS
            ),
        },
        ComparisonResult::DimensionMismatch {
            baseline_size,
            actual_size,
        } => TestImageResult::DimensionMismatch {
            relative_path: case.image.relative_path.clone(),
            baseline_size,
            actual_size,
            baseline_path: baseline_blob_path,
            actual_path: actual_dest_path,
        },
        ComparisonResult::Mismatch { detail, diff_image } => {
            let case_diff_dir = diffs_dir.join(&test_name);
            if let Err(e) = std::fs::create_dir_all(&case_diff_dir) {
                return TestImageResult::IoError {
                    relative_path: case.image.relative_path.clone(),
                    error: format!("Failed to create directory for diff: {e}"),
                };
            }
            let mut diff_file_name = std::ffi::OsString::from("diff_");
            diff_file_name.push(&*raw_file_name);
            let diff_file_path = case_diff_dir.join(&diff_file_name);

            let mut cursor = std::io::Cursor::new(Vec::new());
            if let Err(e) = diff_image.write_to(&mut cursor, image::ImageFormat::Png) {
                return TestImageResult::EncodeError {
                    relative_path: case.image.relative_path.clone(),
                    actual_path: actual_dest_path,
                    error: format!("Failed to encode diff visualization: {e}"),
                };
            }
            let encoded = cursor.into_inner();
            if let Err(e) = crate::io::save_file_atomically(&diff_file_path, &encoded) {
                return TestImageResult::IoError {
                    relative_path: case.image.relative_path.clone(),
                    error: format!("Failed to save diff visualization: {e}"),
                };
            }

            TestImageResult::Mismatch {
                relative_path: case.image.relative_path.clone(),
                detail,
                diff_path: diff_file_path,
                baseline_path: baseline_blob_path,
                actual_path: actual_dest_path,
            }
        }
    }
}

/// Executes diff comparison for the workspace at `base_dir`.
///
/// # Errors
///
/// Returns an error if the workspace is not initialized, if the platform key cannot be
/// resolved, if manifests fail to load, if the previous run's cache cannot be cleared or
/// recreated, if screenshots cannot be scanned, or if generating the HTML/JUnit reports fails.
pub fn run_diff(context: &ResolvedContext) -> Result<DiffReportResult, DiffOpError> {
    use rayon::prelude::*;

    let paths = ensure_initialized(&context.base_dir)?;
    let platform_key = platform_key(context)?;

    let workspace_index = load_merged_index_with_fallback(
        &paths,
        &platform_key,
        context.fallback_platform_key.as_deref(),
    )?;

    let runs_dir = paths.runs_latest();
    match std::fs::remove_dir_all(&runs_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(CoreError::Io(e).into()),
    }
    let diffs_dir = paths.runs_latest_diffs();
    let actual_dir = paths.runs_actual();
    std::fs::create_dir_all(&diffs_dir).map_err(CoreError::Io)?;
    std::fs::create_dir_all(&actual_dir).map_err(CoreError::Io)?;
    let blobs_root = paths.blobs_root();

    let test_cases = load_config_and_scan(context)?;

    let progress_bar = crate::ui::create_progress_bar(test_cases.len() as u64);

    let case_results: Vec<TestCaseResult> = test_cases
        .into_par_iter()
        .map(|case| {
            progress_bar.set_message(case.image.relative_path.display().to_string());
            let result = process_diff_case(
                &case,
                &workspace_index,
                &actual_dir,
                &diffs_dir,
                &blobs_root,
            );
            progress_bar.inc(1);
            TestCaseResult {
                name: case.name,
                result,
            }
        })
        .collect();

    progress_bar.finish_and_clear();

    let total_tests = case_results.len();
    let failed_tests = case_results
        .iter()
        .filter(|tc| !matches!(tc.result, TestImageResult::Success { .. }))
        .count();
    let passed = failed_tests == 0;

    // Generate HTML and JUnit XML reports
    ReportGenerator::generate_all(&runs_dir, &case_results).map_err(DiffOpError::Report)?;

    Ok(DiffReportResult {
        total_tests,
        failed_tests,
        passed,
        runs_dir,
    })
}

#[cfg(all(test, not(miri)))]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::pedantic,
    clippy::nursery,
    reason = "test code: panics are assertions, and pedantic/nursery style lints are not enforced in tests"
)]
mod tests {
    use sha2::Digest;

    use super::*;
    use crate::{
        config::ConfigError, context::ContextError, manifest::ManifestError, scanner::ScannerError,
    };

    #[test]
    fn test_diff_error_display() {
        let err1: DiffOpError = CoreError::NotInitialized.into();
        assert!(err1.to_string().contains("not initialized"));

        let err2: DiffOpError = CoreError::Context(ContextError::Platform(
            crate::platform::PlatformError::InvalidSegment("test".to_string()),
        ))
        .into();
        assert!(err2.to_string().contains("Context resolution error"));

        let err3: DiffOpError = CoreError::Scanner(ScannerError::InvalidTestName {
            name: "bad/name".to_string(),
            reason: "reason".to_string(),
        })
        .into();
        assert!(err3.to_string().contains("Scanner error"));

        let err4: DiffOpError =
            CoreError::Config(ConfigError::Validation("bad config".to_string())).into();
        assert!(err4.to_string().contains("Config error"));

        let err5: DiffOpError =
            CoreError::Manifest(ManifestError::Validation("bad manifest".to_string())).into();
        assert!(err5.to_string().contains("Manifest error"));

        let err6: DiffOpError = CoreError::Io(std::io::Error::other("io test")).into();
        assert!(err6.to_string().contains("IO error"));
    }

    #[test]
    fn test_diff_invalid_platform_error() {
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();

        let mut ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            ..ResolvedContext::default()
        };
        ctx.platform.os = "../invalid".to_string(); // Invalid segment

        let err = run_diff(&ctx).unwrap_err();
        assert!(matches!(
            err,
            DiffOpError::Core(CoreError::Context(ContextError::Platform(_)))
        ));
    }

    #[test]
    fn test_diff_manifest_load_error_and_scanner_error() {
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();

        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            ..ResolvedContext::default()
        };
        let plat_key = ctx.platform.to_key().unwrap();

        // 1. Corrupt manifest file in manifests_dir
        let manifests_dir = gleon_dir.join("manifests").join(&plat_key);
        std::fs::create_dir_all(&manifests_dir).unwrap();
        std::fs::write(manifests_dir.join("test.json"), "invalid json").unwrap();

        let res = run_diff(&ctx);
        assert!(matches!(
            res,
            Err(DiffOpError::Core(CoreError::Manifest(_)))
        ));

        // Clean up corrupt manifest
        std::fs::remove_file(manifests_dir.join("test.json")).unwrap();

        // 2. Invalid screenshot directory name (exclamation mark) to trigger Scanner error
        let bad_dir = temp.path().join("invalid!name");
        std::fs::create_dir_all(&bad_dir).unwrap();
        std::fs::write(bad_dir.join("test.png"), "fake png").unwrap();

        let res2 = run_diff(&ctx);
        assert!(matches!(
            res2,
            Err(DiffOpError::Core(CoreError::Scanner(_)))
        ));
    }

    #[test]
    fn test_diff_blob_read_generic_io_error() {
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();

        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            ..ResolvedContext::default()
        };
        let plat_key = ctx.platform.to_key().unwrap();

        // Create a valid manifest entry
        let manifests_dir = gleon_dir.join("manifests").join(&plat_key);
        std::fs::create_dir_all(&manifests_dir).unwrap();
        let hash = crate::manifest::ImageHash::new(
            "sha256",
            "1111111111111111111111111111111111111111111111111111111111111111",
        )
        .unwrap();
        let phash = crate::manifest::ImageHash::new("dhash", "0000000000000000").unwrap();
        let manifest = crate::manifest::SingleTestManifest::new(hash, phash, 100, 100).unwrap();
        manifest.save(manifests_dir.join("login.json")).unwrap();

        // Create actual screenshot with non-matching hash
        let img = image::RgbaImage::new(10, 10);
        img.save(temp.path().join("login.png")).unwrap();

        // Create blob path as a DIRECTORY so std::fs::read returns EISDIR (generic IO error, not NotFound)
        let blob_dir = gleon_dir
            .join("blobs")
            .join("sha256")
            .join("1111111111111111111111111111111111111111111111111111111111111111");
        std::fs::create_dir_all(&blob_dir).unwrap();

        let res = run_diff(&ctx).unwrap();
        assert_eq!(res.failed_tests, 1);
        assert!(!res.passed);
    }

    #[cfg(all(unix, not(miri)))]
    #[test]
    fn test_diff_actual_read_generic_io_error() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();

        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            ..ResolvedContext::default()
        };
        let plat_key = ctx.platform.to_key().unwrap();
        let manifests_dir = gleon_dir.join("manifests").join(&plat_key);
        std::fs::create_dir_all(&manifests_dir).unwrap();

        let hash = crate::manifest::ImageHash::new(
            "sha256",
            "1111111111111111111111111111111111111111111111111111111111111111",
        )
        .unwrap();
        let phash = crate::manifest::ImageHash::new("dhash", "0000000000000000").unwrap();
        let manifest = crate::manifest::SingleTestManifest::new(hash, phash, 100, 100).unwrap();
        manifest.save(manifests_dir.join("login.json")).unwrap();

        let screenshot = temp.path().join("login.png");
        std::fs::write(&screenshot, "fake png").unwrap();

        // Remove read permissions
        let mut perms = std::fs::metadata(&screenshot).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&screenshot, perms.clone()).unwrap();

        let res = run_diff(&ctx);

        // Restore permissions before assertions
        perms.set_mode(0o644);
        std::fs::set_permissions(&screenshot, perms).unwrap();

        let report = res.unwrap();
        assert_eq!(report.failed_tests, 1);
        assert!(!report.passed);
    }

    #[test]
    #[cfg(all(unix, not(miri)))]
    fn test_diff_write_io_errors_via_poisoning() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();

        let mut ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            ..ResolvedContext::default()
        };
        ctx.platform.os = "linux".to_string();
        let key = ctx.platform.to_key().unwrap();

        let manifests_dir = gleon_dir.join("manifests").join(&key);
        std::fs::create_dir_all(&manifests_dir).unwrap();
        let hash = "1111111111111111111111111111111111111111111111111111111111111111";
        let manifest = crate::manifest::SingleTestManifest::new(
            crate::manifest::ImageHash::new("sha256", hash).unwrap(),
            crate::manifest::ImageHash::new("dhash", "0000000000000000").unwrap(),
            1,
            1,
        )
        .unwrap();
        manifest.save(manifests_dir.join("test.json")).unwrap();

        let blobs_dir = gleon_dir.join("blobs").join("sha256");
        std::fs::create_dir_all(&blobs_dir).unwrap();
        let img = image::ImageBuffer::<image::Rgba<u8>, _>::new(1, 1);
        img.save_with_format(blobs_dir.join(hash), image::ImageFormat::Png)
            .unwrap();

        // Mismatching actual image (same dimensions, different color)
        let mut bad_img = image::ImageBuffer::<image::Rgba<u8>, _>::new(1, 1);
        bad_img.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
        bad_img.save(temp.path().join("test.png")).unwrap();

        let runs_dir = gleon_dir.join("runs").join("latest");
        let actual_dir = runs_dir.join("actuals");
        let diffs_dir = runs_dir.join("diffs");
        std::fs::create_dir_all(&actual_dir).unwrap();
        std::fs::create_dir_all(&diffs_dir).unwrap();

        // Put a file inside actual_dir and diffs_dir so they cannot be deleted if read-only
        std::fs::write(actual_dir.join("dummy"), "").unwrap();
        std::fs::write(diffs_dir.join("dummy"), "").unwrap();

        // Make actual_dir and diffs_dir read-only.
        // remove_dir_all(&runs_dir) will fail to delete them because it can't delete 'dummy'.
        // They will survive, and then save_file_atomically will fail because they are read-only!
        let mut perms = std::fs::metadata(&actual_dir).unwrap().permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(&actual_dir, perms.clone()).unwrap();
        std::fs::set_permissions(&diffs_dir, perms.clone()).unwrap();

        let result = run_diff(&ctx);
        assert!(result.is_err());

        // Restore permissions so tempdir can be cleaned up
        perms.set_mode(0o755);
        std::fs::set_permissions(&actual_dir, perms.clone()).unwrap();
        std::fs::set_permissions(&diffs_dir, perms).unwrap();
    }

    #[test]
    fn test_diff_empty_fallback_platform() {
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();

        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            fallback_platform_key: Some("some-fallback-key".to_string()),
            ..Default::default()
        };

        // No manifests created for primary or fallback platform, so fb_index will be empty.
        // run_diff should run and return no test results because scanner finds nothing (no screenshots created).
        let res = run_diff(&ctx).unwrap();
        assert_eq!(res.total_tests, 0);
    }

    #[test]
    #[cfg(all(unix, not(miri)))]
    fn test_process_diff_case_io_errors() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: `libc::geteuid()` is a side-effect-free POSIX syscall query that returns the process EUID.
        #[expect(
            unsafe_code,
            reason = "`libc::geteuid()` is a side-effect-free POSIX query (see SAFETY comment)"
        )]
        if unsafe { libc::geteuid() } == 0 {
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        let actual_dir = temp.path().join("actual");
        let diffs_dir = temp.path().join("diffs");

        std::fs::create_dir_all(&gleon_dir).unwrap();
        std::fs::create_dir_all(&actual_dir).unwrap();
        std::fs::create_dir_all(&diffs_dir).unwrap();

        let case = crate::scanner::TestCase {
            name: "test".to_string(),
            image: crate::scanner::TestImage {
                absolute_path: temp.path().join("test.png"),
                relative_path: PathBuf::from("test.png"),
            },
            rule: std::sync::Arc::new(crate::config::ScreenshotRule {
                include: vec![],
                mode: gleon_engine::config::Mode::Pixel,
                diff: gleon_engine::config::DiffConfig {
                    threshold: 0.0,
                    anti_alias: false,
                    min_similarity: 1.0,
                    color_tolerance: 8.0,
                },
                masks: vec![],
            }),
        };

        // 1. Missing actual image
        let res1 = process_diff_case(
            &case,
            &WorkspaceIndex::new(),
            &actual_dir,
            &diffs_dir,
            &gleon_dir.join("blobs"),
        );
        assert!(matches!(res1, TestImageResult::IoError { .. }));

        // 2. Decode error on actual image (requires baseline to bypass MissingBaseline)
        std::fs::write(&case.image.absolute_path, "fake png").unwrap();
        let mut index = WorkspaceIndex::new();
        let hash_val = "1111111111111111111111111111111111111111111111111111111111111111";
        index.insert(
            "test".to_string(),
            crate::manifest::SingleTestManifest {
                schema_version: 1,
                hash: crate::manifest::ImageHash::new("sha256", hash_val).unwrap(),
                phash: crate::manifest::ImageHash::new("dhash", "2222222222222222").unwrap(),
                width: 1,
                height: 1,
            },
        );
        let blob_dir = gleon_dir.join("blobs").join("sha256");
        std::fs::create_dir_all(&blob_dir).unwrap();
        std::fs::write(blob_dir.join(hash_val), "fake png").unwrap();
        let res2 = process_diff_case(
            &case,
            &index,
            &actual_dir,
            &diffs_dir,
            &gleon_dir.join("blobs"),
        );
        assert!(matches!(res2, TestImageResult::DecodeError { .. }));

        // Create valid image
        let img = image::ImageBuffer::<image::Rgba<u8>, _>::new(1, 1);
        img.save(&case.image.absolute_path).unwrap();

        // 3. IoError on create actual dir (make actual_dir read-only)
        let mut perms = std::fs::metadata(&actual_dir).unwrap().permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(&actual_dir, perms.clone()).unwrap();

        let case2 = crate::scanner::TestCase {
            name: "test".to_string(),
            image: crate::scanner::TestImage {
                absolute_path: case.image.absolute_path,
                relative_path: PathBuf::from("subdir/test.png"), // Requires create_dir_all
            },
            rule: std::sync::Arc::new(crate::config::ScreenshotRule {
                include: vec![],
                mode: gleon_engine::config::Mode::Pixel,
                diff: gleon_engine::config::DiffConfig {
                    threshold: 0.0,
                    anti_alias: false,
                    min_similarity: 1.0,
                    color_tolerance: 8.0,
                },
                masks: vec![],
            }),
        };
        let res3 = process_diff_case(
            &case2,
            &WorkspaceIndex::new(),
            &actual_dir,
            &diffs_dir,
            &gleon_dir.join("blobs"),
        );

        // Restore permissions before assertions
        perms.set_mode(0o755);
        std::fs::set_permissions(&actual_dir, perms).unwrap();

        assert!(matches!(res3, TestImageResult::IoError { .. }));
    }

    #[test]
    fn test_diff_partial_platform_override_with_fallback_success() {
        let temp = tempfile::tempdir().unwrap();
        let base_path = temp.path();
        let gleon_dir = base_path.join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();

        let linux_key = "5:linux-6:x86_64";
        let macos_key = "5:macos-7:aarch64";

        let mut ctx = ResolvedContext {
            base_dir: base_path.to_path_buf(),
            platform: crate::platform::PlatformInfo {
                os: "linux".to_string(),
                arch: Some("x86_64".to_string()),
                renderer: None,
                labels: std::collections::BTreeMap::new(),
            },
            fallback_platform_key: Some(macos_key.to_string()),
            ..Default::default()
        };

        let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include:
      - "*.png"
    mode: pixel
"#;
        let config_file = gleon_dir.join("gleon.yaml");
        std::fs::write(&config_file, config_yaml).unwrap();
        ctx.config = Some(crate::config::GleonConfig::load_from_file(&config_file).unwrap());

        // Create 2 test screenshot files
        let img1 = image::RgbaImage::new(10, 10);
        img1.save(base_path.join("test1.png")).unwrap();
        let img1_bytes = std::fs::read(base_path.join("test1.png")).unwrap();
        let img1_sha = hex::encode(sha2::Sha256::digest(&img1_bytes));

        let img2 = image::RgbaImage::new(20, 20);
        img2.save(base_path.join("test2.png")).unwrap();
        let img2_bytes = std::fs::read(base_path.join("test2.png")).unwrap();
        let img2_sha = hex::encode(sha2::Sha256::digest(&img2_bytes));

        let blobs_dir = gleon_dir.join("blobs").join("sha256");
        std::fs::create_dir_all(&blobs_dir).unwrap();
        std::fs::write(blobs_dir.join(&img1_sha), &img1_bytes).unwrap();
        std::fs::write(blobs_dir.join(&img2_sha), &img2_bytes).unwrap();

        // 1. Fallback manifests (macos) contains test2 and a dummy test1
        let macos_manifests = gleon_dir.join("manifests").join(macos_key);
        std::fs::create_dir_all(&macos_manifests).unwrap();
        let dhash = crate::manifest::ImageHash::new("dhash", "0000000000000000").unwrap();

        let dummy_sha = "0".repeat(64);
        let macos_m1 = crate::manifest::SingleTestManifest::new(
            crate::manifest::ImageHash::new("sha256", &dummy_sha).unwrap(),
            dhash.clone(),
            10,
            10,
        )
        .unwrap();
        macos_m1.save(macos_manifests.join("test1.json")).unwrap();

        let macos_m2 = crate::manifest::SingleTestManifest::new(
            crate::manifest::ImageHash::new("sha256", &img2_sha).unwrap(),
            dhash.clone(),
            20,
            20,
        )
        .unwrap();
        macos_m2.save(macos_manifests.join("test2.json")).unwrap();

        // 2. Linux manifests contains ONLY test1 (override)
        let linux_manifests = gleon_dir.join("manifests").join(linux_key);
        std::fs::create_dir_all(&linux_manifests).unwrap();
        let linux_m1 = crate::manifest::SingleTestManifest::new(
            crate::manifest::ImageHash::new("sha256", &img1_sha).unwrap(),
            dhash,
            10,
            10,
        )
        .unwrap();
        linux_m1.save(linux_manifests.join("test1.json")).unwrap();

        // Run diff on linux
        let diff_result = run_diff(&ctx).unwrap();
        assert_eq!(diff_result.total_tests, 2);
        assert_eq!(diff_result.failed_tests, 0);
        assert!(diff_result.passed);
    }
}
