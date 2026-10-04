//! Diff operation for running visual comparison tests against baseline snapshots.
//!
//! Every screenshot gets a case report (`.gleon/runs/latest/cases/<name>.json`, `source.tool`
//! [`CLI_TOOL`]) and, when it fails, its images in the artifacts directory, exactly like the
//! integrations record theirs; the reports of the run (`report.md`, ...) are rendered from these
//! case reports, so they always agree with the exit code.

use std::{io, path::Path, time::Instant};

use gleon_engine::config::Zone;
use gleon_model::{
    case::{
        self, ArtifactImages, CASE_SCHEMA_VERSION, CandidateImage, CaseErrorKind, CaseOutcome,
        CaseReport, CaseTimings, Comparison, GoldenImage, Metrics, RegionMetrics, RunId, Source,
        text,
    },
    compare::{self, Compared},
    config::ArtifactsDir,
    platform::PlatformConfig,
    tolerance::Tolerance,
};
use thiserror::Error;

use crate::{
    cases::{Cases, CasesError, new_run_id, platform_of, remove_reports_of},
    context::ResolvedContext,
    manifest::{SingleTestManifest, WorkspaceIndex},
    ops::common::{
        CoreError, ensure_initialized, load_config_and_scan, load_merged_index_with_fallback,
        platform_key,
    },
    report::{ReportError, ReportGenerator},
    scanner::TestCase,
};

/// `source.tool` of the case reports `gleon diff` writes.
pub const CLI_TOOL: &str = "gleon_cli";

/// Errors that can occur during diff execution.
#[derive(Debug, Error)]
pub enum DiffOpError {
    /// Error generating report files.
    #[error("Report error: {0}")]
    Report(#[from] ReportError),

    /// Error reading the case reports of the run.
    #[error(transparent)]
    Cases(#[from] CasesError),

    /// A screenshot cannot be read: the workspace is broken, not the test of the screenshot.
    #[error("cannot read the screenshot '{path}': {source}")]
    Screenshot {
        /// The screenshot, relative to the workspace root.
        path: String,
        /// The underlying error.
        #[source]
        source: io::Error,
    },

    /// A case report or its images cannot be written.
    #[error("cannot record '{name}': {source}")]
    Record {
        /// The test name.
        name: String,
        /// The underlying error.
        #[source]
        source: io::Error,
    },

    /// No screenshot matches the rules: a run that compares nothing must not pass.
    #[error(
        "no screenshots match the `screenshots` rules of .gleon/gleon.yaml, so nothing was \
         compared (integrations such as the Flutter package record their own case reports: run \
         their tests with `gleon test -- <command>` instead)"
    )]
    NoScreenshots,

    /// Error shared across `ops::*` operations.
    #[error(transparent)]
    Core(#[from] CoreError),
}

/// Inputs of `gleon diff` beyond the workspace: where failure images go and which run the case
/// reports belong to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffOptions {
    /// `gleon diff --artifacts`, which beats [`Self::artifacts_env`].
    pub artifacts: Option<ArtifactsDir>,
    /// The value of `GLEON_ARTIFACTS_DIR`, which beats `artifacts:` of the config.
    pub artifacts_env: Option<ArtifactsDir>,
    /// The value of `GLEON_RUN_ID`, stamped on every case report; without it `gleon diff` names
    /// its own run.
    pub run_id: Option<RunId>,
}

/// Result summary of executing `gleon diff`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffReportResult {
    /// Total number of screenshots compared.
    pub total_tests: usize,
    /// Number of screenshots that failed.
    pub failed_tests: usize,
    /// Directory containing this run's output (case reports, artifacts, `report.md`, ...).
    pub runs_dir: std::path::PathBuf,
}

/// What one comparison found, before it is recorded.
struct Judged {
    outcome: CaseOutcome,
    error_kind: Option<CaseErrorKind>,
    message: Option<String>,
    metrics: Option<Metrics>,
    /// The compared regions of a match or mismatch.
    regions: Vec<RegionMetrics>,
    /// The baseline's bytes, when they were read.
    golden: Option<Vec<u8>>,
    /// The PNG-encoded diff of a mismatch.
    diff: Option<Vec<u8>>,
}

impl Judged {
    const fn new(outcome: CaseOutcome) -> Self {
        Self {
            outcome,
            error_kind: None,
            message: None,
            metrics: None,
            regions: Vec::new(),
            golden: None,
            diff: None,
        }
    }

    fn error(kind: CaseErrorKind, message: String) -> Self {
        Self {
            error_kind: Some(kind),
            message: Some(message),
            ..Self::new(CaseOutcome::Error)
        }
    }

    fn with_golden(self, golden: Vec<u8>) -> Self {
        Self {
            golden: Some(golden),
            ..self
        }
    }
}

/// Everything the comparisons of one `gleon diff` share.
struct DiffRun<'a> {
    root: &'a Path,
    gleon_dir: std::path::PathBuf,
    index: &'a WorkspaceIndex,
    blobs_root: std::path::PathBuf,
    artifacts: ArtifactsDir,
    source: Source,
    platform: PlatformConfig,
    run_id: RunId,
}

impl DiffRun<'_> {
    /// Compares the screenshot of `case`, keeps its images when it fails (removing those of an
    /// earlier failure otherwise) and writes its case report.
    fn record(&self, case: &TestCase) -> Result<CaseReport, DiffOpError> {
        let started = Instant::now();
        let golden_path = gleon_model::naming::normalize_path_separators(
            &case.image.relative_path.to_string_lossy(),
        )
        .into_owned();
        let candidate =
            std::fs::read(&case.image.absolute_path).map_err(|source| DiffOpError::Screenshot {
                path: golden_path.clone(),
                source,
            })?;
        let candidate_image = CandidateImage::of(&candidate);
        let tolerance = Tolerance::from_rule(case.rule.mode, &case.rule.diff);
        let masks = case.rule.matched_mask_zones(&case.image.relative_path);
        let manifest = self.index.get(&case.name);
        let judged = self.judge(manifest, &candidate, &candidate_image, &tolerance, &masks);
        let total = started.elapsed();

        let record_error = |source| DiffOpError::Record {
            name: case.name.clone(),
            source,
        };
        let images = match judged.outcome {
            CaseOutcome::Mismatch => ArtifactImages {
                golden: judged.golden.as_deref(),
                candidate: Some(&candidate),
                diff: judged.diff.as_deref(),
            },
            CaseOutcome::DimensionMismatch => ArtifactImages {
                golden: judged.golden.as_deref(),
                candidate: Some(&candidate),
                diff: None,
            },
            // Kept for `gleon approve`, unless it is no PNG at all.
            CaseOutcome::Missing => ArtifactImages {
                candidate: case::png_size(&candidate).map(|_| candidate.as_slice()),
                ..ArtifactImages::default()
            },
            _ => ArtifactImages::default(),
        };
        let artifacts = case::write_artifacts(self.root, &self.artifacts, &case.name, images)
            .map_err(record_error)?;
        let mut golden = GoldenImage::of(
            golden_path,
            judged.golden.as_deref(),
            manifest.map(|manifest| manifest.hash.clone()),
        );
        if judged.outcome == CaseOutcome::Identical {
            // Identical bytes are the baseline's bytes, already hashed.
            golden.sha256.clone_from(&candidate_image.sha256);
        }
        if let Some(manifest) = manifest.filter(|_| golden.width.is_none()) {
            golden.width = Some(manifest.width);
            golden.height = Some(manifest.height);
        }
        let report = CaseReport {
            schema_version: CASE_SCHEMA_VERSION,
            name: case.name.clone(),
            golden,
            candidate: candidate_image,
            source: self.source.clone(),
            platform: self.platform.clone(),
            test: None,
            comparison: Comparison {
                tolerance,
                // `gleon diff` sees no text: its screenshots are compared strictly.
                text_tolerance: None,
                masks,
                policy_version: gleon_engine::ssim::POLICY_VERSION,
            },
            outcome: judged.outcome,
            error_kind: judged.error_kind,
            message: judged.message,
            metrics: judged.metrics,
            regions: judged.regions,
            artifacts,
            timings_ms: CaseTimings::new(total, None),
            run_id: Some(self.run_id.clone()),
            recorded_at: chrono::Utc::now(),
        };
        report.write(&self.gleon_dir).map_err(record_error)?;
        Ok(report)
    }

    /// Compares `candidate` against the baseline of `manifest` under `tolerance`, `masks`
    /// applied to both images.
    fn judge(
        &self,
        manifest: Option<&SingleTestManifest>,
        candidate: &[u8],
        candidate_image: &CandidateImage,
        tolerance: &Tolerance,
        masks: &[Zone],
    ) -> Judged {
        let Some(manifest) = manifest else {
            let mut missing = Judged::new(CaseOutcome::Missing);
            missing.message = Some("no baseline is staged for this screenshot".to_owned());
            return missing;
        };
        // The manifest names its bytes: equal bytes are the baseline, decided without the blob.
        let is_identical = manifest.hash.scheme() == "sha256"
            && candidate_image
                .sha256
                .as_ref()
                .is_some_and(|sha256| manifest.hash.value() == sha256.as_str());
        if is_identical {
            return match SingleTestManifest::validate_image_bytes(candidate) {
                Ok(()) => Judged::new(CaseOutcome::Identical),
                Err(e) => Judged::error(CaseErrorKind::Image, format!("candidate image: {e}")),
            };
        }
        let blob_path = crate::storage::local_blob_path(&self.blobs_root, &manifest.hash);
        let golden = match std::fs::read(&blob_path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Judged::error(
                    CaseErrorKind::Io,
                    format!(
                        "the baseline blob {} is not in .gleon/blobs: run `gleon pull` (or `gleon \
                         diff --auto-pull`)",
                        manifest.hash
                    ),
                );
            }
            Err(e) => {
                return Judged::error(
                    CaseErrorKind::Io,
                    format!("cannot read the baseline blob {}: {e}", manifest.hash),
                );
            }
        };
        let candidate = compare::Candidate::Png(candidate);
        let judged = match compare::compare(&golden, candidate, tolerance, masks, None) {
            Ok(compare::Comparison { compared, .. }) => match compared {
                Compared::Match { metrics, regions } => Judged {
                    metrics: Some(metrics),
                    regions,
                    ..Judged::new(CaseOutcome::Match)
                },
                // Failure messages read like the integrations' (shared texts of the model).
                Compared::Mismatch {
                    metrics,
                    regions,
                    diff_png,
                } => Judged {
                    message: Some(text::metrics_summary(&metrics)),
                    metrics: Some(metrics),
                    regions,
                    diff: Some(diff_png),
                    ..Judged::new(CaseOutcome::Mismatch)
                },
                Compared::DimensionMismatch { golden, candidate } => Judged {
                    message: Some(text::dimension_summary(golden, candidate)),
                    ..Judged::new(CaseOutcome::DimensionMismatch)
                },
            },
            Err(e) => Judged::error(e.kind(), e.to_string()),
        };
        judged.with_golden(golden)
    }
}

/// Removes the output of the previous `gleon diff` from `runs_latest`: its rendered reports and
/// its case reports with their images. Case reports of other tools (test runs of integrations)
/// and the run file stay.
fn clear_previous_run(runs_latest: &Path) -> Result<(), DiffOpError> {
    for output in ReportGenerator::OUTPUT_FILES {
        match std::fs::remove_file(runs_latest.join(output)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(CoreError::Io(e).into()),
            _ => {}
        }
    }
    remove_reports_of(runs_latest, CLI_TOOL)?;
    Ok(())
}

/// Executes diff comparison for the workspace at `base_dir`.
///
/// # Errors
///
/// Returns an error if the workspace is not initialized, if the platform key cannot be
/// resolved, if manifests fail to load, if no screenshot matches the rules (after clearing the previous run), if the previous run's output cannot be cleared, if
/// screenshots cannot be scanned or read, if a case report or its images cannot be written, or if
/// reading the case reports back or rendering the reports fails.
pub fn run_diff(
    context: &ResolvedContext,
    options: &DiffOptions,
) -> Result<DiffReportResult, DiffOpError> {
    use rayon::prelude::*;

    let paths = ensure_initialized(&context.base_dir)?;
    let platform_key = platform_key(context)?;

    let workspace_index = load_merged_index_with_fallback(
        &paths,
        &platform_key,
        context.fallback_platform_key.as_deref(),
    )?;

    // Scanned first: a workspace that cannot be scanned keeps the previous run (and its candidates
    // for `gleon approve`).
    let test_cases = load_config_and_scan(context)?;
    let runs_dir = paths.runs_latest();
    clear_previous_run(&runs_dir)?;
    // After clearing: a workspace without screenshots has no current run to approve from either.
    if test_cases.is_empty() {
        return Err(DiffOpError::NoScreenshots);
    }

    let config = context.config.clone().unwrap_or_default();
    let run = DiffRun {
        root: &context.base_dir,
        gleon_dir: paths.gleon_dir(),
        index: &workspace_index,
        blobs_root: paths.blobs_root(),
        artifacts: config.artifacts_dir(options.artifacts_env.as_ref(), options.artifacts.as_ref()),
        source: Source {
            tool: CLI_TOOL.to_owned(),
            tool_version: env!("CARGO_PKG_VERSION").to_owned(),
            renderer: context.platform.renderer.clone(),
        },
        platform: platform_of(&context.platform),
        run_id: options
            .run_id
            .clone()
            .unwrap_or_else(|| new_run_id(chrono::Utc::now())),
    };

    let progress_bar = crate::ui::create_progress_bar(test_cases.len() as u64);
    let recorded: Vec<Result<CaseReport, DiffOpError>> = test_cases
        .par_iter()
        .map(|case| {
            progress_bar.set_message(case.image.relative_path.display().to_string());
            let report = run.record(case);
            progress_bar.inc(1);
            report
        })
        .collect();
    progress_bar.finish_and_clear();

    // The reports cover what was recorded even when a screenshot could not be, so a broken run
    // still explains itself; the first error decides the result.
    let mut reports = Vec::with_capacity(recorded.len());
    let mut first_error = None;
    for report in recorded {
        match report {
            Ok(report) => reports.push(report),
            Err(e) => {
                let _ = first_error.get_or_insert(e);
            }
        }
    }
    let cases = Cases::new(&runs_dir, reports).with_run_id(run.run_id);
    ReportGenerator::generate_all(&runs_dir, &cases)?;
    if let Some(e) = first_error {
        return Err(e);
    }
    Ok(DiffReportResult {
        total_tests: cases.reports().len(),
        failed_tests: cases.failures().count(),
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
    use gleon_model::platform::PlatformInfo;
    use sha2::Digest;

    use super::*;
    use crate::{
        config::ConfigError, context::ContextError, manifest::ManifestError, scanner::ScannerError,
    };

    /// A workspace with one staged 4x4 screenshot `shots/a.png` (exact rule) and its context.
    fn staged_workspace() -> (tempfile::TempDir, ResolvedContext) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join(".gleon")).unwrap();
        std::fs::write(
            root.join(".gleon/gleon.yaml"),
            "required_version: \">=0.1.0\"\nscreenshots:\n  - include: \"shots/*.png\"\n    mode: pixel\n    diff: { threshold: 0.0 }\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("shots")).unwrap();
        image::RgbaImage::from_pixel(4, 4, image::Rgba([0, 0, 255, 255]))
            .save(root.join("shots/a.png"))
            .unwrap();
        let ctx = ResolvedContext::from_options(&crate::context::ContextOptions::default(), root)
            .unwrap();
        crate::ops::stage_workspace(&ctx, None).unwrap();
        (temp, ctx)
    }

    fn case_report(root: &Path, name: &str) -> CaseReport {
        let bytes = std::fs::read(CaseReport::path(&root.join(".gleon"), name)).unwrap();
        CaseReport::parse(&bytes).unwrap()
    }

    #[test]
    fn test_diff_records_a_case_report_per_screenshot() {
        let (temp, ctx) = staged_workspace();
        let root = temp.path();
        let options = DiffOptions {
            run_id: Some(RunId::new("ci-7").unwrap()),
            ..DiffOptions::default()
        };

        let identical = run_diff(&ctx, &options).unwrap();
        assert_eq!(identical.failed_tests, 0);
        let report = case_report(root, "shots/a");
        assert_eq!(report.outcome, CaseOutcome::Identical);
        assert_eq!(report.source.tool, CLI_TOOL);
        assert_eq!(report.run_id.as_ref().unwrap().as_str(), "ci-7");
        assert_eq!(report.golden.path, "shots/a.png");
        assert_eq!(report.golden.sha256, report.candidate.sha256.clone());
        assert!(report.golden.blob.is_some());
        assert_eq!(
            report.comparison.tolerance,
            Tolerance::Pixel {
                max_diff_ratio: 0.0
            }
        );
        assert!(report.artifacts.is_none());

        image::RgbaImage::from_fn(4, 4, |x, _| {
            image::Rgba([if x == 0 { 255 } else { 0 }, 0, 255, 255])
        })
        .save(root.join("shots/a.png"))
        .unwrap();
        image::RgbaImage::new(2, 2)
            .save(root.join("shots/new.png"))
            .unwrap();
        let failed = run_diff(&ctx, &options).unwrap();
        assert_eq!((failed.total_tests, failed.failed_tests), (2, 2));
        let mismatch = case_report(root, "shots/a");
        assert_eq!(mismatch.outcome, CaseOutcome::Mismatch);
        assert!(matches!(
            mismatch.metrics,
            Some(Metrics::Pixel {
                diff_pixels: 4,
                total_pixels: 16,
                ..
            })
        ));
        assert_eq!(mismatch.regions.len(), 1);
        assert_eq!(
            mismatch.message.as_deref(),
            Some("25.00% (4 of 16px) differ"),
            "the integrations' texts"
        );
        let artifacts = mismatch.artifacts.unwrap();
        for path in [&artifacts.golden, &artifacts.candidate, &artifacts.diff] {
            let path = path.as_deref().unwrap();
            assert!(
                path.starts_with(".gleon/runs/latest/artifacts/shots/a/"),
                "{path}"
            );
            assert!(root.join(path).is_file(), "{path}");
        }
        let missing = case_report(root, "shots/new");
        assert_eq!(missing.outcome, CaseOutcome::Missing);
        assert_eq!(missing.golden.sha256, None);
        assert!(missing.artifacts.unwrap().candidate.is_some());

        let latest = root.join(".gleon/runs/latest");
        let md = std::fs::read_to_string(latest.join("report.md")).unwrap();
        assert!(
            md.contains("| shots/a | shots/a.png | ❌ Mismatch |"),
            "{md}"
        );
        assert!(latest.join("junit.xml").is_file() && latest.join("report.html").is_file());
    }

    #[test]
    fn test_diff_starts_clean_but_keeps_the_reports_of_integrations() {
        let (temp, ctx) = staged_workspace();
        let root = temp.path();
        image::RgbaImage::new(2, 2)
            .save(root.join("shots/gone.png"))
            .unwrap();
        run_diff(&ctx, &DiffOptions::default()).unwrap();
        assert!(
            root.join(".gleon/runs/latest/artifacts/shots/gone/candidate.png")
                .is_file()
        );

        let mut flutter = crate::cases::fixtures::report("test/goldens/x", CaseOutcome::Match);
        flutter.golden.path = "shots/a.png".to_owned();
        flutter.write(&root.join(".gleon")).unwrap();
        let run_file = root.join(".gleon/runs/latest/run.json");
        crate::cases::RunInfo {
            run_id: RunId::new("earlier").unwrap(),
            started_at: chrono::Utc::now() - chrono::TimeDelta::hours(1),
            command: vec!["flutter".to_owned(), "test".to_owned()],
        }
        .write(&root.join(".gleon/runs/latest"))
        .unwrap();

        std::fs::remove_file(root.join("shots/gone.png")).unwrap();
        let result = run_diff(&ctx, &DiffOptions::default()).unwrap();
        assert_eq!(result.total_tests, 1);
        let cases = root.join(".gleon/runs/latest/cases");
        assert!(
            !cases.join("shots/gone.json").exists(),
            "its screenshot is gone"
        );
        assert!(
            !root
                .join(".gleon/runs/latest/artifacts/shots/gone/candidate.png")
                .exists()
        );
        assert!(cases.join("test/goldens/x.json").is_file(), "not ours");
        assert!(run_file.is_file());
    }

    /// Without `GLEON_RUN_ID` a run still has its own id: its reports are read as one run, with no
    /// warning, and reports of integrations without one do not leak into its reports.
    #[test]
    fn test_diff_names_its_own_run() {
        let (temp, ctx) = staged_workspace();
        let root = temp.path();
        let mut flutter = crate::cases::fixtures::report("test/goldens/x", CaseOutcome::Mismatch);
        flutter.golden.path = "shots/a.png".to_owned();
        flutter.write(&root.join(".gleon")).unwrap();

        run_diff(&ctx, &DiffOptions::default()).unwrap();
        let run_id = case_report(root, "shots/a").run_id.unwrap();
        assert!(run_id.as_str().starts_with("run-"), "{run_id:?}");
        let md = std::fs::read_to_string(root.join(".gleon/runs/latest/report.md")).unwrap();
        assert!(md.contains("**Total Tests:** 1\n**Failed:** 0"), "{md}");
        assert!(!md.contains("test/goldens/x"), "{md}");
        assert!(!md.contains("⚠️"), "{md}");

        // The next run is another one.
        run_diff(&ctx, &DiffOptions::default()).unwrap();
        assert_ne!(case_report(root, "shots/a").run_id.unwrap(), run_id);
    }

    /// Under a `GLEON_RUN_ID` an integration shares (CI), the reports of `gleon diff` still tell
    /// exactly what it compared, like its exit code.
    #[test]
    fn test_diff_reports_only_its_cases_under_a_shared_run_id() {
        let (temp, ctx) = staged_workspace();
        let root = temp.path();
        let run_id = RunId::new("ci-1").unwrap();
        let mut flutter = crate::cases::fixtures::report("test/goldens/x", CaseOutcome::Mismatch);
        flutter.run_id = Some(run_id.clone());
        flutter.write(&root.join(".gleon")).unwrap();

        let options = DiffOptions {
            run_id: Some(run_id),
            ..DiffOptions::default()
        };
        let result = run_diff(&ctx, &options).unwrap();
        assert_eq!((result.total_tests, result.failed_tests), (1, 0));
        let md = std::fs::read_to_string(root.join(".gleon/runs/latest/report.md")).unwrap();
        assert!(md.contains("**Total Tests:** 1\n**Failed:** 0"), "{md}");
        assert!(
            root.join(".gleon/runs/latest/cases/test/goldens/x.json")
                .is_file(),
            "the integration's report stays"
        );
    }

    /// Bytes equal to the baseline's by hash are still checked as an image: a manifest of a file
    /// that is no valid PNG gives an `image` error, not a pass.
    #[test]
    fn test_diff_checks_identical_bytes_as_an_image() {
        let (temp, ctx) = staged_workspace();
        let root = temp.path();
        let dir = crate::paths::GleonPaths::new(root).manifests_dir(&platform_key(&ctx).unwrap());
        let mut index = WorkspaceIndex::load(&dir).unwrap();
        let staged = index.get("shots/a").unwrap().clone();
        let corrupt = b"\x89PNG\r\n\x1a\nnot an image".to_vec();
        std::fs::write(root.join("shots/a.png"), &corrupt).unwrap();
        let hash =
            crate::manifest::ImageHash::new("sha256", hex::encode(sha2::Sha256::digest(&corrupt)))
                .unwrap();
        let manifest =
            SingleTestManifest::new(hash, staged.phash, staged.width, staged.height).unwrap();
        index.save_test(&dir, "shots/a", &manifest).unwrap();

        assert_eq!(
            run_diff(&ctx, &DiffOptions::default())
                .unwrap()
                .failed_tests,
            1
        );
        let report = case_report(root, "shots/a");
        assert_eq!(report.error_kind, Some(CaseErrorKind::Image));
        assert!(
            report.message.unwrap().starts_with("candidate image"),
            "decided without the blob"
        );
    }

    /// A workspace that cannot be scanned keeps the previous run: its reports and the candidates
    /// `gleon approve` needs.
    #[test]
    fn test_diff_keeps_the_previous_run_when_the_scan_fails() {
        let (temp, ctx) = staged_workspace();
        let root = temp.path();
        image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 0, 0, 255]))
            .save(root.join("shots/a.png"))
            .unwrap();
        let result = run_diff(&ctx, &DiffOptions::default()).unwrap();
        assert_eq!(result.failed_tests, 1);
        let candidate = root.join(".gleon/runs/latest/artifacts/shots/a/candidate.png");
        assert!(candidate.is_file());

        std::fs::write(root.join("shots/bad name.png"), b"x").unwrap();
        let err = run_diff(&ctx, &DiffOptions::default()).unwrap_err();
        assert!(
            matches!(err, DiffOpError::Core(CoreError::Scanner(_))),
            "{err:?}"
        );
        assert!(candidate.is_file());
        assert_eq!(case_report(root, "shots/a").outcome, CaseOutcome::Mismatch);
        assert!(root.join(".gleon/runs/latest/report.md").is_file());

        // Nothing to compare fails too, but the previous run is gone: its candidates are of
        // screenshots that no longer exist.
        std::fs::remove_file(root.join("shots/bad name.png")).unwrap();
        std::fs::remove_file(root.join("shots/a.png")).unwrap();
        let err = run_diff(&ctx, &DiffOptions::default()).unwrap_err();
        assert!(matches!(err, DiffOpError::NoScreenshots), "{err:?}");
        assert!(err.to_string().starts_with("no screenshots match"), "{err}");
        assert!(!candidate.exists());
        assert!(matches!(
            crate::ops::approve_workspace(&ctx, &[], &[], None),
            Err(crate::ops::ApproveError::NothingToApprove { .. })
        ));
    }

    /// Images in a custom artifacts directory outside `latest/` are approved from and cleaned up
    /// like the default ones.
    #[test]
    fn test_diff_with_a_custom_artifacts_directory() {
        let (temp, ctx) = staged_workspace();
        let root = temp.path();
        image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 0, 0, 255]))
            .save(root.join("shots/a.png"))
            .unwrap();
        let options = DiffOptions {
            artifacts: Some(ArtifactsDir::new(".gleon/runs/ci").unwrap()),
            ..DiffOptions::default()
        };
        assert_eq!(run_diff(&ctx, &options).unwrap().failed_tests, 1);
        let candidate = root.join(".gleon/runs/ci/shots/a/candidate.png");
        assert!(candidate.is_file());

        let approved = crate::ops::approve_workspace(&ctx, &[], &[], None).unwrap();
        assert_eq!(approved.approved_test_cases, ["shots/a"]);
        assert_eq!(run_diff(&ctx, &options).unwrap().failed_tests, 0);
        assert!(!candidate.exists(), "the next run removes the images");
    }

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

        let err6: DiffOpError = CoreError::Io(io::Error::other("io test")).into();
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

        let err = run_diff(&ctx, &DiffOptions::default()).unwrap_err();
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

        let res = run_diff(&ctx, &DiffOptions::default());
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

        let res2 = run_diff(&ctx, &DiffOptions::default());
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
        let manifest = SingleTestManifest::new(hash, phash, 100, 100).unwrap();
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

        let res = run_diff(&ctx, &DiffOptions::default()).unwrap();
        assert_eq!(res.failed_tests, 1);
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
        let manifest = SingleTestManifest::new(hash, phash, 100, 100).unwrap();
        manifest.save(manifests_dir.join("login.json")).unwrap();

        let screenshot = temp.path().join("login.png");
        std::fs::write(&screenshot, "fake png").unwrap();

        // Remove read permissions
        let mut perms = std::fs::metadata(&screenshot).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&screenshot, perms.clone()).unwrap();

        let res = run_diff(&ctx, &DiffOptions::default());

        // Restore permissions before assertions
        perms.set_mode(0o644);
        std::fs::set_permissions(&screenshot, perms).unwrap();

        // An unreadable screenshot breaks the workspace, not one test; the reports still explain
        // the run.
        assert!(
            matches!(res, Err(DiffOpError::Screenshot { ref path, .. }) if path == "login.png"),
            "{res:?}"
        );
        assert!(temp.path().join(".gleon/runs/latest/report.md").is_file());
    }

    #[test]
    #[cfg(all(unix, not(miri)))]
    fn test_diff_fails_when_it_cannot_record() {
        use std::os::unix::fs::PermissionsExt;

        let (temp, ctx) = staged_workspace();
        let root = temp.path();
        image::RgbaImage::new(4, 4)
            .save(root.join("shots/a.png"))
            .unwrap();
        let artifacts = root.join(".gleon/runs/latest/artifacts");
        std::fs::create_dir_all(&artifacts).unwrap();
        std::fs::set_permissions(&artifacts, std::fs::Permissions::from_mode(0o555)).unwrap();
        let is_writable = std::fs::create_dir(artifacts.join("probe")).is_ok(); // As root.

        let result = run_diff(&ctx, &DiffOptions::default());
        std::fs::set_permissions(&artifacts, std::fs::Permissions::from_mode(0o755)).unwrap();
        if !is_writable {
            assert!(
                matches!(result, Err(DiffOpError::Record { ref name, .. }) if name == "shots/a"),
                "{result:?}"
            );
        }
    }

    #[test]
    fn test_diff_reports_unusable_baselines_as_errors() {
        let (temp, ctx) = staged_workspace();
        let root = temp.path();
        let blobs = root.join(".gleon/blobs/sha256");
        let blob = std::fs::read_dir(&blobs)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();

        std::fs::write(&blob, b"not a png").unwrap();
        image::RgbaImage::new(4, 4)
            .save(root.join("shots/a.png"))
            .unwrap();
        run_diff(&ctx, &DiffOptions::default()).unwrap();
        let corrupt = case_report(root, "shots/a");
        assert_eq!(
            (corrupt.outcome, corrupt.error_kind),
            (CaseOutcome::Error, Some(CaseErrorKind::Image))
        );
        assert!(corrupt.message.unwrap().starts_with("golden image: "));
        assert!(corrupt.golden.sha256.is_some(), "the blob was read");

        std::fs::remove_file(&blob).unwrap();
        run_diff(&ctx, &DiffOptions::default()).unwrap();
        let missing_blob = case_report(root, "shots/a");
        assert_eq!(missing_blob.error_kind, Some(CaseErrorKind::Io));
        assert!(missing_blob.message.unwrap().contains("gleon pull"));
        assert_eq!(
            missing_blob.golden.width,
            Some(4),
            "the size comes from the manifest"
        );
        assert!(missing_blob.artifacts.is_none());

        // The staged bytes themselves are identical without the blob: the manifest names them.
        image::RgbaImage::from_pixel(4, 4, image::Rgba([0, 0, 255, 255]))
            .save(root.join("shots/a.png"))
            .unwrap();
        let identical = run_diff(&ctx, &DiffOptions::default()).unwrap();
        assert_eq!(identical.failed_tests, 0);
        assert_eq!(case_report(root, "shots/a").outcome, CaseOutcome::Identical);
    }

    #[test]
    fn test_diff_partial_platform_override_with_fallback_success() {
        let temp = tempfile::tempdir().unwrap();
        let base_path = temp.path();
        let gleon_dir = base_path.join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();

        let linux_key = "linux-x86_64";
        let macos_key = "macos-aarch64";

        let mut ctx = ResolvedContext {
            base_dir: base_path.to_path_buf(),
            platform: PlatformInfo {
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
        let macos_m1 = SingleTestManifest::new(
            crate::manifest::ImageHash::new("sha256", &dummy_sha).unwrap(),
            dhash.clone(),
            10,
            10,
        )
        .unwrap();
        macos_m1.save(macos_manifests.join("test1.json")).unwrap();

        let macos_m2 = SingleTestManifest::new(
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
        let linux_m1 = SingleTestManifest::new(
            crate::manifest::ImageHash::new("sha256", &img1_sha).unwrap(),
            dhash,
            10,
            10,
        )
        .unwrap();
        linux_m1.save(linux_manifests.join("test1.json")).unwrap();

        // Run diff on linux
        let diff_result = run_diff(&ctx, &DiffOptions::default()).unwrap();
        assert_eq!(diff_result.total_tests, 2);
        assert_eq!(diff_result.failed_tests, 0);
    }
}
