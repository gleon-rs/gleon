//! Historical test results logging and static dashboard compiler.
//!
//! Tracks historical test runs in `history.json` and compiles a standalone,
//! interactive static HTML dashboard (`dashboard.html`) for visual reporting across
//! branches and platforms without requiring external server hosting.

use std::{
    collections::BTreeSet,
    num::NonZeroUsize,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use gleon_model::{
    case::{CaseErrorKind, CaseOutcome, CaseReport, Metrics, text},
    platform::PlatformConfig,
};
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use tracing::instrument;

use crate::{
    cases::{Cases, CasesError},
    context::ResolvedContext,
    paths::GleonPaths,
    storage::{ObjectStoreAdapter, StorageConfig, StorageError},
};

/// The schema version of `history.json` this gleon reads and writes; other versions are rejected
/// (start a new history by moving the old file away).
pub const SUPPORTED_SCHEMA_VERSION: u32 = 2;

/// Errors that can occur during history tracking or dashboard compilation.
#[derive(Debug, thiserror::Error)]
pub enum DashboardError {
    /// JSON serialization or deserialization error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Template rendering error.
    #[error("Template rendering error: {0}")]
    Render(#[from] minijinja::Error),

    /// File system I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Remote storage operation error.
    #[error("Storage error: {0}")]
    Storage(#[from] StorageError),

    /// Remote storage was not configured when push was requested.
    #[error("Storage not configured: GLEON_STORAGE_URL is required when --push is enabled")]
    StorageNotConfigured,

    /// The case reports of the run cannot be read.
    #[error(transparent)]
    Cases(#[from] CasesError),

    /// The history has another schema version than this version of gleon.
    #[error(
        "Unsupported schema version {found} of the history {location} (this gleon reads \
         {supported}): move it away to start a new history"
    )]
    UnsupportedSchemaVersion {
        /// The version encountered in `history.json`.
        found: u32,
        /// Maximum version supported by this binary.
        supported: u32,
        /// Which history: the local file or the remote object.
        location: String,
    },
}

impl From<crate::io::IoError> for DashboardError {
    fn from(err: crate::io::IoError) -> Self {
        match err {
            crate::io::IoError::Io(e) => Self::Io(e),
            crate::io::IoError::JsonParse(e) => Self::Json(e),
        }
    }
}

/// Aggregated summary counters for a test run (the rest of `total` passed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RunSummary {
    /// Total test cases in this run.
    pub total: usize,
    /// Number of test cases that failed.
    pub failed: usize,
}

/// Most failures a run keeps in the history (the most telling, [`Cases::failures_by_severity`]);
/// [`RunSummary::failed`] counts them all. A run where a renderer update fails every case stays
/// small.
pub const MAX_FAILURES_PER_RUN: usize = 100;

/// A failed test case of a historical run (passing ones are only counted, and at most
/// [`MAX_FAILURES_PER_RUN`] are kept, so the history stays small for large suites).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestHistoryEntry {
    /// Canonical test name.
    pub name: String,
    /// Outcome of the case.
    pub outcome: CaseOutcome,
    /// Class of an `error` outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<CaseErrorKind>,
    /// Metrics of a `match` or `mismatch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<Metrics>,
    /// Why the case failed, when the report says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl From<&CaseReport> for TestHistoryEntry {
    fn from(report: &CaseReport) -> Self {
        Self {
            name: report.name.clone(),
            outcome: report.outcome,
            error_kind: report.error_kind,
            metrics: report.metrics,
            message: match (&report.message, report.golden.fallback.as_deref()) {
                (Some(message), Some(shared)) => Some(format!(
                    "{message} ({})",
                    text::ComparedWithFallback(shared)
                )),
                (None, Some(shared)) => Some(text::ComparedWithFallback(shared).to_string()),
                (message, None) => message.clone(),
            },
        }
    }
}

/// A historical record representing a single visual regression test run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunHistoryEntry {
    /// Unique identifier for this run (on this platform).
    pub id: String,
    /// When the run recorded its newest case.
    pub timestamp: DateTime<Utc>,
    /// Git branch context.
    pub branch: String,
    /// The platform of the run, e.g. `macos-aarch64`.
    pub platform: String,
    /// Optional Git commit SHA.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_sha: Option<String>,
    /// Aggregated test summary.
    pub summary: RunSummary,
    /// The failed test cases of this run.
    pub failures: Vec<TestHistoryEntry>,
}

impl RunHistoryEntry {
    /// Constructs a `RunHistoryEntry` from the case reports of a run and run metadata.
    #[must_use]
    pub fn from_cases(
        id: impl Into<String>,
        timestamp: DateTime<Utc>,
        branch: impl Into<String>,
        platform: impl Into<String>,
        commit_sha: Option<String>,
        cases: &Cases,
    ) -> Self {
        let failures = cases.failures_by_severity();
        Self {
            id: id.into(),
            timestamp,
            branch: branch.into(),
            platform: platform.into(),
            commit_sha,
            summary: RunSummary {
                total: cases.reports().len(),
                failed: failures.len(),
            },
            failures: failures
                .into_iter()
                .take(MAX_FAILURES_PER_RUN)
                .map(TestHistoryEntry::from)
                .collect(),
        }
    }
}

/// `platform` as people name it: an opaque key as is, structured fields joined with `-`
/// (`macos-aarch64`, `linux-x86_64-chrome-126-theme=dark`).
#[must_use]
pub fn platform_label(platform: &PlatformConfig) -> String {
    let fields = match platform {
        PlatformConfig::Opaque(key) => return key.clone(),
        PlatformConfig::Structured(fields) => fields,
    };
    let mut label = String::new();
    let mut push = |parts: &[&str]| {
        if !label.is_empty() {
            label.push('-');
        }
        for part in parts {
            label.push_str(part);
        }
    };
    for part in fields.os.iter().chain(&fields.arch).chain(&fields.renderer) {
        push(&[part]);
    }
    for (key, value) in fields.labels.iter().flatten() {
        push(&[key, "=", value]);
    }
    label
}

/// The root schema of `history.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DashboardHistory {
    /// Schema format version.
    pub schema_version: u32,
    /// Chronological list of test runs (oldest first).
    pub runs: Vec<RunHistoryEntry>,
}

impl Default for DashboardHistory {
    fn default() -> Self {
        Self {
            schema_version: SUPPORTED_SCHEMA_VERSION,
            runs: Vec::new(),
        }
    }
}

impl DashboardHistory {
    /// Creates a new empty `DashboardHistory`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses `history.json` content from a string, or initializes an empty history if empty;
    /// `location` names the history in errors.
    ///
    /// # Errors
    /// Returns [`DashboardError::Json`] or [`DashboardError::UnsupportedSchemaVersion`].
    pub fn parse_or_empty(raw: &str, location: &str) -> Result<Self, DashboardError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(Self::new());
        }
        parse_history(trimmed.as_bytes(), location)
    }

    /// Adds a run to the history log (see [`Self::merge`] for runs it already has).
    ///
    /// If `truncate_limit` is `Some(limit)` and the number of runs exceeds `limit`,
    /// the oldest runs are dropped from the beginning.
    pub fn append_run(&mut self, entry: RunHistoryEntry, truncate_limit: Option<NonZeroUsize>) {
        self.merge(
            Self {
                runs: vec![entry],
                ..Self::new()
            },
            truncate_limit,
        );
    }

    /// Merges another [`DashboardHistory`] into this one and sorts all runs chronologically by
    /// `timestamp`. A run recorded twice (the same `id`) keeps its newest entry, and on equal
    /// timestamps the entry of `other`: compiling a run again replaces what the history had.
    ///
    /// If `truncate_limit` is `Some(limit)` and the merged collection exceeds `limit`,
    /// the oldest runs are dropped from the beginning.
    pub fn merge(&mut self, mut other: Self, truncate_limit: Option<NonZeroUsize>) {
        // `other` first: the stable sort keeps it ahead of an entry of `self` it ties with.
        other.runs.append(&mut self.runs);
        self.runs = other.runs;

        // The entries of one id become contiguous, newest first; `dedup_by` keeps the first.
        self.runs
            .sort_by(|a, b| a.id.cmp(&b.id).then(b.timestamp.cmp(&a.timestamp)));
        self.runs.dedup_by(|a, b| a.id == b.id);

        // Finally sort chronologically by timestamp (zero allocation via Copy DateTime)
        self.runs.sort_by_key(|a| a.timestamp);

        if let Some(limit) = truncate_limit
            && self.runs.len() > limit.get()
        {
            let excess = self.runs.len() - limit.get();
            let _ = self.runs.drain(0..excess);
        }
    }
}

/// Maximum number of recent runs rendered in the trend chart to prevent SVG DOM explosion.
const MAX_CHART_RUNS: usize = 30;

/// Context structure supplied to the `MiniJinja` template renderer.
#[derive(Serialize)]
struct DashboardView<'a> {
    total_runs: usize,
    passed_runs: usize,
    failed_runs: usize,
    run_pass_rate: Option<f64>,
    test_pass_rate: Option<f64>,
    branches: Vec<&'a str>,
    platforms: Vec<&'a str>,
    runs: &'a [RunHistoryEntry],
    chart_runs: &'a [RunHistoryEntry],
    generated_at: String,
}

/// Execution options for [`DashboardCompiler::execute`].
#[derive(Debug, Clone, Default)]
pub struct DashboardOptions<'a> {
    /// Explicit output path for compiled HTML dashboard.
    pub out_html: Option<&'a Path>,
    /// Limit the maximum number of historical runs kept.
    pub truncate_limit: Option<NonZeroUsize>,
    /// Upload history.json and dashboard.html to remote storage.
    pub push_to_storage: bool,
}

/// Static dashboard compiler for visual regression history.
pub struct DashboardCompiler;

impl DashboardCompiler {
    /// Compiles a static `dashboard.html` string from a [`DashboardHistory`].
    ///
    /// # Errors
    /// Returns [`DashboardError::Render`] if template rendering fails.
    pub fn compile_dashboard(history: &DashboardHistory) -> Result<String, DashboardError> {
        let total_runs = history.runs.len();
        let passed_runs = history
            .runs
            .iter()
            .filter(|r| r.summary.failed == 0)
            .count();
        let failed_runs = total_runs.saturating_sub(passed_runs);
        #[expect(
            clippy::cast_precision_loss,
            reason = "counts are far below 2^52, so the f64 conversion is exact for any realistic input"
        )]
        let run_pass_rate = if total_runs == 0 {
            None
        } else {
            Some((passed_runs as f64 / total_runs as f64) * 100.0)
        };

        let total_tests: usize = history.runs.iter().map(|r| r.summary.total).sum();
        let passed_tests: usize = history
            .runs
            .iter()
            .map(|r| r.summary.total.saturating_sub(r.summary.failed))
            .sum();
        #[expect(
            clippy::cast_precision_loss,
            reason = "counts are far below 2^52, so the f64 conversion is exact for any realistic input"
        )]
        let test_pass_rate = if total_tests == 0 {
            None
        } else {
            Some((passed_tests as f64 / total_tests as f64) * 100.0)
        };

        let chart_runs = if history.runs.len() > MAX_CHART_RUNS {
            &history.runs[history.runs.len() - MAX_CHART_RUNS..]
        } else {
            &history.runs[..]
        };

        let mut branches = BTreeSet::new();
        let mut platforms = BTreeSet::new();
        for run in &history.runs {
            let _ = branches.insert(run.branch.as_str());
            let _ = platforms.insert(run.platform.as_str());
        }

        let view = DashboardView {
            total_runs,
            passed_runs,
            failed_runs,
            run_pass_rate,
            test_pass_rate,
            branches: branches.into_iter().collect(),
            platforms: platforms.into_iter().collect(),
            runs: &history.runs,
            chart_runs,
            generated_at: Utc::now().to_rfc3339(),
        };

        let template = crate::report::JINJA_ENV
            .get_template("dashboard.html")
            .map_err(DashboardError::Render)?;

        let html = template.render(&view).map_err(DashboardError::Render)?;
        Ok(html)
    }

    /// High-level executor that synchronizes history, compiles the dashboard, and pushes to storage if requested.
    ///
    /// Employs optimistic concurrency control (retrying up to 3 times on precondition failure) when uploading
    /// to remote storage.
    ///
    /// # Errors
    /// Returns [`DashboardError`] if report reading, history synchronization, file writes,
    /// or remote storage operations fail.
    #[instrument(skip(context, cases, storage_cfg), level = "debug")]
    pub async fn execute(
        paths: &GleonPaths,
        context: &ResolvedContext,
        cases: &Cases,
        options: &DashboardOptions<'_>,
        storage_cfg: Option<&StorageConfig>,
    ) -> Result<DashboardExecutionResult, DashboardError> {
        let adapter = match storage_cfg {
            Some(cfg) => Some(ObjectStoreAdapter::from_config(cfg)?),
            None if options.push_to_storage => return Err(DashboardError::StorageNotConfigured),
            None => None,
        };

        let recorded_at = cases.recorded_at().unwrap_or_else(Utc::now);
        let platform = run_platform(cases, context);
        // One run id covers every platform of a CI run, so the platform completes the id. Without
        // one, the newest report names the run: compiling the same results twice adds one run.
        let run_id = cases.run_id().map_or_else(
            || generate_run_id(recorded_at, &context.branch, &platform),
            |run_id| format!("{}/{platform}", run_id.as_str()),
        );
        let history_path = paths.history_file();
        let target_html_path = options
            .out_html
            .map_or_else(|| paths.dashboard_file(), Path::to_path_buf);

        // Load local history ONCE and append the current run.
        let mut base_history = load_local_history_or_default(paths)?;
        let run_entry = RunHistoryEntry::from_cases(
            &run_id,
            recorded_at,
            &context.branch,
            platform,
            context.commit_sha.clone(),
            cases,
        );
        base_history.append_run(run_entry, options.truncate_limit);

        let (total_runs, pushed) = push_or_save_history(
            paths,
            options,
            adapter.as_ref(),
            &base_history,
            &history_path,
            &target_html_path,
        )
        .await?;

        Ok(DashboardExecutionResult {
            total_runs,
            history_path,
            html_path: target_html_path,
            pushed,
        })
    }
}

/// The platform of the run of `cases` as its reports name it (several joined with `+`, e.g. when
/// the artifacts of several hosts were merged), or the context's when there are none.
fn run_platform(cases: &Cases, context: &ResolvedContext) -> String {
    let platforms: BTreeSet<_> = cases
        .reports()
        .iter()
        .map(|report| platform_label(&report.platform))
        .collect();
    if platforms.is_empty() {
        platform_label(&crate::cases::platform_of(&context.platform))
    } else {
        platforms.into_iter().collect::<Vec<_>>().join("+")
    }
}

/// Parses `history.json` content of the supported schema version; `location` names it in errors.
fn parse_history(bytes: &[u8], location: &str) -> Result<DashboardHistory, DashboardError> {
    #[derive(Deserialize)]
    struct Version {
        schema_version: u32,
    }
    let Version { schema_version } = serde_json::from_slice(bytes)?;
    if schema_version != SUPPORTED_SCHEMA_VERSION {
        return Err(DashboardError::UnsupportedSchemaVersion {
            found: schema_version,
            supported: SUPPORTED_SCHEMA_VERSION,
            location: location.to_owned(),
        });
    }
    Ok(serde_json::from_slice(bytes)?)
}

async fn push_or_save_history(
    paths: &GleonPaths,
    options: &DashboardOptions<'_>,
    adapter: Option<&ObjectStoreAdapter>,
    base_history: &DashboardHistory,
    history_path: &Path,
    target_html_path: &Path,
) -> Result<(usize, bool), DashboardError> {
    push_or_save_history_with_hook(
        paths,
        options,
        adapter,
        base_history,
        history_path,
        target_html_path,
        |_| {},
    )
    .await
}

async fn push_or_save_history_with_hook<F>(
    paths: &GleonPaths,
    options: &DashboardOptions<'_>,
    adapter: Option<&ObjectStoreAdapter>,
    base_history: &DashboardHistory,
    history_path: &Path,
    target_html_path: &Path,
    mut on_before_upload: F,
) -> Result<(usize, bool), DashboardError>
where
    F: FnMut(usize),
{
    const MAX_PUSH_RETRIES: usize = 3;
    let mut attempt = 0;

    loop {
        attempt += 1;

        let mut history = base_history.clone();
        let mut expected_history_etag = None;
        let mut expected_history_version = None;
        let mut history_create_only = false;

        let mut expected_dashboard_etag = None;
        let mut expected_dashboard_version = None;
        let mut dashboard_create_only = false;

        if let Some(ad) = adapter.filter(|_| options.push_to_storage) {
            if let Some(remote_obj) = ad.get_object("history.json").await? {
                expected_history_etag = remote_obj.e_tag;
                expected_history_version = remote_obj.version;
                let text = std::str::from_utf8(&remote_obj.bytes).map_err(|e| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("invalid UTF-8 in remote history.json: {e}"),
                    )
                })?;
                // The remote history is older than this run: merged into it, not over it.
                let mut remote_history =
                    DashboardHistory::parse_or_empty(text, "history.json of the remote storage")?;
                remote_history.merge(history, options.truncate_limit);
                history = remote_history;
            } else {
                history_create_only = true;
            }

            // Only its version: the page is compiled again from the history.
            if let Some(remote_html) = ad.head_object("dashboard.html").await? {
                expected_dashboard_etag = remote_html.e_tag;
                expected_dashboard_version = remote_html.version;
            } else {
                dashboard_create_only = true;
            }
        }

        // Re-read local history immediately before saving to prevent concurrent local runs from overwriting each other
        let mut local_history = load_local_history_or_default(paths)?;
        local_history.merge(history, options.truncate_limit);

        // Save local history.json
        let serialized_history = serde_json::to_string_pretty(&local_history)?;
        crate::io::save_file_atomically(history_path, serialized_history.as_bytes())?;

        // Compile dashboard.html and save locally
        let html_content = DashboardCompiler::compile_dashboard(&local_history)?;
        crate::io::save_file_atomically(target_html_path, html_content.as_bytes())?;

        let Some(ad) = adapter.filter(|_| options.push_to_storage) else {
            return Ok((local_history.runs.len(), false));
        };

        // Notify hook at the upload boundary before attempting upload
        on_before_upload(attempt);

        let upload_res = upload_history_and_dashboard(
            ad,
            serialized_history.into_bytes(),
            html_content.into_bytes(),
            expected_history_etag.as_deref(),
            expected_history_version.as_deref(),
            history_create_only,
            expected_dashboard_etag.as_deref(),
            expected_dashboard_version.as_deref(),
            dashboard_create_only,
        )
        .await;

        match upload_res {
            Ok(()) => return Ok((local_history.runs.len(), true)),
            Err(StorageError::PreconditionFailed { .. }) if attempt < MAX_PUSH_RETRIES => {
                tracing::warn!(
                    "Concurrent modification on remote history or dashboard; retrying merge (attempt {attempt}/{MAX_PUSH_RETRIES})..."
                );
            }
            Err(e) => return Err(DashboardError::Storage(e)),
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "mirrors the independent etag/version/create-only preconditions of the two uploads"
)]
async fn upload_history_and_dashboard(
    adapter: &ObjectStoreAdapter,
    history_json: Vec<u8>,
    html_content: Vec<u8>,
    expected_history_etag: Option<&str>,
    expected_history_version: Option<&str>,
    history_create_only: bool,
    expected_dashboard_etag: Option<&str>,
    expected_dashboard_version: Option<&str>,
    dashboard_create_only: bool,
) -> Result<(), StorageError> {
    // `history.json` is the source of truth, so it goes first: when its precondition fails
    // (another run was faster), nothing is published and the caller retries on the newer history.
    // A failed HTML upload after it only leaves the page one run behind until the next upload,
    // which renders it again from the history.
    adapter
        .put_object_conditional(
            "history.json",
            bytes::Bytes::from(history_json),
            Some("application/json"),
            expected_history_etag,
            expected_history_version,
            history_create_only,
        )
        .await?;
    adapter
        .put_object_conditional(
            "dashboard.html",
            bytes::Bytes::from(html_content),
            Some("text/html; charset=utf-8"),
            expected_dashboard_etag,
            expected_dashboard_version,
            dashboard_create_only,
        )
        .await
}

/// Result summary of the dashboard execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardExecutionResult {
    /// Number of runs currently recorded in history.
    pub total_runs: usize,
    /// Local path to the updated `history.json`.
    pub history_path: PathBuf,
    /// Local path to the compiled `dashboard.html`.
    pub html_path: PathBuf,
    /// Whether files were uploaded to remote storage.
    pub pushed: bool,
}

fn load_local_history_or_default(paths: &GleonPaths) -> Result<DashboardHistory, DashboardError> {
    let path = paths.history_file();
    match std::fs::read(&path) {
        Ok(bytes) => parse_history(&bytes, &format!("'{}'", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DashboardHistory::new()),
        Err(e) => Err(e.into()),
    }
}

fn generate_run_id(timestamp: DateTime<Utc>, branch: &str, platform: &str) -> String {
    let mut hasher = sha2::Sha256::new();
    hasher.update(branch.len().to_le_bytes());
    hasher.update(b":");
    hasher.update(branch.as_bytes());
    hasher.update(b":");
    hasher.update(platform.len().to_le_bytes());
    hasher.update(b":");
    hasher.update(platform.as_bytes());
    hasher.update(b":");
    hasher.update(timestamp.to_rfc3339().as_bytes());
    let hash = hex::encode(hasher.finalize());
    format!("run-{}-{}", timestamp.format("%Y%m%d%H%M%S"), &hash[..8])
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
    use std::num::NonZeroUsize;

    use super::*;
    use crate::cases::fixtures::{every_outcome, report};

    /// A history entry; `diff_pixels` gives it pixel metrics.
    fn test_entry(
        name: &str,
        outcome: CaseOutcome,
        diff_pixels: Option<u64>,
        message: Option<String>,
    ) -> TestHistoryEntry {
        TestHistoryEntry {
            name: name.to_owned(),
            outcome,
            error_kind: None,
            metrics: diff_pixels.map(|diff_pixels| Metrics::Pixel {
                total_pixels: 1000,
                diff_pixels,
                diff_ratio: diff_pixels as f64 / 1000.0,
                headroom: 0.0,
                text: None,
            }),
            message,
        }
    }

    /// One passing case as the run `run_id`.
    fn passing_run(run_id: &str) -> Cases {
        Cases::new("runs/latest", vec![report("home", CaseOutcome::Match)])
            .with_run_id(gleon_model::case::RunId::new(run_id).unwrap())
    }

    /// A failure compared with another platform's golden says so in the history.
    #[test]
    fn test_history_entries_name_a_fallback_golden() {
        use crate::cases::fixtures::report;

        let mut case = report("a", CaseOutcome::Mismatch);
        case.message = Some("5.00% (5 of 100px) differ".to_owned());
        case.golden.fallback = Some("test/goldens/a.png".to_owned());
        assert_eq!(
            TestHistoryEntry::from(&case).message.as_deref(),
            Some(
                "5.00% (5 of 100px) differ (compared with test/goldens/a.png of the fallback platform)"
            )
        );
        case.message = None;
        assert_eq!(
            TestHistoryEntry::from(&case).message.as_deref(),
            Some("compared with test/goldens/a.png of the fallback platform")
        );
    }

    #[test]
    fn test_history_parse_empty_and_valid() {
        // 1. Empty string yields empty DashboardHistory
        let empty = DashboardHistory::parse_or_empty("   ", "test").unwrap();
        assert_eq!(empty.schema_version, 2);
        assert!(empty.runs.is_empty());

        // 2. Valid JSON parses correctly
        let json = r#"{
            "schema_version": 2,
            "runs": [
                {
                    "id": "run-1",
                    "timestamp": "2026-09-13T10:00:00Z",
                    "branch": "main",
                    "platform": "linux-x86_64",
                    "summary": { "total": 2, "failed": 0 },
                    "failures": []
                }
            ]
        }"#;
        let parsed = DashboardHistory::parse_or_empty(json, "test").unwrap();
        assert_eq!(parsed.runs.len(), 1);
        assert_eq!(parsed.runs[0].id, "run-1");
        assert_eq!(parsed.runs[0].branch, "main");
        assert_eq!(parsed.runs[0].summary.total, 2);

        // 3. Corrupt JSON returns error
        assert!(DashboardHistory::parse_or_empty("not json", "test").is_err());
    }

    #[test]
    fn test_history_schema_version_validation() {
        let bad_json = r#"{
            "schema_version": 99,
            "runs": []
        }"#;
        let err = DashboardHistory::parse_or_empty(bad_json, "'.gleon/history.json'").unwrap_err();
        assert!(matches!(
            err,
            DashboardError::UnsupportedSchemaVersion {
                found: 99,
                supported: 2,
                ..
            }
        ));
        assert!(
            err.to_string().contains("history '.gleon/history.json'"),
            "the error names the history: {err}"
        );
        // History of the first format is not read either.
        assert!(matches!(
            DashboardHistory::parse_or_empty(r#"{"schema_version": 1, "runs": []}"#, "test"),
            Err(DashboardError::UnsupportedSchemaVersion { found: 1, .. })
        ));
    }

    #[test]
    fn test_history_append_run_infinite() {
        let mut history = DashboardHistory::new();
        assert_eq!(history.runs.len(), 0);

        for i in 1..=5 {
            let entry = RunHistoryEntry {
                id: format!("run-{i}"),
                timestamp: Utc::now(),
                branch: "feature".to_string(),
                platform: "macos-aarch64".to_string(),
                commit_sha: None,
                summary: RunSummary {
                    total: 1,
                    failed: 0,
                },
                failures: vec![],
            };
            history.append_run(entry, None);
        }

        assert_eq!(history.runs.len(), 5);
        assert_eq!(history.runs[0].id, "run-1");
        assert_eq!(history.runs[4].id, "run-5");
    }

    #[test]
    fn test_history_truncate() {
        let mut history = DashboardHistory::new();

        for i in 1..=5 {
            let entry = RunHistoryEntry {
                id: format!("run-{i}"),
                timestamp: Utc::now(),
                branch: "main".to_string(),
                platform: "linux-x86_64".to_string(),
                commit_sha: None,
                summary: RunSummary {
                    total: 1,
                    failed: 0,
                },
                failures: vec![],
            };
            history.append_run(entry, NonZeroUsize::new(3));
        }

        // Expected to keep only the newest 3 runs: run-3, run-4, run-5
        assert_eq!(history.runs.len(), 3);
        assert_eq!(history.runs[0].id, "run-3");
        assert_eq!(history.runs[1].id, "run-4");
        assert_eq!(history.runs[2].id, "run-5");
    }

    #[test]
    fn test_generate_run_id_collision_free() {
        let ts = Utc::now();
        let id1 = generate_run_id(ts, "feat:login", "desktop");
        let id2 = generate_run_id(ts, "feat", "login:desktop");
        assert_ne!(
            id1, id2,
            "length prefixing must prevent delimiter collisions"
        );
    }

    #[test]
    fn test_history_merge_deduplicates_and_sorts() {
        let mut local = DashboardHistory::new();
        let mut remote = DashboardHistory::new();

        let t1 = DateTime::parse_from_rfc3339("2026-09-13T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let t2 = DateTime::parse_from_rfc3339("2026-09-13T11:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let t3 = DateTime::parse_from_rfc3339("2026-09-13T10:30:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let run_1 = RunHistoryEntry {
            id: "run-1".to_string(),
            timestamp: t1,
            branch: "main".to_string(),
            platform: "linux-x86_64".to_string(),
            commit_sha: None,
            summary: RunSummary {
                total: 1,
                failed: 0,
            },
            failures: vec![],
        };
        let run_2_local = RunHistoryEntry {
            id: "run-2".to_string(),
            timestamp: t2,
            branch: "feature/local".to_string(),
            platform: "macos-aarch64".to_string(),
            commit_sha: None,
            summary: RunSummary {
                total: 1,
                failed: 0,
            },
            failures: vec![],
        };
        let run_3_remote = RunHistoryEntry {
            id: "run-3".to_string(),
            timestamp: t3,
            branch: "feature/remote".to_string(),
            platform: "windows-x86_64".to_string(),
            commit_sha: None,
            summary: RunSummary {
                total: 1,
                failed: 0,
            },
            failures: vec![],
        };

        // local has run-1 and run-2
        local.append_run(run_1.clone(), None);
        local.append_run(run_2_local, None);

        // remote has run-1 (duplicate) and run-3 (inserted chronologically between 1 and 2)
        remote.append_run(run_1, None);
        remote.append_run(run_3_remote, None);

        local.merge(remote, None);

        // Must contain 3 unique runs sorted by timestamp: run-1, run-3, run-2
        assert_eq!(local.runs.len(), 3);
        assert_eq!(local.runs[0].id, "run-1");
        assert_eq!(local.runs[1].id, "run-3");
        assert_eq!(local.runs[2].id, "run-2");
    }

    #[test]
    fn test_history_merge_deduplicates_same_id_with_different_timestamps() {
        let mut local = DashboardHistory::new();
        let mut remote = DashboardHistory::new();

        let t1 = DateTime::parse_from_rfc3339("2026-09-13T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let t2 = DateTime::parse_from_rfc3339("2026-09-13T10:01:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let t3 = DateTime::parse_from_rfc3339("2026-09-13T10:02:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let run_dup_1 = RunHistoryEntry {
            id: "run-dup".to_string(),
            timestamp: t1,
            branch: "main".to_string(),
            platform: "linux-x86_64".to_string(),
            commit_sha: None,
            summary: RunSummary::default(),
            failures: vec![],
        };
        let run_middle = RunHistoryEntry {
            id: "run-middle".to_string(),
            timestamp: t2,
            branch: "main".to_string(),
            platform: "linux-x86_64".to_string(),
            commit_sha: None,
            summary: RunSummary::default(),
            failures: vec![],
        };
        let run_dup_2 = RunHistoryEntry {
            id: "run-dup".to_string(),
            timestamp: t3,
            branch: "main".to_string(),
            platform: "linux-x86_64".to_string(),
            commit_sha: None,
            summary: RunSummary::default(),
            failures: vec![],
        };

        local.append_run(run_dup_1.clone(), None);
        local.append_run(run_middle.clone(), None);
        remote.append_run(run_dup_2.clone(), None);
        let mut newer_local = remote.clone();
        newer_local.append_run(run_middle, None);
        let older_remote = DashboardHistory {
            runs: vec![run_dup_1],
            ..DashboardHistory::new()
        };

        local.merge(remote, None);

        // One entry per id, the newest one: a run compiled again replaces what the history had.
        assert_eq!(local.runs.len(), 2, "Duplicate run ID must be removed");
        assert_eq!(local.runs[0].id, "run-middle");
        assert_eq!(local.runs[1], run_dup_2);

        // Whichever side has it.
        newer_local.merge(older_remote, None);
        assert_eq!(newer_local.runs, local.runs);

        // Compiled again with the same timestamp (a report removed, not added): the new entry
        // wins over the one the history had.
        let recompiled = RunHistoryEntry {
            summary: RunSummary {
                total: 1,
                failed: 1,
            },
            ..run_dup_2
        };
        local.append_run(recompiled.clone(), None);
        assert_eq!(local.runs[1], recompiled);
    }

    #[test]
    fn test_history_merge_truncate() {
        let mut local = DashboardHistory::new();
        let mut remote = DashboardHistory::new();

        let base_ts = DateTime::parse_from_rfc3339("2026-09-13T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        for i in 1..=3 {
            let entry = RunHistoryEntry {
                id: format!("run-{i}"),
                timestamp: base_ts + chrono::Duration::minutes(i),
                branch: "main".to_string(),
                platform: "linux-x86_64".to_string(),
                commit_sha: None,
                summary: RunSummary::default(),
                failures: vec![],
            };
            local.append_run(entry, None);
        }

        for i in 4..=6 {
            let entry = RunHistoryEntry {
                id: format!("run-{i}"),
                timestamp: base_ts + chrono::Duration::minutes(i),
                branch: "feature".to_string(),
                platform: "macos-aarch64".to_string(),
                commit_sha: None,
                summary: RunSummary::default(),
                failures: vec![],
            };
            remote.append_run(entry, None);
        }

        local.merge(remote, NonZeroUsize::new(3));

        // Should be truncated to the newest 3 runs: run-4, run-5, run-6
        assert_eq!(local.runs.len(), 3);
        assert_eq!(local.runs[0].id, "run-4");
        assert_eq!(local.runs[1].id, "run-5");
        assert_eq!(local.runs[2].id, "run-6");
    }

    #[test]
    fn test_compile_dashboard_html() {
        let mut history = DashboardHistory::new();
        let t1 = DateTime::parse_from_rfc3339("2026-09-13T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let t2 = DateTime::parse_from_rfc3339("2026-09-13T11:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let entry1 = RunHistoryEntry {
            id: "run-1".to_string(),
            timestamp: t1,
            branch: "main".to_string(),
            platform: "macos-aarch64".to_string(),
            commit_sha: Some("abcdef123456".to_string()),
            summary: RunSummary {
                total: 2,
                failed: 0,
            },
            failures: vec![test_entry("auth/login", CaseOutcome::Identical, None, None)],
        };
        let entry2 = RunHistoryEntry {
            id: "run-2".to_string(),
            timestamp: t2,
            branch: "feature/cart".to_string(),
            platform: "linux-x86_64".to_string(),
            commit_sha: None,
            summary: RunSummary {
                total: 2,
                failed: 1,
            },
            failures: vec![
                test_entry("cart/checkout", CaseOutcome::Mismatch, Some(42), None),
                test_entry("cart/item", CaseOutcome::Identical, None, None),
            ],
        };

        history.append_run(entry1, None);
        history.append_run(entry2, None);

        let html = DashboardCompiler::compile_dashboard(&history).unwrap();
        assert!(html.contains("<!DOCTYPE html>"));
        assert!(html.contains("Gleon Regression History"));
        assert!(html.contains("main"));
        assert!(html.contains("feature&#x2f;cart"));
        assert!(html.contains("macos-aarch64"));
        assert!(html.contains("linux-x86_64"));
        assert!(html.contains("Diff pixels: 42"));
        assert!(html.contains("Run Pass Rate"));
        assert!(html.contains("Test Pass Rate"));
        assert!(html.contains("50.0%")); // Run pass rate: 1 of 2 runs passed
        assert!(html.contains("75.0%")); // Test pass rate: 3 of 4 tests passed
    }

    #[test]
    fn test_compile_dashboard_xss_protection() {
        let mut history = DashboardHistory::new();
        let entry = RunHistoryEntry {
            id: "run-xss".to_string(),
            timestamp: Utc::now(),
            branch: "feature/\"><script>alert(1)</script>".to_string(),
            platform: "linux-x86_64".to_string(),
            commit_sha: None,
            summary: RunSummary {
                total: 1,
                failed: 1,
            },
            failures: vec![test_entry(
                "<img src=x onerror=alert('xss')>",
                CaseOutcome::Mismatch,
                Some(1),
                Some("<b>bold error</b>".to_string()),
            )],
        };
        history.append_run(entry, None);

        let html = DashboardCompiler::compile_dashboard(&history).unwrap();
        // Must escape HTML tags
        assert!(!html.contains("<script>alert(1)</script>"));
        assert!(!html.contains("<img src=x onerror="));
        assert!(!html.contains("<b>bold error</b>"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;&#x2f;script&gt;"));
        assert!(html.contains("&lt;img src=x"));
        assert!(html.contains("&lt;b&gt;bold error&lt;&#x2f;b&gt;"));
    }

    #[test]
    fn test_compile_dashboard_empty_history_em_dash() {
        let history = DashboardHistory::new();
        let html = DashboardCompiler::compile_dashboard(&history).unwrap();
        // Zero runs must display '—' instead of NaN or percent
        assert!(html.contains("—"));
        assert!(!html.contains("NaN"));
    }

    #[test]
    fn test_compile_dashboard_ssim_fallback_rendering() {
        let mut history = DashboardHistory::new();
        let entry = RunHistoryEntry {
            id: "run-ssim".to_string(),
            timestamp: Utc::now(),
            branch: "main".to_string(),
            platform: "macos-aarch64".to_string(),
            commit_sha: None,
            summary: RunSummary {
                total: 1,
                failed: 1,
            },
            failures: vec![test_entry(
                "profile/header",
                CaseOutcome::Mismatch,
                Some(99),
                Some("Image dimension mismatch, falling back to pixel diff".to_string()),
            )],
        };
        history.append_run(entry, None);

        let html = DashboardCompiler::compile_dashboard(&history).unwrap();
        assert!(html.contains("Diff pixels: 99"));
        assert!(html.contains("Image dimension mismatch, falling back to pixel diff"));
    }

    #[test]
    fn test_chart_limits_to_recent_runs() {
        let mut history = DashboardHistory::new();
        let base_ts = DateTime::parse_from_rfc3339("2026-09-13T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        for i in 1..=45 {
            let entry = RunHistoryEntry {
                id: format!("run-{i}"),
                timestamp: base_ts + chrono::Duration::minutes(i),
                branch: "main".to_string(),
                platform: "linux-x86_64".to_string(),
                commit_sha: None,
                summary: RunSummary {
                    total: 1,
                    failed: 0,
                },
                failures: vec![],
            };
            history.append_run(entry, None);
        }

        let html = DashboardCompiler::compile_dashboard(&history).unwrap();
        // Total runs is 45, but chart must be capped to MAX_CHART_RUNS (30)
        assert!(html.contains("45")); // Total runs KPI
        assert!(html.contains("Last 30 runs"));
        assert!(!html.contains("Last 45 runs"));
    }

    #[test]
    fn test_platform_label_of_every_form() {
        use gleon_model::platform::PlatformFields;

        assert_eq!(
            platform_label(&PlatformConfig::Opaque("ci-box".to_owned())),
            "ci-box"
        );
        let labeled = PlatformConfig::Structured(PlatformFields {
            os: Some("linux".to_owned()),
            arch: Some("x86_64".to_owned()),
            renderer: Some("chrome-126".to_owned()),
            labels: Some([("theme".to_owned(), "dark".to_owned())].into()),
        });
        assert_eq!(
            platform_label(&labeled),
            "linux-x86_64-chrome-126-theme=dark"
        );
    }

    /// A run without a run id or reports is named after the branch, the platform of the context
    /// and its time.
    #[tokio::test]
    async fn test_dashboard_names_a_run_without_an_id() {
        let temp = tempfile::tempdir().unwrap();
        let paths = GleonPaths::new(temp.path());
        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            ..ResolvedContext::default()
        };
        let cases = Cases::new("runs/latest", Vec::new());
        DashboardCompiler::execute(&paths, &ctx, &cases, &DashboardOptions::default(), None)
            .await
            .unwrap();
        let history = load_local_history_or_default(&paths).unwrap();
        let run = &history.runs[0];
        assert!(run.id.starts_with("run-"), "{}", run.id);
        assert_eq!(run.platform, "unknown");
    }

    #[tokio::test]
    #[cfg(not(miri))]
    async fn test_dashboard_compiler_execute_local_and_remote() {
        let temp = tempfile::tempdir().unwrap();
        let base_dir = temp.path();
        let paths = GleonPaths::new(base_dir);

        let ctx = ResolvedContext {
            base_dir: base_dir.to_path_buf(),
            branch: "feature/dashboard".to_string(),
            ..ResolvedContext::default()
        };

        // 1. First execution in local mode without storage
        let opts_local = DashboardOptions::default();
        let res_local =
            DashboardCompiler::execute(&paths, &ctx, &passing_run("r1"), &opts_local, None)
                .await
                .unwrap();

        assert_eq!(res_local.total_runs, 1);
        assert!(!res_local.pushed);
        assert!(paths.history_file().is_file());
        assert!(paths.dashboard_file().is_file());

        // 2. Push requested but storage not configured -> StorageNotConfigured error
        let opts_push_no_storage = DashboardOptions {
            push_to_storage: true,
            ..Default::default()
        };
        let err_no_storage = DashboardCompiler::execute(
            &paths,
            &ctx,
            &passing_run("r1"),
            &opts_push_no_storage,
            None,
        )
        .await;
        assert!(matches!(
            err_no_storage,
            Err(DashboardError::StorageNotConfigured)
        ));

        // 3. Second execution with remote storage (file://) and push enabled
        let remote_store_dir = temp.path().join("remote_store");
        std::fs::create_dir_all(&remote_store_dir).unwrap();
        let storage_cfg = StorageConfig::new(format!("file://{}", remote_store_dir.display()));
        let opts_remote = DashboardOptions {
            truncate_limit: NonZeroUsize::new(5),
            push_to_storage: true,
            ..Default::default()
        };
        let res_remote = DashboardCompiler::execute(
            &paths,
            &ctx,
            &passing_run("r2"),
            &opts_remote,
            Some(&storage_cfg),
        )
        .await
        .unwrap();

        assert_eq!(res_remote.total_runs, 2);
        assert!(res_remote.pushed);

        // Verify remote storage received both history.json and dashboard.html
        let adapter = ObjectStoreAdapter::from_config(&storage_cfg).unwrap();
        let remote_history = adapter.get_object("history.json").await.unwrap();
        assert!(remote_history.is_some());
        let remote_html = adapter.get_object("dashboard.html").await.unwrap();
        assert!(remote_html.is_some());

        // 4. Remote-local merge test: simulate an external run in remote storage
        let mut external_history = DashboardHistory::new();
        let ext_ts = DateTime::parse_from_rfc3339("2026-09-13T09:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        external_history.append_run(
            RunHistoryEntry {
                id: "run-external-ci".to_string(),
                timestamp: ext_ts,
                branch: "release".to_string(),
                platform: "windows-x86_64".to_string(),
                commit_sha: None,
                summary: RunSummary {
                    total: 1,
                    failed: 0,
                },
                failures: vec![],
            },
            None,
        );
        let external_bytes = bytes::Bytes::from(
            serde_json::to_string(&external_history)
                .unwrap()
                .into_bytes(),
        );
        adapter
            .put_object("history.json", external_bytes, Some("application/json"))
            .await
            .unwrap();

        // Run execute again: local has 2 runs, remote has 1 external run + 1 new run appended = 4 runs
        let opts_merge = DashboardOptions {
            push_to_storage: true,
            ..Default::default()
        };
        let res_merged = DashboardCompiler::execute(
            &paths,
            &ctx,
            &passing_run("r3"),
            &opts_merge,
            Some(&storage_cfg),
        )
        .await
        .unwrap();
        assert_eq!(res_merged.total_runs, 4); // 2 local + 1 remote external + 1 new run
        let again = DashboardCompiler::execute(
            &paths,
            &ctx,
            &passing_run("r3"),
            &opts_merge,
            Some(&storage_cfg),
        )
        .await
        .unwrap();
        assert_eq!(again.total_runs, 4, "a run is recorded once");

        // 7. Verify DashboardExecutionResult derived traits
        assert_eq!(res_merged, res_merged.clone());
        let _ = format!("{res_merged:?}");

        // 8. Remote history with invalid UTF-8 returns error
        adapter
            .put_object("history.json", bytes::Bytes::from_static(b"\xFF\xFF"), None)
            .await
            .unwrap();
        let err_utf8 = DashboardCompiler::execute(
            &paths,
            &ctx,
            &passing_run("r4"),
            &opts_merge,
            Some(&storage_cfg),
        )
        .await;
        assert!(matches!(err_utf8, Err(DashboardError::Io(_))));

        // 9. Local history with corrupt JSON returns error
        std::fs::write(paths.history_file(), b"corrupted local json").unwrap();
        let err_local = DashboardCompiler::execute(
            &paths,
            &ctx,
            &passing_run("r4"),
            &DashboardOptions::default(),
            None,
        )
        .await;
        assert!(matches!(err_local, Err(DashboardError::Json(_))));
        std::fs::remove_file(paths.history_file()).unwrap();

        // 10. Storage upload failure returns DashboardError::Storage
        let read_only_dir = temp.path().join("read_only_remote");
        std::fs::create_dir_all(&read_only_dir).unwrap();
        let bad_storage_cfg = StorageConfig::new(format!("file://{}", read_only_dir.display()));
        let dash_dir = read_only_dir.join("dashboard.html");
        std::fs::create_dir_all(&dash_dir).unwrap();
        let err_upload = DashboardCompiler::execute(
            &paths,
            &ctx,
            &passing_run("r4"),
            &DashboardOptions {
                push_to_storage: true,
                ..Default::default()
            },
            Some(&bad_storage_cfg),
        )
        .await;
        assert!(matches!(err_upload, Err(DashboardError::Storage(_))));
    }

    #[test]
    fn test_from_cases_every_outcome_and_error_display() {
        let cases = Cases::new("runs/latest", every_outcome());
        let entry = RunHistoryEntry::from_cases(
            "run-variants",
            Utc::now(),
            "main",
            "macos-aarch64",
            Some("abcdef123456".to_string()),
            &cases,
        );

        assert_eq!(entry.summary.total, 7);
        assert_eq!(entry.summary.failed, 4);
        assert_eq!(entry.failures.len(), 4, "passing tests are only counted");
        assert_eq!(
            entry.failures[0].outcome,
            CaseOutcome::Mismatch,
            "most telling first"
        );

        // A run that fails everything keeps the most telling failures only.
        let many: Vec<_> = (0..MAX_FAILURES_PER_RUN + 5)
            .map(|i| report(&format!("m/{i:03}"), CaseOutcome::Missing))
            .chain([report("z/mismatch", CaseOutcome::Mismatch)])
            .collect();
        let capped = RunHistoryEntry::from_cases(
            "run-many",
            Utc::now(),
            "main",
            "macos-aarch64",
            None,
            &Cases::new("runs/latest", many),
        );
        assert_eq!(capped.summary.failed, MAX_FAILURES_PER_RUN + 6);
        assert_eq!(capped.failures.len(), MAX_FAILURES_PER_RUN);
        assert_eq!(capped.failures[0].name, "z/mismatch");
        let html = DashboardCompiler::compile_dashboard(&DashboardHistory {
            runs: vec![capped],
            ..DashboardHistory::new()
        })
        .unwrap();
        assert!(html.contains("6 more failures not kept in the history"));
        let error = entry
            .failures
            .iter()
            .find(|t| t.outcome == CaseOutcome::Error)
            .unwrap();
        assert_eq!(error.error_kind, Some(CaseErrorKind::Image));
        assert_eq!(error.message.as_deref(), Some("candidate image: corrupt"));
        let mismatch = entry
            .failures
            .iter()
            .find(|t| t.outcome == CaseOutcome::Mismatch)
            .unwrap();
        assert!(matches!(
            mismatch.metrics,
            Some(Metrics::Pixel { diff_pixels: 5, .. })
        ));

        // Typed entries survive a round trip through history.json.
        let mut history = DashboardHistory::new();
        history.append_run(entry, None);
        let json = serde_json::to_string(&history).unwrap();
        assert_eq!(
            DashboardHistory::parse_or_empty(&json, "test").unwrap(),
            history
        );
        let html = DashboardCompiler::compile_dashboard(&history).unwrap();
        assert!(html.contains("dimension_mismatch"));
        assert!(html.contains("error (image)"));
        assert!(html.contains("Diff pixels: 5"));

        // Test error displays and From implementations
        let io_err = crate::io::IoError::Io(std::io::Error::other("io err"));
        let dash_io: DashboardError = io_err.into();
        assert!(format!("{dash_io}").contains("io err"));

        let parse_err =
            crate::io::IoError::JsonParse(serde_json::from_str::<String>("bad").unwrap_err());
        let dash_parse: DashboardError = parse_err.into();
        assert!(format!("{dash_parse}").contains("JSON error"));

        let unsupported_err = DashboardError::UnsupportedSchemaVersion {
            found: 10,
            supported: 2,
            location: "'h.json'".to_owned(),
        };
        assert!(
            format!("{unsupported_err}")
                .starts_with("Unsupported schema version 10 of the history 'h.json'")
        );

        let not_cfg = DashboardError::StorageNotConfigured;
        assert!(format!("{not_cfg}").contains("GLEON_STORAGE_URL"));

        let stor_err = DashboardError::Storage(StorageError::PreconditionFailed {
            path: "history.json".to_string(),
            source: object_store::Error::AlreadyExists {
                path: "history.json".to_string(),
                source: "already exists".into(),
            },
        });
        assert!(format!("{stor_err}").contains("Storage error"));
    }

    #[test]
    fn test_compile_dashboard_empty_runs_and_zero_tests() {
        let history = DashboardHistory::new();
        let html = DashboardCompiler::compile_dashboard(&history).unwrap();
        assert!(html.contains("No historical test runs recorded"));

        let mut history_with_empty_run = DashboardHistory::new();
        history_with_empty_run.append_run(
            RunHistoryEntry {
                id: "run-empty".to_string(),
                timestamp: Utc::now(),
                branch: "main".to_string(),
                platform: "macos-aarch64".to_string(),
                commit_sha: None,
                summary: RunSummary::default(),
                failures: vec![],
            },
            None,
        );
        let html_empty = DashboardCompiler::compile_dashboard(&history_with_empty_run).unwrap();
        assert!(html_empty.contains("—")); // Pass rates are None, rendered as dash
    }

    #[tokio::test]
    async fn test_upload_history_and_dashboard_precondition_failures() {
        let cfg = StorageConfig::new("memory://");
        let adapter = ObjectStoreAdapter::from_config(&cfg).unwrap();

        // 1. Initial successful upload with create_only=true
        let res = upload_history_and_dashboard(
            &adapter,
            b"{}".to_vec(),
            b"<html></html>".to_vec(),
            None,
            None,
            true,
            None,
            None,
            true,
        )
        .await;
        assert!(res.is_ok());

        // 2. Second upload with create_only=true fails with PreconditionFailed (already exists)
        let res_already_exists = upload_history_and_dashboard(
            &adapter,
            b"{}".to_vec(),
            b"<html></html>".to_vec(),
            None,
            None,
            true,
            None,
            None,
            true,
        )
        .await;
        assert!(matches!(
            res_already_exists,
            Err(StorageError::PreconditionFailed { .. })
        ));

        // 3. Upload with mismatched ETag fails with PreconditionFailed
        let res_bad_etag = upload_history_and_dashboard(
            &adapter,
            b"{}".to_vec(),
            b"<html></html>".to_vec(),
            Some("bad_etag"),
            None,
            false,
            Some("bad_etag"),
            None,
            false,
        )
        .await;
        assert!(matches!(
            res_bad_etag,
            Err(StorageError::PreconditionFailed { .. })
        ));

        // 4. A stale history (mismatched etag) publishes nothing: the page is never ahead of it
        let res_stale_history = upload_history_and_dashboard(
            &adapter,
            b"{}".to_vec(),
            b"<html>newer</html>".to_vec(),
            Some("mismatched_history_etag"),
            None,
            false,
            None,
            None,
            false,
        )
        .await;
        assert!(matches!(
            res_stale_history,
            Err(StorageError::PreconditionFailed { .. })
        ));
        let page = adapter.get_object("dashboard.html").await.unwrap().unwrap();
        assert_eq!(&page.bytes[..], b"<html></html>");

        // 5. The history goes first: a failing page upload leaves the new history in place
        let history = adapter.get_object("history.json").await.unwrap().unwrap();
        let res_page_fail = upload_history_and_dashboard(
            &adapter,
            b"{\"runs\":[]}".to_vec(),
            b"<html>newer</html>".to_vec(),
            history.e_tag.as_deref(),
            None,
            false,
            Some("mismatched_dashboard_etag"),
            None,
            false,
        )
        .await;
        assert!(matches!(
            res_page_fail,
            Err(StorageError::PreconditionFailed { .. })
        ));
        let history = adapter.get_object("history.json").await.unwrap().unwrap();
        assert_eq!(&history.bytes[..], b"{\"runs\":[]}");
    }

    #[tokio::test(flavor = "multi_thread")]
    #[cfg(not(miri))]
    async fn test_dashboard_compiler_retry_on_concurrent_modification() {
        let temp = tempfile::tempdir().unwrap();
        let base_dir = temp.path();
        let paths = GleonPaths::new(base_dir);

        let remote_store_dir = base_dir.join("remote_store_retry");
        std::fs::create_dir_all(&remote_store_dir).unwrap();
        let storage_cfg = StorageConfig::new(format!("file://{}", remote_store_dir.display()));
        let adapter = ObjectStoreAdapter::from_config(&storage_cfg).unwrap();

        let history_path = paths.history_file();
        let target_html_path = paths.dashboard_file();
        let base_history = DashboardHistory::new();
        let options = DashboardOptions {
            push_to_storage: true,
            ..Default::default()
        };

        let remote_dash = remote_store_dir.join("dashboard.html");
        let mut collision_injected = false;

        let res = push_or_save_history_with_hook(
            &paths,
            &options,
            Some(&adapter),
            &base_history,
            &history_path,
            &target_html_path,
            |attempt| {
                if attempt == 1 {
                    use std::io::Write as _;
                    let mut file = std::fs::File::create_new(&remote_dash)
                        .expect("collision file must be created successfully on attempt 1");
                    file.write_all(b"<html>concurrent</html>")
                        .expect("writing collision file should succeed");
                    collision_injected = true;
                }
            },
        )
        .await;

        assert!(
            collision_injected,
            "collision injection must occur before attempt 1 upload"
        );
        let (total_runs, pushed) = res.expect("push_or_save_history should succeed after retry");
        assert!(pushed);
        assert_eq!(total_runs, 0);

        let remote_html = adapter
            .get_object("dashboard.html")
            .await
            .unwrap()
            .expect("remote dashboard.html must be present in storage");
        let html_text = std::str::from_utf8(&remote_html.bytes).unwrap();
        assert_ne!(html_text, "<html>concurrent</html>");
        assert!(html_text.contains("Gleon History Dashboard"));
    }
}
