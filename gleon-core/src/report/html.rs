//! HTML report generation.

use gleon_engine::MismatchDetail;
use minijinja::context;
use serde::{Serialize, Serializer, ser::SerializeSeq};

use super::{ReportError, format::FormattedPath};
use crate::results::{TestCaseResult, TestImageResult};

struct FormattedDimensions(u32, u32);

impl std::fmt::Display for FormattedDimensions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}", self.0, self.1)
    }
}

impl Serialize for FormattedDimensions {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

struct HtmlMismatchMessageView<'a>(&'a MismatchDetail);

impl std::fmt::Display for HtmlMismatchMessageView<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Visual mismatch ({})", self.0)
    }
}

/// The `error` cell of a failure row: either a plain borrowed message, a static literal, or a
/// [`MismatchDetail`] rendered lazily — kept as an enum (instead of an eagerly-formatted
/// `String`) so the common non-`Mismatch` cases stay allocation-free.
enum HtmlFailureError<'a> {
    Str(&'a str),
    Static(&'static str),
    Mismatch(&'a MismatchDetail),
}

impl Serialize for HtmlFailureError<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Str(s) | Self::Static(s) => serializer.serialize_str(s),
            Self::Mismatch(detail) => serializer.collect_str(&HtmlMismatchMessageView(detail)),
        }
    }
}

/// Flat, `derive`d view of a single failed test case for the HTML report template. Built once
/// per failure by [`html_failure_dto`] instead of a hand-written `Serialize` impl matching on
/// `TestImageResult` and emitting each field procedurally.
#[derive(Serialize)]
struct HtmlFailureDto<'a> {
    name: &'a str,
    image: FormattedPath<'a>,
    #[serde(rename = "type")]
    kind: &'static str,
    error: HtmlFailureError<'a>,
    actual_path: Option<FormattedPath<'a>>,
    baseline_path: Option<FormattedPath<'a>>,
    diff_path: Option<FormattedPath<'a>>,
    diff_count: Option<u64>,
    actual_size: Option<FormattedDimensions>,
    baseline_size: Option<FormattedDimensions>,
}

/// A [`HtmlFailureDto`] with every optional field defaulted to `None`, for variants that only
/// populate `image`/`type`/`error`.
const fn bare_failure<'a>(
    name: &'a str,
    relative_path: &'a std::path::Path,
    kind: &'static str,
    error: HtmlFailureError<'a>,
) -> HtmlFailureDto<'a> {
    HtmlFailureDto {
        name,
        image: FormattedPath {
            path: relative_path,
            report_dir: None,
        },
        kind,
        error,
        actual_path: None,
        baseline_path: None,
        diff_path: None,
        diff_count: None,
        actual_size: None,
        baseline_size: None,
    }
}

fn html_failure_dto<'a>(
    tc_name: &'a str,
    res: &'a TestImageResult,
    report_dir: Option<&'a std::path::Path>,
) -> HtmlFailureDto<'a> {
    let with_report_dir = |path: &'a std::path::Path| FormattedPath { path, report_dir };

    match res {
        TestImageResult::Success { .. } => unreachable!(),
        TestImageResult::DecodeError {
            relative_path,
            error,
        } => bare_failure(
            tc_name,
            relative_path,
            "DecodeError",
            HtmlFailureError::Str(error),
        ),
        TestImageResult::IoError {
            relative_path,
            error,
        } => bare_failure(
            tc_name,
            relative_path,
            "IoError",
            HtmlFailureError::Str(error),
        ),
        TestImageResult::EncodeError {
            relative_path,
            actual_path,
            error,
        } => HtmlFailureDto {
            actual_path: Some(with_report_dir(actual_path)),
            ..bare_failure(
                tc_name,
                relative_path,
                "EncodeError",
                HtmlFailureError::Str(error),
            )
        },
        TestImageResult::MissingBaseline {
            relative_path,
            reason,
        } => bare_failure(
            tc_name,
            relative_path,
            "MissingBaseline",
            HtmlFailureError::Str(reason),
        ),
        TestImageResult::DimensionMismatch {
            relative_path,
            baseline_size,
            actual_size,
            baseline_path,
            actual_path,
        } => HtmlFailureDto {
            actual_path: Some(with_report_dir(actual_path)),
            baseline_path: Some(with_report_dir(baseline_path)),
            actual_size: Some(FormattedDimensions(actual_size.0, actual_size.1)),
            baseline_size: Some(FormattedDimensions(baseline_size.0, baseline_size.1)),
            ..bare_failure(
                tc_name,
                relative_path,
                "DimensionMismatch",
                HtmlFailureError::Static("Dimension mismatch"),
            )
        },
        TestImageResult::Mismatch {
            relative_path,
            detail,
            diff_path,
            baseline_path,
            actual_path,
        } => {
            let diff_count = match detail {
                MismatchDetail::Pixel { diff_count } => Some(*diff_count),
                MismatchDetail::Ssim { .. } => None,
            };
            HtmlFailureDto {
                actual_path: Some(with_report_dir(actual_path)),
                baseline_path: Some(with_report_dir(baseline_path)),
                diff_path: Some(with_report_dir(diff_path)),
                diff_count,
                ..bare_failure(
                    tc_name,
                    relative_path,
                    "Mismatch",
                    HtmlFailureError::Mismatch(detail),
                )
            }
        }
    }
}

struct HtmlReportFailuresView<'a> {
    test_cases: &'a [TestCaseResult],
    report_dir: Option<&'a std::path::Path>,
}

impl Serialize for HtmlReportFailuresView<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = serializer.serialize_seq(None)?;
        for tc in self.test_cases {
            let res = &tc.result;
            if !matches!(res, TestImageResult::Success { .. }) {
                seq.serialize_element(&html_failure_dto(&tc.name, res, self.report_dir))?;
            }
        }
        seq.end()
    }
}

impl super::ReportGenerator {
    /// Generates a single self-contained HTML report string linking images via relative paths.
    /// Skips generation entirely if 100% of tests passed by returning None.
    ///
    /// # Panics
    ///
    /// Panics if the bundled template cannot be retrieved (impossible in normal builds).
    ///
    /// # Errors
    ///
    /// Returns [`ReportError::Render`] if template rendering fails.
    pub fn generate_html(
        test_cases: &[TestCaseResult],
        report_dir: Option<&std::path::Path>,
    ) -> Result<Option<String>, ReportError> {
        let total_tests = test_cases.len();
        let failed_tests = test_cases.iter().filter(|tc| !tc.passed()).count();

        if failed_tests == 0 {
            return Ok(None);
        }

        #[expect(
            clippy::expect_used,
            reason = "bundled templates are compile-time assets validated by the test suite"
        )]
        let tmpl = super::JINJA_ENV
            .get_template("report.html")
            .expect("bundled report.html template is registered");

        // Resolved once here rather than inside `make_relative_path`: image paths recorded on
        // disk are absolute while `--out report.html` hands us a relative (often empty)
        // `report_dir`, and every failing test's `actual`/`baseline`/`diff` path would otherwise
        // each pay for their own `current_dir()` syscall during rendering.
        let absolute_report_dir = report_dir.map(super::format::to_absolute);
        let report_dir = absolute_report_dir.as_deref();

        let ctx = context! {
            total_tests => total_tests,
            failed_tests => failed_tests,
            failures => HtmlReportFailuresView { test_cases, report_dir },
        };

        tmpl.render(ctx).map(Some).map_err(|e| ReportError::Render {
            template: "report.html",
            source: e,
        })
    }
}

#[cfg(test)]
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
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::report::ReportGenerator;

    #[test]
    fn test_generate_html_skips_on_success() {
        let tc = TestCaseResult {
            name: "billing".to_string(),
            result: TestImageResult::Success {
                relative_path: PathBuf::from("form.png"),
            },
        };
        assert!(
            ReportGenerator::generate_html(&[tc], None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn test_generate_html_on_failure() {
        let tc = TestCaseResult {
            name: "billing".to_string(),
            result: TestImageResult::Mismatch {
                relative_path: PathBuf::from("form.png"),
                detail: MismatchDetail::Pixel { diff_count: 5 },
                diff_path: PathBuf::from(".gleon/diffs/diff.png"),
                baseline_path: PathBuf::from("baseline.png"),
                actual_path: PathBuf::from(".gleon/actual/actual.png"),
            },
        };
        let report_dir = PathBuf::from(".gleon/reports");
        let html = ReportGenerator::generate_html(&[tc], Some(&report_dir))
            .expect("Render should succeed")
            .expect("Expected HTML output");
        assert!(html.contains("..&#x2f;actual&#x2f;actual.png"));
        assert!(html.contains("Visual mismatch (5 pixels)"));
    }

    #[test]
    fn test_generate_html_relativizes_absolute_paths_against_a_relative_report_dir() {
        // `gleon report html --out report.html` yields a report_dir of "" (relative) while the
        // recorded image paths are absolute. Returning the absolute path verbatim embeds
        // `file:///Users/...` links that break the moment the artifact leaves the runner.
        let tc = TestCaseResult {
            name: "billing".to_string(),
            result: TestImageResult::Mismatch {
                relative_path: PathBuf::from("form.png"),
                detail: MismatchDetail::Pixel { diff_count: 5 },
                diff_path: std::env::current_dir().unwrap().join(".gleon/diffs/d.png"),
                baseline_path: PathBuf::from("baseline.png"),
                actual_path: std::env::current_dir().unwrap().join(".gleon/actual/a.png"),
            },
        };

        let html = ReportGenerator::generate_html(&[tc], Some(Path::new("")))
            .unwrap()
            .unwrap();

        let cwd = std::env::current_dir().unwrap();
        let cwd_str = cwd.to_string_lossy().replace('/', "&#x2f;");
        assert!(
            !html.contains(&cwd_str),
            "absolute paths must be relativized against the report dir"
        );
        assert!(html.contains(".gleon&#x2f;actual&#x2f;a.png"));
    }

    #[test]
    fn test_generate_html_empty_list() {
        let html_res = ReportGenerator::generate_html(&[], None).unwrap();
        assert!(html_res.is_none());
    }

    #[test]
    fn test_generate_html_all_variants() {
        let tests = vec![
            TestCaseResult {
                name: "dim_mismatch".to_string(),
                result: TestImageResult::DimensionMismatch {
                    relative_path: PathBuf::from("rel.png"),
                    actual_path: PathBuf::from("actual.png"),
                    baseline_path: PathBuf::from("baseline.png"),
                    actual_size: (10, 10),
                    baseline_size: (20, 20),
                },
            },
            TestCaseResult {
                name: "missing".to_string(),
                result: TestImageResult::MissingBaseline {
                    relative_path: PathBuf::from("rel.png"),
                    reason: "missing baseline".to_string(),
                },
            },
            TestCaseResult {
                name: "decode".to_string(),
                result: TestImageResult::DecodeError {
                    relative_path: PathBuf::from("rel.png"),
                    error: "corrupt".to_string(),
                },
            },
            TestCaseResult {
                name: "io".to_string(),
                result: TestImageResult::IoError {
                    relative_path: PathBuf::from("rel.png"),
                    error: "disk error".to_string(),
                },
            },
            TestCaseResult {
                name: "encode".to_string(),
                result: TestImageResult::EncodeError {
                    relative_path: PathBuf::from("rel.png"),
                    actual_path: PathBuf::from("actual.png"),
                    error: "encode fail".to_string(),
                },
            },
            TestCaseResult {
                name: "ssim_mismatch".to_string(),
                result: TestImageResult::Mismatch {
                    relative_path: PathBuf::from("rel.png"),
                    actual_path: PathBuf::from("actual.png"),
                    baseline_path: PathBuf::from("baseline.png"),
                    diff_path: PathBuf::from("diff.png"),
                    detail: MismatchDetail::Ssim {
                        ssim_score: 0.5,
                        min_ssim: 0.5,
                        max_excess: 0.0,
                        region: None,
                    },
                },
            },
            TestCaseResult {
                name: "pass".to_string(),
                result: TestImageResult::Success {
                    relative_path: PathBuf::from("rel.png"),
                },
            },
        ];

        let html = ReportGenerator::generate_html(&tests, None)
            .unwrap()
            .unwrap();
        assert!(html.contains("dim_mismatch"));
        assert!(html.contains("missing"));
        assert!(html.contains("decode"));
        assert!(html.contains("io"));
        assert!(html.contains("encode"));
        assert!(html.contains("Dimension mismatch"));
        assert!(html.contains("Visual mismatch (min local SSIM 0.5000)"));
        assert!(!html.contains("pass"));
    }
}
