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
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use tracing::instrument;

use crate::{
    context::ResolvedContext,
    paths::GleonPaths,
    results::{TestCaseResult, TestImageResult},
    storage::{ObjectStoreAdapter, StorageConfig, StorageError},
};

/// Current supported schema version for `history.json`.
pub const SUPPORTED_SCHEMA_VERSION: u32 = 1;

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

    /// Platform key resolution error.
    #[error("Platform error: {0}")]
    Platform(#[from] crate::platform::PlatformError),

    /// Remote storage was not configured when push was requested.
    #[error("Storage not configured: GLEON_STORAGE_URL is required when --push is enabled")]
    StorageNotConfigured,

    /// Error reading the input report JSON file.
    #[error("Failed to read report JSON from '{path}': {source}")]
    ReportLoad {
        /// Path to the unreadable report file.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Error parsing the input report JSON file.
    #[error("Failed to parse report JSON from '{path}': {source}")]
    ReportParse {
        /// Path to the unparseable report file.
        path: PathBuf,
        /// Underlying JSON parsing error.
        #[source]
        source: serde_json::Error,
    },

    /// The history schema version is newer than supported by this version of Gleon.
    #[error("Unsupported history schema version {found}, maximum supported is {supported}")]
    UnsupportedSchemaVersion {
        /// The version encountered in `history.json`.
        found: u32,
        /// Maximum version supported by this binary.
        supported: u32,
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

/// Aggregated summary counters for a test run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RunSummary {
    /// Total test cases in this run.
    pub total: usize,
    /// Number of test cases that passed.
    pub passed: usize,
    /// Number of test cases that failed.
    pub failed: usize,
}

/// Typed status of an individual test case in historical logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum TestHistoryStatus {
    /// Comparison matched baseline within tolerance.
    Success,
    /// Visual difference exceeded tolerance.
    Mismatch,
    /// Dimensions differed between actual and baseline.
    DimensionMismatch,
    /// Failed to decode image.
    DecodeError,
    /// Baseline image was missing.
    MissingBaseline,
    /// File I/O error occurred.
    IoError,
    /// Failed to encode image.
    EncodeError,
}

impl std::fmt::Display for TestHistoryStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

/// Recorded outcome of an individual test case within a historical run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestHistoryEntry {
    /// Name / path of the test case.
    pub name: String,
    /// Status description.
    pub status: TestHistoryStatus,
    /// Optional error details or failure reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Number of mismatched pixels if applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_count: Option<u64>,
}

/// A historical record representing a single visual regression test run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunHistoryEntry {
    /// Unique identifier for this run.
    pub id: String,
    /// Timestamp when the run occurred.
    pub timestamp: DateTime<Utc>,
    /// Git branch context.
    pub branch: String,
    /// Resolved platform key (e.g. `macos-aarch64`).
    pub platform: String,
    /// Optional Git commit SHA.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_sha: Option<String>,
    /// Aggregated test summary.
    pub summary: RunSummary,
    /// Outcomes of individual test cases evaluated in this run.
    pub tests: Vec<TestHistoryEntry>,
}

impl RunHistoryEntry {
    /// Constructs a `RunHistoryEntry` from a list of [`TestCaseResult`]s and run metadata.
    #[must_use]
    pub fn from_test_results(
        id: impl Into<String>,
        timestamp: DateTime<Utc>,
        branch: impl Into<String>,
        platform: impl Into<String>,
        commit_sha: Option<String>,
        test_cases: &[TestCaseResult],
    ) -> Self {
        let total = test_cases.len();
        let passed = test_cases.iter().filter(|tc| tc.passed()).count();
        let failed = total.saturating_sub(passed);

        let tests = test_cases
            .iter()
            .map(|tc| {
                let (status, error, diff_count) = match &tc.result {
                    TestImageResult::Success { .. } => (TestHistoryStatus::Success, None, None),
                    TestImageResult::Mismatch { detail, .. } => match detail {
                        gleon_engine::MismatchDetail::Pixel { diff_count } => {
                            (TestHistoryStatus::Mismatch, None, Some(*diff_count))
                        }
                        detail @ gleon_engine::MismatchDetail::Ssim { .. } => {
                            (TestHistoryStatus::Mismatch, Some(detail.to_string()), None)
                        }
                    },
                    TestImageResult::DimensionMismatch {
                        baseline_size,
                        actual_size,
                        ..
                    } => (
                        TestHistoryStatus::DimensionMismatch,
                        Some(format!(
                            "Expected {}x{}, got {}x{}",
                            baseline_size.0, baseline_size.1, actual_size.0, actual_size.1
                        )),
                        None,
                    ),
                    TestImageResult::DecodeError { error, .. } => {
                        (TestHistoryStatus::DecodeError, Some(error.clone()), None)
                    }
                    TestImageResult::MissingBaseline { reason, .. } => (
                        TestHistoryStatus::MissingBaseline,
                        Some(reason.clone()),
                        None,
                    ),
                    TestImageResult::IoError { error, .. } => {
                        (TestHistoryStatus::IoError, Some(error.clone()), None)
                    }
                    TestImageResult::EncodeError { error, .. } => {
                        (TestHistoryStatus::EncodeError, Some(error.clone()), None)
                    }
                };

                TestHistoryEntry {
                    name: tc.name.clone(),
                    status,
                    error,
                    diff_count,
                }
            })
            .collect();

        Self {
            id: id.into(),
            timestamp,
            branch: branch.into(),
            platform: platform.into(),
            commit_sha,
            summary: RunSummary {
                total,
                passed,
                failed,
            },
            tests,
        }
    }
}

/// The root schema of `history.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

    /// Validates that the schema version does not exceed [`SUPPORTED_SCHEMA_VERSION`].
    ///
    /// # Errors
    /// Returns [`DashboardError::UnsupportedSchemaVersion`] if `schema_version` is too new.
    pub const fn validate_schema(&self) -> Result<(), DashboardError> {
        if self.schema_version > SUPPORTED_SCHEMA_VERSION {
            return Err(DashboardError::UnsupportedSchemaVersion {
                found: self.schema_version,
                supported: SUPPORTED_SCHEMA_VERSION,
            });
        }
        Ok(())
    }

    /// Parses `history.json` content from a string, or initializes an empty history if empty.
    ///
    /// # Errors
    /// Returns [`DashboardError::Json`] or [`DashboardError::UnsupportedSchemaVersion`].
    pub fn parse_or_empty(raw: &str) -> Result<Self, DashboardError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(Self::new());
        }
        let history: Self = serde_json::from_str(trimmed)?;
        history.validate_schema()?;
        Ok(history)
    }

    /// Appends a new run entry to the history log.
    ///
    /// If `truncate_limit` is `Some(limit)` and the number of runs exceeds `limit`,
    /// the oldest runs are dropped from the beginning.
    pub fn append_run(&mut self, entry: RunHistoryEntry, truncate_limit: Option<NonZeroUsize>) {
        self.runs.push(entry);

        if let Some(limit) = truncate_limit
            && self.runs.len() > limit.get()
        {
            let excess = self.runs.len() - limit.get();
            let _ = self.runs.drain(0..excess);
        }
    }

    /// Merges another [`DashboardHistory`] into this one, deduplicating runs by `id`
    /// and sorting all runs chronologically by `timestamp`.
    ///
    /// If `truncate_limit` is `Some(limit)` and the merged collection exceeds `limit`,
    /// the oldest runs are dropped from the beginning.
    pub fn merge(&mut self, mut other: Self, truncate_limit: Option<NonZeroUsize>) {
        self.runs.append(&mut other.runs);

        // Sort by id first so all runs with the same id are guaranteed to be contiguous
        self.runs.sort_by(|a, b| a.id.cmp(&b.id));

        // Deduplicate runs by id (guaranteed contiguous)
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
        let passed_tests: usize = history.runs.iter().map(|r| r.summary.passed).sum();
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
    #[instrument(skip(context, storage_cfg), level = "debug")]
    pub async fn execute(
        paths: &GleonPaths,
        context: &ResolvedContext,
        report_path: &Path,
        options: &DashboardOptions<'_>,
        storage_cfg: Option<&StorageConfig>,
    ) -> Result<DashboardExecutionResult, DashboardError> {
        let test_cases = load_report_test_cases(report_path)?;

        let adapter = match storage_cfg {
            Some(cfg) => Some(ObjectStoreAdapter::from_config(cfg)?),
            None if options.push_to_storage => return Err(DashboardError::StorageNotConfigured),
            None => None,
        };

        let now = Utc::now();
        let platform_key = context.platform.to_key()?;
        let run_id = generate_run_id(now, &context.branch, &platform_key);
        let history_path = paths.history_file();
        let target_html_path = options
            .out_html
            .map_or_else(|| paths.dashboard_file(), Path::to_path_buf);

        // Load local history ONCE and append the current run.
        let mut base_history = load_local_history_or_default(paths)?;
        let run_entry = RunHistoryEntry::from_test_results(
            &run_id,
            now,
            &context.branch,
            &platform_key,
            context.commit_sha.clone(),
            &test_cases,
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
                let remote_history = DashboardHistory::parse_or_empty(text)?;
                history.merge(remote_history, options.truncate_limit);
            } else {
                history_create_only = true;
            }

            if let Some(remote_html) = ad.get_object("dashboard.html").await? {
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

fn load_report_test_cases(report_path: &Path) -> Result<Vec<TestCaseResult>, DashboardError> {
    let report_bytes = std::fs::read(report_path).map_err(|source| DashboardError::ReportLoad {
        path: report_path.to_path_buf(),
        source,
    })?;
    serde_json::from_slice(&report_bytes).map_err(|source| DashboardError::ReportParse {
        path: report_path.to_path_buf(),
        source,
    })
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
    let res_html = adapter
        .put_object_conditional(
            "dashboard.html",
            bytes::Bytes::from(html_content),
            Some("text/html; charset=utf-8"),
            expected_dashboard_etag,
            expected_dashboard_version,
            dashboard_create_only,
        )
        .await;

    match res_html {
        Ok(()) => {
            adapter
                .put_object_conditional(
                    "history.json",
                    bytes::Bytes::from(history_json),
                    Some("application/json"),
                    expected_history_etag,
                    expected_history_version,
                    history_create_only,
                )
                .await
        }
        Err(e) => Err(e),
    }
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
    let local_file = paths.history_file();
    match crate::io::load_json::<DashboardHistory, _>(&local_file) {
        Ok(h) => {
            h.validate_schema()?;
            Ok(h)
        }
        Err(crate::io::IoError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(DashboardHistory::new())
        }
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

    #[test]
    fn test_history_parse_empty_and_valid() {
        // 1. Empty string yields empty DashboardHistory
        let empty = DashboardHistory::parse_or_empty("   ").unwrap();
        assert_eq!(empty.schema_version, 1);
        assert!(empty.runs.is_empty());

        // 2. Valid JSON parses correctly
        let json = r#"{
            "schema_version": 1,
            "runs": [
                {
                    "id": "run-1",
                    "timestamp": "2026-09-13T10:00:00Z",
                    "branch": "main",
                    "platform": "linux-x86_64",
                    "summary": { "total": 2, "passed": 2, "failed": 0 },
                    "tests": []
                }
            ]
        }"#;
        let parsed = DashboardHistory::parse_or_empty(json).unwrap();
        assert_eq!(parsed.runs.len(), 1);
        assert_eq!(parsed.runs[0].id, "run-1");
        assert_eq!(parsed.runs[0].branch, "main");
        assert_eq!(parsed.runs[0].summary.passed, 2);

        // 3. Corrupt JSON returns error
        assert!(DashboardHistory::parse_or_empty("not json").is_err());
    }

    #[test]
    fn test_history_schema_version_validation() {
        let bad_json = r#"{
            "schema_version": 99,
            "runs": []
        }"#;
        let res = DashboardHistory::parse_or_empty(bad_json);
        assert!(matches!(
            res,
            Err(DashboardError::UnsupportedSchemaVersion {
                found: 99,
                supported: 1
            })
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
                    passed: 1,
                    failed: 0,
                },
                tests: vec![],
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
                    passed: 1,
                    failed: 0,
                },
                tests: vec![],
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
                passed: 1,
                failed: 0,
            },
            tests: vec![],
        };
        let run_2_local = RunHistoryEntry {
            id: "run-2".to_string(),
            timestamp: t2,
            branch: "feature/local".to_string(),
            platform: "macos-aarch64".to_string(),
            commit_sha: None,
            summary: RunSummary {
                total: 1,
                passed: 1,
                failed: 0,
            },
            tests: vec![],
        };
        let run_3_remote = RunHistoryEntry {
            id: "run-3".to_string(),
            timestamp: t3,
            branch: "feature/remote".to_string(),
            platform: "windows-x86_64".to_string(),
            commit_sha: None,
            summary: RunSummary {
                total: 1,
                passed: 1,
                failed: 0,
            },
            tests: vec![],
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
            tests: vec![],
        };
        let run_middle = RunHistoryEntry {
            id: "run-middle".to_string(),
            timestamp: t2,
            branch: "main".to_string(),
            platform: "linux-x86_64".to_string(),
            commit_sha: None,
            summary: RunSummary::default(),
            tests: vec![],
        };
        let run_dup_2 = RunHistoryEntry {
            id: "run-dup".to_string(),
            timestamp: t3,
            branch: "main".to_string(),
            platform: "linux-x86_64".to_string(),
            commit_sha: None,
            summary: RunSummary::default(),
            tests: vec![],
        };

        local.append_run(run_dup_1, None);
        local.append_run(run_middle, None);
        remote.append_run(run_dup_2, None);

        local.merge(remote, None);

        // Must strictly deduplicate runs with the same id regardless of timestamps
        assert_eq!(local.runs.len(), 2, "Duplicate run ID must be removed");
        assert_eq!(local.runs[0].id, "run-dup");
        assert_eq!(local.runs[1].id, "run-middle");
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
                tests: vec![],
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
                tests: vec![],
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
                passed: 2,
                failed: 0,
            },
            tests: vec![TestHistoryEntry {
                name: "auth/login".to_string(),
                status: TestHistoryStatus::Success,
                error: None,
                diff_count: None,
            }],
        };
        let entry2 = RunHistoryEntry {
            id: "run-2".to_string(),
            timestamp: t2,
            branch: "feature/cart".to_string(),
            platform: "linux-x86_64".to_string(),
            commit_sha: None,
            summary: RunSummary {
                total: 2,
                passed: 1,
                failed: 1,
            },
            tests: vec![
                TestHistoryEntry {
                    name: "cart/checkout".to_string(),
                    status: TestHistoryStatus::Mismatch,
                    error: None,
                    diff_count: Some(42),
                },
                TestHistoryEntry {
                    name: "cart/item".to_string(),
                    status: TestHistoryStatus::Success,
                    error: None,
                    diff_count: None,
                },
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
                passed: 0,
                failed: 1,
            },
            tests: vec![TestHistoryEntry {
                name: "<img src=x onerror=alert('xss')>".to_string(),
                status: TestHistoryStatus::Mismatch,
                error: Some("<b>bold error</b>".to_string()),
                diff_count: Some(1),
            }],
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
                passed: 0,
                failed: 1,
            },
            tests: vec![TestHistoryEntry {
                name: "profile/header".to_string(),
                status: TestHistoryStatus::Mismatch,
                error: Some("Image dimension mismatch, falling back to pixel diff".to_string()),
                diff_count: Some(99),
            }],
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
                    passed: 1,
                    failed: 0,
                },
                tests: vec![],
            };
            history.append_run(entry, None);
        }

        let html = DashboardCompiler::compile_dashboard(&history).unwrap();
        // Total runs is 45, but chart must be capped to MAX_CHART_RUNS (30)
        assert!(html.contains("45")); // Total runs KPI
        assert!(html.contains("Last 30 runs"));
        assert!(!html.contains("Last 45 runs"));
    }

    #[tokio::test]
    #[cfg(not(miri))]
    async fn test_dashboard_compiler_execute_local_and_remote() {
        let temp = tempfile::tempdir().unwrap();
        let base_dir = temp.path();
        let paths = GleonPaths::new(base_dir);

        let report_path = base_dir.join("report.json");
        let test_cases = vec![TestCaseResult {
            name: "home".to_string(),
            result: TestImageResult::Success {
                relative_path: PathBuf::from("home.png"),
            },
        }];
        crate::io::save_json_atomically(&report_path, &test_cases).unwrap();

        let ctx = ResolvedContext {
            base_dir: base_dir.to_path_buf(),
            branch: "feature/dashboard".to_string(),
            ..ResolvedContext::default()
        };

        // 1. First execution in local mode without storage
        let opts_local = DashboardOptions::default();
        let res_local = DashboardCompiler::execute(&paths, &ctx, &report_path, &opts_local, None)
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
        let err_no_storage =
            DashboardCompiler::execute(&paths, &ctx, &report_path, &opts_push_no_storage, None)
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
            &report_path,
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
                    passed: 1,
                    failed: 0,
                },
                tests: vec![],
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
        let res_merged =
            DashboardCompiler::execute(&paths, &ctx, &report_path, &opts_merge, Some(&storage_cfg))
                .await
                .unwrap();
        assert_eq!(res_merged.total_runs, 4); // 2 local + 1 remote external + 1 new run

        // 5. Error case: Missing report path returns ReportLoad error
        let non_existent = base_dir.join("non_existent_report.json");
        let err_missing = DashboardCompiler::execute(
            &paths,
            &ctx,
            &non_existent,
            &DashboardOptions::default(),
            None,
        )
        .await;
        assert!(matches!(
            err_missing,
            Err(DashboardError::ReportLoad { .. })
        ));

        // 6. Error case: Corrupt report JSON returns ReportParse error
        let corrupt_report = base_dir.join("corrupt_report.json");
        std::fs::write(&corrupt_report, b"invalid json").unwrap();
        let err_parse = DashboardCompiler::execute(
            &paths,
            &ctx,
            &corrupt_report,
            &DashboardOptions::default(),
            None,
        )
        .await;
        assert!(matches!(err_parse, Err(DashboardError::ReportParse { .. })));

        // 7. Verify DashboardExecutionResult derived traits
        assert_eq!(res_merged, res_merged.clone());
        let _ = format!("{res_merged:?}");

        // 8. Remote history with invalid UTF-8 returns error
        adapter
            .put_object("history.json", bytes::Bytes::from_static(b"\xFF\xFF"), None)
            .await
            .unwrap();
        let err_utf8 =
            DashboardCompiler::execute(&paths, &ctx, &report_path, &opts_merge, Some(&storage_cfg))
                .await;
        assert!(matches!(err_utf8, Err(DashboardError::Io(_))));

        // 9. Local history with corrupt JSON returns error
        std::fs::write(paths.history_file(), b"corrupted local json").unwrap();
        let err_local = DashboardCompiler::execute(
            &paths,
            &ctx,
            &report_path,
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
            &report_path,
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
    fn test_from_test_results_all_variants_and_error_display() {
        let test_cases = vec![
            TestCaseResult {
                name: "test_success".to_string(),
                result: TestImageResult::Success {
                    relative_path: PathBuf::from("success.png"),
                },
            },
            TestCaseResult {
                name: "test_ssim".to_string(),
                result: TestImageResult::Mismatch {
                    relative_path: PathBuf::from("ssim.png"),
                    detail: gleon_engine::MismatchDetail::Ssim {
                        ssim_score: 0.8542,
                        min_ssim: 0.8542,
                        max_excess: 0.0,
                        region: None,
                    },
                    diff_path: PathBuf::from("diff.png"),
                    baseline_path: PathBuf::from("base.png"),
                    actual_path: PathBuf::from("act.png"),
                },
            },
            TestCaseResult {
                name: "test_decode_error".to_string(),
                result: TestImageResult::DecodeError {
                    relative_path: PathBuf::from("decode.png"),
                    error: "corrupt png".to_string(),
                },
            },
            TestCaseResult {
                name: "test_missing_baseline".to_string(),
                result: TestImageResult::MissingBaseline {
                    relative_path: PathBuf::from("missing.png"),
                    reason: "no baseline staged".to_string(),
                },
            },
            TestCaseResult {
                name: "test_io_error".to_string(),
                result: TestImageResult::IoError {
                    relative_path: PathBuf::from("io.png"),
                    error: "disk failure".to_string(),
                },
            },
            TestCaseResult {
                name: "test_encode_error".to_string(),
                result: TestImageResult::EncodeError {
                    relative_path: PathBuf::from("encode.png"),
                    actual_path: PathBuf::from("act.png"),
                    error: "encoder out of memory".to_string(),
                },
            },
            TestCaseResult {
                name: "test_dimension_mismatch".to_string(),
                result: TestImageResult::DimensionMismatch {
                    relative_path: PathBuf::from("dim.png"),
                    baseline_size: (100, 100),
                    actual_size: (200, 200),
                    baseline_path: PathBuf::from("base.png"),
                    actual_path: PathBuf::from("act.png"),
                },
            },
        ];

        let entry = RunHistoryEntry::from_test_results(
            "run-variants",
            Utc::now(),
            "main",
            "macos-aarch64",
            Some("abcdef123456".to_string()),
            &test_cases,
        );

        assert_eq!(entry.summary.total, 7);
        assert_eq!(entry.summary.passed, 1);
        assert_eq!(entry.summary.failed, 6);
        assert_eq!(entry.tests.len(), 7);

        // Verify status Display implementation
        let statuses = [
            TestHistoryStatus::Success,
            TestHistoryStatus::Mismatch,
            TestHistoryStatus::DimensionMismatch,
            TestHistoryStatus::DecodeError,
            TestHistoryStatus::MissingBaseline,
            TestHistoryStatus::IoError,
            TestHistoryStatus::EncodeError,
        ];
        for s in statuses {
            assert!(!format!("{s}").is_empty());
        }

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
            supported: 1,
        };
        assert!(format!("{unsupported_err}").contains("Unsupported history schema version 10"));

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
                tests: vec![],
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

        // 4. First upload (dashboard.html) succeeds, second (history.json with mismatched etag) fails
        let res_second_fail = upload_history_and_dashboard(
            &adapter,
            b"{}".to_vec(),
            b"<html></html>".to_vec(),
            Some("mismatched_history_etag"),
            None,
            false,
            None,
            None,
            false,
        )
        .await;
        assert!(matches!(
            res_second_fail,
            Err(StorageError::PreconditionFailed { .. })
        ));
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
