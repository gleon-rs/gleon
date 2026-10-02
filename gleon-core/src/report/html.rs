//! HTML report generation.

use gleon_model::case::{CaseErrorKind, CaseReport, Metrics};
use minijinja::context;
use serde::Serialize;

use super::{
    ReportError,
    format::{CaseSummary, image_link},
};
use crate::cases::Cases;

/// Flat view of a single failed case for the HTML report template.
#[derive(Serialize)]
struct HtmlFailureDto<'a> {
    name: &'a str,
    image: &'a str,
    /// The outcome (`mismatch`, `dimension_mismatch`, `missing`, `error`); the template shows the
    /// images a case kept.
    outcome: &'static str,
    /// The class of an error (`image` for undecodable images).
    error_kind: Option<&'static str>,
    error: String,
    actual_path: Option<String>,
    baseline_path: Option<String>,
    diff_path: Option<String>,
    diff_count: Option<u64>,
    actual_size: Option<String>,
    baseline_size: Option<String>,
}

fn size(width: Option<u32>, height: Option<u32>) -> Option<String> {
    Some(format!("{}x{}", width?, height?))
}

fn html_failure_dto<'a>(
    cases: &Cases,
    report: &'a CaseReport,
    report_dir: &std::path::Path,
) -> HtmlFailureDto<'a> {
    let image = |path: Option<&String>| Some(image_link(&cases.artifact_path(path?)?, report_dir));
    let artifacts = report.artifacts.as_ref();
    let actual_path = image(artifacts.and_then(|a| a.candidate.as_ref()));
    let baseline_path = image(artifacts.and_then(|a| a.golden.as_ref()));
    let diff_path = image(artifacts.and_then(|a| a.diff.as_ref()));
    HtmlFailureDto {
        name: &report.name,
        image: &report.golden.path,
        outcome: report.outcome.as_str(),
        error_kind: report.error_kind.map(CaseErrorKind::as_str),
        error: CaseSummary(report).to_string(),
        actual_path,
        baseline_path,
        diff_path,
        diff_count: match report.metrics {
            Some(Metrics::Pixel { diff_pixels, .. }) => Some(diff_pixels),
            _ => None,
        },
        actual_size: size(report.candidate.width, report.candidate.height),
        baseline_size: size(report.golden.width, report.golden.height),
    }
}

impl super::ReportGenerator {
    /// Generates a single self-contained HTML report string of the failed cases, most telling first,
    /// linking their images relative to `report_dir` (where the page goes; `""` for the working
    /// directory). Skips generation entirely if 100% of tests passed by returning None.
    ///
    /// # Panics
    ///
    /// Panics if the bundled template cannot be retrieved (impossible in normal builds).
    ///
    /// # Errors
    ///
    /// Returns [`ReportError::Render`] if template rendering fails.
    pub fn generate_html(
        cases: &Cases,
        report_dir: &std::path::Path,
    ) -> Result<Option<String>, ReportError> {
        let total_tests = cases.reports().len();
        let failed_tests = cases.failures().count();

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

        let failures: Vec<_> = cases
            .failures_by_severity()
            .into_iter()
            .map(|report| html_failure_dto(cases, report, report_dir))
            .collect();

        let ctx = context! {
            total_tests => total_tests,
            failed_tests => failed_tests,
            failures => failures,
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

    use gleon_model::case::CaseOutcome;

    use super::*;
    use crate::{
        cases::fixtures::{every_outcome, report},
        report::ReportGenerator,
    };

    #[test]
    fn test_generate_html_skips_on_success() {
        let cases = Cases::new(
            "runs/latest",
            vec![
                report("a", CaseOutcome::Identical),
                report("b", CaseOutcome::Match),
                report("c", CaseOutcome::Updated),
            ],
        );
        assert!(
            ReportGenerator::generate_html(&cases, Path::new(""))
                .unwrap()
                .is_none()
        );
        let empty = Cases::new("runs/latest", Vec::new());
        assert!(
            ReportGenerator::generate_html(&empty, Path::new(""))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn test_generate_html_links_the_artifacts_relative_to_the_report() {
        let cases = Cases::new(
            "/w/.gleon/runs/latest",
            vec![report("billing/form", CaseOutcome::Mismatch)],
        );
        let html = ReportGenerator::generate_html(&cases, Path::new("/w/.gleon/runs/latest"))
            .unwrap()
            .unwrap();
        assert!(
            html.contains("artifacts&#x2f;billing&#x2f;form&#x2f;candidate.png"),
            "{html}"
        );
        assert!(html.contains("artifacts&#x2f;billing&#x2f;form&#x2f;diff.png"));
        assert!(html.contains("Mismatch: 5.00% (5 of 100px) differ"));
        assert!(html.contains("(5 diffs)"));
        assert!(!html.contains("&#x2f;w&#x2f;"), "paths are relative");
    }

    #[test]
    fn test_generate_html_relativizes_against_a_relative_report_dir() {
        // `gleon report html --out report.html` yields a report_dir of "" (relative) while the
        // run directory may be absolute. Returning the absolute path verbatim embeds
        // `file:///Users/...` links that break the moment the artifact leaves the runner.
        let cwd = std::env::current_dir().unwrap();
        let cases = Cases::new(
            cwd.join(".gleon/runs/latest"),
            vec![report("billing", CaseOutcome::Mismatch)],
        );

        let html = ReportGenerator::generate_html(&cases, Path::new(""))
            .unwrap()
            .unwrap();

        let cwd_str = cwd.to_string_lossy().replace('/', "&#x2f;");
        assert!(
            !html.contains(&cwd_str),
            "absolute paths must be relativized against the report dir"
        );
        assert!(html.contains(".gleon&#x2f;runs&#x2f;latest&#x2f;artifacts&#x2f;billing"));
    }

    #[test]
    fn test_generate_html_every_outcome() {
        let cases = Cases::new(PathBuf::from("/w/.gleon/runs/latest"), every_outcome());
        let html = ReportGenerator::generate_html(&cases, Path::new(""))
            .unwrap()
            .unwrap();
        assert!(html.contains("Failed: 4"));
        assert!(html.contains("Total: 7"));
        assert!(html.contains("Dimension Mismatch: golden is 10x10px, test image is 20x10px"));
        assert!(html.contains("Baseline (10x10)"));
        assert!(html.contains("Error (image): candidate image: corrupt"));
        assert!(html.contains("Failed to decode image file."));
        assert!(html.contains("Missing Baseline: no golden yet"));
        // The candidate of a new golden is shown.
        assert!(html.contains("New screenshot (10x10)"));
        assert!(html.contains("artifacts&#x2f;test&#x2f;missing&#x2f;candidate.png"));
        assert!(!html.contains("test&#x2f;identical"));
        assert!(!html.contains("test&#x2f;updated"));
    }

    #[test]
    fn test_generate_html_without_images_shows_no_comparison() {
        let mut mismatch = report("a", CaseOutcome::Mismatch);
        mismatch.artifacts = None;
        let cases = Cases::new("/w/.gleon/runs/latest", vec![mismatch]);
        let html = ReportGenerator::generate_html(&cases, Path::new(""))
            .unwrap()
            .unwrap();
        assert!(!html.contains("slider-container"), "no images, no slider");
        assert!(html.contains("Mismatch: 5.00%"));
    }

    /// The images a case kept are shown, whatever it lacks: a mismatch without its diff still
    /// compares golden and candidate, a size that is unknown is left out.
    #[test]
    fn test_generate_html_shows_what_a_case_kept() {
        let mut mismatch = report("a", CaseOutcome::Mismatch);
        mismatch.artifacts.as_mut().unwrap().diff = None;
        let mut dimensions = report("b", CaseOutcome::DimensionMismatch);
        dimensions.candidate.width = None;
        let cases = Cases::new("/w/.gleon/runs/latest", vec![mismatch, dimensions]);
        let html = ReportGenerator::generate_html(&cases, Path::new("/w/.gleon/runs/latest"))
            .unwrap()
            .unwrap();
        assert!(html.contains("slider-container"), "{html}");
        assert!(!html.contains("Diff Image"), "{html}");
        assert!(html.contains("Actual</div>"), "{html}");
        assert!(!html.contains("None"), "{html}");
    }
}
