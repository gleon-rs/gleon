//! The results of a test run, read back from its case reports.
//!
//! The case reports in `.gleon/runs/latest/cases/<platform>/<name>.json` are written by
//! integrations such as the Flutter package and by `gleon diff`; the reports, the dashboard and
//! `gleon approve` read them as one run. The directory keeps the latest report of every golden on
//! every platform, from whichever run wrote it: a report is keyed by its golden's name and its
//! platform's key, so platforms sharing a workspace (a macOS host and a Linux container on the
//! same checkout) never replace each other's reports.
//!
//! A report lies at `cases/<its platform key>/<its name>.json`; readers skip any other file (an
//! flat layout of an older gleon, a copy), so one golden of one platform is one report.
//!
//! A run is still picked as a whole: its [`RunId`] is the unit of a run ([`RUN_ID_ENV`], set by
//! CI, by `gleon test`, which also writes [`RUN_FILE`], and by `gleon diff`). Test processes on
//! several platforms that share one `GLEON_RUN_ID` (`gleon test` keeps a preset one) form one
//! joint run; without a run id each platform's latest run is picked, and reports without a run id
//! are read together; see [`Cases::load`] for how runs are picked.
//!
//! `golden.path` is the golden file of an integration, and for `gleon diff` the screenshot whose
//! baseline (`golden.blob`) lives in the manifests: either way the file the case is about.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    io,
    path::{Component, Path, PathBuf},
};

use chrono::{DateTime, Utc};
use gleon_model::{
    case::{CaseOutcome, CaseParseError, CaseReport, RUN_ID_ENV, RunId},
    fs::Durability,
    platform::{PlatformConfig, PlatformFields, PlatformInfo, PlatformKey},
};
use serde::{Deserialize, Serialize};

/// The run file in `.gleon/runs/latest/`, written by `gleon test` when it starts a run.
pub const RUN_FILE: &str = "run.json";

/// The directory of the case reports inside `.gleon/runs/latest/`.
const CASES_SUBDIR: &str = "cases";

/// Where the run output lives, relative to the workspace root; artifact paths in case reports
/// start with it.
const RUNS_PREFIX: &str = ".gleon/runs/";

/// The run output of the latest run, relative to the workspace root.
const RUNS_LATEST_PREFIX: &str = ".gleon/runs/latest/";

/// Errors reading the case reports of a run.
#[derive(Debug, thiserror::Error)]
pub enum CasesError {
    /// The case reports or the run file could not be read.
    #[error("cannot read '{path}': {source}")]
    Io {
        /// The file or directory.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The run file is invalid.
    #[error("invalid run file '{path}': {source}")]
    RunFile {
        /// The run file.
        path: PathBuf,
        /// Why it is invalid.
        #[source]
        source: serde_json::Error,
    },
    /// A directory given as a run has no case reports directory.
    #[error(
        "'{path}' is no run: it has no `cases/` directory (pass the `latest` directory of a \
         `.gleon/runs`, e.g. of a downloaded CI artifact)"
    )]
    NotARun {
        /// The directory.
        path: PathBuf,
    },
}

/// The run `gleon test` started: `.gleon/runs/latest/run.json`. Unknown fields (of a newer
/// gleon) are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunInfo {
    /// The id every case report of the run carries.
    pub run_id: RunId,
    /// The platform the run tests (where `gleon test` runs): the run file names the run of this
    /// platform only, another platform sharing the workspace keeps its own latest run.
    pub platform: PlatformKey,
    /// When the run started.
    pub started_at: DateTime<Utc>,
    /// The test command, one argument per item.
    pub command: Vec<String>,
}

impl RunInfo {
    /// Reads the run file of `runs_latest`; `None` if there is none.
    ///
    /// # Errors
    /// Returns [`CasesError::Io`] if the file cannot be read, or [`CasesError::RunFile`] if it is
    /// invalid.
    pub fn read(runs_latest: &Path) -> Result<Option<Self>, CasesError> {
        let path = runs_latest.join(RUN_FILE);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(CasesError::Io { path, source }),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|source| CasesError::RunFile { path, source })
    }

    /// Writes the run file of `runs_latest` (atomically; the next run replaces it).
    ///
    /// # Errors
    /// Returns the I/O error of writing the file.
    pub fn write(&self, runs_latest: &Path) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        gleon_model::fs::write_atomically(&runs_latest.join(RUN_FILE), &json, Durability::Atomic)
    }
}

/// A new run id, `run-<UTC time>-<8 hex digits>`: the time orders runs, the digits (random per
/// process) tell apart runs started in the same second.
///
/// # Panics
/// Never: the id has only letters, digits and `-`, far below the length limit.
#[must_use]
pub fn new_run_id(now: DateTime<Utc>) -> RunId {
    use std::hash::BuildHasher as _;

    let random = std::hash::RandomState::new().hash_one((now, std::process::id()));
    let id = format!(
        "run-{}-{:08x}",
        now.format("%Y%m%dT%H%M%SZ"),
        random & 0xFFFF_FFFF
    );
    #[expect(
        clippy::expect_used,
        reason = "letters, digits and `-` only, far below the length limit"
    )]
    RunId::new(id).expect("a generated run id is valid")
}

/// The platform of a run as its case reports record it, so `key` gives the key of its
/// manifests.
#[must_use]
pub fn platform_of(info: &PlatformInfo) -> PlatformConfig {
    PlatformConfig::Structured(PlatformFields {
        os: Some(info.os.clone()),
        arch: info.arch.clone(),
        renderer: info.renderer.clone(),
        labels: (!info.labels.is_empty()).then(|| info.labels.clone()),
    })
}

/// The case reports of one run, sorted by name, then platform, each with the key of its platform.
#[derive(Debug, Clone)]
pub struct Cases {
    runs_latest: PathBuf,
    run_id: Option<RunId>,
    /// The platform key of each report (`keys[i]` of `reports[i]`).
    keys: Vec<PlatformKey>,
    reports: Vec<CaseReport>,
    spans_platforms: bool,
    warnings: Vec<String>,
}

impl Cases {
    /// `reports` as a run of `runs_latest` (the directory standing in for
    /// `.gleon/runs/latest`, which their artifact paths are resolved against). A report whose
    /// platform has no key (never one gleon parsed or wrote) is skipped with a warning.
    #[must_use]
    pub fn new(runs_latest: impl Into<PathBuf>, reports: Vec<CaseReport>) -> Self {
        let mut keyless = 0_usize;
        let keyed = reports
            .into_iter()
            .filter_map(|report| {
                let key = report.platform_key().ok();
                keyless += usize::from(key.is_none());
                key.map(|key| (key, report))
            })
            .collect();
        let warnings = if keyless == 0 {
            Vec::new()
        } else {
            vec![format!(
                "skipped {keyless} case report(s) whose platform has no key"
            )]
        };
        Self::from_keyed(runs_latest, keyed).with_warnings(warnings)
    }

    /// `reports` with their platform keys as a run of `runs_latest`.
    fn from_keyed(runs_latest: impl Into<PathBuf>, mut reports: Vec<Keyed>) -> Self {
        reports
            .sort_by(|(a_key, a), (b_key, b)| a.name.cmp(&b.name).then_with(|| a_key.cmp(b_key)));
        let spans_platforms = reports
            .first()
            .is_some_and(|(first, _)| reports.iter().any(|(key, _)| key != first));
        let (keys, reports) = reports.into_iter().unzip();
        Self {
            runs_latest: runs_latest.into(),
            run_id: None,
            keys,
            reports,
            spans_platforms,
            warnings: Vec::new(),
        }
    }

    /// `self` as the run `run_id`.
    #[must_use]
    pub fn with_run_id(self, run_id: RunId) -> Self {
        Self {
            run_id: Some(run_id),
            ..self
        }
    }

    /// `self` with only the reports `keep` returns `true` for.
    #[must_use]
    pub fn retain(self, mut keep: impl FnMut(&CaseReport) -> bool) -> Self {
        let reports = self
            .keys
            .into_iter()
            .zip(self.reports)
            .filter(|(_, report)| keep(report))
            .collect();
        Self {
            run_id: self.run_id,
            warnings: self.warnings,
            ..Self::from_keyed(self.runs_latest, reports)
        }
    }

    /// `self` with `warnings` for the reader.
    #[must_use]
    pub fn with_warnings(self, warnings: Vec<String>) -> Self {
        Self { warnings, ..self }
    }

    /// Reads the case reports in `runs_latest/cases/` (`runs_latest` is the `.gleon/runs/latest`
    /// of a workspace, or a copy of it such as a downloaded CI artifact) and keeps one run:
    /// - `run_id` when given (the caller's [`RUN_ID_ENV`]), on every platform: one joint run;
    /// - else one run per platform (each platform's reports in `cases/<platform>/`, so platforms
    ///   that ran apart into one workspace are both read):
    ///   - on the platform of [`RUN_FILE`] (where `gleon test` ran), its run, unless a newer
    ///     report of the platform belongs to another run (the tests ran again without
    ///     `gleon test`);
    ///   - else the run of the platform's newest report;
    ///   - else, when that report has no run id, every report of the platform except those whose
    ///     golden no longer exists (inside a workspace; `missing` goldens never exist), with a
    ///     warning that they may mix runs.
    ///
    ///   When the platforms come from different runs, a warning says so: one `GLEON_RUN_ID` for
    ///   every platform reads them as one run.
    ///
    /// Reports that are invalid or of another schema version (an older or newer integration
    /// wrote them), or that lie elsewhere than `cases/<their platform key>/<their name>.json` (the
    /// flat layout of an older gleon, a copy), are skipped with a warning, so one broken file never
    /// hides the others and one golden of one platform is one report.
    ///
    /// # Errors
    /// Returns [`CasesError`] if the reports cannot be listed or the run file (read without
    /// `run_id`) is unreadable or invalid.
    pub fn load(runs_latest: &Path, run_id: Option<&RunId>) -> Result<Self, CasesError> {
        let (reports, mut warnings) = read_reports(&runs_latest.join(CASES_SUBDIR))?;
        if let Some(run_id) = run_id {
            let reports = reports
                .into_iter()
                .filter(|(_, report)| report.run_id.as_ref() == Some(run_id))
                .collect();
            return Ok(Self::from_keyed(runs_latest, reports)
                .with_warnings(warnings)
                .with_run_id(run_id.clone()));
        }
        let run_file = RunInfo::read(runs_latest)?;
        let selected = select_runs(&reports, run_file.as_ref());

        let root = workspace_root(runs_latest);
        let reports: Vec<_> = reports
            .into_iter()
            .filter(|(key, report)| match selected.get(key) {
                Some(Some(run_id)) => report.run_id.as_ref() == Some(run_id),
                Some(None) => {
                    report.outcome == CaseOutcome::Missing
                        || root.is_none_or(|root| {
                            // The compared golden: a platform's own one may not exist yet.
                            root.join(report.golden.compared())
                                .try_exists()
                                .unwrap_or(true)
                        })
                }
                None => false,
            })
            .collect();
        let without_run = reports
            .iter()
            .filter(|(key, report)| report.run_id.is_none() && selected.get(key) == Some(&None))
            .count();
        if without_run > 0 {
            warnings.push(format!(
                "{without_run} case report(s) without a run id may mix runs, and without \
                 metrics only failures are recorded: run the tests with `gleon test -- \
                 <command>` or set {RUN_ID_ENV}; `gleon clean` removes old reports"
            ));
        }

        // The runs of the platforms that were read.
        let read: BTreeSet<&PlatformKey> = reports.iter().map(|(key, _)| key).collect();
        let runs: Vec<(&PlatformKey, Option<&RunId>)> = selected
            .iter()
            .filter(|(key, _)| read.contains(key))
            .map(|(key, run_id)| (key, run_id.as_ref()))
            .collect();
        let is_one_run = runs.windows(2).all(|pair| pair[0].1 == pair[1].1);
        let run_id = if reports.is_empty() {
            // Nothing recorded yet: the run every platform waits for, if they agree.
            run_file.map(|run| run.run_id).filter(|run_id| {
                selected
                    .values()
                    .all(|selected| selected.as_ref() == Some(run_id))
            })
        } else if is_one_run {
            runs.first().and_then(|(_, run_id)| run_id.cloned())
        } else {
            None
        };
        if !is_one_run {
            let mut list = String::new();
            for (key, run_id) in &runs {
                let separator = if list.is_empty() { "" } else { ", " };
                let run_id = run_id.map_or("no run id", RunId::as_str);
                let _infallible = write!(list, "{separator}{key}: {run_id}");
            }
            warnings.push(format!(
                "platforms come from different runs ({list}): set one {RUN_ID_ENV} for every \
                 platform to read them as one run"
            ));
        }
        let cases = Self::from_keyed(runs_latest, reports).with_warnings(warnings);
        Ok(match run_id {
            Some(run_id) => cases.with_run_id(run_id),
            None => cases,
        })
    }

    /// The run these reports belong to; `None` when they carry no single run id.
    #[must_use]
    pub const fn run_id(&self) -> Option<&RunId> {
        self.run_id.as_ref()
    }

    /// The reports, sorted by name, then platform.
    #[must_use]
    pub fn reports(&self) -> &[CaseReport] {
        &self.reports
    }

    /// The reports with the keys of their platforms, sorted by name, then platform.
    pub fn keyed(&self) -> impl ExactSizeIterator<Item = (&PlatformKey, &CaseReport)> {
        self.keys.iter().zip(&self.reports)
    }

    /// Whether the reports come from more than one platform (a joint run), so readers tell the
    /// reports of one golden apart by their platform.
    #[must_use]
    pub const fn spans_platforms(&self) -> bool {
        self.spans_platforms
    }

    /// What the reader should know about the selection (skipped or possibly mixed reports).
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// The reports whose outcome fails the test ([`CaseOutcome::is_failure`]) with the keys of
    /// their platforms, sorted by name, then platform.
    pub fn failures(&self) -> impl Iterator<Item = (&PlatformKey, &CaseReport)> {
        self.keyed()
            .filter(|(_, report)| report.outcome.is_failure())
    }

    /// The failures, most telling first: mismatches, dimension mismatches, errors, then missing
    /// goldens (each by name, then platform). Short reports (a PR comment) show these first.
    #[must_use]
    pub fn failures_by_severity(&self) -> Vec<(&PlatformKey, &CaseReport)> {
        let rank = |outcome| match outcome {
            CaseOutcome::Mismatch => 0,
            CaseOutcome::DimensionMismatch => 1,
            CaseOutcome::Error => 2,
            _ => 3,
        };
        let mut failures: Vec<_> = self.failures().collect();
        failures.sort_by_key(|(_, report)| rank(report.outcome));
        failures
    }

    /// When the newest report was recorded.
    #[must_use]
    pub fn recorded_at(&self) -> Option<DateTime<Utc>> {
        self.reports.iter().map(|report| report.recorded_at).max()
    }

    /// The file of an artifact path of a report (relative to the workspace root, under
    /// `.gleon/runs/`) inside the directory standing in for `.gleon/runs/latest`; `None` for any
    /// other path.
    #[must_use]
    pub fn artifact_path(&self, path: &str) -> Option<PathBuf> {
        artifact_path(&self.runs_latest, path)
    }
}

/// The run each platform of `reports` reads ([`Cases::load`] without a run id): for the platform
/// of `run_file`, its run unless the platform's newest report left it (belongs to another run and
/// is newer than its start); else, and for every other platform, the run of the platform's newest
/// report; `None` when it has no run id.
fn select_runs(
    reports: &[Keyed],
    run_file: Option<&RunInfo>,
) -> BTreeMap<PlatformKey, Option<RunId>> {
    let mut newest = BTreeMap::<&PlatformKey, &CaseReport>::new();
    for (key, report) in reports {
        let kept = newest.entry(key).or_insert(report);
        if kept.recorded_at < report.recorded_at {
            *kept = report;
        }
    }
    newest
        .into_iter()
        .map(|(key, newest)| {
            // The run file names the run of its own platform only: there, a run that has not
            // written a report yet still hides the older ones. Another platform sharing the
            // workspace keeps its latest run.
            let from_run_file = run_file
                .filter(|run| {
                    run.platform == *key
                        && (newest.run_id.as_ref() == Some(&run.run_id)
                            || newest.recorded_at <= run.started_at)
                })
                .map(|run| &run.run_id);
            (
                key.clone(),
                from_run_file.or(newest.run_id.as_ref()).cloned(),
            )
        })
        .collect()
}

/// Checks that `dir`, given as a copy of `.gleon/runs/latest`, is one: it has `cases/`. A typo, or
/// the root of a downloaded artifact (which holds `latest/`), is no run read as an empty one.
///
/// # Errors
/// Returns [`CasesError::NotARun`] otherwise.
pub fn check_run_dir(dir: &Path) -> Result<(), CasesError> {
    if dir.join(CASES_SUBDIR).is_dir() {
        Ok(())
    } else {
        Err(CasesError::NotARun {
            path: dir.to_path_buf(),
        })
    }
}

/// The workspace root of `runs_latest` when it is the `.gleon/runs/latest` of a workspace.
fn workspace_root(runs_latest: &Path) -> Option<&Path> {
    let runs = runs_latest.parent()?;
    let gleon = runs.parent()?;
    let is_workspace = runs_latest.file_name()? == "latest"
        && runs.file_name()? == "runs"
        && gleon.file_name()? == ".gleon";
    is_workspace.then(|| gleon.parent()).flatten()
}

/// See [`Cases::artifact_path`].
fn artifact_path(runs_latest: &Path, path: &str) -> Option<PathBuf> {
    let (base, rest) = if let Some(rest) = path.strip_prefix(RUNS_LATEST_PREFIX) {
        (runs_latest.to_path_buf(), rest)
    } else {
        // The directory holding `runs_latest`: `..` of the working directory (`""`) or the root.
        let runs = runs_latest
            .parent()
            .map_or_else(|| runs_latest.join(".."), Path::to_path_buf);
        (runs, path.strip_prefix(RUNS_PREFIX)?)
    };
    Some(rest.split('/').fold(base, |dir, name| dir.join(name)))
}

/// A report file and what parsing it gave.
type Parsed = (PathBuf, Result<CaseReport, CaseParseError>);

/// A report with the key of its platform.
type Keyed = (PlatformKey, CaseReport);

/// Every readable report under `cases_dir` with its file, in walk order. A file removed while
/// listing (a concurrent `gleon diff` cleaning up) is no report.
fn parse_reports(cases_dir: &Path) -> Result<Vec<Parsed>, CasesError> {
    let files = list_reports(cases_dir)?;
    let parsed = crate::io::map_files(&files, |path| match std::fs::read(path) {
        Ok(bytes) => Ok(Some(CaseReport::parse(&bytes))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(CasesError::Io {
            path: path.clone(),
            source,
        }),
    });
    let mut reports = Vec::with_capacity(files.len());
    for (path, parsed) in files.into_iter().zip(parsed) {
        if let Some(report) = parsed? {
            reports.push((path, report));
        }
    }
    Ok(reports)
}

/// Every valid report in `cases_dir` with its platform key, plus warnings for the skipped ones:
/// invalid reports, reports of another schema version, and reports that lie elsewhere than
/// `<platform key>/<name>.json` (so one golden of one platform is one report).
fn read_reports(cases_dir: &Path) -> Result<(Vec<Keyed>, Vec<String>), CasesError> {
    let mut reports = Vec::new();
    let mut other_versions = BTreeMap::<u32, usize>::new();
    let (mut invalid, mut first_invalid) = (0, None);
    let (mut misplaced, mut first_misplaced) = (0_usize, None::<PathBuf>);
    for (path, parsed) in parse_reports(cases_dir)? {
        let relative = path.strip_prefix(cases_dir).unwrap_or(&path);
        match parsed {
            Ok(report) => match report.platform_key() {
                Ok(key) if lies_at(relative, &key, &report.name) => reports.push((key, report)),
                _ => {
                    misplaced += 1;
                    // The first by path, so the warning is the same on every run.
                    if first_misplaced
                        .as_deref()
                        .is_none_or(|first| relative < first)
                    {
                        first_misplaced = Some(relative.to_path_buf());
                    }
                }
            },
            Err(CaseParseError::UnsupportedVersion(version)) => {
                *other_versions.entry(version).or_default() += 1;
            }
            Err(error) => {
                invalid += 1;
                first_invalid.get_or_insert_with(|| format!("{}: {error}", relative.display()));
            }
        }
    }
    let mut warnings: Vec<_> = other_versions
        .into_iter()
        .map(|(version, count)| {
            format!(
                "skipped {count} case report(s) of schema version {version}: update the \
                 integration that wrote them"
            )
        })
        .collect();
    if let Some(first) = first_invalid {
        warnings.push(format!(
            "skipped {invalid} invalid case report(s), e.g. {first}"
        ));
    }
    if let Some(first) = first_misplaced {
        warnings.push(format!(
            "{misplaced} case report(s) outside their platform's directory \
             (`cases/<platform>/<name>.json`) were skipped, e.g. '{}': reports of an older gleon \
             or copied by hand; `gleon clean` removes them",
            first.display()
        ));
    }
    Ok((reports, warnings))
}

/// Whether `relative` (a report file relative to the cases directory) is
/// `<key>/<name>.json`, compared component by component.
fn lies_at(relative: &Path, key: &PlatformKey, name: &str) -> bool {
    let mut actual = relative.components().map(|component| match component {
        Component::Normal(part) => part.to_str(),
        _ => None,
    });
    let mut expected = std::iter::once(key.as_str())
        .chain(name.split('/'))
        .peekable();
    while let Some(segment) = expected.next() {
        let Some(Some(part)) = actual.next() else {
            return false;
        };
        let part = if expected.peek().is_none() {
            part.strip_suffix(".json")
        } else {
            Some(part)
        };
        if part != Some(segment) {
            return false;
        }
    }
    actual.next().is_none()
}

/// The `.json` files under `cases_dir`, in walk order; none if it does not exist.
fn list_reports(cases_dir: &Path) -> Result<Vec<PathBuf>, CasesError> {
    let mut files = Vec::new();
    for entry in ignore::WalkBuilder::new(cases_dir)
        .standard_filters(false)
        .build()
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                let is_missing_root = e.depth() == Some(0)
                    && e.io_error()
                        .is_some_and(|e| e.kind() == io::ErrorKind::NotFound);
                if is_missing_root {
                    return Ok(Vec::new());
                }
                let source = e
                    .into_io_error()
                    .unwrap_or_else(|| io::Error::other("cannot walk the case reports"));
                return Err(CasesError::Io {
                    path: cases_dir.to_path_buf(),
                    source,
                });
            }
        };
        let is_report = entry.file_type().is_some_and(|ft| ft.is_file())
            && entry.path().extension().is_some_and(|ext| ext == "json");
        if is_report {
            files.push(entry.into_path());
        }
    }
    Ok(files)
}

/// Removes the case reports `tool` wrote for the platform `platform` (in
/// `runs_latest/cases/<platform>/`) and the images they list.
///
/// A tool that compares every golden in one process (`gleon diff`) so starts each run clean; the
/// reports of other tools and of other platforms stay (also one of another platform copied into
/// this directory). Reports that cannot be parsed are left alone.
///
/// # Errors
/// Returns [`CasesError::Io`] if the reports cannot be listed or read, or a file cannot be
/// removed (the first such file; every other one is still tried).
pub fn remove_reports_of(
    runs_latest: &Path,
    tool: &str,
    platform: &PlatformKey,
) -> Result<(), CasesError> {
    let mut files = Vec::new();
    for (file, report) in parse_reports(&runs_latest.join(CASES_SUBDIR).join(platform))? {
        // Readers check that a report lies in its platform's directory; its key is checked here
        // too, so a report copied into this directory never removes another platform's images.
        let Some(report) = report.ok().filter(|report| {
            report.source.tool == tool && report.platform_key().is_ok_and(|own| own == *platform)
        }) else {
            continue;
        };
        files.push(file);
        let images = report.artifacts.iter().flat_map(|artifacts| {
            [&artifacts.golden, &artifacts.candidate, &artifacts.diff]
                .into_iter()
                .flatten()
        });
        files.extend(images.filter_map(|path| artifact_path(runs_latest, path)));
    }
    // Removed on several threads, like they are read: one at a time takes seconds for a large run.
    crate::io::map_files(&files, |path| match std::fs::remove_file(path) {
        Err(source) if source.kind() != io::ErrorKind::NotFound => Err(CasesError::Io {
            path: path.clone(),
            source,
        }),
        _ => Ok(()),
    })
    .into_iter()
    .collect()
}

/// Case reports for tests of the readers.
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test fixtures: invalid literals are bugs of the test"
)]
pub(crate) mod fixtures {
    use std::time::Duration;

    use gleon_model::{
        case::{
            Artifacts, CASE_SCHEMA_VERSION, CandidateImage, CaseErrorKind, CaseOutcome, CaseReport,
            CaseTimings, Comparison, GoldenImage, Metrics, Sha256Hex, Source,
        },
        platform::PlatformConfig,
        tolerance::Tolerance,
    };

    /// A consistent report of `outcome` for the golden `<name>.png` written by the Flutter
    /// integration on this platform: pixel metrics for `match` and `mismatch`, an `image` error,
    /// and the images a failure keeps under `.gleon/runs/latest/artifacts/<platform>/<name>/`.
    pub fn report(name: &str, outcome: CaseOutcome) -> CaseReport {
        report_on(name, outcome, PlatformConfig::host())
    }

    /// [`report`] of `platform`, its images under that platform's directory.
    pub fn report_on(name: &str, outcome: CaseOutcome, platform: PlatformConfig) -> CaseReport {
        use CaseOutcome as O;

        let candidate = Sha256Hex::new("0".repeat(64)).unwrap();
        let golden_sha = match outcome {
            O::Missing => None,
            O::Identical => Some(candidate.clone()),
            _ => Some(Sha256Hex::new("1".repeat(64)).unwrap()),
        };
        let metrics = match outcome {
            O::Match => Some(Metrics::Pixel {
                total_pixels: 100,
                diff_pixels: 0,
                diff_ratio: 0.0,
                headroom: 0.01,
                text: None,
            }),
            O::Mismatch => Some(Metrics::Pixel {
                total_pixels: 100,
                diff_pixels: 5,
                diff_ratio: 0.05,
                headroom: -0.05,
                text: None,
            }),
            _ => None,
        };
        let key = platform.key().unwrap();
        let artifact =
            |file: &str| Some(format!(".gleon/runs/latest/artifacts/{key}/{name}/{file}"));
        let artifacts = match outcome {
            O::Mismatch => Some(Artifacts {
                golden: artifact("golden.png"),
                candidate: artifact("candidate.png"),
                diff: artifact("diff.png"),
            }),
            O::DimensionMismatch => Some(Artifacts {
                golden: artifact("golden.png"),
                candidate: artifact("candidate.png"),
                diff: None,
            }),
            O::Missing => Some(Artifacts {
                candidate: artifact("candidate.png"),
                ..Artifacts::default()
            }),
            _ => None,
        };
        let message = match outcome {
            O::Error => Some("candidate image: corrupt".to_owned()),
            O::Missing => Some("no golden yet".to_owned()),
            O::DimensionMismatch => Some("golden is 10x10px, test image is 20x10px".to_owned()),
            _ => None,
        };
        CaseReport {
            schema_version: CASE_SCHEMA_VERSION,
            name: name.to_owned(),
            golden: GoldenImage {
                path: format!("{name}.png"),
                sha256: golden_sha,
                blob: None,
                width: Some(10),
                height: Some(10),
                fallback: None,
            },
            candidate: CandidateImage {
                sha256: Some(candidate),
                width: Some(if outcome == O::DimensionMismatch {
                    20
                } else {
                    10
                }),
                height: Some(10),
            },
            source: Source {
                tool: "gleon_flutter".to_owned(),
                tool_version: "0.1.0".to_owned(),
                renderer: None,
            },
            platform,
            test: None,
            comparison: Comparison {
                tolerance: Tolerance::Exact {},
                masks: Vec::new(),
                policy_version: 2,
                text_tolerance: None,
            },
            outcome,
            error_kind: (outcome == O::Error).then_some(CaseErrorKind::Image),
            message,
            metrics,
            regions: Vec::new(),
            artifacts,
            timings_ms: CaseTimings::new(Duration::from_millis(1), None),
            run_id: None,
            recorded_at: chrono::Utc::now(),
        }
    }

    /// Every outcome once, named after it.
    pub fn every_outcome() -> Vec<CaseReport> {
        [
            CaseOutcome::Identical,
            CaseOutcome::Match,
            CaseOutcome::Mismatch,
            CaseOutcome::DimensionMismatch,
            CaseOutcome::Error,
            CaseOutcome::Updated,
            CaseOutcome::Missing,
        ]
        .into_iter()
        .map(|outcome| report(&format!("test/{}", outcome.as_str()), outcome))
        .collect()
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
    use chrono::TimeDelta;

    use super::{fixtures::report, *};

    /// A workspace with `.gleon/runs/latest` and its root.
    fn workspace() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let runs_latest = temp.path().join(".gleon/runs/latest");
        std::fs::create_dir_all(&runs_latest).unwrap();
        (temp, runs_latest)
    }

    /// Writes `report` into `runs_latest/cases/`, recorded `age_secs` seconds ago in run `run`.
    fn write(runs_latest: &Path, mut report: CaseReport, run: Option<&str>, age_secs: i64) {
        report.run_id = run.map(|run| RunId::new(run).unwrap());
        report.recorded_at = Utc::now() - TimeDelta::seconds(age_secs);
        report
            .write(runs_latest.parent().unwrap().parent().unwrap())
            .unwrap();
    }

    fn names(cases: &Cases) -> Vec<&str> {
        cases.reports().iter().map(|r| r.name.as_str()).collect()
    }

    /// A platform other than the host's.
    fn foreign() -> PlatformConfig {
        let key = if PlatformKey::host() == "freebsd-riscv64" {
            "netbsd-riscv64"
        } else {
            "freebsd-riscv64"
        };
        PlatformConfig::Opaque(key.to_owned())
    }

    #[test]
    fn test_load_without_reports_is_empty() {
        let (_temp, runs_latest) = workspace();
        let cases = Cases::load(&runs_latest, None).unwrap();
        assert!(cases.reports().is_empty());
        assert!(cases.warnings().is_empty());
        assert_eq!(cases.run_id(), None);
        assert_eq!(cases.recorded_at(), None);
    }

    #[test]
    fn test_load_takes_the_newest_run_whole() {
        let (_temp, runs_latest) = workspace();
        write(
            &runs_latest,
            report("a", CaseOutcome::Match),
            Some("old"),
            60,
        );
        write(
            &runs_latest,
            report("b", CaseOutcome::Mismatch),
            Some("new"),
            10,
        );
        write(
            &runs_latest,
            report("c", CaseOutcome::Missing),
            Some("new"),
            5,
        );

        let cases = Cases::load(&runs_latest, None).unwrap();
        assert_eq!(
            names(&cases),
            ["b", "c"],
            "missing goldens belong to the run"
        );
        assert_eq!(cases.run_id().unwrap().as_str(), "new");
        assert!(cases.warnings().is_empty());
        assert_eq!(cases.failures().count(), 2);

        let old = RunId::new("old").unwrap();
        let cases = Cases::load(&runs_latest, Some(&old)).unwrap();
        assert_eq!(names(&cases), ["a"], "the caller's run id wins");
    }

    #[test]
    fn test_load_prefers_the_run_file_unless_a_newer_report_left_it() {
        let (_temp, runs_latest) = workspace();
        write(
            &runs_latest,
            report("a", CaseOutcome::Match),
            Some("gleon-test"),
            60,
        );
        write(
            &runs_latest,
            report("b", CaseOutcome::Match),
            Some("other"),
            120,
        );
        let run = RunInfo {
            run_id: RunId::new("gleon-test").unwrap(),
            platform: PlatformKey::host().clone(),
            started_at: Utc::now() - TimeDelta::seconds(90),
            command: vec!["flutter".to_owned(), "test".to_owned()],
        };
        run.write(&runs_latest).unwrap();
        assert_eq!(RunInfo::read(&runs_latest).unwrap().as_ref(), Some(&run));
        assert_eq!(names(&Cases::load(&runs_latest, None).unwrap()), ["a"]);

        // A run that recorded nothing yet is still the run.
        let started_now = RunInfo {
            run_id: RunId::new("started").unwrap(),
            platform: PlatformKey::host().clone(),
            started_at: Utc::now() - TimeDelta::seconds(1),
            ..run.clone()
        };
        started_now.write(&runs_latest).unwrap();
        assert!(
            Cases::load(&runs_latest, None)
                .unwrap()
                .reports()
                .is_empty()
        );

        // The tests ran again without `gleon test`: the newer run wins.
        write(
            &runs_latest,
            report("c", CaseOutcome::Match),
            Some("bare"),
            0,
        );
        assert_eq!(names(&Cases::load(&runs_latest, None).unwrap()), ["c"]);

        std::fs::write(runs_latest.join(RUN_FILE), "{").unwrap();
        assert!(matches!(
            Cases::load(&runs_latest, None),
            Err(CasesError::RunFile { .. })
        ));
        // The caller's run id needs no run file.
        let bare = RunId::new("bare").unwrap();
        assert_eq!(
            names(&Cases::load(&runs_latest, Some(&bare)).unwrap()),
            ["c"]
        );

        // A newer gleon may add fields.
        let mut newer = serde_json::to_value(&run).unwrap();
        newer["host"] = "ci".into();
        std::fs::write(runs_latest.join(RUN_FILE), newer.to_string()).unwrap();
        assert_eq!(RunInfo::read(&runs_latest).unwrap(), Some(run));
    }

    #[test]
    fn test_new_run_ids_are_valid_and_distinct() {
        let now = Utc::now();
        let first = new_run_id(now);
        let second = new_run_id(now);
        assert!(first.as_str().starts_with("run-"), "{first:?}");
        assert_eq!(first.as_str().len(), "run-20261001T120000Z-0123abcd".len());
        assert_ne!(first, second);
    }

    #[test]
    fn test_check_run_dir_wants_the_cases_directory() {
        let (temp, runs_latest) = workspace();
        assert!(matches!(
            check_run_dir(&runs_latest),
            Err(CasesError::NotARun { .. })
        ));
        std::fs::create_dir_all(runs_latest.join("cases")).unwrap();
        check_run_dir(&runs_latest).unwrap();
        let err = check_run_dir(&temp.path().join(".gleon/runs")).unwrap_err();
        assert!(err.to_string().contains("`latest`"), "{err}");
    }

    #[test]
    fn test_load_without_run_ids_keeps_live_goldens_and_warns() {
        let (temp, runs_latest) = workspace();
        std::fs::write(temp.path().join("live.png"), b"png").unwrap();
        write(&runs_latest, report("live", CaseOutcome::Match), None, 10);
        write(&runs_latest, report("gone", CaseOutcome::Match), None, 10);
        write(&runs_latest, report("new", CaseOutcome::Missing), None, 10);
        // Compared with the shared golden: this platform's own one does not exist yet.
        let mut fallback = report("fallback", CaseOutcome::Match);
        fallback.golden.path = "linux-x86_64/live.png".to_owned();
        fallback.golden.fallback = Some("live.png".to_owned());
        write(&runs_latest, fallback, None, 10);
        write(
            &runs_latest,
            report("ci", CaseOutcome::Match),
            Some("ci-run"),
            60,
        );
        std::fs::write(temp.path().join("ci.png"), b"png").unwrap();

        let cases = Cases::load(&runs_latest, None).unwrap();
        assert_eq!(names(&cases), ["ci", "fallback", "live", "new"]);
        assert_eq!(cases.run_id(), None);
        assert_eq!(cases.warnings().len(), 1);
        assert!(cases.warnings()[0].starts_with("3 case report(s) without a run id"));

        // Outside a workspace (a downloaded artifact) nothing can be checked on disk.
        let copy = temp.path().join("download");
        std::fs::create_dir_all(&copy).unwrap();
        for entry in ["gone", "live"] {
            let mut gone = report(entry, CaseOutcome::Match);
            gone.recorded_at = Utc::now();
            let file = copy
                .join("cases")
                .join(PlatformKey::host())
                .join(format!("{entry}.json"));
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, serde_json::to_vec(&gone).unwrap()).unwrap();
        }
        assert_eq!(names(&Cases::load(&copy, None).unwrap()), ["gone", "live"]);
    }

    #[test]
    fn test_load_skips_other_schema_versions_and_broken_reports() {
        let (_temp, runs_latest) = workspace();
        write(&runs_latest, report("a", CaseOutcome::Match), Some("r"), 0);
        let cases_dir = runs_latest.join("cases");
        let mut v1 = serde_json::to_value(report("old", CaseOutcome::Match)).unwrap();
        v1["schema_version"] = 1.into();
        std::fs::write(cases_dir.join("old.json"), v1.to_string()).unwrap();
        std::fs::write(cases_dir.join("notes.txt"), "not a report").unwrap();
        std::fs::write(cases_dir.join("broken.json"), "{}").unwrap();

        // One broken file never hides the others: it is reported, without local paths.
        let cases = Cases::load(&runs_latest, None).unwrap();
        assert_eq!(names(&cases), ["a"]);
        assert_eq!(cases.warnings().len(), 2, "{:?}", cases.warnings());
        assert_eq!(
            cases.warnings()[0],
            "skipped 1 case report(s) of schema version 1: update the integration that wrote them"
        );
        assert!(
            cases.warnings()[1].starts_with("skipped 1 invalid case report(s), e.g. broken.json: "),
            "{:?}",
            cases.warnings()
        );
        assert!(!cases.warnings()[1].contains(&*runs_latest.to_string_lossy()));
    }

    /// A report lives at `cases/<its platform key>/<its name>.json`: a copy elsewhere (by hand,
    /// or the flat `cases/<name>.json` of an older gleon) is skipped with one warning, so one
    /// golden of one platform is one report.
    #[test]
    fn test_load_skips_reports_outside_their_platform_directory() {
        let (_temp, runs_latest) = workspace();
        write(&runs_latest, report("a", CaseOutcome::Match), Some("r"), 0);
        let mut copy = report("a", CaseOutcome::Mismatch);
        copy.run_id = Some(RunId::new("r").unwrap());
        let json = serde_json::to_vec(&copy).unwrap();
        let cases_dir = runs_latest.join("cases");
        let host = PlatformKey::host();
        for file in [
            // The flat layout of an older gleon.
            cases_dir.join("a.json"),
            // Copied by hand under another name, or into another platform's directory.
            cases_dir.join(host).join("copy.json"),
            cases_dir.join(foreign().key().unwrap()).join("a.json"),
        ] {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, &json).unwrap();
        }

        let cases = Cases::load(&runs_latest, None).unwrap();
        assert_eq!(names(&cases), ["a"]);
        assert_eq!(cases.reports()[0].outcome, CaseOutcome::Match);
        assert!(!cases.spans_platforms());
        assert_eq!(
            cases.warnings(),
            ["3 case report(s) outside their platform's directory \
                 (`cases/<platform>/<name>.json`) were skipped, e.g. 'a.json': reports of an \
                 older gleon or copied by hand; `gleon clean` removes them"]
        );
    }

    /// Without a run id each platform's run is picked on its own: platforms that ran apart (two
    /// CI jobs into one workspace) are both read, with a warning that they are two runs.
    #[test]
    fn test_load_picks_the_run_of_each_platform() {
        let (_temp, runs_latest) = workspace();
        let on = |platform: PlatformConfig, name: &str| {
            fixtures::report_on(name, CaseOutcome::Match, platform)
        };
        write(
            &runs_latest,
            report("a", CaseOutcome::Match),
            Some("run-a"),
            10,
        );
        write(
            &runs_latest,
            report("old", CaseOutcome::Match),
            Some("run-0"),
            60,
        );
        write(&runs_latest, on(foreign(), "a"), Some("run-b"), 0);
        write(&runs_latest, on(foreign(), "older"), Some("run-c"), 30);

        let cases = Cases::load(&runs_latest, None).unwrap();
        let keyed: Vec<_> = cases
            .keyed()
            .map(|(key, report)| format!("{key}/{}", report.name))
            .collect();
        let (host, foreign_key) = (PlatformKey::host(), foreign().key().unwrap());
        let mut expected = vec![format!("{host}/a"), format!("{foreign_key}/a")];
        expected.sort();
        assert_eq!(keyed, expected, "each platform's newest run");
        assert!(cases.spans_platforms());
        assert_eq!(cases.run_id(), None, "no single run");
        let mut runs = [(host.as_str(), "run-a"), (foreign_key.as_str(), "run-b")];
        runs.sort_unstable();
        assert_eq!(
            cases.warnings(),
            [format!(
                "platforms come from different runs ({}: {}, {}: {}): set one {RUN_ID_ENV} for \
                 every platform to read them as one run",
                runs[0].0, runs[0].1, runs[1].0, runs[1].1
            )]
        );

        // The caller's run id reads that run only, on every platform.
        let run_b = RunId::new("run-b").unwrap();
        let cases = Cases::load(&runs_latest, Some(&run_b)).unwrap();
        assert_eq!(names(&cases), ["a"]);
        assert_eq!(cases.keyed().next().unwrap().0, &foreign_key);
        assert_eq!(cases.run_id(), Some(&run_b));
        assert!(cases.warnings().is_empty());

        // One run on both platforms is one run.
        write(&runs_latest, on(foreign(), "b"), Some("run-a"), 0);
        write(
            &runs_latest,
            report("b", CaseOutcome::Match),
            Some("run-a"),
            0,
        );
        let cases = Cases::load(&runs_latest, None).unwrap();
        assert_eq!(names(&cases), ["a", "b", "b"]);
        assert_eq!(cases.run_id().map(RunId::as_str), Some("run-a"));
        assert!(cases.warnings().is_empty(), "{:?}", cases.warnings());
    }

    /// `run.json` names the run of the platform `gleon test` ran on; another platform that ran
    /// since (its own newer run) keeps that run.
    #[test]
    fn test_load_applies_the_run_file_per_platform() {
        let (_temp, runs_latest) = workspace();
        write(
            &runs_latest,
            report("a", CaseOutcome::Match),
            Some("gleon-test"),
            60,
        );
        write(
            &runs_latest,
            report("stale", CaseOutcome::Match),
            Some("before"),
            120,
        );
        let foreign_case = fixtures::report_on("a", CaseOutcome::Mismatch, foreign());
        write(&runs_latest, foreign_case.clone(), Some("before"), 120);
        RunInfo {
            run_id: RunId::new("gleon-test").unwrap(),
            platform: PlatformKey::host().clone(),
            started_at: Utc::now() - TimeDelta::seconds(90),
            command: vec!["flutter".to_owned(), "test".to_owned()],
        }
        .write(&runs_latest)
        .unwrap();
        // The run file names this platform's run: its older reports are not read. The other
        // platform (a container on the same checkout) keeps its own latest run, with a warning,
        // however long before the run file it ran.
        let cases = Cases::load(&runs_latest, None).unwrap();
        assert_eq!(names(&cases), ["a", "a"]);
        assert_eq!(cases.run_id(), None);
        assert_eq!(cases.warnings().len(), 1, "{:?}", cases.warnings());
        for run in [": before", ": gleon-test"] {
            assert!(cases.warnings()[0].contains(run), "{:?}", cases.warnings());
        }

        // It ran since in a run of its own: that run is read.
        write(&runs_latest, foreign_case, Some("container"), 0);
        let cases = Cases::load(&runs_latest, None).unwrap();
        assert_eq!(names(&cases), ["a", "a"]);
        assert!(
            cases.warnings()[0].contains(": container"),
            "{:?}",
            cases.warnings()
        );

        // Without reports of another platform, the run file's run is the run.
        std::fs::remove_dir_all(
            runs_latest
                .join("cases")
                .join(foreign().key().unwrap().as_str()),
        )
        .unwrap();
        let cases = Cases::load(&runs_latest, None).unwrap();
        assert_eq!(names(&cases), ["a"]);
        assert_eq!(cases.run_id().map(RunId::as_str), Some("gleon-test"));
        assert!(cases.warnings().is_empty(), "{:?}", cases.warnings());
    }

    #[test]
    fn test_load_keeps_a_report_per_platform_of_a_name() {
        let (_temp, runs_latest) = workspace();
        let host = PlatformKey::host();
        let foreign = foreign();
        let foreign_key = foreign.key().unwrap();
        write(&runs_latest, report("a", CaseOutcome::Match), Some("r"), 10);
        write(
            &runs_latest,
            fixtures::report_on("a", CaseOutcome::Mismatch, foreign.clone()),
            Some("r"),
            0,
        );
        write(&runs_latest, report("b", CaseOutcome::Match), Some("r"), 5);
        assert!(
            runs_latest
                .join("cases")
                .join(host)
                .join("a.json")
                .is_file()
        );
        assert!(
            runs_latest
                .join("cases")
                .join(&foreign_key)
                .join("a.json")
                .is_file()
        );

        let cases = Cases::load(&runs_latest, None).unwrap();
        let keys: Vec<_> = cases
            .reports()
            .iter()
            .map(|r| (r.name.as_str(), r.platform_key().unwrap()))
            .collect();
        let mut expected = vec![
            ("a", host.clone()),
            ("a", foreign_key.clone()),
            ("b", host.clone()),
        ];
        expected.sort();
        assert_eq!(keys, expected, "sorted by name, then platform");
        assert!(cases.spans_platforms());
        assert_eq!(cases.failures().count(), 1);

        let one_platform = Cases::new(
            "runs/latest",
            vec![
                report("a", CaseOutcome::Match),
                report("b", CaseOutcome::Match),
            ],
        );
        assert!(!one_platform.spans_platforms());
        assert!(!Cases::new("runs/latest", Vec::new()).spans_platforms());
    }

    #[test]
    fn test_artifact_paths_resolve_inside_the_run_directory() {
        let cases = Cases::new("/w/.gleon/runs/latest", Vec::new());
        assert_eq!(
            cases.artifact_path(".gleon/runs/latest/artifacts/a/b/diff.png"),
            Some(PathBuf::from(
                "/w/.gleon/runs/latest/artifacts/a/b/diff.png"
            ))
        );
        assert_eq!(
            cases.artifact_path(".gleon/runs/ci/a/candidate.png"),
            Some(PathBuf::from("/w/.gleon/runs/ci/a/candidate.png"))
        );
        assert_eq!(cases.artifact_path("test/goldens/a.png"), None);
        // A run named relative to the working directory, down to the directory itself.
        assert_eq!(
            artifact_path(Path::new("latest"), ".gleon/runs/ci/a.png"),
            Some(PathBuf::from("ci/a.png"))
        );
        assert_eq!(
            artifact_path(Path::new(""), ".gleon/runs/ci/a.png"),
            Some(PathBuf::from("../ci/a.png"))
        );
        assert_eq!(
            workspace_root(Path::new("/w/.gleon/runs/latest")),
            Some(Path::new("/w"))
        );
        assert_eq!(workspace_root(Path::new("/downloads/latest")), None);
    }

    #[test]
    fn test_failures_by_severity_and_outcomes_that_fail() {
        let failing: Vec<_> = fixtures::every_outcome()
            .into_iter()
            .filter(|r| r.outcome.is_failure())
            .map(|r| r.outcome)
            .collect();
        assert_eq!(
            failing,
            [
                CaseOutcome::Mismatch,
                CaseOutcome::DimensionMismatch,
                CaseOutcome::Error,
                CaseOutcome::Missing
            ]
        );
        let cases = Cases::new("runs/latest", fixtures::every_outcome());
        let by_severity: Vec<_> = cases
            .failures_by_severity()
            .iter()
            .map(|(_, r)| r.name.as_str())
            .collect();
        assert_eq!(
            by_severity,
            [
                "test/mismatch",
                "test/dimension_mismatch",
                "test/error",
                "test/missing"
            ]
        );
    }

    /// Files that exist but cannot be read are errors, unlike missing ones.
    #[cfg(unix)]
    #[test]
    fn test_load_reports_unreadable_files() {
        use std::os::unix::fs::PermissionsExt as _;

        let (_temp, runs_latest) = workspace();
        let locked = runs_latest.join("cases/locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(runs_latest.join(RUN_FILE), "{}").unwrap();
        let lock = |path: &Path, mode| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        lock(&locked, 0o000);
        lock(&runs_latest.join(RUN_FILE), 0o000);
        let is_root = std::fs::read_dir(&locked).is_ok();
        let walked = list_reports(&runs_latest.join("cases"));
        let run_file = RunInfo::read(&runs_latest);
        lock(&locked, 0o755);
        lock(&runs_latest.join(RUN_FILE), 0o644);
        if !is_root {
            assert!(matches!(walked, Err(CasesError::Io { .. })), "{walked:?}");
            assert!(
                matches!(run_file, Err(CasesError::Io { .. })),
                "{run_file:?}"
            );
        }
    }

    /// A report that exists but cannot be read fails the run instead of vanishing from it, and
    /// so does a report that cannot be removed.
    #[cfg(unix)]
    #[test]
    fn test_unreadable_and_unremovable_reports_are_errors() {
        use std::os::unix::fs::PermissionsExt as _;

        let (_temp, runs_latest) = workspace();
        let mut cli = report("a", CaseOutcome::Match);
        cli.source.tool = "gleon_cli".to_owned();
        write(&runs_latest, cli, Some("r"), 0);
        let cases = runs_latest.join("cases").join(PlatformKey::host());
        let file = cases.join("a.json");
        let mode = |path: &Path, mode| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        mode(&file, 0o000);
        let is_root = std::fs::read(&file).is_ok();
        let loaded = Cases::load(&runs_latest, None);
        mode(&file, 0o644);
        mode(&cases, 0o555);
        let removed = remove_reports_of(&runs_latest, "gleon_cli", PlatformKey::host());
        mode(&cases, 0o755);
        if !is_root {
            assert!(matches!(loaded, Err(CasesError::Io { ref path, .. }) if *path == file));
            assert!(matches!(removed, Err(CasesError::Io { ref path, .. }) if *path == file));
        }
    }

    /// The platform of a case gives the key of the manifests its run reads, opaque or not.
    #[test]
    fn test_platform_of_gives_the_manifest_key() {
        let opaque = PlatformInfo {
            os: "ci-box".to_owned(),
            arch: None,
            renderer: None,
            labels: BTreeMap::new(),
        };
        let structured = PlatformInfo {
            os: "linux".to_owned(),
            arch: Some("x86_64".to_owned()),
            renderer: Some("flutter-3.47".to_owned()),
            labels: [("theme".to_owned(), "dark".to_owned())].into(),
        };
        for info in [opaque, structured] {
            assert_eq!(platform_of(&info).key().unwrap(), info.key().unwrap());
        }
    }

    #[test]
    fn test_remove_reports_of_a_tool_and_their_images() {
        let (_temp, runs_latest) = workspace();
        let host = PlatformKey::host();
        let cli_report = |platform: PlatformConfig| {
            let mut cli = fixtures::report_on("cli/a", CaseOutcome::Mismatch, platform);
            cli.source.tool = "gleon_cli".to_owned();
            cli
        };
        write(&runs_latest, cli_report(PlatformConfig::host()), None, 0);
        write(&runs_latest, cli_report(foreign()), None, 0);
        write(
            &runs_latest,
            report("flutter", CaseOutcome::Mismatch),
            None,
            0,
        );
        let foreign_key = foreign().key().unwrap();
        let images = |key: &str| runs_latest.join("artifacts").join(key).join("cli/a");
        for key in [host.as_str(), foreign_key.as_str()] {
            std::fs::create_dir_all(images(key)).unwrap();
            for file in ["golden.png", "candidate.png", "diff.png"] {
                std::fs::write(images(key).join(file), b"png").unwrap();
            }
        }
        let cases = runs_latest.join("cases");
        std::fs::write(cases.join(host).join("broken.json"), "{").unwrap();
        // Another platform's report copied into this platform's directory is not this one's.
        let stray = cases.join(host).join("stray.json");
        std::fs::write(&stray, serde_json::to_vec(&cli_report(foreign())).unwrap()).unwrap();

        remove_reports_of(&runs_latest, "gleon_cli", host).unwrap();
        assert!(!cases.join(host).join("cli/a.json").exists());
        assert_eq!(
            std::fs::read_dir(images(host.as_str())).unwrap().count(),
            0,
            "the images go, the folder stays for `gleon clean`"
        );
        assert!(cases.join(host).join("flutter.json").is_file());
        assert!(cases.join(host).join("broken.json").is_file());
        assert!(stray.is_file(), "a report of another platform stays");
        assert!(
            cases.join(&foreign_key).join("cli/a.json").is_file(),
            "another platform's reports stay"
        );
        assert_eq!(
            std::fs::read_dir(images(foreign_key.as_str()))
                .unwrap()
                .count(),
            3
        );
        remove_reports_of(&runs_latest.join("missing"), "gleon_cli", host).unwrap();
        let elsewhere = PlatformKey::parse("openbsd-sparc64").unwrap();
        remove_reports_of(&runs_latest, "gleon_cli", &elsewhere).unwrap();
    }
}
