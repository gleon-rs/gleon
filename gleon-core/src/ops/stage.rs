//! Staging operation for processing, masking, and persisting baseline screenshots.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use gleon_model::{
    case::Sha256Hex,
    compare::{self, CompareError},
};
use thiserror::Error;

use crate::{
    context::ResolvedContext,
    manifest::{ManifestError, SingleTestManifest, WorkspaceIndex},
    ops::common::{
        CoreError, build_manifest, ensure_initialized, index_keys_missing_from,
        load_config_and_scan, phash_of,
    },
    scanner::{FileScanner, TestCase},
};

/// Errors that can occur during staging.
#[derive(Debug, Error)]
pub enum StageError {
    /// Error decoding image file.
    #[error("Image decode error for '{path}'")]
    ImageDecode {
        /// The path to the image that failed to decode.
        path: PathBuf,
        /// The underlying decode error.
        #[source]
        source: image::ImageError,
    },

    /// A screenshot no comparison under its rule could use as a baseline
    /// ([`compare::check_size`]: under `mode: ssim` larger than the SSIM analysis budget), which
    /// `gleon diff` and `gleon status` would fail.
    #[error("'{path}' cannot be compared under its rule: {source}")]
    Uncomparable {
        /// The path to the screenshot.
        path: PathBuf,
        /// Why it cannot be compared.
        #[source]
        source: CompareError,
    },

    /// A screenshot changed between its check and the write of its blob.
    #[error("'{path}' changed while it was staged: stage again")]
    Changed {
        /// The path to the screenshot.
        path: PathBuf,
    },

    /// Error shared across `ops::*` operations.
    #[error(transparent)]
    Core(#[from] CoreError),
}

/// Result summary of staging screenshots.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StageResult {
    /// List of test case names staged.
    pub staged_test_cases: Vec<String>,
    /// Number of total screenshots staged.
    pub total_screenshots_staged: usize,
}

/// Applies the given path filters to a list of test cases in-place.
pub(crate) fn filter_test_cases(test_cases: &mut Vec<TestCase>, filter_paths: Option<&[PathBuf]>) {
    if let Some(filters) = filter_paths {
        // An absolute filter is resolved like the workspace root the cases lie under
        // ([`crate::paths::find_workspace_root`]), so a path through a symbolic link matches.
        let normalized_filters: Vec<_> = filters
            .iter()
            .map(|f| {
                let f = if f.is_absolute() {
                    gleon_model::fs::canonical(f).unwrap_or_else(|_| f.clone())
                } else {
                    f.clone()
                };
                let norm = FileScanner::normalize_path_str(&f).into_owned();
                (f, norm)
            })
            .collect();

        test_cases.retain(|case| {
            let rel_norm = FileScanner::normalize_path_str(&case.image.relative_path);
            normalized_filters.iter().any(|(f, norm_f)| {
                case.image.absolute_path.starts_with(f)
                    || case.image.relative_path.starts_with(f)
                    || (rel_norm.len() >= norm_f.len()
                        && rel_norm.as_bytes()[..norm_f.len()]
                            .eq_ignore_ascii_case(norm_f.as_bytes())
                        && (rel_norm.len() == norm_f.len()
                            || norm_f.as_bytes().last() == Some(&b'/')
                            || rel_norm.as_bytes()[norm_f.len()] == b'/'))
            })
        });
    }
}

/// Executes staging pipeline across the workspace.
///
/// Every screenshot is checked before anything is written, so a bad one stages nothing; a blob
/// already in the cache that holds its bytes is not written again, and an unchanged screenshot
/// keeps its manifest without being decoded.
///
/// # Errors
///
/// Returns an error if the workspace is not initialized, if the platform key cannot be
/// resolved, if screenshots cannot be scanned, if any screenshot fails to decode or no comparison
/// under its rule could use it ([`StageError::Uncomparable`]), if a screenshot changes while it
/// is staged ([`StageError::Changed`]), or if reading/writing manifests, blobs, or other
/// filesystem data fails.
pub fn stage_workspace(
    context: &ResolvedContext,
    filter_paths: Option<&[PathBuf]>,
) -> Result<StageResult, StageError> {
    use rayon::prelude::*;

    let paths = ensure_initialized(&context.base_dir)?;
    let platform_key = crate::ops::common::platform_key(context)?;
    // Before anything is created: a goldens workspace gets no blobs or manifests.
    let mut test_cases = load_config_and_scan(context)?;

    let blobs_dir = paths.blob_scheme_dir("sha256");
    let manifests_dir = paths.manifests_dir(&platform_key);
    std::fs::create_dir_all(&blobs_dir).map_err(CoreError::Io)?;
    std::fs::create_dir_all(&manifests_dir).map_err(CoreError::Io)?;

    filter_test_cases(&mut test_cases, filter_paths);

    let pb = crate::ui::create_progress_bar(test_cases.len() as u64);
    pb.set_message("Staging screenshots...");

    let mut workspace_index = WorkspaceIndex::load(&manifests_dir).map_err(CoreError::Manifest)?;

    // Checked in parallel, writing nothing: one bad screenshot stages none.
    let checked: Result<Vec<Checked>, StageError> = test_cases
        .into_par_iter()
        .map(|case| {
            let staged = workspace_index.get(&case.name);
            let checked = check_screenshot(case, staged);
            pb.inc(1);
            checked
        })
        .collect();
    pb.finish_and_clear();
    let checked = checked?;

    // Each content once (cases often share one). A blob of the cache stays when it holds the
    // bytes its name says (`storage::holds_blob`); a missing one, a symbolic link (replaced,
    // never written through) or one holding other bytes (a changed cache) is written.
    let blobs: HashMap<&str, &Checked> = checked
        .iter()
        .map(|item| (item.sha256.as_str(), item))
        .collect();
    blobs
        .into_par_iter()
        .filter(|&(sha256, item)| {
            !crate::storage::holds_blob(&blobs_dir.join(sha256), &item.sha256)
        })
        .try_for_each(|(sha256, item)| {
            write_blob(
                &blobs_dir.join(sha256),
                &item.absolute_path,
                &item.relative_path,
                &item.sha256,
            )
        })?;

    // Clean up orphan manifests when performing a full workspace stage (no path filters)
    if filter_paths.is_none() {
        let scanned_names: std::collections::HashSet<_> =
            checked.iter().map(|item| item.case_name.as_str()).collect();
        let orphan_names: Vec<String> = index_keys_missing_from(&workspace_index, &scanned_names)
            .map(String::from)
            .collect();
        for existing in orphan_names {
            workspace_index
                .remove_test(&manifests_dir, &existing)
                .map_err(CoreError::Manifest)?;
        }
    }

    let mut staged_test_cases = Vec::new();
    let mut total_screenshots_staged = 0;

    for item in checked {
        let is_unchanged = workspace_index
            .get(&item.case_name)
            .is_some_and(|existing| existing == &item.manifest);

        if !is_unchanged {
            workspace_index
                .save_test(&manifests_dir, &item.case_name, &item.manifest)
                .map_err(CoreError::Manifest)?;
            total_screenshots_staged += 1;
            staged_test_cases.push(item.case_name);
        }
    }

    Ok(StageResult {
        staged_test_cases,
        total_screenshots_staged,
    })
}

/// A screenshot that passed its checks, with the manifest it gets.
struct Checked {
    case_name: String,
    absolute_path: PathBuf,
    relative_path: PathBuf,
    sha256: Sha256Hex,
    manifest: SingleTestManifest,
}

/// Checks the screenshot of `case`, writing nothing: a decodable image (its size from the
/// header) that its rule can compare, measured for its manifest. Unchanged bytes keep the manifest
/// `staged` without being decoded.
fn check_screenshot(
    case: TestCase,
    staged: Option<&SingleTestManifest>,
) -> Result<Checked, StageError> {
    let png_bytes = std::fs::read(&case.image.absolute_path).map_err(CoreError::Io)?;
    let decode_error = |e| match e {
        ManifestError::Image(source) => StageError::ImageDecode {
            path: case.image.relative_path.clone(),
            source,
        },
        other => CoreError::Manifest(other).into(),
    };
    let (width, height) =
        SingleTestManifest::validate_image_bytes(&png_bytes).map_err(decode_error)?;
    // From the header, before the pixels are decoded: no comparison could use it.
    compare::check_size(&case.rule.tolerance, width, height).map_err(|source| {
        StageError::Uncomparable {
            path: case.image.relative_path.clone(),
            source,
        }
    })?;
    let sha256 = Sha256Hex::of(&png_bytes);
    let manifest = match staged {
        Some(staged) if staged.hash.value() == sha256.as_str() => staged.clone(),
        _ => {
            let image = SingleTestManifest::decode_validated(&png_bytes).map_err(decode_error)?;
            build_manifest(sha256.as_str(), &phash_of(image), width, height)?
        }
    };
    Ok(Checked {
        case_name: case.name,
        absolute_path: case.image.absolute_path,
        relative_path: case.image.relative_path,
        sha256,
        manifest,
    })
}

/// Writes the blob `blob` ([`crate::storage::save_blob`]: atomically and durably in place of
/// whatever is there, never through a link) from the screenshot at `path` (`relative` for
/// messages), read again: it must still hold the bytes it was checked with (`sha256`).
fn write_blob(
    blob: &Path,
    path: &Path,
    relative: &Path,
    sha256: &Sha256Hex,
) -> Result<(), StageError> {
    let png_bytes = std::fs::read(path).map_err(CoreError::Io)?;
    if Sha256Hex::of(&png_bytes) != *sha256 {
        return Err(StageError::Changed {
            path: relative.to_path_buf(),
        });
    }
    crate::storage::save_blob(blob, &png_bytes).map_err(|e| StageError::Core(CoreError::Io(e)))
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
    use gleon_model::compare::CompareError;

    use super::*;
    use crate::{config::ConfigError, context::ContextError, scanner::ScannerError};

    #[test]
    fn test_stage_error_display() {
        let err1: StageError = CoreError::NotInitialized.into();
        assert!(err1.to_string().contains("not initialized"));

        let err2: StageError = CoreError::Context(ContextError::Platform(
            crate::platform::PlatformError::InvalidSegment("test".to_string()),
        ))
        .into();
        assert!(err2.to_string().contains("Context resolution error"));

        let err3: StageError = CoreError::Scanner(ScannerError::InvalidTestName {
            name: "bad/name".to_string(),
            reason: "reason".to_string(),
        })
        .into();
        assert!(err3.to_string().contains("Scanner error"));

        let err4: StageError =
            CoreError::Config(ConfigError::Validation("bad config".to_string())).into();
        assert!(err4.to_string().contains("Config error"));

        let err5: StageError =
            CoreError::Manifest(ManifestError::Validation("bad manifest".to_string())).into();
        assert!(err5.to_string().contains("Manifest error"));

        let img_err = image::ImageError::Limits(image::error::LimitError::from_kind(
            image::error::LimitErrorKind::DimensionError,
        ));
        let err6 = StageError::ImageDecode {
            path: PathBuf::from("a.png"),
            source: img_err,
        };
        assert!(err6.to_string().contains("Image decode error"));
        assert!(std::error::Error::source(&err6).is_some());

        let err7 = StageError::Uncomparable {
            path: PathBuf::from("ssim/big.png"),
            source: CompareError::TooLarge {
                width: 4097,
                height: 4096,
            },
        };
        assert_eq!(
            err7.to_string(),
            "'ssim/big.png' cannot be compared under its rule: 4097x4096 exceeds the SSIM \
             analysis budget of 16777216 pixels; use exact or pixel mode or a smaller capture"
        );
        assert!(std::error::Error::source(&err7).is_some());

        let err8: StageError = CoreError::Io(std::io::Error::other("io test")).into();
        assert!(err8.to_string().contains("IO error"));
    }

    /// The blob is written from the screenshot read again: one that changed since its check is
    /// refused, never stored under the old hash.
    #[test]
    fn test_write_blob_refuses_a_changed_screenshot() {
        let temp = tempfile::tempdir().unwrap();
        let screenshot = temp.path().join("a.png");
        std::fs::write(&screenshot, b"now").unwrap();
        let blob = temp.path().join("blob");
        let relative = Path::new("shots/a.png");

        let err = write_blob(&blob, &screenshot, relative, &Sha256Hex::of(b"then")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "'shots/a.png' changed while it was staged: stage again"
        );
        assert!(!blob.exists());
        write_blob(&blob, &screenshot, relative, &Sha256Hex::of(b"now")).unwrap();
        assert_eq!(std::fs::read(&blob).unwrap(), b"now");
    }

    #[test]
    fn test_stage_result_derived() {
        let res = StageResult {
            staged_test_cases: vec!["test1".to_string()],
            total_screenshots_staged: 1,
        };
        let cloned = res.clone();
        assert_eq!(res, cloned);
        assert!(!format!("{res:?}").is_empty());
        let default_res = StageResult::default();
        assert_eq!(default_res.total_screenshots_staged, 0);
    }

    #[test]
    fn test_filter_test_cases() {
        use std::sync::Arc;

        use crate::scanner::{TestCase, TestImage};

        let rule = Arc::new(crate::config::ScreenshotRule {
            include: vec![],
            tolerance: gleon_model::tolerance::Tolerance::pixel(0.01),
            masks: vec![],
            text_tolerance: None,
        });

        let mut cases = vec![
            TestCase {
                name: "test1".to_string(),
                image: TestImage {
                    relative_path: PathBuf::from("a/test1.png"),
                    absolute_path: PathBuf::from("/base/a/test1.png"),
                },
                rule: rule.clone(),
            },
            TestCase {
                name: "test2".to_string(),
                image: TestImage {
                    relative_path: PathBuf::from("b/test2.png"),
                    absolute_path: PathBuf::from("/base/b/test2.png"),
                },
                rule: rule.clone(),
            },
        ];

        // 1. None should keep all
        let mut cases_clone = cases.clone();
        filter_test_cases(&mut cases_clone, None);
        assert_eq!(cases_clone.len(), 2);

        // 2. Filter keeping only test1 via absolute path
        let mut cases_clone2 = cases.clone();
        let filter1 = vec![PathBuf::from("/base/a")];
        filter_test_cases(&mut cases_clone2, Some(&filter1));
        assert_eq!(cases_clone2.len(), 1);
        assert_eq!(cases_clone2[0].name, "test1");

        // 3. Filter with mixed casing (e.g. "A/TEST1.PNG")
        let filter_mixed = vec![PathBuf::from("A/TEST1.PNG")];
        filter_test_cases(&mut cases, Some(&filter_mixed));
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].name, "test1");

        // 4. Filter string prefix bug (e.g. "a" should not match "a_other")
        let mut cases_prefix = vec![
            TestCase {
                name: "test_a".to_string(),
                image: TestImage {
                    relative_path: PathBuf::from("a/test1.png"),
                    absolute_path: PathBuf::from("/base/a/test1.png"),
                },
                rule: rule.clone(),
            },
            TestCase {
                name: "test_a_other".to_string(),
                image: TestImage {
                    relative_path: PathBuf::from("a_other/test1.png"),
                    absolute_path: PathBuf::from("/base/a_other/test1.png"),
                },
                rule,
            },
        ];
        let filter_prefix = vec![PathBuf::from("a")];
        filter_test_cases(&mut cases_prefix, Some(&filter_prefix));
        assert_eq!(cases_prefix.len(), 1);
        assert_eq!(cases_prefix[0].name, "test_a");
    }
}
