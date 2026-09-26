//! Status operation for categorizing workspace screenshots against baseline manifests.

use std::path::PathBuf;

use serde::Serialize;
use thiserror::Error;

use crate::{
    context::ResolvedContext,
    ops::common::{
        CoreError, ensure_initialized, index_keys_missing_from, load_config_and_scan,
        load_merged_index_with_fallback, platform_key, sha256_hex_matches,
    },
};

/// Errors that can occur during status evaluation.
#[derive(Debug, Error)]
pub enum StatusError {
    /// Error shared across `ops::*` operations.
    #[error(transparent)]
    Core(#[from] CoreError),
}

/// Grouped result of evaluating status across the workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct StatusReport {
    /// Screenshots present on disk but not yet tracked in the manifest.
    pub added: Vec<PathBuf>,
    /// Tracked screenshots whose content differs from the manifest.
    pub modified: Vec<PathBuf>,
    /// Manifest entries whose screenshot is missing on disk.
    pub deleted: Vec<PathBuf>,
}

impl StatusReport {
    /// Returns true if there are no added, modified, or deleted screenshots.
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.deleted.is_empty()
    }

    /// Formats the report as pretty-printed JSON.
    ///
    /// # Errors
    ///
    /// Returns an error if the report fails to serialize to JSON (not expected in practice,
    /// since all fields are simple serializable types).
    pub fn format_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Formats the report as human-readable text.
    #[must_use]
    pub fn format_text(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        if self.is_clean() {
            out.push_str("Nothing to report. Workspace is up to date.\n");
            return out;
        }

        if !self.added.is_empty() {
            out.push_str("Added:\n");
            for path in &self.added {
                let _ = writeln!(out, "  {}", path.display());
            }
        }

        if !self.modified.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("Modified:\n");
            for path in &self.modified {
                let _ = writeln!(out, "  {}", path.display());
            }
        }

        if !self.deleted.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("Deleted:\n");
            for path in &self.deleted {
                let _ = writeln!(out, "  {}", path.display());
            }
        }

        out
    }
}

/// Classifies one scanned test case against `workspace_index`: `(Some(path), None)` if it's
/// untracked (added), `(None, Some(path))` if its content differs from the baseline once masks
/// are applied (modified), or `(None, None)` if unchanged.
///
/// Pulled out of [`check_status`]'s parallel scan so the per-case comparison logic (hash/blob
/// lookup, mask-aware image diffing) reads as its own unit rather than an ~80-line closure body.
fn classify_test_case(
    case: &crate::scanner::TestCase,
    workspace_index: &crate::manifest::WorkspaceIndex,
    blobs_root: &std::path::Path,
) -> Result<(Option<PathBuf>, Option<PathBuf>), StatusError> {
    let img = &case.image;

    let Some(manifest) = workspace_index.get(&case.name) else {
        return Ok((Some(img.relative_path.clone()), None));
    };

    let raw_bytes = std::fs::read(&img.absolute_path).map_err(CoreError::Io)?;
    let is_unchanged = if manifest.hash.scheme() == "sha256" {
        if sha256_hex_matches(&manifest.hash, &raw_bytes) {
            crate::storage::has_usable_local_blob(blobs_root, &manifest.hash)
        } else {
            false
        }
    } else {
        let baseline_blob_path = crate::storage::local_blob_path(blobs_root, &manifest.hash);
        match std::fs::read(&baseline_blob_path) {
            Ok(b_bytes) => raw_bytes == b_bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(CoreError::Io(e).into()),
        }
    };

    if is_unchanged {
        crate::manifest::SingleTestManifest::validate_image_bytes(&raw_bytes)
            .map_err(CoreError::Manifest)?;
        return Ok((None, None));
    }

    let matched_zones = case.rule.matched_mask_zones(&img.relative_path);
    if matched_zones.is_empty() {
        return Ok((None, Some(img.relative_path.clone())));
    }

    let baseline_blob_path = crate::storage::local_blob_path(blobs_root, &manifest.hash);
    let b_bytes = match std::fs::read(&baseline_blob_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((None, Some(img.relative_path.clone())));
        }
        Err(e) => return Err(CoreError::Io(e).into()),
    };

    // Same resource-limited decoder as `gleon diff` and the Flutter package.
    let decode = |label: &str, bytes: &[u8]| {
        gleon_engine::decode::decode_rgba(bytes).map_err(|e| {
            CoreError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Invalid {label} image: {e}"),
            ))
        })
    };
    let mut b_rgba = decode("baseline", &b_bytes)?;
    let mut a_rgba = decode("actual", &raw_bytes)?;
    gleon_engine::masking::apply_masks(&mut b_rgba, &matched_zones);
    gleon_engine::masking::apply_masks(&mut a_rgba, &matched_zones);
    if b_rgba == a_rgba {
        return Ok((None, None));
    }

    Ok((None, Some(img.relative_path.clone())))
}

/// Evaluates status for the workspace at `base_dir`.
///
/// # Errors
///
/// Returns an error if the workspace is not initialized, if the platform key cannot be
/// resolved, if manifests or workspace configuration fail to load, if screenshots cannot be
/// scanned, or if reading/decoding actual or baseline images fails.
pub fn check_status(context: &ResolvedContext) -> Result<StatusReport, StatusError> {
    use rayon::prelude::*;

    let paths = ensure_initialized(&context.base_dir)?;
    let platform_key = platform_key(context)?;

    let workspace_index = load_merged_index_with_fallback(
        &paths,
        &platform_key,
        context.fallback_platform_key.as_deref(),
    )?;

    let blobs_root = paths.blobs_root();

    // Scan workspace screenshots
    let test_cases = load_config_and_scan(context)?;

    let (mut added, mut modified) = test_cases
        .par_iter()
        .map(|case| classify_test_case(case, &workspace_index, &blobs_root))
        .try_fold(
            || (Vec::new(), Vec::new()),
            |mut acc, item| -> Result<_, StatusError> {
                let (opt_add, opt_mod) = match item {
                    Ok(val) => val,
                    Err(e) => return Err(e),
                };
                if let Some(p) = opt_add {
                    acc.0.push(p);
                }
                if let Some(p) = opt_mod {
                    acc.1.push(p);
                }
                Ok(acc)
            },
        )
        .try_reduce(
            || (Vec::new(), Vec::new()),
            |mut a, b| -> Result<_, StatusError> {
                a.0.extend(b.0);
                a.1.extend(b.1);
                Ok(a)
            },
        )?;

    let seen_test_cases: std::collections::HashSet<&str> =
        test_cases.iter().map(|c| c.name.as_str()).collect();

    let mut deleted = Vec::new();

    // Identify deleted test cases (staged in index but no longer present on disk)
    for staged_name in index_keys_missing_from(&workspace_index, &seen_test_cases) {
        let mut p = String::with_capacity(staged_name.len() + 4);
        p.push_str(staged_name);
        p.push_str(".png");
        deleted.push(PathBuf::from(p));
    }

    added.sort();
    modified.sort();
    deleted.sort();

    Ok(StatusReport {
        added,
        modified,
        deleted,
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
        config::ConfigError,
        context::ContextError,
        manifest::{ManifestError, WorkspaceIndex},
        scanner::ScannerError,
    };

    #[test]
    fn test_status_error_display() {
        let err1: StatusError = CoreError::NotInitialized.into();
        assert!(err1.to_string().contains("not initialized"));

        let err2: StatusError = CoreError::Context(ContextError::Platform(
            crate::platform::PlatformError::InvalidSegment("test".to_string()),
        ))
        .into();
        assert!(err2.to_string().contains("Context resolution error"));

        let err3: StatusError = CoreError::Scanner(ScannerError::InvalidTestName {
            name: "bad/name".to_string(),
            reason: "reason".to_string(),
        })
        .into();
        assert!(err3.to_string().contains("Scanner error"));

        let err4: StatusError =
            CoreError::Config(ConfigError::Validation("bad config".to_string())).into();
        assert!(err4.to_string().contains("Config error"));

        let err5: StatusError =
            CoreError::Manifest(ManifestError::Validation("bad manifest".to_string())).into();
        assert!(err5.to_string().contains("Manifest error"));

        let err6: StatusError = CoreError::Io(std::io::Error::other("io test")).into();
        assert!(err6.to_string().contains("IO error"));
    }

    #[test]
    fn test_status_report_format() {
        let report = StatusReport {
            added: vec![PathBuf::from("a.png")],
            modified: vec![PathBuf::from("b.png")],
            deleted: vec![PathBuf::from("c.png")],
        };
        assert!(!report.is_clean());
        let text = report.format_text();
        assert!(text.contains("Added:"));
        assert!(text.contains("a.png"));
        assert!(text.contains("Modified:"));
        assert!(text.contains("b.png"));
        assert!(text.contains("Deleted:"));
        assert!(text.contains("c.png"));

        let clean = StatusReport::default();
        assert!(clean.is_clean());
        assert!(clean.format_text().contains("Nothing to report"));

        let json = report.format_json().unwrap();
        assert!(json.contains("a.png"));
    }

    #[test]
    fn test_status_deleted_dotted_test_name() {
        let report = StatusReport {
            added: vec![],
            modified: vec![],
            deleted: vec![PathBuf::from("auth/user.v2.png")],
        };
        let text = report.format_text();
        assert!(text.contains("auth/user.v2.png"));
    }

    #[test]
    fn test_status_detects_deleted_dotted_test_case() {
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
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
        let manifest = crate::manifest::SingleTestManifest::new(hash, phash, 1, 1).unwrap();
        manifest
            .save(manifests_dir.join("auth").join("user.v2.json"))
            .unwrap();

        let report = check_status(&ctx).unwrap();
        assert_eq!(report.deleted, vec![PathBuf::from("auth/user.v2.png")]);
    }

    #[test]
    fn test_status_missing_baseline_blob_returns_modified() {
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            ..Default::default()
        };
        let plat_key = ctx.platform.to_key().unwrap();
        let manifests_dir = gleon_dir.join("manifests").join(&plat_key);
        std::fs::create_dir_all(&manifests_dir).unwrap();

        // Create actual image on disk
        let screenshots_dir = temp.path().join("screenshots");
        std::fs::create_dir_all(&screenshots_dir).unwrap();
        let img = image::RgbaImage::new(10, 10);
        let actual_path = screenshots_dir.join("login.png");
        img.save(&actual_path).unwrap();

        // Create manifest pointing to a non-existent blob
        let hash = crate::manifest::ImageHash::new(
            "sha256",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        let phash = crate::manifest::ImageHash::new("dhash", "0000000000000000").unwrap();
        let manifest = crate::manifest::SingleTestManifest::new(hash, phash, 10, 10).unwrap();
        manifest
            .save(manifests_dir.join("screenshots").join("login.json"))
            .unwrap();

        let report = check_status(&ctx).unwrap();
        assert_eq!(
            report.modified,
            vec![PathBuf::from("screenshots/login.png")]
        );
    }

    #[test]
    fn test_status_fallback_platform() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".gleon")).unwrap();
        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            platform: crate::platform::PlatformInfo {
                os: "unknown_os".to_string(),
                arch: None,
                renderer: None,
                labels: std::collections::BTreeMap::new(),
            },
            ..Default::default()
        };

        let report = check_status(&ctx).unwrap();
        assert!(report.is_clean());
    }

    #[test]
    fn test_status_missing_blob_with_masks() {
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        let ctx_temp = ResolvedContext::default();

        let platform_key = ctx_temp.platform.to_key().unwrap();
        let manifests_dir = gleon_dir.join("manifests").join(&platform_key);
        std::fs::create_dir_all(&manifests_dir).unwrap();

        let mut index = WorkspaceIndex::new();
        let hash_val = "1111111111111111111111111111111111111111111111111111111111111111";
        index
            .save_test(
                &manifests_dir,
                "test",
                &crate::manifest::SingleTestManifest {
                    schema_version: 1,
                    hash: crate::manifest::ImageHash::new("sha256", hash_val).unwrap(),
                    phash: crate::manifest::ImageHash::new("dhash", "2222222222222222").unwrap(),
                    width: 1,
                    height: 1,
                },
            )
            .unwrap();

        let img = image::RgbaImage::new(10, 10);
        img.save(temp.path().join("test.png")).unwrap();

        // Define a mask so status.rs attempts to read the baseline blob
        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            config: Some(crate::config::GleonConfig {
                screenshots: vec![crate::config::ScreenshotRule {
                    include: vec![crate::config::GlobPattern::new("**/*.png").unwrap()],
                    mode: gleon_engine::config::Mode::Pixel,
                    diff: gleon_engine::config::DiffConfig::default(),
                    masks: vec![crate::config::MaskRule {
                        path: crate::config::GlobPattern::new("**/*.png").unwrap(),
                        zones: vec![gleon_engine::config::Zone {
                            x: 0,
                            y: 0,
                            width: gleon_engine::config::Dimension::Pixels(1),
                            height: gleon_engine::config::Dimension::Pixels(1),
                        }],
                    }],
                }],
                ..Default::default()
            }),
            ..Default::default()
        };

        // Blob doesn't exist, so we expect Modified (NotFound branch lines 175-178)
        let res = check_status(&ctx);
        let res_unwrapped = res.unwrap();
        assert_eq!(res_unwrapped.modified.len(), 1);

        // Now create a CORRUPT baseline blob (lines 181-189)
        let blob_dir = gleon_dir.join("blobs").join("sha256");
        std::fs::create_dir_all(&blob_dir).unwrap();
        std::fs::write(blob_dir.join(hash_val), "fake png data").unwrap();

        let res2 = check_status(&ctx);
        assert!(matches!(res2, Err(StatusError::Core(CoreError::Io(_)))));
    }

    #[test]
    fn test_status_invalid_platform_key() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".gleon")).unwrap();

        // Construct an invalid platform info that fails to_key() (e.g. empty os and version)
        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            platform: crate::platform::PlatformInfo {
                os: String::new(),
                arch: None,
                renderer: None,
                labels: std::collections::BTreeMap::new(),
            },
            ..Default::default()
        };
        let res = check_status(&ctx);
        assert!(matches!(
            res,
            Err(StatusError::Core(CoreError::Context(
                ContextError::Platform(_)
            )))
        ));
    }

    #[test]
    fn test_status_non_sha256_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            ..ResolvedContext::default()
        };

        let platform_key = ctx.platform.to_key().unwrap();
        let manifests_dir = gleon_dir.join("manifests").join(&platform_key);
        std::fs::create_dir_all(&manifests_dir).unwrap();

        let img = image::RgbaImage::new(10, 10);
        let mut img_bytes = Vec::new();
        img.write_to(
            &mut std::io::Cursor::new(&mut img_bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
        std::fs::write(temp.path().join("test.png"), &img_bytes).unwrap();

        let hash_val = hex::encode(sha2::Sha512::digest(&img_bytes));

        let mut index = WorkspaceIndex::new();
        index
            .save_test(
                &manifests_dir,
                "test",
                &crate::manifest::SingleTestManifest {
                    schema_version: 1,
                    hash: crate::manifest::ImageHash::new("sha512", &hash_val).unwrap(),
                    phash: crate::manifest::ImageHash::new("dhash", "2222222222222222").unwrap(),
                    width: 10,
                    height: 10,
                },
            )
            .unwrap();

        let blob_dir = gleon_dir.join("blobs").join("sha512");
        std::fs::create_dir_all(&blob_dir).unwrap();
        std::fs::write(blob_dir.join(&hash_val), &img_bytes).unwrap();

        let res = check_status(&ctx).unwrap();
        assert!(res.is_clean());
    }

    #[test]
    fn test_status_sha256_missing_blob() {
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            ..ResolvedContext::default()
        };

        let platform_key = ctx.platform.to_key().unwrap();
        let manifests_dir = gleon_dir.join("manifests").join(&platform_key);
        std::fs::create_dir_all(&manifests_dir).unwrap();

        let img = image::RgbaImage::new(10, 10);
        let mut img_bytes = Vec::new();
        img.write_to(
            &mut std::io::Cursor::new(&mut img_bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
        std::fs::write(temp.path().join("test.png"), &img_bytes).unwrap();

        let hash_val = hex::encode(sha2::Sha256::digest(&img_bytes));

        let mut index = WorkspaceIndex::new();
        index
            .save_test(
                &manifests_dir,
                "test",
                &crate::manifest::SingleTestManifest {
                    schema_version: 1,
                    hash: crate::manifest::ImageHash::new("sha256", &hash_val).unwrap(),
                    phash: crate::manifest::ImageHash::new("dhash", "2222222222222222").unwrap(),
                    width: 10,
                    height: 10,
                },
            )
            .unwrap();

        // Deliberately do NOT create the blob file.
        // Even though the actual image matches the sha256 in the manifest,
        // status should return modified because the baseline blob is unusable.
        let res = check_status(&ctx).unwrap();
        assert!(!res.is_clean());
        assert_eq!(res.modified.len(), 1);
        assert_eq!(res.modified[0].to_string_lossy(), "test.png");
    }

    #[test]
    fn test_status_sparse_fallback_overlay() {
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

        // Create images on disk: t1 (clean override), t2 (clean fallback), t3 (added)
        let img1 = image::RgbaImage::new(10, 10);
        img1.save(base_path.join("t1.png")).unwrap();
        let img1_bytes = std::fs::read(base_path.join("t1.png")).unwrap();
        let img1_sha = hex::encode(sha2::Sha256::digest(&img1_bytes));

        let img2 = image::RgbaImage::new(20, 20);
        img2.save(base_path.join("t2.png")).unwrap();
        let img2_bytes = std::fs::read(base_path.join("t2.png")).unwrap();
        let img2_sha = hex::encode(sha2::Sha256::digest(&img2_bytes));

        let img3 = image::RgbaImage::new(30, 30);
        img3.save(base_path.join("t3.png")).unwrap();

        let blobs_dir = gleon_dir.join("blobs").join("sha256");
        std::fs::create_dir_all(&blobs_dir).unwrap();
        std::fs::write(blobs_dir.join(&img1_sha), &img1_bytes).unwrap();
        std::fs::write(blobs_dir.join(&img2_sha), &img2_bytes).unwrap();

        let dhash = crate::manifest::ImageHash::new("dhash", "0000000000000000").unwrap();

        // 1. Fallback manifests (macos) contains t1 (dummy), t2 (clean), t4 (deleted)
        let macos_manifests = gleon_dir.join("manifests").join(macos_key);
        std::fs::create_dir_all(&macos_manifests).unwrap();
        let dummy_sha = "0".repeat(64);
        let m_t1 = crate::manifest::SingleTestManifest::new(
            crate::manifest::ImageHash::new("sha256", &dummy_sha).unwrap(),
            dhash.clone(),
            10,
            10,
        )
        .unwrap();
        m_t1.save(macos_manifests.join("t1.json")).unwrap();

        let m_t2 = crate::manifest::SingleTestManifest::new(
            crate::manifest::ImageHash::new("sha256", &img2_sha).unwrap(),
            dhash.clone(),
            20,
            20,
        )
        .unwrap();
        m_t2.save(macos_manifests.join("t2.json")).unwrap();

        let m_t4 = crate::manifest::SingleTestManifest::new(
            crate::manifest::ImageHash::new("sha256", &dummy_sha).unwrap(),
            dhash.clone(),
            40,
            40,
        )
        .unwrap();
        m_t4.save(macos_manifests.join("t4.json")).unwrap();

        // 2. Linux manifests contains ONLY t1 (override matching img1)
        let linux_manifests = gleon_dir.join("manifests").join(linux_key);
        std::fs::create_dir_all(&linux_manifests).unwrap();
        let l_t1 = crate::manifest::SingleTestManifest::new(
            crate::manifest::ImageHash::new("sha256", &img1_sha).unwrap(),
            dhash,
            10,
            10,
        )
        .unwrap();
        l_t1.save(linux_manifests.join("t1.json")).unwrap();

        let status = check_status(&ctx).unwrap();
        assert_eq!(status.added, vec![PathBuf::from("t3.png")]);
        assert!(status.modified.is_empty());
        assert_eq!(status.deleted, vec![PathBuf::from("t4.png")]);
    }

    #[test]
    fn test_status_with_masks_matching_and_differing() {
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        let ctx_temp = ResolvedContext::default();

        let platform_key = ctx_temp.platform.to_key().unwrap();
        let manifests_dir = gleon_dir.join("manifests").join(&platform_key);
        let blobs_dir = gleon_dir.join("blobs").join("sha256");
        std::fs::create_dir_all(&manifests_dir).unwrap();
        std::fs::create_dir_all(&blobs_dir).unwrap();

        // Create a 10x10 white image for baseline
        let mut baseline_img = image::RgbaImage::new(10, 10);
        for pixel in baseline_img.pixels_mut() {
            *pixel = image::Rgba([255, 255, 255, 255]);
        }
        let baseline_path = temp.path().join("baseline.png");
        baseline_img.save(&baseline_path).unwrap();
        let baseline_bytes = std::fs::read(&baseline_path).unwrap();
        let (baseline_hash, ..) = crate::ops::common::hash_and_measure(&baseline_bytes).unwrap();

        // Save blob in blobs_dir
        std::fs::write(blobs_dir.join(&baseline_hash), &baseline_bytes).unwrap();

        let mut index = WorkspaceIndex::new();
        index
            .save_test(
                &manifests_dir,
                "test1",
                &crate::manifest::SingleTestManifest {
                    schema_version: 1,
                    hash: crate::manifest::ImageHash::new("sha256", &baseline_hash).unwrap(),
                    phash: crate::manifest::ImageHash::new("dhash", "0000000000000000").unwrap(),
                    width: 10,
                    height: 10,
                },
            )
            .unwrap();

        index
            .save_test(
                &manifests_dir,
                "test2",
                &crate::manifest::SingleTestManifest {
                    schema_version: 1,
                    hash: crate::manifest::ImageHash::new("sha256", &baseline_hash).unwrap(),
                    phash: crate::manifest::ImageHash::new("dhash", "0000000000000000").unwrap(),
                    width: 10,
                    height: 10,
                },
            )
            .unwrap();

        // Actual test1: 10x10 white image, but pixel (0, 0) is black.
        // With mask at (0, 0, 1, 1), they become equal after mask!
        let mut actual_img1 = baseline_img.clone();
        actual_img1.put_pixel(0, 0, image::Rgba([0, 0, 0, 255]));
        actual_img1.save(temp.path().join("test1.png")).unwrap();

        // Actual test2: 10x10 white image, but pixel (9, 9) is black.
        // With mask at (0, 0, 1, 1), they STILL DIFFER after mask!
        let mut actual_img2 = baseline_img;
        actual_img2.put_pixel(9, 9, image::Rgba([0, 0, 0, 255]));
        actual_img2.save(temp.path().join("test2.png")).unwrap();

        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            config: Some(crate::config::GleonConfig {
                screenshots: vec![crate::config::ScreenshotRule {
                    include: vec![crate::config::GlobPattern::new("**/*.png").unwrap()],
                    mode: gleon_engine::config::Mode::Pixel,
                    diff: gleon_engine::config::DiffConfig::default(),
                    masks: vec![crate::config::MaskRule {
                        path: crate::config::GlobPattern::new("**/*.png").unwrap(),
                        zones: vec![gleon_engine::config::Zone {
                            x: 0,
                            y: 0,
                            width: gleon_engine::config::Dimension::Pixels(1),
                            height: gleon_engine::config::Dimension::Pixels(1),
                        }],
                    }],
                }],
                ..Default::default()
            }),
            ..Default::default()
        };

        let report = check_status(&ctx).unwrap();
        // test1 is masked so unchanged
        assert!(!report.modified.contains(&PathBuf::from("test1.png")));
        // test2 still differs so modified
        assert!(report.modified.contains(&PathBuf::from("test2.png")));
    }
}
