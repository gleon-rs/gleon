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
    borrow::Cow,
    cell::OnceCell,
    fs, io,
    path::Path,
    time::{Duration, Instant},
};

use gleon_engine::{Region, config::Zone, masking::clamped_zones};
use gleon_model::{
    case::{
        self, ArtifactImages, Artifacts, CASE_SCHEMA_VERSION, CandidateImage, CaseErrorKind,
        CaseOutcome, CaseReport, CaseTimings, GoldenImage, Metrics, RegionMetrics, Source,
        TestInfo,
    },
    compare::{Candidate, Compared, Comparison, Text},
    fs::Durability,
    platform::{self, PlatformConfig, PlatformKey},
    tolerance::{TextTolerance, Tolerance},
};

use crate::{
    compare::{self, Timed},
    error::{ErrorKind, Failure},
    session::{ArtifactNames, Fallback, Goldens, Plan, Session},
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
    /// The candidate: PNG bytes, or raw pixels (compare mode only).
    pub candidate: Candidate<'a>,
    /// The call's tolerance; `None` uses the `.gleon/gleon.yaml` rule, else exact.
    pub tolerance: Option<Tolerance>,
    /// The call's masks.
    pub masks: Vec<Zone>,
    /// The text regions of the candidate (pixels).
    pub text_regions: Vec<Region>,
    /// The call's tolerance of text; `None` uses the rule's.
    pub text: Option<TextTolerance>,
}

/// Runs `request` in `session`.
#[must_use]
pub fn run(session: &Session, request: &Request<'_>) -> Finished {
    let started = Instant::now();
    if request.mode == Mode::Update && !matches!(request.candidate, Candidate::Png(_)) {
        return Finished::failed(Failure::invalid_input(
            "gleon: update mode takes the candidate as PNG",
        ));
    }
    // Planned first, also in update mode: which file this platform writes depends on the config
    // of the workspace, and an invalid one writes nothing.
    let plan = match session.plan(
        request.golden_path,
        request.tolerance,
        request.masks.clone(),
        request.text,
    ) {
        Ok(plan) => plan,
        Err(failure) => return Finished::failed(failure),
    };
    let warning = session.missing_workspace_warning(&plan);
    let finished = match request.candidate {
        Candidate::Png(candidate) if request.mode == Mode::Update => {
            update(session, request, &plan, candidate, started)
        }
        _ => compare(session, request, &plan, started),
    };
    finished.warn(warning)
}

fn update(
    session: &Session,
    request: &Request<'_>,
    plan: &Plan,
    candidate: &[u8],
    started: Instant,
) -> Finished {
    let path = &plan.goldens.target;
    let current = fs::read(path).ok();
    // Rewriting the same bytes would cost a flush to disk and touch the file for build tools.
    let written = if current.as_deref() == Some(candidate) {
        Ok(())
    } else {
        gleon_model::fs::write_atomically(path, candidate, Durability::Durable)
    };
    let call = |golden| Call {
        session,
        request,
        plan,
        golden,
        fallback: None,
        encoded: OnceCell::new(),
        started,
    };
    match written {
        // After an update the golden is the candidate.
        Ok(()) => call(Some(candidate)).finish(
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
    }
}

fn compare(session: &Session, request: &Request<'_>, plan: &Plan, started: Instant) -> Finished {
    let (golden, fallback) = read_golden(&plan.goldens);
    let call = Call {
        session,
        request,
        plan,
        golden: golden.as_ref().ok().and_then(Option::as_deref),
        fallback,
        encoded: OnceCell::new(),
        started,
    };
    // The call asked for a text tolerance that cannot apply (an SSIM rule, or a candidate
    // without text regions): say so instead of comparing the text like everything else
    // silently.
    let unused_text = (request.text.is_some() && call.text().is_none())
        .then(|| text::unused_text_tolerance(request.golden_uri));
    let finished = match golden.as_ref().map(Option::as_deref) {
        Err(reason) => call.error(
            Failure::io(text::could_not_compare(&call.compared_uri(), reason)),
            reason.clone(),
        ),
        // The candidate is kept for `gleon approve`, unless it is no PNG at all.
        Ok(None) => call.finish(
            CaseOutcome::Missing,
            Details {
                images: ArtifactImages {
                    candidate: call
                        .candidate_png()
                        .filter(|png| case::png_size(png).is_some()),
                    ..ArtifactImages::default()
                },
                ..Details::default()
            },
            Verdict::Missing,
            || call.missing_message(),
        ),
        // Identical encodings are identical pixels: no decoding at all, so the masks are checked
        // against the size in the PNG header.
        Ok(Some(golden)) if matches!(request.candidate, Candidate::Png(png) if png == golden) => {
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
        Ok(Some(golden)) => {
            let text = call.text().map(|tolerance| Text {
                regions: &request.text_regions,
                tolerance,
            });
            judge(
                &call,
                compare::compare(
                    golden,
                    request.candidate,
                    &plan.tolerance,
                    &plan.masks,
                    text,
                ),
            )
        }
    };
    finished.warn(unused_text)
}

/// The golden this platform compares: its own, else the shared one of another platform (then
/// returned too). `None` when neither exists.
fn read_golden(goldens: &Goldens) -> (Result<Option<Vec<u8>>, String>, Option<&Fallback>) {
    let read = |path: &Path| match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        // A directory is no golden either (reading one fails differently per OS).
        Err(e) if e.kind() == io::ErrorKind::NotFound || path.is_dir() => Ok(None),
        Err(e) => Err(format!("cannot read the golden: {e}")),
    };
    match (read(&goldens.target), &goldens.fallback) {
        (Ok(None), Some(fallback)) => match read(&fallback.golden) {
            Ok(None) => (Ok(None), None),
            golden => (golden, Some(fallback)),
        },
        (golden, _) => (golden, None),
    }
}

/// The warning for `count` masks of `golden_uri` that reached beyond the image, if any.
fn clamped_masks(golden_uri: &str, count: usize) -> Option<String> {
    (count > 0).then(|| text::clamped_masks(golden_uri, count))
}

/// Finishes `call` according to the engine's comparison.
fn judge(call: &Call<'_>, Timed { comparison, native }: Timed) -> Finished {
    let uri = call.request.golden_uri;
    let Comparison {
        compared,
        clamped_masks: clamped,
    } = match comparison {
        Ok(comparison) => comparison,
        Err(Failure { kind, message }) => {
            return call.error(
                Failure::new(
                    kind,
                    text::could_not_compare(&call.compared_uri(), &message),
                ),
                message,
            );
        }
    };
    match compared {
        Compared::Match { metrics, regions } => call
            .finish(
                CaseOutcome::Match,
                Details {
                    metrics: Some(metrics),
                    regions,
                    native: Some(native),
                    images: call.pass_images(&metrics),
                    ..Details::default()
                },
                Verdict::Match,
                String::new,
            )
            .warn(clamped_masks(uri, clamped)),
        Compared::DimensionMismatch { golden, candidate } => {
            let summary = text::dimension_summary(golden, candidate);
            let reason = format!("image sizes differ: {summary}.{}", call.fallback_clause());
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
        Compared::Mismatch {
            metrics,
            regions,
            diff_png,
        } => {
            let summary = text::metrics_summary(&metrics);
            let mut tolerance = text::tolerance(&call.plan.tolerance);
            if let Some(text) = call.text() {
                tolerance = format!("{tolerance}, {}", text::text_tolerance(&text));
            }
            let reason = format!("{summary} (gleon {tolerance}).{}", call.fallback_clause());
            let details = Details {
                message: Some(summary),
                metrics: Some(metrics),
                regions,
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
    /// The compared regions of a match or mismatch.
    regions: Vec<RegionMetrics>,
    native: Option<Duration>,
    /// The images for the artifacts directory; none for errors and passes (but the candidate of
    /// a pass against another platform's golden), which remove the images of an earlier failure.
    images: ArtifactImages<'a>,
}

/// One planned call with the golden's bytes (`None` for a missing golden).
struct Call<'a> {
    session: &'a Session,
    request: &'a Request<'a>,
    plan: &'a Plan,
    golden: Option<&'a [u8]>,
    /// The shared golden of another platform, when `golden` is it because this platform has no
    /// own golden yet.
    fallback: Option<&'a Fallback>,
    /// The PNG of raw candidate pixels, encoded once when a failure keeps the candidate.
    encoded: OnceCell<Option<Vec<u8>>>,
    started: Instant,
}

impl Call<'_> {
    /// The candidate as PNG: the given bytes, or the raw pixels encoded on first use (`None` if
    /// they cannot be). Only failures need it, so a passing raw candidate is never encoded.
    fn candidate_png(&self) -> Option<&[u8]> {
        match self.request.candidate {
            Candidate::Png(png) => Some(png),
            Candidate::Rgba { .. } => self
                .encoded
                .get_or_init(|| self.request.candidate.to_png().map(Cow::into_owned))
                .as_deref(),
        }
    }

    /// The candidate of the case report: hashed when there is a PNG of it (given, or encoded
    /// for a failure), else the size of the raw pixels.
    fn candidate_image(&self) -> CandidateImage {
        match self.request.candidate {
            Candidate::Png(png) => CandidateImage::of(png),
            Candidate::Rgba { width, height, .. } => self
                .encoded
                .get()
                .and_then(Option::as_deref)
                .map_or_else(|| CandidateImage::raw(width, height), CandidateImage::of),
        }
    }

    /// The tolerance of text that applies, for text regions in pixel or exact mode: the plan's,
    /// else the default of the compared golden ([`TextTolerance::resolve`]).
    fn text(&self) -> Option<TextTolerance> {
        let is_pixel = matches!(
            self.plan.tolerance,
            Tolerance::Exact {} | Tolerance::Pixel { .. }
        );
        let is_own = self.plan.goldens.is_own && self.fallback.is_none();
        (is_pixel && !self.request.text_regions.is_empty())
            .then(|| TextTolerance::resolve(self.plan.text, is_own))
    }

    /// The key of this platform's own golden, as the integration's key (`golden_uri`) names the
    /// shared one, when the workspace keeps one per platform.
    fn own_uri(&self) -> Option<String> {
        self.plan
            .goldens
            .fallback
            .as_ref()
            .map(|_| platform::platform_golden(self.request.golden_uri, PlatformKey::host()))
    }

    /// The key of the golden this call compares, for messages about it.
    fn compared_uri(&self) -> Cow<'_, str> {
        match self.own_uri() {
            Some(own) if self.fallback.is_none() => Cow::Owned(own),
            _ => Cow::Borrowed(self.request.golden_uri),
        }
    }

    /// The clause of a failure against another platform's shared golden; empty for any other.
    fn fallback_clause(&self) -> String {
        match (self.fallback, self.own_uri()) {
            (Some(fallback), Some(own)) => text::fallback(fallback.platform.as_str(), &own),
            _ => String::new(),
        }
    }

    /// The images a recorded pass keeps: the candidate of a pass against another platform's
    /// golden that differs from it, so `gleon approve` can make it this platform's own; none
    /// otherwise (approving a pass without differences copies the compared golden).
    fn pass_images(&self, metrics: &Metrics) -> ArtifactImages<'_> {
        let keeps = self.fallback.is_some() && self.plan.recorded().is_some() && metrics.differs();
        ArtifactImages {
            candidate: keeps.then(|| self.candidate_png()).flatten(),
            ..ArtifactImages::default()
        }
    }

    /// The message of a missing golden: Flutter's, naming the golden this platform compares and,
    /// on a platform with its own goldens, the shared one of the fallback platform it lacks too.
    fn missing_message(&self) -> String {
        let mut message = text::missing_golden(&self.compared_uri());
        if let Some(fallback) = &self.plan.goldens.fallback {
            message.push_str(&text::missing_fallback(
                fallback.platform.as_str(),
                self.request.golden_uri,
            ));
        }
        message
    }

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
    fn images<'a>(&'a self, diff_png: Option<&'a [u8]>) -> ArtifactImages<'a> {
        ArtifactImages {
            golden: self.golden,
            candidate: self.candidate_png(),
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

    /// Makes the artifacts folder of the golden on this platform
    /// (`<artifacts dir>/<host key>/<name>/`) hold exactly `images` (none removes an earlier
    /// failure's) and returns their paths; nothing outside a workspace. Other platforms' images of
    /// the golden stay. Like the case report, the images are a side channel: failing to update
    /// them is a warning.
    fn keep(&self, images: ArtifactImages<'_>) -> Result<Option<Artifacts>, String> {
        let Some(golden) = &self.plan.in_workspace else {
            return Ok(None);
        };
        let platform_key = PlatformKey::host();
        case::write_artifacts(
            &golden.workspace.root,
            &golden.artifacts,
            platform_key,
            &golden.name,
            images,
        )
        .map_err(|e| {
            format!(
                "gleon: cannot update the artifacts {}/{platform_key}/{}: {e}",
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
            // this golden on this platform must not outlive it.
            let stale = CaseReport::path(
                &golden.workspace.gleon_dir(),
                PlatformKey::host(),
                &golden.name,
            );
            return match fs::remove_file(&stale) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(format!(
                    "gleon: cannot remove the case report {}: {e}",
                    stale.display()
                )),
                _ => Ok(String::new()),
            };
        }
        let fallback = self.fallback.map(|fallback| fallback.path.clone());
        let console = if record.is_some_and(|record| record.console) {
            text::console_line(
                fallback.as_deref().unwrap_or(&golden.golden_path),
                outcome,
                &self.plan.tolerance,
                self.text(),
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
            golden: GoldenImage {
                fallback,
                ..GoldenImage::of(golden.golden_path.clone(), self.golden, None)
            },
            candidate: self.candidate_image(),
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
                text_tolerance: self.text(),
            },
            outcome,
            error_kind: details.error_kind,
            message: details.message,
            metrics: details.metrics,
            regions: if details.regions.is_empty() {
                details
                    .metrics
                    .map(RegionMetrics::whole_image)
                    .into_iter()
                    .collect()
            } else {
                details.regions
            },
            artifacts,
            timings_ms: CaseTimings::new(total, details.native),
            run_id: self.session.run_id.clone(),
            recorded_at: chrono::Utc::now(),
        };
        let gleon_dir = golden.workspace.gleon_dir();
        report.write(&gleon_dir).map(|()| console).map_err(|e| {
            format!(
                "gleon: cannot write the case report {}: {e}",
                CaseReport::path(&gleon_dir, PlatformKey::host(), &golden.name).display()
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
            (&names.candidate, self.candidate_png()),
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
        text::failure(&self.compared_uri(), reason, &feedback, plan.has_workspace)
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
        gleon_model::compare::encode_png(&img).unwrap()
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
            // Without the `\\?\` prefix of Windows, which takes no `/` separators.
            let root = crate::session::canonical(dir.path()).unwrap();
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
                    candidate: Candidate::Png(candidate),
                    tolerance,
                    masks,
                    text_regions: Vec::new(),
                    text: None,
                },
            )
        }

        fn failures(&self) -> Vec<String> {
            entries(&self.root.join("test/failures"))
        }

        /// Compares the raw pixels of `candidate` with `text_regions` under `text` (the rule's
        /// when `None`) and `tolerance`.
        fn compare_raw(
            &self,
            session: &Session,
            candidate: &image::RgbaImage,
            tolerance: Option<Tolerance>,
            text_regions: Vec<Region>,
            text: Option<TextTolerance>,
        ) -> Finished {
            run(
                session,
                &Request {
                    mode: Mode::Compare,
                    golden_path: &self.golden,
                    golden_uri: "goldens/a.png",
                    failures_dir: &self.failures,
                    test_name: None,
                    candidate: Candidate::Rgba {
                        width: candidate.width(),
                        height: candidate.height(),
                        pixels: candidate.as_raw(),
                    },
                    tolerance,
                    masks: Vec::new(),
                    text_regions,
                    text,
                },
            )
        }

        /// The images kept for the golden on this platform under the default artifacts directory.
        fn artifacts(&self) -> Vec<String> {
            entries(
                &self
                    .root
                    .join(".gleon/runs/latest/artifacts")
                    .join(PlatformKey::host())
                    .join("test/goldens/a"),
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
            CaseReport::path(
                &self.root.join(".gleon"),
                PlatformKey::host(),
                "test/goldens/a",
            )
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
        assert_eq!(case.golden.sha256, case.candidate.sha256.clone());
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
            entries(
                &fixture
                    .root
                    .join(".gleon/runs/latest/cases")
                    .join(PlatformKey::host())
                    .join("test/goldens")
            ),
            ["a.json"],
            "no temporary files are left"
        );
    }

    /// Raw pixels compare like their PNG: equal pixels match (with metrics, never `identical`,
    /// which is about bytes) and no PNG is encoded for it; a failure encodes the candidate once
    /// for the artifacts, the failure feedback and the hash `gleon approve` checks.
    #[test]
    fn test_raw_candidates_encode_a_png_only_for_failures() {
        let fixture = Fixture::new(Some(METRICS));
        let session = fixture.session(None);
        let pixels = |dot| image::load_from_memory(&png(4, 4, dot)).unwrap().to_rgba8();

        let same = fixture.compare_raw(&session, &pixels(false), None, Vec::new(), None);
        assert_eq!(same.verdict, Verdict::Match);
        let case = fixture.case();
        assert_eq!(case.candidate, CandidateImage::raw(4, 4));
        assert!(fixture.artifacts().is_empty());

        let changed = fixture.compare_raw(&session, &pixels(true), None, Vec::new(), None);
        assert_eq!(changed.verdict, Verdict::Mismatch);
        let case = fixture.case();
        let kept = fixture
            .root
            .join(case.artifacts.unwrap().candidate.unwrap());
        let png = fs::read(kept).unwrap();
        assert_eq!(case.candidate, CandidateImage::of(&png));
        assert_eq!(
            image::load_from_memory(&png).unwrap().to_rgba8(),
            pixels(true)
        );
        assert!(
            fixture.failures().contains(&"a_testImage.png".to_owned()),
            "{:?}",
            fixture.failures()
        );
    }

    /// Text regions are compared under the text tolerance (the call's, else the rule's, else 1:
    /// text never fails), everything else strictly; the case report keeps the text tolerance and
    /// the worst tile. In SSIM mode text regions do not apply.
    #[test]
    fn test_text_regions_are_compared_under_the_text_tolerance() {
        let white = Rgba([255, 255, 255, 255]);
        let golden = image::RgbaImage::from_pixel(32, 16, white);
        let text = vec![Region {
            x: 0,
            y: 0,
            width: 16,
            height: 16,
        }];
        // A changed glyph: 32 of the 256 pixels of the text's tile.
        let mut glyph = golden.clone();
        for i in 0..32 {
            glyph.put_pixel(i % 16, i / 16, Rgba([0, 0, 0, 255]));
        }
        let mut outside = golden.clone();
        outside.put_pixel(20, 3, Rgba([0, 0, 0, 255]));
        let fixture_with = |yaml: &str| {
            let fixture = Fixture::new(Some(yaml));
            fs::write(
                &fixture.golden,
                gleon_model::compare::encode_png(&golden).unwrap(),
            )
            .unwrap();
            fixture
        };

        // Without a rule's: text never fails, the rest is exact.
        let fixture = fixture_with(METRICS);
        let session = fixture.session(None);
        let finished = fixture.compare_raw(&session, &glyph, None, text.clone(), None);
        assert_eq!(finished.verdict, Verdict::Match, "{}", finished.message);
        let case = fixture.case();
        assert_eq!(case.comparison.text_tolerance, Some(TextTolerance::DEFAULT));
        assert!(matches!(
            case.metrics,
            Some(Metrics::Pixel {
                total_pixels: 256,
                diff_pixels: 0,
                text: Some(case::TextMetrics {
                    pixels: 256,
                    diff_pixels: 32,
                    ..
                }),
                ..
            })
        ));
        assert_eq!(case.regions.len(), 2);
        assert_eq!(case.regions[1].kind, case::RegionKind::Text);
        let finished = fixture.compare_raw(&session, &outside, None, text.clone(), None);
        assert_eq!(finished.verdict, Verdict::Mismatch);
        assert!(
            finished
                .message
                .contains("(gleon pixel ≤ 0.00%, text ignored)"),
            "{}",
            finished.message
        );

        // The rule's: 10% of a tile; the call's beats it.
        let fixture = fixture_with(&METRICS.replace(
            "diff: { threshold: 0 }",
            "diff: { threshold: 0 }\n    text_tolerance: 0.1",
        ));
        let session = fixture.session(None);
        let finished = fixture.compare_raw(&session, &glyph, None, text.clone(), None);
        assert_eq!(finished.verdict, Verdict::Mismatch);
        assert!(
            finished.message.contains(
                ": text up to 12.50% of a tile differs (gleon pixel ≤ 0.00%, text ≤ 10.00% per tile)"
            ),
            "{}",
            finished.message
        );
        let loose = Some(TextTolerance(0.2));
        let finished = fixture.compare_raw(&session, &glyph, None, text.clone(), loose);
        assert_eq!(finished.verdict, Verdict::Match, "{}", finished.message);

        // SSIM compares text like everything else; no text tolerance is recorded.
        let ssim = Tolerance::Ssim {
            min_similarity: 0.5,
            color_tolerance: 255.0,
        };
        let finished = fixture.compare_raw(&session, &glyph, Some(ssim), text.clone(), None);
        assert_eq!(finished.verdict, Verdict::Mismatch, "{}", finished.message);
        assert_eq!(fixture.case().comparison.text_tolerance, None);
        assert!(finished.warning.is_empty(), "the rule's text: no warning");

        // A text tolerance of the call that cannot apply warns: under SSIM, and without text
        // regions (byte inputs).
        for (tolerance, regions) in [(Some(ssim), text), (None, Vec::new())] {
            let finished = fixture.compare_raw(&session, &glyph, tolerance, regions, loose);
            assert!(
                finished
                    .warning
                    .contains("the text tolerance of golden \"goldens/a.png\" did not apply"),
                "{}",
                finished.warning
            );
        }
    }

    /// A platform other than this one.
    fn foreign_platform() -> &'static str {
        if cfg!(target_os = "linux") {
            "windows-x86_64"
        } else {
            "linux-x86_64"
        }
    }

    /// Text on white with one line of text in its first tile: the golden, one pixel of noise in
    /// the text, a changed glyph (12.5% of the tile) and a changed pixel outside the text.
    struct TextImages {
        golden: image::RgbaImage,
        noise: image::RgbaImage,
        glyph: image::RgbaImage,
        outside: image::RgbaImage,
        regions: Vec<Region>,
    }

    impl TextImages {
        fn new() -> Self {
            let golden = image::RgbaImage::from_pixel(32, 16, Rgba([255, 255, 255, 255]));
            let black = Rgba([0, 0, 0, 255]);
            let mut noise = golden.clone();
            noise.put_pixel(3, 3, Rgba([250, 250, 250, 255]));
            let mut glyph = golden.clone();
            for i in 0..32 {
                glyph.put_pixel(i % 16, i / 16, black);
            }
            let mut outside = golden.clone();
            outside.put_pixel(20, 3, black);
            let regions = vec![Region {
                x: 0,
                y: 0,
                width: 16,
                height: 16,
            }];
            Self {
                golden,
                noise,
                glyph,
                outside,
                regions,
            }
        }

        /// A fixture whose shared golden is [`Self::golden`] in a workspace with metrics and the
        /// shared goldens of `fallback_platform`.
        fn fixture(&self, fallback_platform: &str) -> Fixture {
            self.fixture_with(fallback_platform, "diff: { threshold: 0 }")
        }

        /// [`Self::fixture`] with the rule's `diff:` line replaced by `rule`.
        fn fixture_with(&self, fallback_platform: &str, rule: &str) -> Fixture {
            let fixture = Fixture::new(Some(&format!(
                "{}fallback_platform: {fallback_platform}\n",
                METRICS.replace("diff: { threshold: 0 }", rule)
            )));
            fs::write(&fixture.golden, self.png()).unwrap();
            fixture
        }

        fn png(&self) -> Vec<u8> {
            gleon_model::compare::encode_png(&self.golden).unwrap()
        }
    }

    impl Fixture {
        /// This platform's own golden beside the shared one.
        fn own_golden(&self) -> PathBuf {
            self.root
                .join("test/goldens")
                .join(PlatformKey::host())
                .join("a.png")
        }
    }

    /// On the platform of the shared goldens (`fallback_platform`) text is compared almost
    /// exactly: whatever the call or rule says, at most `TextTolerance::OWN_PLATFORM`.
    #[test]
    fn test_on_the_fallback_platform_text_is_compared_almost_exactly() {
        let images = TextImages::new();
        let fixture = images.fixture(PlatformKey::host().as_str());
        let session = fixture.session(None);
        let compare = |candidate, text| {
            fixture.compare_raw(&session, candidate, None, images.regions.clone(), text)
        };

        let noise = compare(&images.noise, None);
        assert_eq!(noise.verdict, Verdict::Match, "{}", noise.message);
        let case = fixture.case();
        assert_eq!(
            case.comparison.text_tolerance,
            Some(TextTolerance::OWN_PLATFORM)
        );
        assert_eq!(
            (case.golden.path.as_str(), case.golden.fallback),
            ("test/goldens/a.png", None)
        );

        let glyph = compare(&images.glyph, None);
        assert_eq!(glyph.verdict, Verdict::Mismatch);
        assert!(
            glyph.message.contains("text ≤ 5.00% per tile)."),
            "{}",
            glyph.message
        );
        assert!(!glyph.message.contains("Compared with"));
        assert!(glyph.warning.is_empty(), "{}", glyph.warning);

        // An explicit tolerance replaces the default: 1 turns text comparison off here too,
        // 0 fails even the noise.
        for (text, verdict) in [
            (TextTolerance(0.5), Verdict::Match),
            (TextTolerance::DEFAULT, Verdict::Match),
        ] {
            let finished = compare(&images.glyph, Some(text));
            assert_eq!(finished.verdict, verdict, "{text:?}: {}", finished.message);
            assert_eq!(fixture.case().comparison.text_tolerance, Some(text));
        }
        let strict = compare(&images.noise, Some(TextTolerance(0.0)));
        assert_eq!(strict.verdict, Verdict::Mismatch);
        let rule_off = images.fixture_with(
            PlatformKey::host().as_str(),
            "diff: { threshold: 0 }\n    text_tolerance: 1",
        );
        let finished = rule_off.compare_raw(
            &rule_off.session(None),
            &images.glyph,
            None,
            images.regions.clone(),
            None,
        );
        assert_eq!(finished.verdict, Verdict::Match, "the rule's 1 too");

        let candidate = png(4, 4, true);
        let updated = fixture.run(&session, Mode::Update, &candidate);
        assert_eq!(updated.verdict, Verdict::Updated);
        assert_eq!(fs::read(&fixture.golden).unwrap(), candidate);
        assert!(!fixture.own_golden().parent().unwrap().exists());
    }

    /// Another platform compares the shared golden, with text under the text tolerance, until it
    /// has its own golden, which it compares like the fallback platform its shared one.
    #[test]
    fn test_another_platform_falls_back_to_the_shared_golden() {
        let images = TextImages::new();
        let fixture = images.fixture(foreign_platform());
        let session = fixture.session(None);
        let compare = |candidate| {
            fixture.compare_raw(&session, candidate, None, images.regions.clone(), None)
        };
        let own_path = format!("test/goldens/{}/a.png", PlatformKey::host());

        let glyph = compare(&images.glyph);
        assert_eq!(glyph.verdict, Verdict::Match, "{}", glyph.message);
        assert!(
            glyph.console.starts_with("gleon ✓ test/goldens/a.png  "),
            "{}",
            glyph.console
        );
        let case = fixture.case();
        assert_eq!(case.golden.path, own_path, "where `gleon approve` writes");
        assert_eq!(case.golden.fallback.as_deref(), Some("test/goldens/a.png"));
        assert_eq!(
            case.golden.sha256,
            Some(case::Sha256Hex::of(&images.png())),
            "the compared golden"
        );
        assert_eq!(
            case.name, "test/goldens/a",
            "the same name on every platform"
        );
        assert_eq!(case.comparison.text_tolerance, Some(TextTolerance::DEFAULT));
        // The pass keeps its candidate, so `gleon approve` can make it this platform's golden.
        let kept = case.artifacts.unwrap();
        assert_eq!((kept.golden, kept.diff), (None, None));
        let candidate = fs::read(fixture.root.join(kept.candidate.unwrap())).unwrap();
        assert_eq!(case.candidate, CandidateImage::of(&candidate));
        assert_eq!(
            image::load_from_memory(&candidate).unwrap().to_rgba8(),
            images.glyph
        );
        assert!(
            fixture.failures().is_empty(),
            "a pass writes no failure feedback"
        );

        let outside = compare(&images.outside);
        assert_eq!(outside.verdict, Verdict::Mismatch);
        assert!(
            outside.message.contains(&format!(
                "(gleon pixel ≤ 0.00%, text ignored). Compared with the {} golden: this \
                 platform has no own golden \"goldens/{}/a.png\" yet (record or approve it to \
                 compare text too).",
                foreign_platform(),
                PlatformKey::host()
            )),
            "{}",
            outside.message
        );
        assert_eq!(
            fixture.failures(),
            ["a_gleonDiff.png", "a_masterImage.png", "a_testImage.png"]
        );
        assert_eq!(
            fs::read(
                fixture
                    .root
                    .join(fixture.case().artifacts.unwrap().golden.unwrap())
            )
            .unwrap(),
            images.png(),
            "the compared golden is kept"
        );

        fs::create_dir_all(fixture.own_golden().parent().unwrap()).unwrap();
        fs::write(fixture.own_golden(), images.png()).unwrap();
        let glyph = compare(&images.glyph);
        assert_eq!(glyph.verdict, Verdict::Mismatch, "its own golden");
        assert!(
            glyph.message.starts_with(&format!(
                "Golden \"goldens/{}/a.png\": ",
                PlatformKey::host()
            )),
            "the message names the compared golden: {}",
            glyph.message
        );
        assert!(
            !glyph.message.contains("Compared with"),
            "{}",
            glyph.message
        );
        let case = fixture.case();
        assert_eq!((case.golden.path, case.golden.fallback), (own_path, None));
        assert_eq!(
            case.comparison.text_tolerance,
            Some(TextTolerance::OWN_PLATFORM)
        );
        assert_eq!(
            compare(&images.noise).verdict,
            Verdict::Match,
            "noise of this platform's own font engine passes"
        );
        assert!(
            fixture.case().artifacts.is_none(),
            "a pass against its own golden keeps nothing"
        );

        // Messages about the compared golden name this platform's own one.
        fs::write(
            fixture.own_golden(),
            b"version https://git-lfs.github.com/spec/v1",
        )
        .unwrap();
        let corrupt = compare(&images.glyph);
        assert_eq!(corrupt.error_kind, ErrorKind::Image);
        assert!(
            corrupt.message.starts_with(&format!(
                "Golden \"goldens/{}/a.png\": gleon could not compare: golden image",
                PlatformKey::host()
            )),
            "{}",
            corrupt.message
        );
    }

    /// Without metrics a pass records nothing, so a pass against another platform's golden keeps
    /// no candidate either.
    #[test]
    fn test_fallback_passes_keep_their_candidate_only_when_recorded() {
        let images = TextImages::new();
        let fixture = images.fixture(foreign_platform());
        let session = fixture.session(Some("0"));
        let finished =
            fixture.compare_raw(&session, &images.glyph, None, images.regions.clone(), None);
        assert_eq!(finished.verdict, Verdict::Match, "{}", finished.message);
        assert!(fixture.artifacts().is_empty());
        assert!(!fixture.case_path().exists());
    }

    /// Update mode and missing goldens of another platform go to its own golden; the shared one is
    /// never written there.
    #[test]
    fn test_another_platform_writes_its_own_golden() {
        let images = TextImages::new();
        let fixture = images.fixture(foreign_platform());
        let session = fixture.session(None);
        let candidate = png(4, 4, true);
        let own_path = format!("test/goldens/{}/a.png", PlatformKey::host());

        let updated = fixture.run(&session, Mode::Update, &candidate);
        assert_eq!(updated.verdict, Verdict::Updated, "{}", updated.message);
        assert_eq!(fs::read(fixture.own_golden()).unwrap(), candidate);
        assert_eq!(fs::read(&fixture.golden).unwrap(), images.png());
        let case = fixture.case();
        assert_eq!(
            (case.golden.path, case.golden.fallback),
            (own_path.clone(), None)
        );
        assert_eq!(
            fixture.run(&session, Mode::Compare, &candidate).verdict,
            Verdict::Identical
        );

        fs::remove_file(fixture.own_golden()).unwrap();
        fs::remove_file(&fixture.golden).unwrap();
        let missing = fixture.run(&session, Mode::Compare, &candidate);
        assert_eq!(missing.verdict, Verdict::Missing);
        assert_eq!(
            missing.message,
            format!(
                "Could not be compared against non-existent file: \"goldens/{}/a.png\" (nor the \
                 {foreign} golden \"goldens/a.png\": record a new golden on {foreign} first, \
                 every other platform compares it until it has its own)",
                PlatformKey::host(),
                foreign = foreign_platform()
            )
        );
        let case = fixture.case();
        assert_eq!((case.golden.path, case.golden.fallback), (own_path, None));
        assert!(case.artifacts.unwrap().candidate.is_some());
    }

    /// A pass against another platform's golden without a single differing pixel keeps no
    /// candidate (approving it copies the compared golden); the report still names the fallback.
    #[test]
    fn test_fallback_passes_without_differences_keep_nothing() {
        let images = TextImages::new();
        let fixture = images.fixture(foreign_platform());
        let session = fixture.session(None);
        let same =
            fixture.compare_raw(&session, &images.golden, None, images.regions.clone(), None);
        assert_eq!(same.verdict, Verdict::Match, "{}", same.message);
        let case = fixture.case();
        assert_eq!(case.golden.fallback.as_deref(), Some("test/goldens/a.png"));
        assert!(case.artifacts.is_none());
        assert!(fixture.artifacts().is_empty());

        let identical = fixture.run(&session, Mode::Compare, &images.png());
        assert_eq!(identical.verdict, Verdict::Identical);
        assert!(fixture.case().artifacts.is_none());
    }

    #[test]
    fn test_update_mode_takes_a_png() {
        let fixture = Fixture::new(Some(METRICS));
        let pixels = image::RgbaImage::from_pixel(4, 4, RED);
        let finished = run(
            &fixture.session(None),
            &Request {
                mode: Mode::Update,
                golden_path: &fixture.golden,
                golden_uri: "goldens/a.png",
                failures_dir: &fixture.failures,
                test_name: None,
                candidate: Candidate::Rgba {
                    width: 4,
                    height: 4,
                    pixels: pixels.as_raw(),
                },
                tolerance: None,
                masks: Vec::new(),
                text_regions: Vec::new(),
                text: None,
            },
        );
        assert_eq!(finished.error_kind, ErrorKind::InvalidInput);
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
        let artifact = |file: &str| {
            format!(
                ".gleon/runs/latest/artifacts/{}/test/goldens/a/{file}",
                PlatformKey::host()
            )
        };

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
        let host = PlatformKey::host();
        fixture.run(&fixture.session(None), Mode::Compare, &png(4, 4, true));
        assert_eq!(
            entries(
                &fixture
                    .root
                    .join(".gleon/runs/shots")
                    .join(host)
                    .join("test/goldens/a")
            )
            .len(),
            3
        );
        assert_eq!(
            fixture.case().artifacts.unwrap().diff,
            Some(format!(".gleon/runs/shots/{host}/test/goldens/a/diff.png"))
        );

        let session = fixture.session_with(SessionOptions {
            artifacts_env: Some(" .gleon/runs/ram ".to_owned()),
            ..SessionOptions::default()
        });
        fixture.run(&session, Mode::Compare, &png(4, 4, true));
        assert_eq!(
            entries(
                &fixture
                    .root
                    .join(".gleon/runs/ram")
                    .join(host)
                    .join("test/goldens/a")
            )
            .len(),
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
    fn test_platforms_keep_each_others_reports_and_images() {
        let fixture = Fixture::new(Some(METRICS));
        let session = fixture.session(None);
        // Another platform: no process reports an OS `gleon`.
        let foreign = "gleon-test";
        let runs = fixture.root.join(".gleon/runs/latest");
        let foreign_case = runs.join("cases").join(foreign).join("test/goldens/a.json");
        let foreign_candidate = runs
            .join("artifacts")
            .join(foreign)
            .join("test/goldens/a/candidate.png");
        for file in [&foreign_case, &foreign_candidate] {
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, b"another platform's").unwrap();
        }

        let mismatch = fixture.run(&session, Mode::Compare, &png(4, 4, true));
        assert_eq!(mismatch.verdict, Verdict::Mismatch);
        assert_eq!(
            fixture.artifacts(),
            ["candidate.png", "diff.png", "golden.png"]
        );
        assert_eq!(fixture.case().outcome, CaseOutcome::Mismatch);

        let pass = fixture.run(&session, Mode::Compare, &png(4, 4, false));
        assert_eq!(pass.verdict, Verdict::Identical);
        assert!(fixture.artifacts().is_empty(), "the own images are removed");
        assert_eq!(fixture.case().outcome, CaseOutcome::Identical);
        assert!(
            fixture
                .case_path()
                .starts_with(runs.join("cases").join(PlatformKey::host()))
        );
        for file in [&foreign_case, &foreign_candidate] {
            assert_eq!(fs::read(file).unwrap(), b"another platform's", "{file:?}");
        }
    }

    /// The valid report and images of the golden on another platform: this platform's own,
    /// recorded there (`<dir>/<foreign>/test/goldens/a/`).
    fn write_foreign_case(fixture: &Fixture, dir: &str) -> (PathBuf, PathBuf) {
        let foreign = foreign_platform();
        let (os, arch) = foreign.split_once('-').unwrap();
        let mut report = fixture.case_json();
        report["platform"] = serde_json::json!({"os": os, "arch": arch});
        let images = format!("{dir}/{foreign}/test/goldens/a");
        for field in ["golden", "candidate", "diff"] {
            report["artifacts"][field] = format!("{images}/{field}.png").into();
        }
        let case = fixture
            .root
            .join(".gleon/runs/latest/cases")
            .join(foreign)
            .join("test/goldens/a.json");
        fs::create_dir_all(case.parent().unwrap()).unwrap();
        fs::write(&case, report.to_string()).unwrap();
        CaseReport::parse(&fs::read(&case).unwrap()).unwrap();
        let folder = fixture.root.join(&images);
        fs::create_dir_all(&folder).unwrap();
        for file in ["golden.png", "candidate.png", "diff.png"] {
            fs::write(folder.join(file), b"another platform's").unwrap();
        }
        (case, folder)
    }

    /// A pass without metrics removes the report and images of this platform's earlier failure
    /// only: another platform's valid report of the golden stays.
    #[test]
    fn test_a_pass_without_metrics_removes_only_its_own_report() {
        let fixture = Fixture::new(Some(METRICS));
        let mismatch = fixture.run(&fixture.session(None), Mode::Compare, &png(4, 4, true));
        assert_eq!(mismatch.verdict, Verdict::Mismatch);
        let (foreign_case, foreign_images) =
            write_foreign_case(&fixture, ".gleon/runs/latest/artifacts");

        let pass = fixture.run(
            &fixture.session(Some("0")),
            Mode::Compare,
            &png(4, 4, false),
        );
        assert_eq!(pass.verdict, Verdict::Identical);
        assert!(!fixture.case_path().exists(), "its own report is removed");
        assert!(fixture.artifacts().is_empty(), "its own images are removed");
        assert!(CaseReport::parse(&fs::read(&foreign_case).unwrap()).is_ok());
        assert_eq!(entries(&foreign_images).len(), 3);
    }

    /// `GLEON_ARTIFACTS_DIR` holds the images of every platform, each in its directory.
    #[test]
    fn test_a_custom_artifacts_dir_of_two_platforms() {
        let fixture = Fixture::new(Some(METRICS));
        let session = fixture.session_with(SessionOptions {
            artifacts_env: Some(".gleon/runs/ci".to_owned()),
            ..SessionOptions::default()
        });
        let host = PlatformKey::host();
        fixture.run(&session, Mode::Compare, &png(4, 4, true));
        let own = fixture
            .root
            .join(".gleon/runs/ci")
            .join(host)
            .join("test/goldens/a");
        assert_eq!(entries(&own).len(), 3);
        assert_eq!(
            fixture.case().artifacts.unwrap().candidate,
            Some(format!(
                ".gleon/runs/ci/{host}/test/goldens/a/candidate.png"
            ))
        );
        let (foreign_case, foreign_images) = write_foreign_case(&fixture, ".gleon/runs/ci");

        let pass = fixture.run(&session, Mode::Compare, &png(4, 4, false));
        assert_eq!(pass.verdict, Verdict::Identical);
        assert!(entries(&own).is_empty());
        assert_eq!(fixture.case().outcome, CaseOutcome::Identical);
        assert!(CaseReport::parse(&fs::read(&foreign_case).unwrap()).is_ok());
        assert_eq!(entries(&foreign_images).len(), 3);
    }

    #[test]
    fn test_unwritable_artifacts_are_a_warning() {
        let fixture = Fixture::new(Some(METRICS));
        fs::create_dir_all(fixture.root.join(".gleon/runs/latest")).unwrap();
        fs::write(fixture.root.join(".gleon/runs/latest/artifacts"), b"").unwrap();
        let finished = fixture.run(&fixture.session(None), Mode::Compare, &png(4, 4, true));
        assert_eq!(finished.verdict, Verdict::Mismatch);
        assert!(
            finished.warning.starts_with(&format!(
                "gleon: cannot update the artifacts .gleon/runs/latest/artifacts/{}/test/goldens/a: ",
                PlatformKey::host()
            )),
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
        assert_eq!(case.golden.sha256, case.candidate.sha256);

        let broken = Fixture::new(Some("not: [valid"));
        let finished = broken.run(&broken.session(None), Mode::Update, &candidate);
        assert_eq!(finished.verdict, Verdict::Error);
        assert_eq!(
            fs::read(&broken.golden).unwrap(),
            png(4, 4, false),
            "the config decides which golden to write: nothing is written"
        );

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
            entries(
                &fixture
                    .root
                    .join(".gleon/runs/latest/cases")
                    .join(PlatformKey::host())
                    .join("test/goldens")
            ),
            ["a.json"]
        );
        assert_eq!(fixture.failures().len(), 3, "no temporary files are left");
    }
}
