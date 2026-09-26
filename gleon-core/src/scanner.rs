//! File scanner and image decoder for visual regression tests.

use std::path::{Path, PathBuf};

use crate::{
    config::GleonConfig,
    naming::{normalize_test_name, validate_test_name},
    walk::build_globset,
};

/// Errors that can occur during visual regression testing files scanning.
#[derive(Debug, thiserror::Error)]
pub enum ScannerError {
    /// IO error during file or directory access.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Error compiling a glob pattern.
    #[error("Pattern compilation error: {0}")]
    Pattern(#[from] globset::Error),

    /// Invalid test name format.
    #[error("Invalid test name '{name}': {reason}")]
    InvalidTestName {
        /// The invalid test case name.
        name: String,
        /// The validation failure reason.
        reason: String,
    },
}

use std::borrow::Cow;

/// A single test screenshot file within a `TestCase`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestImage {
    /// Relative path from the base directory (e.g. "billing/stripe/form.png")
    pub relative_path: PathBuf,
    /// Absolute path to the file on disk
    pub absolute_path: PathBuf,
}

/// A single visual regression test case corresponding to one screenshot.
#[derive(Debug, Clone)]
pub struct TestCase {
    /// The test name (relative path without extension, e.g. "billing/stripe/form")
    pub name: String,
    /// The screenshot belonging to this test case
    pub image: TestImage,
    /// The configuration rule that matched this test case
    pub rule: std::sync::Arc<crate::config::ScreenshotRule>,
}

/// Scanner for visual regression test screenshots.
pub struct FileScanner;

impl FileScanner {
    /// Scans the workspace based on the rules in `GleonConfig` and a given base directory.
    ///
    /// # Errors
    /// Returns [`ScannerError::Pattern`] if any include/exclude glob fails to compile, or
    /// [`ScannerError::InvalidTestName`] if a derived test name fails validation.
    pub fn scan_workspace(
        config: &GleonConfig,
        base_dir: &Path,
    ) -> Result<Vec<TestCase>, ScannerError> {
        let exclude_set = build_globset(&config.exclude)?;

        let mut rule_sets = Vec::new();
        for rule in &config.screenshots {
            rule_sets.push((
                std::sync::Arc::new(rule.clone()),
                build_globset(&rule.include)?,
            ));
        }

        let walker = Self::build_walker(base_dir, &exclude_set);

        let mut temp_cases = std::collections::BTreeMap::<
            String,
            (TestImage, std::sync::Arc<crate::config::ScreenshotRule>),
        >::new();

        for entry_res in walker {
            let entry = match entry_res {
                Ok(e) => e,
                Err(err) => {
                    tracing::warn!("Skipping unreadable directory or path: {}", err);
                    continue;
                }
            };

            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }
            let path = entry.path();
            if !path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
            {
                continue;
            }

            let rel_path = match path.strip_prefix(base_dir) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("Failed to strip base_dir prefix from path: {}", e);
                    continue;
                }
            };
            let rel_path_str = Self::normalize_path_str(rel_path);

            if exclude_set.is_match(rel_path_str.as_ref()) {
                continue;
            }

            let matched_rule = rule_sets
                .iter()
                .find(|(_, inc_set)| inc_set.is_match(rel_path_str.as_ref()));

            if let Some((rule_arc, _)) = matched_rule {
                let path_without_ext = rel_path.with_extension("");
                let test_name_cow = Self::normalize_path_str(&path_without_ext);
                let test_name_norm = test_name_cow.as_ref();

                if temp_cases.contains_key(test_name_norm) {
                    tracing::warn!(
                        "Duplicate test name '{}' detected for relative path {:?}. Skipping duplicate.",
                        test_name_norm,
                        rel_path
                    );
                } else {
                    if let Err(reason) = validate_test_name(test_name_norm) {
                        return Err(ScannerError::InvalidTestName {
                            name: test_name_norm.to_string(),
                            reason: reason.to_string(),
                        });
                    }
                    temp_cases.insert(
                        test_name_norm.to_string(),
                        (
                            TestImage {
                                relative_path: rel_path.to_path_buf(),
                                absolute_path: path.to_path_buf(),
                            },
                            rule_arc.clone(),
                        ),
                    );
                }
            }
        }

        let cases = temp_cases
            .into_iter()
            .map(|(name, (image, rule))| TestCase { name, image, rule })
            .collect();
        Ok(cases)
    }

    /// Builds a `WalkBuilder` configured for gleon directory scanning.
    fn build_walker(base_dir: &Path, exclude_set: &globset::GlobSet) -> ignore::Walk {
        let exclude_for_filter = exclude_set.clone();
        let base_dir_for_filter = base_dir.to_path_buf();

        ignore::WalkBuilder::new(base_dir)
            .standard_filters(false)
            .filter_entry(move |entry| {
                if entry.file_type().is_some_and(|ft| ft.is_dir())
                    && entry
                        .file_name()
                        .to_str()
                        .is_some_and(crate::walk::is_default_pruned_dir)
                {
                    return false;
                }
                if !exclude_for_filter.is_empty()
                    && let Ok(rel_path) = entry.path().strip_prefix(&base_dir_for_filter)
                {
                    if rel_path.as_os_str().is_empty() {
                        return true;
                    }
                    let rel_path_str = match rel_path.to_str() {
                        Some(s)
                            if !s.contains('\\') && !s.bytes().any(|b| b.is_ascii_uppercase()) =>
                        {
                            Cow::Borrowed(s)
                        }
                        _ => Self::normalize_path_str(rel_path),
                    };
                    if exclude_for_filter.is_match(rel_path_str.as_ref()) {
                        return false;
                    }
                }
                true
            })
            .build()
    }

    /// Normalizes path separators and folds ASCII case into a canonical test identity.
    ///
    /// This is for building/looking up test identities (manifest keys), where case-insensitive
    /// matching is correct. It must **not** be used for real filesystem or Git index paths on
    /// case-sensitive systems — use [`crate::naming::normalize_path_separators`] there instead,
    /// which normalizes separators only.
    #[must_use]
    pub fn normalize_path_str(path: &Path) -> Cow<'_, str> {
        match path.to_string_lossy() {
            Cow::Borrowed(s) => normalize_test_name(s),
            Cow::Owned(s) => match normalize_test_name(&s) {
                Cow::Borrowed(_) => Cow::Owned(s),
                Cow::Owned(new_s) => Cow::Owned(new_s),
            },
        }
    }
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
    use crate::config::GlobPattern;

    /// Builds the `GleonConfig` equivalent of the old `scan_files(include, exclude, ..)` call,
    /// so the behaviours those tests pinned keep being exercised through the real entry point.
    fn config_from(include: &[GlobPattern], exclude: &[GlobPattern]) -> GleonConfig {
        GleonConfig {
            screenshots: vec![crate::config::ScreenshotRule {
                include: include.to_vec(),
                mode: gleon_engine::config::Mode::Pixel,
                diff: gleon_engine::config::DiffConfig::default(),
                masks: vec![],
            }],
            exclude: exclude.to_vec(),
            ..GleonConfig::default()
        }
    }
    use gleon_engine::MismatchDetail;

    use super::*;
    use crate::results::{TestCaseResult, TestImageResult};

    // Tiny 1x1 valid PNG bytes
    const VALID_PNG_BYTES: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn test_normalize_path_str() {
        let p1 = Path::new("billing/stripe/form.png");
        assert_eq!(
            FileScanner::normalize_path_str(p1),
            "billing/stripe/form.png"
        );

        let p2 = Path::new("billing\\stripe\\form.png");
        assert_eq!(
            FileScanner::normalize_path_str(p2),
            "billing/stripe/form.png"
        );
    }

    #[test]
    fn test_scan_workspace_success_and_corrupt() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path();

        // Create billing/stripe/form.png (valid)
        let billing_dir = base_path.join("billing").join("stripe");
        std::fs::create_dir_all(&billing_dir).unwrap();
        std::fs::write(billing_dir.join("form.png"), VALID_PNG_BYTES).unwrap();

        // Create settings/corrupt.png (invalid png)
        let settings_dir = base_path.join("settings");
        std::fs::create_dir_all(&settings_dir).unwrap();
        std::fs::write(settings_dir.join("corrupt.png"), b"not a png").unwrap();

        // Create ignored file (e.g. not a png)
        std::fs::write(billing_dir.join("notes.txt"), b"some text").unwrap();

        let include = vec![GlobPattern::new("**/*.png").unwrap()];
        let exclude = vec![];

        let cases =
            FileScanner::scan_workspace(&config_from(&include, &exclude), base_path).unwrap();

        // We expect two test cases: "billing/stripe/form" and "settings/corrupt"
        assert_eq!(cases.len(), 2);

        // First test case: billing/stripe
        assert_eq!(cases[0].name, "billing/stripe/form");
        assert_eq!(
            cases[0].image.relative_path,
            Path::new("billing/stripe/form.png")
        );
        assert_eq!(
            cases[1].image.relative_path,
            Path::new("settings/corrupt.png")
        );
    }

    #[test]
    fn test_scan_workspace_with_excludes() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path();

        let billing_dir = base_path.join("billing").join("stripe");
        std::fs::create_dir_all(&billing_dir).unwrap();
        std::fs::write(billing_dir.join("form.png"), VALID_PNG_BYTES).unwrap();

        let settings_dir = base_path.join("settings");
        std::fs::create_dir_all(&settings_dir).unwrap();
        std::fs::write(settings_dir.join("profile.png"), VALID_PNG_BYTES).unwrap();

        let include = vec![GlobPattern::new("**/*.png").unwrap()];
        // Exclude everything under settings/
        let exclude = vec![GlobPattern::new("settings/**/*.png").unwrap()];

        let cases =
            FileScanner::scan_workspace(&config_from(&include, &exclude), base_path).unwrap();

        // Only billing/stripe/form should remain
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].name, "billing/stripe/form");
    }

    #[test]
    fn test_scan_workspace_with_uppercase_excludes() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path();

        let billing_dir = base_path.join("Billing").join("Stripe");
        std::fs::create_dir_all(&billing_dir).unwrap();
        std::fs::write(billing_dir.join("Form.png"), VALID_PNG_BYTES).unwrap();

        let settings_dir = base_path.join("Settings");
        std::fs::create_dir_all(&settings_dir).unwrap();
        std::fs::write(settings_dir.join("Profile.png"), VALID_PNG_BYTES).unwrap();

        let include = vec![GlobPattern::new("**/*.png").unwrap()];
        // Exclude with lowercase glob matching uppercase folder
        let exclude = vec![GlobPattern::new("settings/**/*.png").unwrap()];

        let cases =
            FileScanner::scan_workspace(&config_from(&include, &exclude), base_path).unwrap();

        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].name, "billing/stripe/form");
    }

    #[test]
    fn test_scan_workspace_empty_results() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path();

        let billing_dir = base_path.join("billing");
        std::fs::create_dir_all(&billing_dir).unwrap();
        std::fs::write(billing_dir.join("notes.txt"), b"not a png").unwrap();

        let include = vec![GlobPattern::new("**/*.png").unwrap()];
        let exclude = vec![];

        let cases =
            FileScanner::scan_workspace(&config_from(&include, &exclude), base_path).unwrap();
        assert!(
            cases.is_empty(),
            "Expected empty results when no PNG files match include patterns"
        );
    }

    #[test]
    fn test_scan_workspace_include_mismatch() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path();

        let billing_dir = base_path.join("billing");
        std::fs::create_dir_all(&billing_dir).unwrap();
        // This is a PNG but won't match our specific include pattern "settings/**/*.png"
        std::fs::write(billing_dir.join("form.png"), VALID_PNG_BYTES).unwrap();

        let include = vec![GlobPattern::new("settings/**/*.png").unwrap()];
        let exclude = vec![];

        let cases =
            FileScanner::scan_workspace(&config_from(&include, &exclude), base_path).unwrap();
        assert!(
            cases.is_empty(),
            "Expected empty results when PNG does not match include set"
        );
    }

    #[test]
    fn test_scan_workspace() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path();

        let billing_dir = base_path.join("billing");
        std::fs::create_dir_all(&billing_dir).unwrap();
        std::fs::write(billing_dir.join("form.png"), VALID_PNG_BYTES).unwrap();

        // Construct mock GleonConfig
        let raw_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include: "billing/**/*.png"
exclude:
  - "**/corrupt.png"
"#;
        let config: GleonConfig = serde_yaml::from_str(raw_yaml).unwrap();

        let cases = FileScanner::scan_workspace(&config, base_path).unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].name, "billing/form");
        assert_eq!(cases[0].image.relative_path, Path::new("billing/form.png"));
    }

    #[test]
    fn test_normalize_path_str_backslash() {
        let backslash_path = Path::new("billing\\form.png");
        let normalized = FileScanner::normalize_path_str(backslash_path);
        assert_eq!(normalized, "billing/form.png");
    }

    #[test]
    fn test_scan_workspace_error() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path();

        // Invalid directory name (space)
        let billing_dir = base_path.join("billing dir");
        std::fs::create_dir_all(&billing_dir).unwrap();
        std::fs::write(billing_dir.join("form.png"), VALID_PNG_BYTES).unwrap();

        let raw_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include: "billing dir/**/*.png"
"#;
        let config: GleonConfig = serde_yaml::from_str(raw_yaml).unwrap();

        let result = FileScanner::scan_workspace(&config, base_path);
        assert!(result.is_err());
        assert!(matches!(
            result.err().unwrap(),
            ScannerError::InvalidTestName { .. }
        ));
    }

    #[test]
    fn test_scan_workspace_multiple_rules_same_directory() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path();

        let billing_dir = base_path.join("billing");
        std::fs::create_dir_all(&billing_dir).unwrap();
        std::fs::write(billing_dir.join("form.png"), VALID_PNG_BYTES).unwrap();
        std::fs::write(billing_dir.join("receipt.png"), VALID_PNG_BYTES).unwrap();

        // Construct mock GleonConfig with two rules targeting different files in the same directory
        let raw_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include: "billing/form.png"
    mode: pixel
  - include: "billing/receipt.png"
    mode: ssim
"#;
        let config: GleonConfig = serde_yaml::from_str(raw_yaml).unwrap();

        let cases = FileScanner::scan_workspace(&config, base_path).unwrap();
        // We should get 2 separate TestCases for the "billing" directory
        // because they matched different rules.
        assert_eq!(cases.len(), 2);

        let pixel_case = cases
            .iter()
            .find(|c| c.rule.mode == gleon_engine::config::Mode::Pixel)
            .unwrap();
        let ssim_case = cases
            .iter()
            .find(|c| c.rule.mode == gleon_engine::config::Mode::Ssim)
            .unwrap();

        assert_eq!(pixel_case.name, "billing/form");
        assert_eq!(
            pixel_case.image.relative_path,
            Path::new("billing/form.png")
        );

        assert_eq!(ssim_case.name, "billing/receipt");
        assert_eq!(
            ssim_case.image.relative_path,
            Path::new("billing/receipt.png")
        );
    }

    #[test]
    fn test_scan_invalid_test_name_returns_error() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path();

        // Folder with invalid character (space)
        let billing_dir = base_path.join("billing dir");
        std::fs::create_dir_all(&billing_dir).unwrap();
        std::fs::write(billing_dir.join("form.png"), VALID_PNG_BYTES).unwrap();

        let include = vec![GlobPattern::new("**/*.png").unwrap()];
        let exclude = vec![];

        let result = FileScanner::scan_workspace(&config_from(&include, &exclude), base_path);
        assert!(result.is_err());
        assert!(matches!(
            result.err().unwrap(),
            ScannerError::InvalidTestName { .. }
        ));
    }

    #[cfg(all(unix, not(miri)))]
    fn make_unreadable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(path, perms).unwrap();
    }

    #[cfg(all(unix, not(miri)))]
    fn make_readable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    #[test]
    #[cfg(all(unix, not(miri)))]
    fn test_unreadable_directory_ignored() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path();

        let unreadable_dir = base_path.join("unreadable");
        std::fs::create_dir(&unreadable_dir).unwrap();
        std::fs::write(unreadable_dir.join("image.png"), VALID_PNG_BYTES).unwrap();

        let readable_dir = base_path.join("readable");
        std::fs::create_dir(&readable_dir).unwrap();
        std::fs::write(readable_dir.join("image.png"), VALID_PNG_BYTES).unwrap();

        make_unreadable(&unreadable_dir);

        let include = vec![GlobPattern::new("**/*.png").unwrap()];
        let exclude = vec![];

        let result = FileScanner::scan_workspace(&config_from(&include, &exclude), base_path);

        // Always restore permissions before running assertions!
        make_readable(&unreadable_dir);

        let cases = result.unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].name, "readable/image");
    }

    #[test]
    fn test_derived_traits() {
        // This test ensures that derived traits (like Debug) are executed.
        let io_err = ScannerError::Io(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert!(!format!("{io_err:?}").is_empty());
        assert!(!format!("{io_err}").is_empty());

        let invalid_err = ScannerError::InvalidTestName {
            name: "Foo".to_string(),
            reason: "UpperCase".to_string(),
        };
        assert!(!format!("{invalid_err:?}").is_empty());
        assert!(!format!("{invalid_err}").is_empty());

        let pattern_err = ScannerError::Pattern(globset::Glob::new("[").unwrap_err());
        assert!(!format!("{pattern_err:?}").is_empty());

        let mismatch_detail = MismatchDetail::Pixel { diff_count: 42 };
        assert!(!format!("{mismatch_detail:?}").is_empty());
        assert_eq!(mismatch_detail, MismatchDetail::Pixel { diff_count: 42 });

        let ssim_detail = MismatchDetail::Ssim {
            ssim_score: 0.99,
            min_ssim: 0.99,
            max_excess: 0.0,
            region: None,
        };
        assert!(!format!("{ssim_detail:?}").is_empty());

        let image_res = TestImageResult::DecodeError {
            relative_path: PathBuf::from("a.png"),
            error: "bad data".to_string(),
        };
        assert!(!format!("{image_res:?}").is_empty());

        let tc_res = TestCaseResult {
            name: "test".to_string(),
            result: image_res,
        };
        assert!(!format!("{tc_res:?}").is_empty());
    }

    #[test]
    fn test_scan_workspace_case_insensitive_extension() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path();

        let billing_dir = base_path.join("billing");
        std::fs::create_dir_all(&billing_dir).unwrap();
        std::fs::write(billing_dir.join("form.PNG"), VALID_PNG_BYTES).unwrap();
        std::fs::write(billing_dir.join("profile.PnG"), VALID_PNG_BYTES).unwrap();

        let include = vec![GlobPattern::new("**/*.png").unwrap()];
        let exclude = vec![];

        let cases =
            FileScanner::scan_workspace(&config_from(&include, &exclude), base_path).unwrap();
        assert_eq!(
            cases.len(),
            2,
            "Expected to find both uppercase and mixed-case PNG files"
        );
    }

    #[test]
    fn test_test_image_and_case_clone() {
        let test_image = TestImage {
            relative_path: PathBuf::from("rel.png"),
            absolute_path: PathBuf::from("abs.png"),
        };

        // Ensure they are cloneable
        let cloned_img = test_image.clone();
        assert_eq!(cloned_img.relative_path, test_image.relative_path);

        let test_case = TestCase {
            name: "test_case".to_string(),
            image: test_image,
            rule: std::sync::Arc::new(crate::config::ScreenshotRule {
                include: vec![],
                mode: gleon_engine::config::Mode::Pixel,
                diff: gleon_engine::config::DiffConfig {
                    threshold: 0.0,
                    anti_alias: false,
                    min_similarity: 0.99,
                    color_tolerance: 8.0,
                },
                masks: vec![],
            }),
        };

        let cloned_case = test_case.clone();
        assert_eq!(cloned_case.name, test_case.name);
        assert_eq!(
            cloned_case.image.relative_path,
            test_case.image.relative_path
        );
    }

    #[test]
    fn test_normalize_separators() {
        let p1 = Path::new("billing/stripe/form.png");
        let res1 = FileScanner::normalize_path_str(p1);
        assert_eq!(res1, "billing/stripe/form.png");
        assert!(matches!(res1, Cow::Borrowed(_)));

        let p2 = Path::new("clean_path.png");
        let res2 = FileScanner::normalize_path_str(p2);
        assert_eq!(res2, "clean_path.png");
        assert!(matches!(res2, Cow::Borrowed(_)));
    }

    #[test]
    fn test_scan_workspace_nested_entries_vacant_and_occupied() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_path = temp_dir.path();

        let billing_dir = base_path.join("billing").join("stripe");
        std::fs::create_dir_all(&billing_dir).unwrap();
        std::fs::write(billing_dir.join("form1.png"), VALID_PNG_BYTES).unwrap();
        std::fs::write(billing_dir.join("form2.png"), VALID_PNG_BYTES).unwrap();

        let raw_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include: "billing/stripe/*.png"
"#;
        let config: GleonConfig = serde_yaml::from_str(raw_yaml).unwrap();

        let cases = FileScanner::scan_workspace(&config, base_path).unwrap();
        assert_eq!(cases.len(), 2);
    }
}
