//! One golden comparison (or update-mode write), end to end: planning, reading the golden,
//! comparing, keeping the images of a failure, recording the case report and writing the
//! integration's failure artifacts.
//!
//! The order of the steps is part of the contract: an invalid config fails every golden inside
//! the workspace, even byte-identical or missing ones; a golden that cannot be read or written is
//! an `io` error that is still recorded; the images in the artifacts directory come first (a pass
//! removes the images of an earlier failure), then the case report listing them, then the
//! integration's failure artifacts. A failure of a golden covered by a rule is always recorded
//! (`gleon report` and `gleon approve` read it); a pass only with metrics, so nothing is written
//! for a pass without them.

#![forbid(unsafe_code)]

use std::{
    fs, io,
    path::Path,
    time::{Duration, Instant},
};

use gleon_engine::{config::Zone, masking::clamped_zones};
use gleon_model::{
    case::{
        self, ArtifactImages, Artifacts, CASE_SCHEMA_VERSION, CandidateImage, CaseErrorKind,
        CaseOutcome, CaseReport, CaseTimings, GoldenImage, Metrics, RegionMetrics, Source,
        TestInfo,
    },
    fs::Durability,
    platform::PlatformConfig,
    tolerance::Tolerance,
};

use crate::{
    compare::{self, Comparison},
    error::{ErrorKind, Failure},
    session::{ArtifactNames, Plan, Session},
    text,
};

/// Result code of a call (`u8` across the ABI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Verdict {
    /// Byte-identical PNGs.
    Identical = 0,
    /// Within the tolerance.
    Match = 1,
    /// Beyond the tolerance; the message says why.
    Mismatch = 2,
    /// Different image sizes.
    DimensionMismatch = 3,
    /// Invalid input, config, image or I/O failure (see [`ErrorKind`]); never a pass.
    Error = 4,
    /// The golden was written (update mode).
    Updated = 5,
    /// The golden does not exist.
    Missing = 6,
}

/// What the integration gets back: the verdict and the texts to show.
#[derive(Debug)]
pub struct Finished {
    /// The verdict.
    pub verdict: Verdict,
    /// The class of an [`Verdict::Error`]; [`ErrorKind::None`] otherwise.
    pub error_kind: ErrorKind,
    /// The complete test failure message; empty for a pass.
    pub message: String,
    /// The console line; empty unless metrics print one.
    pub console: String,
    /// Warnings to print, one per line; usually empty.
    pub warning: String,
}

impl Finished {
    /// A failed call.
    #[must_use]
    pub fn failed(failure: Failure) -> Self {
        Self {
            verdict: Verdict::Error,
            error_kind: failure.kind,
            message: failure.message,
            console: String::new(),
            warning: String::new(),
        }
    }

    /// Adds `line`, if any, to the warnings.
    fn warn(mut self, line: Option<impl AsRef<str>>) -> Self {
        if let Some(line) = line {
            if !self.warning.is_empty() {
                self.warning.push('\n');
            }
            self.warning.push_str(line.as_ref());
        }
        self
    }
}

/// Whether a call compares or writes the golden.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Compare the candidate against the golden.
    Compare,
    /// Write the candidate as the golden (`--update-goldens` in Flutter).
    Update,
}

/// One call from the integration.
#[derive(Debug)]
pub struct Request<'a> {
    /// Compare or update.
    pub mode: Mode,
    /// The golden file.
    pub golden_path: &'a Path,
    /// The golden's key as shown in messages (`goldens/swatch.png`).
    pub golden_uri: &'a str,
    /// The directory for failure artifacts, as shown in messages.
    pub failures_dir: &'a str,
    /// The running test's full name, when known.
    pub test_name: Option<&'a str>,
    /// The candidate PNG.
    pub candidate: &'a [u8],
    /// The call's tolerance; `None` uses the `.gleon/gleon.yaml` rule, else exact.
    pub tolerance: Option<Tolerance>,
    /// The call's masks.
    pub masks: Vec<Zone>,
}

/// Runs `request` in `session`.
#[must_use]
pub fn run(session: &Session, request: &Request<'_>) -> Finished {
    let started = Instant::now();
    if let Some(failure) = session.failure() {
        return Finished::failed(failure.clone());
    }
    match request.mode {
        Mode::Compare => compare(session, request, started),
        Mode::Update => update(session, request, started),
    }
}

fn update(session: &Session, request: &Request<'_>, started: Instant) -> Finished {
    let path = request.golden_path;
    let current = fs::read(path).ok();
    // Rewriting the same bytes would cost a flush to disk and touch the file for build tools.
    let written = if current.as_deref() == Some(request.candidate) {
        Ok(())
    } else {
        gleon_model::fs::write_atomically(path, request.candidate, Durability::Durable)
    };
    let plan = match session.plan(path, request.tolerance, request.masks.clone()) {
        Ok(plan) => plan,
        Err(failure) => return Finished::failed(failure),
    };
    let warning = session.missing_workspace_warning(&plan);
    let call = |golden| Call {
        session,
        request,
        plan: &plan,
        golden,
        started,
    };
    let finished = match written {
        // After an update the golden is the candidate.
        Ok(()) => call(Some(request.candidate)).finish(
            CaseOutcome::Updated,
            Details::default(),
            Verdict::Updated,
            String::new,
        ),
        Err(e) => call(current.as_deref()).error(
            Failure::io(format!(
                "gleon: cannot write the golden {}: {e}",
                path.display()
            )),
            format!("cannot write the golden: {e}"),
        ),
    };
    finished.warn(warning)
}

fn compare(session: &Session, request: &Request<'_>, started: Instant) -> Finished {
    let path = request.golden_path;
    let golden = match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        // A directory is no golden either (reading one fails differently per OS).
        Err(e) if e.kind() == io::ErrorKind::NotFound || path.is_dir() => Ok(None),
        Err(e) => Err(format!("cannot read the golden: {e}")),
    };
    let plan = match session.plan(path, request.tolerance, request.masks.clone()) {
        Ok(plan) => plan,
        Err(failure) => return Finished::failed(failure),
    };
    let warning = session.missing_workspace_warning(&plan);
    let call = Call {
        session,
        request,
        plan: &plan,
        golden: golden.as_ref().ok().and_then(Option::as_deref),
        started,
    };
    let finished = match golden.as_ref().map(Option::as_deref) {
        Err(reason) => call.error(
            Failure::io(text::could_not_compare(request.golden_uri, reason)),
            reason.clone(),
        ),
        // The candidate is kept for `gleon approve`, unless it is no PNG at all.
        Ok(None) => call.finish(
            CaseOutcome::Missing,
            Details {
                images: ArtifactImages {
                    candidate: case::png_size(request.candidate).map(|_| request.candidate),
                    ..ArtifactImages::default()
                },
                ..Details::default()
            },
            Verdict::Missing,
            || text::missing_golden(request.golden_uri),
        ),
        // Identical encodings are identical pixels: no decoding at all, so the masks are checked
        // against the size in the PNG header.
        Ok(Some(golden)) if golden == request.candidate => {
            let clamped = case::png_size(golden).map_or(0, |(width, height)| {
                clamped_zones(&plan.masks, width, height)
            });
            call.finish(
                CaseOutcome::Identical,
                Details::default(),
                Verdict::Identical,
                String::new,
            )
            .warn(clamped_masks(request.golden_uri, clamped))
        }
        Ok(Some(golden)) => judge(
            &call,
            compare::compare(golden, request.candidate, &plan.tolerance, &plan.masks),
        ),
    };
    finished.warn(warning)
}

/// The warning for `count` masks of `golden_uri` that reached beyond the image, if any.
fn clamped_masks(golden_uri: &str, count: usize) -> Option<String> {
    (count > 0).then(|| text::clamped_masks(golden_uri, count))
}

/// Finishes `call` according to the engine's `comparison`.
fn judge(call: &Call<'_>, comparison: Comparison) -> Finished {
    let uri = call.request.golden_uri;
    match comparison {
        Comparison::Match {
            metrics,
            clamped_masks: clamped,
            native,
        } => call
            .finish(
                CaseOutcome::Match,
                Details {
                    metrics: Some(metrics),
                    native: Some(native),
                    ..Details::default()
                },
                Verdict::Match,
                String::new,
            )
            .warn(clamped_masks(uri, clamped)),
        Comparison::Error(Failure { kind, message }) => call.error(
            Failure::new(kind, text::could_not_compare(uri, &message)),
            message,
        ),
        Comparison::DimensionMismatch {
            golden,
            candidate,
            native,
        } => {
            let summary = text::dimension_summary(golden, candidate);
            let reason = format!("image sizes differ: {summary}.");
            let details = Details {
                message: Some(summary),
                native: Some(native),
                images: call.images(None),
                ..Details::default()
            };
            call.finish(
                CaseOutcome::DimensionMismatch,
                details,
                Verdict::DimensionMismatch,
                || call.failure(&reason, None),
            )
        }
        Comparison::Mismatch {
            metrics,
            diff_png,
            clamped_masks: clamped,
            native,
        } => {
            let summary = text::metrics_summary(&metrics);
            let reason = format!(
                "{summary} (gleon {}).",
                text::tolerance(&call.plan.tolerance)
            );
            let details = Details {
                message: Some(summary),
                metrics: Some(metrics),
                native: Some(native),
                images: call.images(Some(&diff_png)),
                ..Details::default()
            };
            call.finish(CaseOutcome::Mismatch, details, Verdict::Mismatch, || {
                call.failure(&reason, Some(&diff_png))
            })
            .warn(clamped_masks(uri, clamped))
        }
    }
}

/// What a case report says beyond the outcome, and the images to keep.
#[derive(Debug, Default)]
struct Details<'a> {
    message: Option<String>,
    error_kind: Option<CaseErrorKind>,
    metrics: Option<Metrics>,
    native: Option<Duration>,
    /// The images for the artifacts directory; none for passes and errors, which remove the
    /// images of an earlier failure.
    images: ArtifactImages<'a>,
}

/// One planned call with the golden's bytes (`None` for a missing golden).
struct Call<'a> {
    session: &'a Session,
    request: &'a Request<'a>,
    plan: &'a Plan,
    golden: Option<&'a [u8]>,
    started: Instant,
}

impl Call<'_> {
    /// Keeps the images of `details` in the artifacts directory and records the case report (and
    /// console line) when the plan asks for them, then finishes with `verdict` and the message
    /// built by `message`; `message` runs last, so the integration's failure artifacts are written
    /// after the report.
    fn finish(
        &self,
        outcome: CaseOutcome,
        details: Details<'_>,
        verdict: Verdict,
        message: impl FnOnce() -> String,
    ) -> Finished {
        // The comparison itself, not the writing of its outputs.
        let total = self.started.elapsed();
        let setup = self.ensure_gitignore(outcome);
        let (artifacts, kept) = match self.keep(details.images) {
            Ok(artifacts) => (artifacts, None),
            Err(warning) => (None, Some(warning)),
        };
        let recorded = self.record(outcome, details, artifacts, total);
        let finished = Finished {
            verdict,
            error_kind: ErrorKind::None,
            message: message(),
            console: String::new(),
            warning: String::new(),
        }
        .warn(setup)
        .warn(kept);
        match recorded {
            Ok(console) => Finished {
                console,
                ..finished
            },
            Err(warning) => finished.warn(Some(warning)),
        }
    }

    /// Finishes with an error of `failure`'s kind: recorded as `error` with `reason`, the message
    /// is `failure`'s.
    fn error(&self, failure: Failure, reason: String) -> Finished {
        let Failure { kind, message } = failure;
        let details = Details {
            message: Some(reason),
            error_kind: kind.case_kind(),
            ..Details::default()
        };
        Finished {
            error_kind: kind,
            ..self.finish(CaseOutcome::Error, details, Verdict::Error, || message)
        }
    }

    /// The golden and candidate of this call plus `diff_png`, as images to keep.
    const fn images<'a>(&'a self, diff_png: Option<&'a [u8]>) -> ArtifactImages<'a> {
        ArtifactImages {
            golden: self.golden,
            candidate: Some(self.request.candidate),
            diff: diff_png,
        }
    }

    /// Creates `.gleon/.gitignore` (which ignores `runs/`, where the images and reports go) when
    /// this call writes into the workspace (a failure, or any outcome with metrics); returns the
    /// warning when it cannot, since the writes themselves may still succeed.
    fn ensure_gitignore(&self, outcome: CaseOutcome) -> Option<String> {
        let golden = self.plan.in_workspace.as_ref()?;
        if golden.record.is_none() && !outcome.is_failure() {
            return None;
        }
        golden.workspace.ensure_gitignore().err().map(|e| {
            format!(
                "gleon: cannot create {}: {e}",
                golden.workspace.gleon_dir().join(".gitignore").display()
            )
        })
    }

    /// Makes the artifacts folder of the golden hold exactly `images` (none removes an earlier
    /// failure's) and returns their paths; nothing outside a workspace. Like the case report, the
    /// images are a side channel: failing to update them is a warning.
    fn keep(&self, images: ArtifactImages<'_>) -> Result<Option<Artifacts>, String> {
        let Some(golden) = &self.plan.in_workspace else {
            return Ok(None);
        };
        case::write_artifacts(
            &golden.workspace.root,
            &golden.artifacts,
            &golden.name,
            images,
        )
        .map_err(|e| {
            format!(
                "gleon: cannot update the artifacts {}/{}: {e}",
                golden.artifacts.as_str(),
                golden.name
            )
        })
    }

    /// Writes the case report of a golden covered by a rule (of a pass only with metrics; a pass
    /// without them removes the report of an earlier failure) and returns the console line (empty
    /// without metrics or with `console: false`). A report that cannot be written is returned as a
    /// warning: reports are a side channel and never change the verdict.
    fn record(
        &self,
        outcome: CaseOutcome,
        details: Details<'_>,
        artifacts: Option<Artifacts>,
        total: Duration,
    ) -> Result<String, String> {
        let record = self.plan.recorded().map(|(_, record)| record);
        let Some(golden) = self.plan.in_workspace.as_ref() else {
            return Ok(String::new());
        };
        if record.is_none() && !outcome.is_failure() {
            // A pass without metrics records nothing, but the report of an earlier failure of
            // this golden must not outlive it.
            let stale = CaseReport::path(&golden.workspace.gleon_dir(), &golden.name);
            return match fs::remove_file(&stale) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(format!(
                    "gleon: cannot remove the case report {}: {e}",
                    stale.display()
                )),
                _ => Ok(String::new()),
            };
        }
        let console = if record.is_some_and(|record| record.console) {
            text::console_line(
                &golden.golden_path,
                outcome,
                &self.plan.tolerance,
                case::millis(total),
                details.metrics.as_ref(),
                details.message.as_deref(),
            )
        } else {
            String::new()
        };
        let integration = &self.session.integration;
        let report = CaseReport {
            schema_version: CASE_SCHEMA_VERSION,
            name: golden.name.clone(),
            golden: GoldenImage::of(golden.golden_path.clone(), self.golden, None),
            candidate: CandidateImage::of(self.request.candidate),
            source: Source {
                tool: integration.tool.clone(),
                tool_version: integration.tool_version.clone(),
                renderer: integration.renderer.clone(),
            },
            platform: PlatformConfig::host(),
            test: self.request.test_name.map(|name| TestInfo {
                name: Some(name.to_owned()),
            }),
            comparison: case::Comparison {
                tolerance: self.plan.tolerance,
                masks: self.plan.masks.clone(),
                policy_version: gleon_engine::ssim::POLICY_VERSION,
            },
            outcome,
            error_kind: details.error_kind,
            message: details.message,
            metrics: details.metrics,
            regions: details
                .metrics
                .map(RegionMetrics::whole_image)
                .into_iter()
                .collect(),
            artifacts,
            timings_ms: CaseTimings::new(total, details.native),
            run_id: self.session.run_id.clone(),
            recorded_at: chrono::Utc::now(),
        };
        let gleon_dir = golden.workspace.gleon_dir();
        report.write(&gleon_dir).map(|()| console).map_err(|e| {
            format!(
                "gleon: cannot write the case report {}: {e}",
                CaseReport::path(&gleon_dir, &golden.name).display()
            )
        })
    }

    /// Writes the failure artifacts (named by the integration's [`ArtifactNames`]) into the
    /// request's failures directory and returns the failure message for `reason` pointing at
    /// them. An artifact this failure has no image for is removed, so a diff left by an earlier
    /// mismatch never passes for this failure's.
    fn failure(&self, reason: &str, diff_png: Option<&[u8]>) -> String {
        let Self {
            session,
            request,
            plan,
            golden,
            ..
        } = self;
        // `goldens/swatch.png` -> `swatch`; only the last extension goes (`a.b.png` -> `a.b`).
        let stem = request
            .golden_path
            .file_stem()
            .map_or_else(|| "golden".into(), |stem| stem.to_string_lossy());
        let dir = Path::new(request.failures_dir);
        let names = &session.integration.artifacts;
        let files = [
            (&names.diff, diff_png),
            (&names.golden, *golden),
            (&names.candidate, Some(request.candidate)),
        ];
        let written = files.into_iter().try_for_each(|(pattern, bytes)| {
            let file = dir.join(ArtifactNames::file(pattern, &stem));
            bytes.map_or_else(
                || remove_stale(&file),
                |bytes| gleon_model::fs::write_atomically(&file, bytes, Durability::Atomic),
            )
        });
        let feedback = written.map_or_else(
            |e| {
                format!(
                    "\ngleon: cannot write failure feedback to {}: {e}",
                    request.failures_dir
                )
            },
            |()| text::feedback(request.failures_dir),
        );
        text::failure(request.golden_uri, reason, &feedback, plan.has_workspace)
    }
}

/// Removes `file`; a file that is not there is fine.
fn remove_stale(file: &Path) -> io::Result<()> {
    match fs::remove_file(file) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
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
    use std::path::PathBuf;

    use gleon_engine::config::Dimension;
    use gleon_model::case::CaseReport;
    use image::{DynamicImage, ImageFormat, Rgba};

    use super::*;
    use crate::session::{Integration, SessionOptions};

    const RED: Rgba<u8> = Rgba([255, 0, 0, 255]);
    const BLUE: Rgba<u8> = Rgba([0, 0, 255, 255]);
    /// Real `flutter test` renders of the example app (text is Ahem squares).
    const COUNTER_0: &[u8] = include_bytes!("../tests/fixtures/counter_initial.png");
    const COUNTER_3: &[u8] = include_bytes!("../tests/fixtures/counter_three_taps.png");

    fn png(width: u32, height: u32, dot: bool) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(width, height, |x, y| {
            if dot && (x, y) == (1, 1) { BLUE } else { RED }
        });
        compare::encode_png(&img).unwrap()
    }

    /// `image` re-encoded as PNG in another pixel format.
    fn re_encode(image: &DynamicImage) -> Vec<u8> {
        let mut bytes = Vec::new();
        image
            .write_to(&mut io::Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        bytes
    }

    /// `png` with a `tEXt` chunk after `IHDR`: other bytes, the same pixels.
    fn with_text_chunk(png: &[u8]) -> Vec<u8> {
        fn crc32(bytes: &[u8]) -> u32 {
            let mut crc = !0u32;
            for &byte in bytes {
                crc ^= u32::from(byte);
                for _ in 0..8 {
                    crc = if crc & 1 == 1 {
                        (crc >> 1) ^ 0xedb8_8320
                    } else {
                        crc >> 1
                    };
                }
            }
            !crc
        }
        let body = b"tEXtSoftware\0another encoder";
        let mut chunk = u32::try_from(body.len() - 4)
            .unwrap()
            .to_be_bytes()
            .to_vec();
        chunk.extend_from_slice(body);
        chunk.extend_from_slice(&crc32(body).to_be_bytes());
        // Signature (8) + IHDR chunk (4 + 4 + 13 + 4).
        let mut out = png[..33].to_vec();
        out.extend_from_slice(&chunk);
        out.extend_from_slice(&png[33..]);
        out
    }

    /// A test directory with `goldens/a.png`, optionally inside a workspace with `yaml`.
    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        golden: PathBuf,
        failures: String,
    }

    impl Fixture {
        fn new(yaml: Option<&str>) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = fs::canonicalize(dir.path()).unwrap();
            if let Some(yaml) = yaml {
                fs::create_dir_all(root.join(".gleon")).unwrap();
                fs::write(root.join(".gleon/gleon.yaml"), yaml).unwrap();
            }
            let golden = root.join("test/goldens/a.png");
            fs::create_dir_all(golden.parent().unwrap()).unwrap();
            fs::write(&golden, png(4, 4, false)).unwrap();
            let failures = format!("{}/test/failures/", root.display());
            Self {
                _dir: dir,
                root,
                golden,
                failures,
            }
        }

        fn session(&self, metrics_env: Option<&str>) -> Session {
            self.session_with(SessionOptions {
                metrics_env: metrics_env.map(str::to_owned),
                ..SessionOptions::default()
            })
        }

        /// A session of the Flutter integration with the environment of `options`.
        fn session_with(&self, options: SessionOptions) -> Session {
            Session::new(SessionOptions {
                finds_workspaces: true,
                integration: Integration {
                    tool: "gleon_flutter".to_owned(),
                    tool_version: "0.1.0".to_owned(),
                    renderer: Some("flutter-3.47.5".to_owned()),
                    artifacts: ArtifactNames {
                        golden: "{name}_masterImage.png".to_owned(),
                        candidate: "{name}_testImage.png".to_owned(),
                        diff: "{name}_gleonDiff.png".to_owned(),
                    },
                },
                ..options
            })
        }

        fn run(&self, session: &Session, mode: Mode, candidate: &[u8]) -> Finished {
            self.run_with(session, mode, candidate, None, vec![])
        }

        fn run_with(
            &self,
            session: &Session,
            mode: Mode,
            candidate: &[u8],
            tolerance: Option<Tolerance>,
            masks: Vec<Zone>,
        ) -> Finished {
            run(
                session,
                &Request {
                    mode,
                    golden_path: &self.golden,
                    golden_uri: "goldens/a.png",
                    failures_dir: &self.failures,
                    test_name: Some("group test"),
                    candidate,
                    tolerance,
                    masks,
                },
            )
        }

        fn failures(&self) -> Vec<String> {
            entries(&self.root.join("test/failures"))
        }

        /// The images kept for the golden under the default artifacts directory.
        fn artifacts(&self) -> Vec<String> {
            entries(
                &self
                    .root
                    .join(".gleon/runs/latest/artifacts/test/goldens/a"),
            )
        }

        fn case_json(&self) -> serde_json::Value {
            serde_json::from_slice(&fs::read(self.case_path()).unwrap()).unwrap()
        }

        /// The case report, which must also parse as the model type (`deny_unknown_fields`).
        fn case(&self) -> CaseReport {
            serde_json::from_slice(&fs::read(self.case_path()).unwrap()).unwrap()
        }

        fn case_path(&self) -> PathBuf {
            self.root
                .join(".gleon/runs/latest/cases/test/goldens/a.json")
        }
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .map(|entries| {
                entries
                    .map(|e| e.unwrap().file_name().into_string().unwrap())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    const METRICS: &str = r#"
required_version: ">=0.1.0"
screenshots:
  - include: "test/goldens/*.png"
    mode: pixel
    diff: { threshold: 0 }
metrics:
  enabled: true
"#;

    #[test]
    fn test_identical_without_workspace_writes_nothing() {
        let fixture = Fixture::new(None);
        let session = fixture.session(None);
        let finished = fixture.run(&session, Mode::Compare, &png(4, 4, false));
        assert_eq!(finished.verdict, Verdict::Identical);
        assert_eq!(finished.error_kind, ErrorKind::None);
        assert!(finished.message.is_empty() && finished.console.is_empty());
        assert!(fixture.failures().is_empty());
        assert!(!fixture.root.join(".gleon").exists());
    }

    #[test]
    fn test_a_mismatch_writes_artifacts_and_points_at_the_config() {
        let fixture = Fixture::new(None);
        let session = fixture.session(Some("1"));
        let finished = fixture.run(&session, Mode::Compare, &png(4, 4, true));
        assert_eq!(finished.verdict, Verdict::Mismatch);
        assert_eq!(
            finished.message,
            format!(
                "Golden \"goldens/a.png\": 6.25% (1 of 16px) differ (gleon exact).\n\
                 Failure feedback can be found at {}\n\
                 Tip: tolerances can be set per golden in .gleon/gleon.yaml, the same file as \
                 the gleon CLI.",
                fixture.failures
            )
        );
        assert_eq!(finished.warning, text::MISSING_WORKSPACE_WARNING);
        assert_eq!(
            fixture.failures(),
            ["a_gleonDiff.png", "a_masterImage.png", "a_testImage.png"]
        );
        let again = fixture.run(&session, Mode::Compare, &png(4, 4, true));
        assert!(again.warning.is_empty(), "warned once per session");
    }

    #[test]
    fn test_a_dimension_mismatch_has_no_diff_image_not_even_a_stale_one() {
        let fixture = Fixture::new(None);
        let session = fixture.session(None);
        fixture.run(&session, Mode::Compare, &png(4, 4, true));
        assert_eq!(fixture.failures().len(), 3);
        let finished = fixture.run(&session, Mode::Compare, &png(5, 4, false));
        assert_eq!(finished.verdict, Verdict::DimensionMismatch);
        assert!(finished.message.starts_with(
            "Golden \"goldens/a.png\": image sizes differ: golden is 4x4px, test image is 5x4px."
        ));
        assert_eq!(fixture.failures(), ["a_masterImage.png", "a_testImage.png"]);
    }

    #[test]
    fn test_a_corrupt_candidate_is_an_image_error_without_artifacts() {
        let fixture = Fixture::new(None);
        let finished = fixture.run(&fixture.session(None), Mode::Compare, b"garbage");
        assert_eq!(finished.verdict, Verdict::Error);
        assert_eq!(finished.error_kind, ErrorKind::Image);
        assert!(
            finished
                .message
                .starts_with("Golden \"goldens/a.png\": gleon could not compare: candidate image")
        );
        assert!(fixture.failures().is_empty());
    }

    #[test]
    fn test_a_missing_golden_without_a_workspace() {
        let fixture = Fixture::new(None);
        fs::remove_file(&fixture.golden).unwrap();
        let finished = fixture.run(
            &fixture.session(Some("1")),
            Mode::Compare,
            &png(4, 4, false),
        );
        assert_eq!(finished.verdict, Verdict::Missing);
        assert_eq!(finished.error_kind, ErrorKind::None);
        assert_eq!(
            finished.message,
            "Could not be compared against non-existent file: \"goldens/a.png\""
        );
        assert_eq!(finished.warning, text::MISSING_WORKSPACE_WARNING);
        assert!(fixture.failures().is_empty());
    }

    #[test]
    fn test_a_missing_golden_is_recorded() {
        let fixture = Fixture::new(Some(METRICS));
        fs::remove_file(&fixture.golden).unwrap();
        let candidate = png(4, 4, false);
        let finished = fixture.run(&fixture.session(None), Mode::Compare, &candidate);
        assert_eq!(finished.verdict, Verdict::Missing);
        assert!(
            finished
                .console
                .starts_with("gleon ? test/goldens/a.png  missing  "),
            "{}",
            finished.console
        );
        let case = fixture.case();
        assert_eq!(case.outcome, CaseOutcome::Missing);
        assert_eq!(case.golden.path, "test/goldens/a.png");
        assert_eq!((case.golden.sha256, case.golden.width), (None, None));
        assert_eq!(case.candidate.width, Some(4));
    }

    #[test]
    fn test_an_invalid_config_fails_even_identical_and_missing_goldens() {
        let fixture = Fixture::new(Some("not: [valid"));
        let session = fixture.session(None);
        let identical = fixture.run(&session, Mode::Compare, &png(4, 4, false));
        fs::remove_file(&fixture.golden).unwrap();
        let missing = fixture.run(&session, Mode::Compare, &png(4, 4, false));
        for finished in [identical, missing] {
            assert_eq!(finished.verdict, Verdict::Error);
            assert_eq!(finished.error_kind, ErrorKind::Config);
            assert!(finished.message.starts_with(&format!(
                "gleon: {}: ",
                fixture.root.join(".gleon").join("gleon.yaml").display()
            )));
        }
    }

    #[test]
    fn test_cases_are_recorded_for_passes_and_failures() {
        let fixture = Fixture::new(Some(METRICS));
        let session = fixture.session(None);
        let identical = fixture.run(&session, Mode::Compare, &png(4, 4, false));
        assert_eq!(identical.verdict, Verdict::Identical);
        assert!(
            identical
                .console
                .starts_with("gleon = test/goldens/a.png  identical  "),
            "{}",
            identical.console
        );
        let case = fixture.case();
        assert_eq!(case.outcome, CaseOutcome::Identical);
        assert_eq!(case.name, "test/goldens/a");
        assert_eq!(case.golden.path, "test/goldens/a.png");
        assert_eq!(case.golden.width, Some(4));
        assert_eq!(case.golden.sha256, Some(case.candidate.sha256.clone()));
        assert_eq!(case.test.unwrap().name.as_deref(), Some("group test"));
        assert_eq!(case.source.renderer.as_deref(), Some("flutter-3.47.5"));
        assert_eq!(case.source.tool, "gleon_flutter");
        assert!(case.metrics.is_none());
        assert_eq!(
            (case.golden.blob, case.artifacts, case.run_id),
            (None, None, None)
        );
        assert_eq!(case.error_kind, None);
        assert_eq!(
            fs::read_to_string(fixture.root.join(".gleon/.gitignore")).unwrap(),
            "blobs/\nruns/\n.env\n.env.local\ncredentials\ndashboard.html\nhistory.json\n"
        );

        let mismatch = fixture.run(&session, Mode::Compare, &png(4, 4, true));
        assert_eq!(mismatch.verdict, Verdict::Mismatch);
        assert!(!mismatch.message.contains("Tip:"), "a workspace exists");
        let case = fixture.case_json();
        assert_eq!(case["outcome"], "mismatch");
        assert_eq!(case["message"], "6.25% (1 of 16px) differ");
        assert_eq!(case["metrics"]["diff_pixels"], 1);
        assert_eq!(case["regions"][0]["kind"], "image");
        assert!(case["timings_ms"]["native"].is_f64());
        fixture.case();
        assert_eq!(
            entries(&fixture.root.join(".gleon/runs/latest/cases/test/goldens")),
            ["a.json"],
            "no temporary files are left"
        );
    }

    const RULE_WITHOUT_METRICS: &str = r#"
required_version: ">=0.1.0"
screenshots:
  - include: "test/goldens/*.png"
    diff: { threshold: 0 }
"#;

    #[test]
    fn test_failures_keep_artifacts_next_to_the_failures_dir() {
        let fixture = Fixture::new(Some(METRICS));
        let session = fixture.session(None);
        let artifact = |file: &str| format!(".gleon/runs/latest/artifacts/test/goldens/a/{file}");

        let mismatch = fixture.run(&session, Mode::Compare, &png(4, 4, true));
        assert_eq!(mismatch.verdict, Verdict::Mismatch);
        assert!(
            mismatch.message.contains(&format!(
                "Failure feedback can be found at {}",
                fixture.failures
            )),
            "{}",
            mismatch.message
        );
        assert_eq!(fixture.failures().len(), 3);
        assert_eq!(
            fixture.artifacts(),
            ["candidate.png", "diff.png", "golden.png"]
        );
        assert_eq!(
            fixture.case().artifacts,
            Some(Artifacts {
                golden: Some(artifact("golden.png")),
                candidate: Some(artifact("candidate.png")),
                diff: Some(artifact("diff.png")),
            })
        );
        assert_eq!(
            fs::read(fixture.root.join(artifact("golden.png"))).unwrap(),
            png(4, 4, false)
        );

        let dimension = fixture.run(&session, Mode::Compare, &png(5, 4, false));
        assert_eq!(dimension.verdict, Verdict::DimensionMismatch);
        assert_eq!(fixture.artifacts(), ["candidate.png", "golden.png"]);
        assert_eq!(fixture.case().artifacts.unwrap().diff, None);

        let identical = fixture.run(&session, Mode::Compare, &png(4, 4, false));
        assert_eq!(identical.verdict, Verdict::Identical);
        assert_eq!(fixture.case().artifacts, None, "a pass keeps no images");
        assert!(
            fixture.artifacts().is_empty(),
            "and removes those of the earlier failure"
        );

        fs::remove_file(&fixture.golden).unwrap();
        let failures = fixture.failures();
        let candidate = png(4, 4, true);
        let missing = fixture.run(&session, Mode::Compare, &candidate);
        assert_eq!(missing.verdict, Verdict::Missing);
        assert_eq!(
            fixture.failures(),
            failures,
            "Flutter writes nothing for a missing golden"
        );
        assert_eq!(fixture.artifacts(), ["candidate.png"]);
        assert_eq!(
            fixture.case().artifacts,
            Some(Artifacts {
                candidate: Some(artifact("candidate.png")),
                ..Artifacts::default()
            })
        );
        assert_eq!(
            fs::read(fixture.root.join(artifact("candidate.png"))).unwrap(),
            candidate
        );

        let garbage = fixture.run(&session, Mode::Compare, b"not a png");
        assert_eq!(garbage.verdict, Verdict::Missing);
        assert!(
            fixture.artifacts().is_empty(),
            "no candidate that is no PNG to approve"
        );
    }

    /// Without metrics a failure is still recorded with its images (for `gleon report` and
    /// `gleon approve`), only passes and the console line are left out.
    #[test]
    fn test_failures_are_recorded_without_metrics_but_not_without_a_rule() {
        let fixture = Fixture::new(Some(RULE_WITHOUT_METRICS));
        let finished = fixture.run(&fixture.session(None), Mode::Compare, &png(4, 4, true));
        assert_eq!(finished.verdict, Verdict::Mismatch);
        assert!(finished.warning.is_empty(), "{}", finished.warning);
        assert!(
            finished.console.is_empty(),
            "the console line is a metrics feature"
        );
        assert_eq!(fixture.artifacts().len(), 3);
        let case = fixture.case();
        assert_eq!(case.outcome, CaseOutcome::Mismatch);
        assert!(case.artifacts.unwrap().candidate.is_some());
        assert!(
            fixture.root.join(".gleon/.gitignore").is_file(),
            "the images under .gleon/runs are ignored"
        );

        let broken = Fixture::new(Some(RULE_WITHOUT_METRICS));
        let finished = broken.run(&broken.session(None), Mode::Compare, &[7; 64]);
        assert_eq!(finished.verdict, Verdict::Error);
        assert_eq!(broken.case().error_kind, Some(CaseErrorKind::Image));
        assert!(broken.root.join(".gleon/.gitignore").is_file());

        // Fixed: the failure's report goes with its images, nothing new is recorded.
        let finished = fixture.run(&fixture.session(None), Mode::Compare, &png(4, 4, false));
        assert_eq!(finished.verdict, Verdict::Identical);
        assert!(finished.warning.is_empty(), "{}", finished.warning);
        assert!(!fixture.case_path().exists(), "no stale failure");
        assert!(fixture.artifacts().is_empty());

        let passing = Fixture::new(Some(RULE_WITHOUT_METRICS));
        let finished = passing.run(&passing.session(None), Mode::Compare, &png(4, 4, false));
        assert_eq!(finished.verdict, Verdict::Identical);
        assert!(!passing.case_path().exists());
        assert!(
            !passing.root.join(".gleon/.gitignore").exists(),
            "a pass without metrics writes nothing into the workspace"
        );

        let unmatched = Fixture::new(Some(
            "required_version: \">=0.1.0\"\nscreenshots: [{ include: \"other/*.png\" }]",
        ));
        let finished = unmatched.run(&unmatched.session(None), Mode::Compare, &png(4, 4, true));
        assert_eq!(finished.verdict, Verdict::Mismatch);
        assert!(unmatched.artifacts().is_empty());
        assert!(!unmatched.case_path().exists(), "no rule, no report");
        assert_eq!(unmatched.failures().len(), 3);
    }

    #[test]
    fn test_the_artifacts_dir_follows_the_config_then_the_environment() {
        let yaml = format!("{METRICS}artifacts: .gleon/runs/shots\n");
        let fixture = Fixture::new(Some(&yaml));
        fixture.run(&fixture.session(None), Mode::Compare, &png(4, 4, true));
        assert_eq!(
            entries(&fixture.root.join(".gleon/runs/shots/test/goldens/a")).len(),
            3
        );
        assert_eq!(
            fixture.case().artifacts.unwrap().diff.as_deref(),
            Some(".gleon/runs/shots/test/goldens/a/diff.png")
        );

        let session = fixture.session_with(SessionOptions {
            artifacts_env: Some(" .gleon/runs/ram ".to_owned()),
            ..SessionOptions::default()
        });
        fixture.run(&session, Mode::Compare, &png(4, 4, true));
        assert_eq!(
            entries(&fixture.root.join(".gleon/runs/ram/test/goldens/a")).len(),
            3
        );
        assert!(fixture.artifacts().is_empty());

        let invalid = fixture.session_with(SessionOptions {
            artifacts_env: Some("/tmp/ram".to_owned()),
            ..SessionOptions::default()
        });
        let finished = fixture.run(&invalid, Mode::Compare, &png(4, 4, false));
        assert_eq!(finished.verdict, Verdict::Error);
        assert_eq!(finished.error_kind, ErrorKind::Config);
        assert!(
            finished.message.starts_with(
                "gleon: GLEON_ARTIFACTS_DIR: '/tmp/ram' must be `.gleon/runs/latest/artifacts`"
            ),
            "{}",
            finished.message
        );
    }

    #[test]
    fn test_the_run_id_is_recorded() {
        let fixture = Fixture::new(Some(METRICS));
        let session = fixture.session_with(SessionOptions {
            run_id_env: Some("1234567890".to_owned()),
            ..SessionOptions::default()
        });
        fixture.run(&session, Mode::Compare, &png(4, 4, false));
        assert_eq!(fixture.case_json()["run_id"], "1234567890");

        let invalid = fixture.session_with(SessionOptions {
            run_id_env: Some("run 1".to_owned()),
            ..SessionOptions::default()
        });
        let finished = fixture.run(&invalid, Mode::Compare, &png(4, 4, false));
        assert_eq!(finished.error_kind, ErrorKind::Config);
        assert!(
            finished
                .message
                .starts_with("gleon: GLEON_RUN_ID: a run id must be"),
            "{}",
            finished.message
        );
    }

    #[test]
    fn test_unwritable_artifacts_are_a_warning() {
        let fixture = Fixture::new(Some(METRICS));
        fs::create_dir_all(fixture.root.join(".gleon/runs/latest")).unwrap();
        fs::write(fixture.root.join(".gleon/runs/latest/artifacts"), b"").unwrap();
        let finished = fixture.run(&fixture.session(None), Mode::Compare, &png(4, 4, true));
        assert_eq!(finished.verdict, Verdict::Mismatch);
        assert!(
            finished.warning.starts_with(
                "gleon: cannot update the artifacts .gleon/runs/latest/artifacts/test/goldens/a: "
            ),
            "{}",
            finished.warning
        );
        assert_eq!(fixture.failures().len(), 3);
        assert_eq!(fixture.case().artifacts, None);
    }

    #[cfg(unix)]
    #[test]
    fn test_a_failed_gitignore_warns_once_and_blocks_nothing() {
        use std::os::unix::fs::PermissionsExt as _;

        let fixture = Fixture::new(Some(METRICS));
        let gleon = fixture.root.join(".gleon");
        fs::create_dir_all(gleon.join("runs/latest/artifacts")).unwrap();
        fs::create_dir_all(gleon.join("runs/latest/cases")).unwrap();
        fs::set_permissions(&gleon, fs::Permissions::from_mode(0o555)).unwrap();
        let is_writable = tempfile::tempfile_in(&gleon).is_ok(); // Always writable as root.
        let finished = fixture.run(&fixture.session(None), Mode::Compare, &png(4, 4, true));
        fs::set_permissions(&gleon, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(finished.verdict, Verdict::Mismatch);
        assert_eq!(
            finished.warning.starts_with("gleon: cannot create "),
            !is_writable,
            "{}",
            finished.warning
        );
        assert_eq!(finished.warning.lines().count(), usize::from(!is_writable));
        assert_eq!(fixture.artifacts().len(), 3);
        assert_eq!(fixture.case().outcome, CaseOutcome::Mismatch);
    }

    #[test]
    fn test_identical_goldens_report_clipped_masks_too() {
        let fixture = Fixture::new(None);
        let mask = Zone {
            x: 3,
            y: 0,
            width: Dimension::Pixels(2),
            height: Dimension::Pixels(1),
        };
        let finished = fixture.run_with(
            &fixture.session(None),
            Mode::Compare,
            &png(4, 4, false),
            None,
            vec![mask],
        );
        assert_eq!(finished.verdict, Verdict::Identical);
        assert_eq!(finished.warning, text::clamped_masks("goldens/a.png", 1));
    }

    #[test]
    fn test_an_image_error_is_recorded() {
        let fixture = Fixture::new(Some(METRICS));
        let finished = fixture.run(&fixture.session(None), Mode::Compare, b"garbage");
        assert_eq!(finished.error_kind, ErrorKind::Image);
        let case = fixture.case();
        assert_eq!(case.outcome, CaseOutcome::Error);
        assert_eq!(case.error_kind, Some(CaseErrorKind::Image));
        assert_eq!(case.artifacts, None);
        assert!(case.message.unwrap().starts_with("candidate image"));
        assert!(
            finished
                .console
                .starts_with("gleon ! test/goldens/a.png  candidate image"),
            "{}",
            finished.console
        );
    }

    #[test]
    fn test_a_match_within_the_call_tolerance_is_recorded() {
        let fixture = Fixture::new(Some(METRICS));
        let tolerance = Tolerance::Pixel {
            max_diff_ratio: 0.1,
        };
        let finished = fixture.run_with(
            &fixture.session(None),
            Mode::Compare,
            &png(4, 4, true),
            Some(tolerance),
            vec![],
        );
        assert_eq!(finished.verdict, Verdict::Match);
        assert!(finished.message.is_empty());
        assert!(
            finished
                .console
                .starts_with("gleon ✓ test/goldens/a.png  pixel 6.25% (1 px, ≤10.00%, +3.75%)  "),
            "{}",
            finished.console
        );
        let case = fixture.case();
        assert_eq!(case.outcome, CaseOutcome::Match);
        assert_eq!(case.comparison.tolerance, tolerance);
        assert!(case.message.is_none());
    }

    #[test]
    fn test_ssim_rules_and_call_masks_run_end_to_end() {
        let yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include: "test/goldens/*.png"
    mode: ssim
metrics:
  enabled: true
"#;
        let fixture = Fixture::new(Some(yaml));
        fs::write(&fixture.golden, png(64, 64, false)).unwrap();
        let session = fixture.session(None);
        let changed = png(64, 64, true);
        let unmasked = fixture.run(&session, Mode::Compare, &changed);
        assert_eq!(unmasked.verdict, Verdict::Mismatch, "{}", unmasked.message);
        assert!(
            unmasked.message.contains("(gleon ssim ≥ "),
            "{}",
            unmasked.message
        );

        let mask = |x, width| Zone {
            x,
            y: 0,
            width: Dimension::Pixels(width),
            height: Dimension::Pixels(4),
        };
        let masked = fixture.run_with(
            &session,
            Mode::Compare,
            &changed,
            None,
            vec![mask(0, 4), mask(60, 10)],
        );
        assert_eq!(masked.verdict, Verdict::Match, "{}", masked.message);
        assert!(
            masked
                .console
                .starts_with("gleon ✓ test/goldens/a.png  ssim ")
        );
        assert_eq!(
            masked.warning,
            text::clamped_masks("goldens/a.png", 1),
            "the mask reaching x = 70 is reported"
        );
        assert_eq!(fixture.case().comparison.masks.len(), 2);
    }

    #[test]
    fn test_real_flutter_goldens() {
        let fixture = Fixture::new(None);
        fs::write(&fixture.golden, COUNTER_0).unwrap();
        let session = fixture.session(None);

        let decoded = image::load_from_memory(COUNTER_0).unwrap();
        for same_pixels in [
            re_encode(&decoded),
            re_encode(&DynamicImage::ImageRgba16(decoded.to_rgba16())),
            with_text_chunk(COUNTER_0),
        ] {
            assert_ne!(same_pixels, COUNTER_0);
            let finished = fixture.run(&session, Mode::Compare, &same_pixels);
            assert_eq!(finished.verdict, Verdict::Match, "{}", finished.message);
        }

        // The counter text changes from 0 to 3: a regression under every tolerance, including the
        // example's cross-OS calibration (`flutter/example/.gleon/gleon.yaml`).
        for tolerance in [
            None,
            Some(Tolerance::Ssim {
                min_similarity: 0.8,
                color_tolerance: 8.0,
            }),
            Some(Tolerance::Ssim {
                min_similarity: 0.73,
                color_tolerance: 46.0,
            }),
        ] {
            let finished = fixture.run_with(&session, Mode::Compare, COUNTER_3, tolerance, vec![]);
            assert_eq!(finished.verdict, Verdict::Mismatch, "{tolerance:?}");
        }
    }

    #[test]
    fn test_opaque_images_without_alpha_match_their_rgba_golden() {
        let fixture = Fixture::new(None);
        let session = fixture.session(None);
        let rgba = image::load_from_memory(&png(4, 4, true)).unwrap();
        fs::write(&fixture.golden, re_encode(&rgba)).unwrap();
        for opaque in [
            DynamicImage::ImageRgb8(rgba.to_rgb8()),
            DynamicImage::ImageRgba16(rgba.to_rgba16()),
        ] {
            let finished = fixture.run(&session, Mode::Compare, &re_encode(&opaque));
            assert_eq!(finished.verdict, Verdict::Match, "{}", finished.message);
        }
        let gray = image::GrayImage::from_pixel(4, 4, image::Luma([128]));
        fs::write(
            &fixture.golden,
            re_encode(&DynamicImage::ImageLuma8(gray.clone())),
        )
        .unwrap();
        let as_rgba = DynamicImage::ImageRgba8(DynamicImage::ImageLuma8(gray).to_rgba8());
        let finished = fixture.run(&session, Mode::Compare, &re_encode(&as_rgba));
        assert_eq!(finished.verdict, Verdict::Match, "{}", finished.message);
    }

    #[test]
    fn test_an_unwritable_case_report_is_a_warning_not_a_failure() {
        let fixture = Fixture::new(Some(METRICS));
        // A file where the cases directory should be.
        fs::create_dir_all(fixture.root.join(".gleon/runs/latest")).unwrap();
        fs::write(fixture.root.join(".gleon/runs/latest/cases"), b"").unwrap();
        let finished = fixture.run(&fixture.session(None), Mode::Compare, &png(4, 4, false));
        assert_eq!(finished.verdict, Verdict::Identical);
        assert!(finished.console.is_empty());
        assert!(
            finished
                .warning
                .starts_with("gleon: cannot write the case report"),
            "{}",
            finished.warning
        );
    }

    #[test]
    fn test_unwritable_failure_feedback_is_reported_in_the_message() {
        let fixture = Fixture::new(None);
        fs::write(fixture.root.join("test/failures"), b"").unwrap();
        let finished = fixture.run(&fixture.session(None), Mode::Compare, &png(4, 4, true));
        assert_eq!(finished.verdict, Verdict::Mismatch);
        assert!(
            finished
                .message
                .contains("gleon: cannot write failure feedback to"),
            "{}",
            finished.message
        );
    }

    #[test]
    fn test_a_stale_artifact_that_cannot_be_removed_is_reported() {
        let fixture = Fixture::new(None);
        fs::create_dir_all(fixture.root.join("test/failures/a_gleonDiff.png/inside")).unwrap();
        let finished = fixture.run(&fixture.session(None), Mode::Compare, &png(5, 4, false));
        assert_eq!(finished.verdict, Verdict::DimensionMismatch);
        assert!(
            finished
                .message
                .contains("gleon: cannot write failure feedback to"),
            "{}",
            finished.message
        );
    }

    #[test]
    fn test_warnings_are_one_per_line() {
        let fixture = Fixture::new(None);
        let mask = Zone {
            x: 3,
            y: 3,
            width: Dimension::Pixels(2),
            height: Dimension::Pixels(2),
        };
        let finished = fixture.run_with(
            &fixture.session(Some("1")),
            Mode::Compare,
            &png(4, 4, true),
            None,
            vec![mask],
        );
        assert_eq!(finished.verdict, Verdict::Mismatch);
        assert_eq!(
            finished.warning,
            format!(
                "{}\n{}",
                text::clamped_masks("goldens/a.png", 1),
                text::MISSING_WORKSPACE_WARNING
            )
        );
    }

    #[test]
    fn test_a_directory_is_not_a_golden() {
        let fixture = Fixture::new(None);
        fs::remove_file(&fixture.golden).unwrap();
        fs::create_dir_all(fixture.golden.join("inside")).unwrap();
        let finished = fixture.run(&fixture.session(None), Mode::Compare, &png(4, 4, false));
        assert_eq!(finished.verdict, Verdict::Missing);
    }

    #[cfg(unix)]
    #[test]
    fn test_an_unreadable_golden_is_an_io_error() {
        use std::os::unix::fs::PermissionsExt as _;

        let fixture = Fixture::new(None);
        fs::set_permissions(&fixture.golden, fs::Permissions::from_mode(0o000)).unwrap();
        let is_readable = fs::read(&fixture.golden).is_ok(); // Always readable as root.
        let finished = fixture.run(&fixture.session(None), Mode::Compare, &png(4, 4, true));
        fs::set_permissions(&fixture.golden, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(finished.error_kind == ErrorKind::Io, !is_readable);
        assert_eq!(
            finished.message.contains("cannot read the golden"),
            !is_readable,
            "{}",
            finished.message
        );

        let recorded = Fixture::new(Some(METRICS));
        fs::write(&recorded.golden, png(4, 4, true)).unwrap();
        fs::set_permissions(&recorded.golden, fs::Permissions::from_mode(0o000)).unwrap();
        let finished = recorded.run(&recorded.session(None), Mode::Compare, &png(4, 4, false));
        fs::set_permissions(&recorded.golden, fs::Permissions::from_mode(0o644)).unwrap();
        if !is_readable {
            assert_eq!(finished.error_kind, ErrorKind::Io);
            let case = recorded.case();
            assert_eq!(
                (case.outcome, case.error_kind),
                (CaseOutcome::Error, Some(CaseErrorKind::Io))
            );
            assert!(
                case.message
                    .unwrap()
                    .starts_with("cannot read the golden: ")
            );
            assert_eq!(case.golden.sha256, None);
        }
    }

    #[test]
    fn test_update_writes_the_golden_and_records_it() {
        let fixture = Fixture::new(Some(METRICS));
        let candidate = png(4, 4, true);
        let finished = fixture.run(&fixture.session(None), Mode::Update, &candidate);
        assert_eq!(finished.verdict, Verdict::Updated);
        assert_eq!(fs::read(&fixture.golden).unwrap(), candidate);
        assert!(
            finished.console.contains("  updated  "),
            "{}",
            finished.console
        );
        let case = fixture.case();
        assert_eq!(case.outcome, CaseOutcome::Updated);
        assert_eq!(case.golden.sha256, Some(case.candidate.sha256));

        let broken = Fixture::new(Some("not: [valid"));
        let finished = broken.run(&broken.session(None), Mode::Update, &candidate);
        assert_eq!(
            finished.verdict,
            Verdict::Error,
            "the golden is written first"
        );
        assert_eq!(fs::read(&broken.golden).unwrap(), candidate);

        let blocked = [Fixture::new(None), Fixture::new(Some(METRICS))];
        for fixture in &blocked {
            fs::remove_file(&fixture.golden).unwrap();
            fs::create_dir_all(fixture.golden.join("inside")).unwrap();
            let finished = fixture.run(&fixture.session(None), Mode::Update, &candidate);
            assert_eq!(finished.error_kind, ErrorKind::Io);
            assert!(
                finished
                    .message
                    .starts_with("gleon: cannot write the golden"),
                "{}",
                finished.message
            );
        }
        let case = blocked[1].case();
        assert_eq!(
            (case.outcome, case.error_kind),
            (CaseOutcome::Error, Some(CaseErrorKind::Io)),
            "a failed update is recorded"
        );
        assert!(
            case.message
                .unwrap()
                .starts_with("cannot write the golden: ")
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_update_leaves_an_unchanged_golden_alone() {
        use std::os::unix::fs::PermissionsExt as _;

        let fixture = Fixture::new(None);
        let dir = fixture.golden.parent().unwrap().to_path_buf();
        // A read-only directory: any rewrite would fail to create its temporary file.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
        let is_writable = tempfile::tempfile_in(&dir).is_ok(); // Always writable as root.
        let session = fixture.session(None);
        let unchanged = fixture.run(&session, Mode::Update, &png(4, 4, false));
        let changed = fixture.run(&session, Mode::Update, &png(4, 4, true));
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(unchanged.verdict, Verdict::Updated, "{}", unchanged.message);
        assert_eq!(changed.error_kind == ErrorKind::Io, !is_writable);
    }

    #[test]
    fn test_concurrent_calls_share_a_session() {
        let fixture = Fixture::new(Some(METRICS));
        let session = fixture.session(None);
        let candidate = png(4, 4, true);
        std::thread::scope(|scope| {
            let calls: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| fixture.run(&session, Mode::Compare, &candidate)))
                .collect();
            for call in calls {
                assert_eq!(call.join().unwrap().verdict, Verdict::Mismatch);
            }
        });
        assert_eq!(fixture.case().outcome, CaseOutcome::Mismatch);
        assert_eq!(
            entries(&fixture.root.join(".gleon/runs/latest/cases/test/goldens")),
            ["a.json"]
        );
        assert_eq!(fixture.failures().len(), 3, "no temporary files are left");
    }
}
