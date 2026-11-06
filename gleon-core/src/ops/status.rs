//! Status operation for categorizing workspace screenshots against baseline manifests.

use std::path::PathBuf;

use gleon_model::{case::Sha256Hex, compare};
use serde::Serialize;
use thiserror::Error;

use crate::{
    context::ResolvedContext,
    ops::common::{
        Baseline, BlobState, CoreError, baseline_of, ensure_initialized, index_keys_missing_from,
        load_config_and_scan, load_merged_index_with_fallback, local_blob_state, platform_key,
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
    /// Tracked screenshots that would fail their rule (`mode`, `diff`, masks) against their
    /// baseline, exactly as `gleon diff` judges them.
    pub modified: Vec<PathBuf>,
    /// Tracked screenshots whose bytes changed but which pass their rule: `gleon diff` passes
    /// them, `gleon stage` would record their new bytes.
    pub within_tolerance: Vec<PathBuf>,
    /// Tracked screenshots that cannot be compared with their baseline, and why: a blob missing
    /// from `.gleon/blobs` or not holding its bytes, an image that cannot be decoded, images over
    /// the SSIM analysis budget. `gleon diff` fails them too.
    pub uncomparable: Vec<Explained>,
    /// Manifest entries whose screenshot is missing on disk.
    pub deleted: Vec<PathBuf>,
    /// Unchanged screenshots (the bytes their manifest names) whose blob is missing from
    /// `.gleon/blobs`: `gleon pull` fetches it, `gleon push` needs it. No change to stage.
    pub uncached: Vec<PathBuf>,
    /// Unchanged screenshots whose blob in `.gleon/blobs` is there but unusable (a directory, a
    /// symbolic link, unreadable), and why: `gleon pull` cannot simply fetch it. No change to
    /// stage.
    pub unusable_blobs: Vec<Explained>,
}

/// A tracked screenshot listed with why.
///
/// One that cannot be compared with its baseline ([`StatusReport::uncomparable`]), or an
/// unchanged one whose blob in `.gleon/blobs` is unusable ([`StatusReport::unusable_blobs`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Explained {
    /// The screenshot, relative to the workspace root.
    pub path: PathBuf,
    /// Why it is listed, for the user.
    pub reason: String,
}

impl StatusReport {
    /// Returns true if no screenshot is added, modified, within tolerance, uncomparable or
    /// deleted: the staged manifests describe the screenshots, so `gleon stage` writes no
    /// manifest. (The local blobs may still need care: [`Self::uncached`],
    /// [`Self::unusable_blobs`].)
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.added.is_empty()
            && self.modified.is_empty()
            && self.within_tolerance.is_empty()
            && self.uncomparable.is_empty()
            && self.deleted.is_empty()
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

    /// Formats the report as human-readable text: a section per list that is not empty, then
    /// one line on the blobs missing from `.gleon/blobs`. A clean report says so: up to date when
    /// the local blobs are too, else only that no change is to be staged.
    #[must_use]
    pub fn format_text(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        if self.is_clean() {
            out.push_str(
                if self.uncached.is_empty() && self.unusable_blobs.is_empty() {
                    "Nothing to report. Workspace is up to date.\n"
                } else {
                    "No changes to stage.\n"
                },
            );
        }
        let explained = |entries: &[Explained]| -> Vec<String> {
            entries
                .iter()
                .map(|entry| format!("{}: {}", entry.path.display(), entry.reason))
                .collect()
        };
        let paths = |paths: &[PathBuf]| -> Vec<String> {
            paths
                .iter()
                .map(|path| path.display().to_string())
                .collect()
        };
        let sections = [
            ("Added:", paths(&self.added)),
            ("Modified:", paths(&self.modified)),
            (
                "Within tolerance (they pass their rule; `gleon stage` records the new bytes):",
                paths(&self.within_tolerance),
            ),
            (
                "Cannot compare (`gleon diff` fails them too):",
                explained(&self.uncomparable),
            ),
            ("Deleted:", paths(&self.deleted)),
            (
                "Unchanged, but their blob in .gleon/blobs is unusable:",
                explained(&self.unusable_blobs),
            ),
        ];
        for (title, lines) in sections.iter().filter(|(_, lines)| !lines.is_empty()) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(title);
            out.push('\n');
            for line in lines {
                let _ = writeln!(out, "  {line}");
            }
        }
        match self.uncached.len() {
            0 => {}
            1 => out.push_str(
                "\n1 unchanged screenshot has no blob in .gleon/blobs: run `gleon pull`.\n",
            ),
            count => {
                let _ = write!(
                    out,
                    "\n{count} unchanged screenshots have no blob in .gleon/blobs: run `gleon \
                     pull`.\n"
                );
            }
        }
        out
    }

    /// Files `class` under its list.
    fn push(&mut self, class: Class) {
        match class {
            Class::Added(path) => self.added.push(path),
            Class::Modified(path) => self.modified.push(path),
            Class::WithinTolerance(path) => self.within_tolerance.push(path),
            Class::Uncomparable(entry) => self.uncomparable.push(entry),
            Class::Uncached(path) => self.uncached.push(path),
            Class::UnusableBlob(entry) => self.unusable_blobs.push(entry),
            Class::Unchanged => {}
        }
    }

    /// Appends the lists of `other`.
    fn append(&mut self, other: Self) {
        self.added.extend(other.added);
        self.modified.extend(other.modified);
        self.within_tolerance.extend(other.within_tolerance);
        self.uncomparable.extend(other.uncomparable);
        self.deleted.extend(other.deleted);
        self.uncached.extend(other.uncached);
        self.unusable_blobs.extend(other.unusable_blobs);
    }

    /// Sorts every list by path, so parallel classification prints in a stable order.
    fn sort(&mut self) {
        self.added.sort();
        self.modified.sort();
        self.within_tolerance.sort();
        self.uncomparable
            .sort_by(|left, right| left.path.cmp(&right.path));
        self.deleted.sort();
        self.uncached.sort();
        self.unusable_blobs
            .sort_by(|left, right| left.path.cmp(&right.path));
    }
}

/// Where one scanned screenshot belongs in a [`StatusReport`].
#[derive(Debug, PartialEq, Eq)]
enum Class {
    /// Not tracked by a manifest.
    Added(PathBuf),
    /// Fails its rule against its baseline.
    Modified(PathBuf),
    /// Other bytes than the baseline's that pass its rule.
    WithinTolerance(PathBuf),
    /// Cannot be compared with its baseline.
    Uncomparable(Explained),
    /// The baseline's bytes, whose blob is missing from `.gleon/blobs`.
    Uncached(PathBuf),
    /// The baseline's bytes, whose blob in `.gleon/blobs` is there but unusable.
    UnusableBlob(Explained),
    /// The baseline's bytes.
    Unchanged,
}

/// Classifies one scanned test case against `workspace_index`, judged as `gleon diff` judges
/// it: the same baseline ([`baseline_of`]: the manifest's hash, else the local blob) and the
/// same comparison under the rule's tolerance and masks ([`compare::verdict`], which encodes no
/// diff). A screenshot that cannot be compared is a failed case in `gleon diff`; here it is
/// listed with the reason instead of failing the whole status.
///
/// # Errors
/// Returns [`CoreError::Io`] if the screenshot cannot be read: the workspace is broken, not the
/// comparison of the screenshot (like in `gleon diff`).
fn classify_test_case(
    case: &crate::scanner::TestCase,
    workspace_index: &crate::manifest::WorkspaceIndex,
    blobs_root: &std::path::Path,
) -> Result<Class, StatusError> {
    let path = &case.image.relative_path;
    let Some(manifest) = workspace_index.get(&case.name) else {
        return Ok(Class::Added(path.clone()));
    };
    let uncomparable = |reason: String| {
        Class::Uncomparable(Explained {
            path: path.clone(),
            reason,
        })
    };

    let candidate = std::fs::read(&case.image.absolute_path).map_err(CoreError::Io)?;
    let candidate_sha256 = Sha256Hex::of(&candidate);
    let golden = match baseline_of(manifest, &candidate, &candidate_sha256, blobs_root) {
        // Decided by the manifest's sha256 alone: the blob may be missing or broken in the local
        // cache.
        Baseline::Identical { blob_read: false } => {
            return Ok(match local_blob_state(blobs_root, &manifest.hash) {
                BlobState::Usable => Class::Unchanged,
                BlobState::Missing => Class::Uncached(path.clone()),
                BlobState::Unusable(reason) => Class::UnusableBlob(Explained {
                    path: path.clone(),
                    reason,
                }),
            });
        }
        Baseline::Identical { blob_read: true } => return Ok(Class::Unchanged),
        Baseline::Unavailable { message, .. } => return Ok(uncomparable(message)),
        Baseline::Golden { png, .. } => png,
    };
    let masks = case.rule.matched_mask_zones(path);
    let verdict = compare::verdict(
        &golden,
        compare::Candidate::Png(&candidate),
        &case.rule.tolerance,
        &masks,
    );
    Ok(match verdict {
        Ok(compare::Verdict::Match) => Class::WithinTolerance(path.clone()),
        Ok(compare::Verdict::Mismatch | compare::Verdict::DimensionMismatch) => {
            Class::Modified(path.clone())
        }
        Err(e) => uncomparable(e.to_string()),
    })
}

/// Evaluates status for the workspace at `base_dir`.
///
/// # Errors
///
/// Returns an error if the workspace is not initialized, if the platform key cannot be
/// resolved, if manifests or workspace configuration fail to load, or if screenshots cannot be
/// scanned or read.
pub fn check_status(context: &ResolvedContext) -> Result<StatusReport, StatusError> {
    use rayon::prelude::*;

    let paths = ensure_initialized(&context.base_dir)?;
    let platform_key = platform_key(context)?;

    let workspace_index = load_merged_index_with_fallback(
        &paths,
        &platform_key,
        context.fallback_platform_key.as_ref(),
    )?;

    let blobs_root = paths.blobs_root();

    // Scan workspace screenshots
    let test_cases = load_config_and_scan(context)?;

    let mut report = test_cases
        .par_iter()
        .map(|case| classify_test_case(case, &workspace_index, &blobs_root))
        .try_fold(StatusReport::default, |mut report, class| {
            report.push(class?);
            Ok::<_, StatusError>(report)
        })
        .try_reduce(StatusReport::default, |mut report, other| {
            report.append(other);
            Ok(report)
        })?;

    let seen_test_cases: std::collections::HashSet<&str> =
        test_cases.iter().map(|c| c.name.as_str()).collect();

    // Identify deleted test cases (staged in index but no longer present on disk)
    for staged_name in index_keys_missing_from(&workspace_index, &seen_test_cases) {
        let mut p = String::with_capacity(staged_name.len() + 4);
        p.push_str(staged_name);
        p.push_str(".png");
        report.deleted.push(PathBuf::from(p));
    }

    report.sort();
    Ok(report)
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
    use std::path::Path;

    use gleon_model::{platform::PlatformKey, tolerance::Tolerance};
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
            within_tolerance: vec![PathBuf::from("d.png")],
            uncomparable: vec![Explained {
                path: PathBuf::from("e.png"),
                reason: "candidate image: broken".to_owned(),
            }],
            deleted: vec![PathBuf::from("c.png")],
            uncached: vec![PathBuf::from("f.png"), PathBuf::from("g.png")],
            unusable_blobs: vec![Explained {
                path: PathBuf::from("h.png"),
                reason: "no regular file".to_owned(),
            }],
        };
        assert!(!report.is_clean());
        assert_eq!(
            report.format_text(),
            "Added:\n  a.png\n\nModified:\n  b.png\n\nWithin tolerance (they pass their rule; \
             `gleon stage` records the new bytes):\n  d.png\n\nCannot compare (`gleon diff` \
             fails them too):\n  e.png: candidate image: broken\n\nDeleted:\n  c.png\n\n\
             Unchanged, but their blob in .gleon/blobs is unusable:\n  h.png: no regular \
             file\n\n2 unchanged screenshots have no blob in .gleon/blobs: run `gleon pull`.\n"
        );
        let json: serde_json::Value = serde_json::from_str(&report.format_json().unwrap()).unwrap();
        assert_eq!(
            json["uncomparable"],
            serde_json::json!([{"path": "e.png", "reason": "candidate image: broken"}])
        );
        assert_eq!(json["uncached"], serde_json::json!(["f.png", "g.png"]));
        assert_eq!(
            json["unusable_blobs"],
            serde_json::json!([{"path": "h.png", "reason": "no regular file"}])
        );
        // A broken cache changes no manifest: clean, but named.
        let unusable = StatusReport {
            unusable_blobs: vec![Explained {
                path: PathBuf::from("h.png"),
                reason: "no regular file".to_owned(),
            }],
            ..StatusReport::default()
        };
        assert!(unusable.is_clean());
        assert_eq!(
            unusable.format_text(),
            "No changes to stage.\n\nUnchanged, but their blob in \
             .gleon/blobs is unusable:\n  h.png: no regular file\n"
        );

        for (unclean, why) in [
            (
                StatusReport {
                    within_tolerance: vec![PathBuf::from("d.png")],
                    ..StatusReport::default()
                },
                "`gleon stage` would record d.png",
            ),
            (
                StatusReport {
                    uncomparable: vec![Explained {
                        path: PathBuf::from("e.png"),
                        reason: "broken".to_owned(),
                    }],
                    ..StatusReport::default()
                },
                "e.png fails `gleon diff`",
            ),
        ] {
            assert!(!unclean.is_clean(), "{why}");
        }

        let clean = StatusReport::default();
        assert!(clean.is_clean());
        assert_eq!(
            clean.format_text(),
            "Nothing to report. Workspace is up to date.\n"
        );
        // Blobs missing from the local cache change no manifest: clean, with a hint.
        let uncached = StatusReport {
            uncached: vec![PathBuf::from("f.png")],
            ..StatusReport::default()
        };
        assert!(uncached.is_clean());
        assert_eq!(
            uncached.format_text(),
            "No changes to stage.\n\n1 unchanged screenshot has no blob in \
             .gleon/blobs: run `gleon pull`.\n"
        );
    }

    #[test]
    fn test_status_deleted_dotted_test_name() {
        let report = StatusReport {
            deleted: vec![PathBuf::from("auth/user.v2.png")],
            ..StatusReport::default()
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
        let plat_key = ctx.platform.key().unwrap();
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
    fn test_status_missing_baseline_blob_is_uncomparable() {
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            ..Default::default()
        };
        let plat_key = ctx.platform.key().unwrap();
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
        assert_uncomparable(&report, "screenshots/login.png", "run `gleon pull`");
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

        let platform_key = ctx_temp.platform.key().unwrap();
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

        // The bytes differ from the manifest, so status reads the baseline blob.
        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            config: Some(crate::config::GleonConfig {
                screenshots: vec![crate::config::ScreenshotRule {
                    include: vec![crate::config::GlobPattern::new("**/*.png").unwrap()],
                    tolerance: Tolerance::pixel(0.01),
                    masks: vec![crate::config::MaskRule {
                        path: crate::config::GlobPattern::new("**/*.png").unwrap(),
                        zones: vec![gleon_engine::config::Zone {
                            x: 0,
                            y: 0,
                            width: gleon_engine::config::Dimension::Pixels(1),
                            height: gleon_engine::config::Dimension::Pixels(1),
                        }],
                    }],
                    text_tolerance: None,
                }],
                ..Default::default()
            }),
            ..Default::default()
        };

        // The blob does not exist: it cannot be compared, the reason says why.
        let report = check_status(&ctx).unwrap();
        assert_uncomparable(&report, "test.png", "run `gleon pull`");

        // A blob that does not hold the bytes its name says cannot be compared either: a failed
        // case in `gleon diff`, never a pass and never an error of the whole status.
        let blob_dir = gleon_dir.join("blobs").join("sha256");
        std::fs::create_dir_all(&blob_dir).unwrap();
        std::fs::write(blob_dir.join(hash_val), "fake png data").unwrap();

        let report = check_status(&ctx).unwrap();
        assert_uncomparable(&report, "test.png", "does not match its hash");
    }

    #[test]
    fn test_status_invalid_platform_key() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".gleon")).unwrap();

        // Construct an invalid platform info that fails key() (e.g. empty os and version)
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

        let platform_key = ctx.platform.key().unwrap();
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
    fn test_status_sha256_identical_without_blob_is_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            ..ResolvedContext::default()
        };

        let platform_key = ctx.platform.key().unwrap();
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

        // Deliberately do NOT create the blob file: the manifest's sha256 names the screenshot
        // bytes, so they are the baseline, decided without the blob, like `gleon diff` decides it;
        // the missing blob (`gleon push` needs it) is listed apart.
        let res = check_status(&ctx).unwrap();
        assert!(res.is_clean(), "{res:?}");
        assert_eq!(res.uncached, vec![PathBuf::from("test.png")]);

        // With the blob in place nothing is listed.
        let blob_dir = gleon_dir.join("blobs").join("sha256");
        std::fs::create_dir_all(&blob_dir).unwrap();
        std::fs::write(blob_dir.join(&hash_val), &img_bytes).unwrap();
        assert_eq!(check_status(&ctx).unwrap(), StatusReport::default());
    }

    #[test]
    fn test_status_sparse_fallback_overlay() {
        let temp = tempfile::tempdir().unwrap();
        let base_path = temp.path();
        let gleon_dir = base_path.join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();

        let linux_key = "linux-x86_64";
        let macos_key = "macos-aarch64";

        let mut ctx = ResolvedContext {
            base_dir: base_path.to_path_buf(),
            platform: crate::platform::PlatformInfo {
                os: "linux".to_string(),
                arch: Some("x86_64".to_string()),
                renderer: None,
                labels: std::collections::BTreeMap::new(),
            },
            fallback_platform_key: Some(PlatformKey::parse(macos_key).unwrap()),
            ..Default::default()
        };

        let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include:
      - "*.png"
    mode: pixel
    diff: { threshold: 0.01 }
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

        let platform_key = ctx_temp.platform.key().unwrap();
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
                    tolerance: Tolerance::pixel(0.0),
                    masks: vec![crate::config::MaskRule {
                        path: crate::config::GlobPattern::new("**/*.png").unwrap(),
                        zones: vec![gleon_engine::config::Zone {
                            x: 0,
                            y: 0,
                            width: gleon_engine::config::Dimension::Pixels(1),
                            height: gleon_engine::config::Dimension::Pixels(1),
                        }],
                    }],
                    text_tolerance: None,
                }],
                ..Default::default()
            }),
            ..Default::default()
        };

        // No differing pixel is tolerated, so only the mask decides.
        let report = check_status(&ctx).unwrap();
        // test1 differs only under the mask: it passes, with other bytes.
        assert_eq!(report.within_tolerance, vec![PathBuf::from("test1.png")]);
        // test2 still differs outside the mask.
        assert_eq!(report.modified, vec![PathBuf::from("test2.png")]);
    }

    /// The status of a workspace whose `test.png` (`candidate` on disk) is tracked with
    /// `baseline` as its staged blob, compared under `tolerance` without masks.
    fn status_of(baseline: &[u8], candidate: &[u8], tolerance: Tolerance) -> StatusReport {
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            config: Some(crate::config::GleonConfig {
                screenshots: vec![crate::config::ScreenshotRule {
                    include: vec![crate::config::GlobPattern::new("*.png").unwrap()],
                    tolerance,
                    masks: vec![],
                    text_tolerance: None,
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let manifests_dir = gleon_dir
            .join("manifests")
            .join(ctx.platform.key().unwrap());
        let blobs_dir = gleon_dir.join("blobs").join("sha256");
        std::fs::create_dir_all(&manifests_dir).unwrap();
        std::fs::create_dir_all(&blobs_dir).unwrap();
        let hash = hex::encode(sha2::Sha256::digest(baseline));
        let (width, height) = gleon_model::case::png_size(baseline).unwrap();
        std::fs::write(blobs_dir.join(&hash), baseline).unwrap();
        WorkspaceIndex::new()
            .save_test(
                &manifests_dir,
                "test",
                &crate::manifest::SingleTestManifest {
                    schema_version: 1,
                    hash: crate::manifest::ImageHash::new("sha256", &hash).unwrap(),
                    phash: crate::manifest::ImageHash::new("dhash", "0000000000000000").unwrap(),
                    width,
                    height,
                },
            )
            .unwrap();
        std::fs::write(temp.path().join("test.png"), candidate).unwrap();
        check_status(&ctx).unwrap()
    }

    fn png(image: &image::RgbaImage) -> Vec<u8> {
        compare::encode_png(image).unwrap()
    }

    /// The same pixels as `image` (opaque) in another PNG color type, so other bytes.
    fn png_rgb(image: &image::RgbaImage) -> Vec<u8> {
        let rgb = image::DynamicImage::ImageRgba8(image.clone()).to_rgb8();
        let mut bytes = Vec::new();
        rgb.write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
        bytes
    }

    const WHITE: image::Rgba<u8> = image::Rgba([255, 255, 255, 255]);

    /// A white 10x10 image and the same with one black pixel (1% of the pixels).
    fn white_and_one_black_pixel() -> (Vec<u8>, Vec<u8>) {
        let white = image::RgbaImage::from_pixel(10, 10, WHITE);
        let mut changed = white.clone();
        changed.put_pixel(4, 4, image::Rgba([0, 0, 0, 255]));
        (png(&white), png(&changed))
    }

    /// Only `within_tolerance` lists `test.png`.
    fn within_tolerance() -> StatusReport {
        StatusReport {
            within_tolerance: vec![PathBuf::from("test.png")],
            ..StatusReport::default()
        }
    }

    /// Asserts that `report` lists only `path` as uncomparable, its reason containing `needle`.
    fn assert_uncomparable(report: &StatusReport, path: &str, needle: &str) {
        assert!(
            matches!(
                report,
                StatusReport { uncomparable, .. }
                    if matches!(
                        uncomparable.as_slice(),
                        [Explained { path: found, reason }]
                            if found == Path::new(path) && reason.contains(needle)
                    )
            ),
            "{report:?} should list {path} as uncomparable with {needle:?}"
        );
        assert_eq!(
            StatusReport {
                uncomparable: vec![],
                ..report.clone()
            },
            StatusReport::default(),
            "nothing else is listed"
        );
    }

    /// Only `modified` lists `test.png`.
    fn modified() -> StatusReport {
        StatusReport {
            modified: vec![PathBuf::from("test.png")],
            ..StatusReport::default()
        }
    }

    /// The headline case: a change `gleon diff` passes under the rule's threshold is no
    /// modification, but `gleon stage` would still record it.
    #[test]
    fn test_status_pixel_threshold_tolerates_a_small_change() {
        let (baseline, candidate) = white_and_one_black_pixel();
        let report = status_of(&baseline, &candidate, Tolerance::pixel(0.05));
        assert_eq!(report, within_tolerance());
    }

    #[test]
    fn test_status_exact_rule_reports_one_pixel() {
        let (baseline, candidate) = white_and_one_black_pixel();
        let report = status_of(&baseline, &candidate, Tolerance::Exact {});
        assert_eq!(report, modified());
    }

    /// Other bytes of the same pixels pass even an exact rule, like in `gleon diff`: re-encoded,
    /// the screenshot is within tolerance.
    #[test]
    fn test_status_re_encoded_png_with_the_same_pixels_is_within_tolerance() {
        let white = image::RgbaImage::from_pixel(10, 10, WHITE);
        let (baseline, candidate) = (png(&white), png_rgb(&white));
        assert_ne!(baseline, candidate);
        let report = status_of(&baseline, &candidate, Tolerance::Exact {});
        assert_eq!(report, within_tolerance());
    }

    #[test]
    fn test_status_dimension_mismatch_is_modified() {
        let red = image::Rgba([255, 0, 0, 255]);
        let baseline = png(&image::RgbaImage::from_pixel(10, 10, red));
        let candidate = png(&image::RgbaImage::from_pixel(12, 10, red));
        let report = status_of(&baseline, &candidate, Tolerance::pixel(0.5));
        assert_eq!(report, modified());
    }

    #[test]
    fn test_status_ssim_rule() {
        let ssim = Tolerance::Ssim {
            min_similarity: 0.8,
            color_tolerance: 8.0,
        };
        let red = image::RgbaImage::from_pixel(64, 64, image::Rgba([255, 0, 0, 255]));
        let mut half_blue = red.clone();
        for (x, _, pixel) in half_blue.enumerate_pixels_mut() {
            if x < 32 {
                *pixel = image::Rgba([0, 0, 255, 255]);
            }
        }
        assert_eq!(status_of(&png(&red), &png(&half_blue), ssim), modified());
        assert_eq!(
            status_of(&png(&red), &png_rgb(&red), ssim),
            within_tolerance()
        );
    }

    /// Images over the SSIM analysis budget cannot be compared: a failed case in `gleon diff`,
    /// listed with the reason, not an error of the whole status.
    #[test]
    fn test_status_ssim_over_the_analysis_budget_is_uncomparable() {
        let ssim = Tolerance::Ssim {
            min_similarity: 0.8,
            color_tolerance: 8.0,
        };
        let big = image::RgbaImage::from_pixel(4097, 4096, image::Rgba([255, 0, 0, 255]));
        let report = status_of(&png(&big), &png_rgb(&big), ssim);
        assert_uncomparable(&report, "test.png", "SSIM analysis budget");
    }

    /// A manifest of another hash scheme: equal bytes are unchanged, other bytes are judged
    /// under the rule like for sha256.
    #[test]
    fn test_status_non_sha256_manifest_judges_changed_bytes() {
        let status = |baseline: &[u8], candidate: &[u8]| {
            let temp = tempfile::tempdir().unwrap();
            let gleon_dir = temp.path().join(".gleon");
            let ctx = ResolvedContext {
                base_dir: temp.path().to_path_buf(),
                config: Some(crate::config::GleonConfig {
                    screenshots: vec![crate::config::ScreenshotRule {
                        include: vec![crate::config::GlobPattern::new("*.png").unwrap()],
                        tolerance: Tolerance::Exact {},
                        masks: vec![],
                        text_tolerance: None,
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            };
            let manifests_dir = gleon_dir
                .join("manifests")
                .join(ctx.platform.key().unwrap());
            std::fs::create_dir_all(&manifests_dir).unwrap();
            let hash = hex::encode(sha2::Sha512::digest(baseline));
            let blob_dir = gleon_dir.join("blobs").join("sha512");
            std::fs::create_dir_all(&blob_dir).unwrap();
            std::fs::write(blob_dir.join(&hash), baseline).unwrap();
            WorkspaceIndex::new()
                .save_test(
                    &manifests_dir,
                    "test",
                    &crate::manifest::SingleTestManifest {
                        schema_version: 1,
                        hash: crate::manifest::ImageHash::new("sha512", &hash).unwrap(),
                        phash: crate::manifest::ImageHash::new("dhash", "0000000000000000")
                            .unwrap(),
                        width: 10,
                        height: 10,
                    },
                )
                .unwrap();
            std::fs::write(temp.path().join("test.png"), candidate).unwrap();
            check_status(&ctx).unwrap()
        };
        let white = image::RgbaImage::from_pixel(10, 10, WHITE);
        let (baseline, changed) = white_and_one_black_pixel();
        assert!(status(&baseline, &baseline).is_clean());
        assert_eq!(status(&baseline, &png_rgb(&white)), within_tolerance());
        assert_eq!(status(&baseline, &changed), modified());
    }

    /// A blob that is a symbolic link is no blob (like for `gleon pull` and `gleon push`): never
    /// followed, so the changed screenshot cannot be compared.
    #[cfg(unix)]
    #[test]
    fn test_status_never_follows_a_symlinked_blob() {
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            ..Default::default()
        };
        let manifests_dir = gleon_dir
            .join("manifests")
            .join(ctx.platform.key().unwrap());
        let blobs_dir = gleon_dir.join("blobs").join("sha256");
        std::fs::create_dir_all(&manifests_dir).unwrap();
        std::fs::create_dir_all(&blobs_dir).unwrap();
        let (baseline, _) = white_and_one_black_pixel();
        let white = image::RgbaImage::from_pixel(10, 10, WHITE);
        let (hash, ..) = crate::ops::common::hash_and_measure(&baseline).unwrap();
        // The link points at the very same pixels: following it would list the screenshot as
        // within tolerance.
        let elsewhere = temp.path().join("elsewhere.bin");
        std::fs::write(&elsewhere, &baseline).unwrap();
        std::os::unix::fs::symlink(&elsewhere, blobs_dir.join(&hash)).unwrap();
        WorkspaceIndex::new()
            .save_test(
                &manifests_dir,
                "test",
                &crate::manifest::SingleTestManifest {
                    schema_version: 1,
                    hash: crate::manifest::ImageHash::new("sha256", &hash).unwrap(),
                    phash: crate::manifest::ImageHash::new("dhash", "0000000000000000").unwrap(),
                    width: 10,
                    height: 10,
                },
            )
            .unwrap();
        std::fs::write(temp.path().join("test.png"), png_rgb(&white)).unwrap();

        assert_uncomparable(&check_status(&ctx).unwrap(), "test.png", "no regular file");
    }

    /// An unchanged screenshot (the bytes its manifest names) whose blob in `.gleon/blobs` is
    /// there but unusable: named with the reason, since `gleon pull` cannot simply fetch it.
    #[test]
    fn test_status_names_an_unusable_blob_of_an_unchanged_screenshot() {
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            ..Default::default()
        };
        let manifests_dir = gleon_dir
            .join("manifests")
            .join(ctx.platform.key().unwrap());
        let blobs_dir = gleon_dir.join("blobs").join("sha256");
        std::fs::create_dir_all(&manifests_dir).unwrap();
        std::fs::create_dir_all(&blobs_dir).unwrap();
        let png = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/baseline_100x100.png"),
        )
        .unwrap();
        let hash = hex::encode(sha2::Sha256::digest(&png));
        WorkspaceIndex::new()
            .save_test(
                &manifests_dir,
                "test",
                &crate::manifest::SingleTestManifest {
                    schema_version: 1,
                    hash: crate::manifest::ImageHash::new("sha256", &hash).unwrap(),
                    phash: crate::manifest::ImageHash::new("dhash", "0000000000000000").unwrap(),
                    width: 100,
                    height: 100,
                },
            )
            .unwrap();
        std::fs::write(temp.path().join("test.png"), &png).unwrap();
        let unusable = |report: StatusReport| {
            assert!(report.is_clean(), "{report:?}");
            assert!(
                matches!(
                    report.unusable_blobs.as_slice(),
                    [Explained { path, reason }]
                        if path == Path::new("test.png") && reason.contains("no regular file")
                ),
                "{report:?}"
            );
            assert!(report.uncached.is_empty(), "{report:?}");
        };

        // A directory where the blob belongs: `gleon pull` could not write over it.
        std::fs::create_dir(blobs_dir.join(&hash)).unwrap();
        unusable(check_status(&ctx).unwrap());

        // A symbolic link: never taken for a blob.
        #[cfg(unix)]
        {
            std::fs::remove_dir(blobs_dir.join(&hash)).unwrap();
            let elsewhere = temp.path().join("elsewhere.bin");
            std::fs::write(&elsewhere, &png).unwrap();
            std::os::unix::fs::symlink(&elsewhere, blobs_dir.join(&hash)).unwrap();
            unusable(check_status(&ctx).unwrap());
        }
    }
}
