//! Staging operation for processing, masking, and persisting baseline screenshots.

use std::path::PathBuf;

use thiserror::Error;

use crate::{
    context::ResolvedContext,
    manifest::{ManifestError, WorkspaceIndex},
    ops::common::{
        CoreError, build_manifest, ensure_initialized, hash_and_measure, index_keys_missing_from,
    },
    scanner::FileScanner,
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
pub(crate) fn filter_test_cases(
    test_cases: &mut Vec<crate::scanner::TestCase>,
    filter_paths: Option<&[PathBuf]>,
) {
    if let Some(filters) = filter_paths {
        let normalized_filters: Vec<_> = filters
            .iter()
            .map(|f| (f, FileScanner::normalize_path_str(f).into_owned()))
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
/// # Errors
///
/// Returns an error if the workspace is not initialized, if the platform key cannot be
/// resolved, if screenshots cannot be scanned, if any screenshot fails to decode, or if
/// reading/writing manifests, blobs, or other filesystem data fails.
pub fn stage_workspace(
    context: &ResolvedContext,
    filter_paths: Option<&[PathBuf]>,
) -> Result<StageResult, StageError> {
    use rayon::prelude::*;

    struct StagedItem {
        case_name: String,
        sha256_hex: String,
        phash_str: String,
        width: u32,
        height: u32,
    }

    let paths = ensure_initialized(&context.base_dir)?;
    let platform_key = crate::ops::common::platform_key(context)?;

    let blobs_dir = paths.blob_scheme_dir("sha256");
    let manifests_dir = paths.manifests_dir(&platform_key);
    std::fs::create_dir_all(&blobs_dir).map_err(CoreError::Io)?;
    std::fs::create_dir_all(&manifests_dir).map_err(CoreError::Io)?;

    let config = context.config.clone().unwrap_or_default();

    let mut test_cases =
        FileScanner::scan_workspace(&config, &context.base_dir).map_err(CoreError::Scanner)?;

    filter_test_cases(&mut test_cases, filter_paths);

    let pb = crate::ui::create_progress_bar(test_cases.len() as u64);
    pb.set_message("Staging screenshots...");

    let mut workspace_index = WorkspaceIndex::load(&manifests_dir).map_err(CoreError::Manifest)?;

    let processed_results: Result<Vec<StagedItem>, StageError> = test_cases
        .into_par_iter()
        .map(|case| {
            let png_bytes = std::fs::read(&case.image.absolute_path).map_err(CoreError::Io)?;
            let (sha256_hex, phash_str, width, height) =
                hash_and_measure(&png_bytes).map_err(|e| match e {
                    ManifestError::Image(source) => StageError::ImageDecode {
                        path: case.image.relative_path.clone(),
                        source,
                    },
                    other => CoreError::Manifest(other).into(),
                })?;

            // Save blob to .gleon/blobs/sha256/<sha256_hex>
            let blob_path = blobs_dir.join(&sha256_hex);
            crate::io::save_file_atomically(&blob_path, &png_bytes).map_err(CoreError::from)?;

            pb.inc(1);

            Ok(StagedItem {
                case_name: case.name,
                sha256_hex,
                phash_str,
                width,
                height,
            })
        })
        .collect();

    let processed_results = match processed_results {
        Ok(res) => {
            pb.finish_and_clear();
            res
        }
        Err(e) => {
            pb.finish_and_clear();
            return Err(e);
        }
    };

    // Clean up orphan manifests when performing a full workspace stage (no path filters)
    if filter_paths.is_none() {
        let scanned_names: std::collections::HashSet<_> = processed_results
            .iter()
            .map(|item| item.case_name.as_str())
            .collect();
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

    for item in processed_results {
        let new_manifest =
            build_manifest(&item.sha256_hex, &item.phash_str, item.width, item.height)?;

        let is_unchanged = workspace_index
            .get(&item.case_name)
            .is_some_and(|existing| existing == &new_manifest);

        if !is_unchanged {
            workspace_index
                .save_test(&manifests_dir, &item.case_name, &new_manifest)
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

        let err8: StageError = CoreError::Io(std::io::Error::other("io test")).into();
        assert!(err8.to_string().contains("IO error"));
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
            mode: gleon_engine::config::Mode::Pixel,
            diff: gleon_engine::config::DiffConfig::default(),
            masks: vec![],
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
