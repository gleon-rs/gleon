//! `JUnit` XML report generation.

use gleon_model::case::{CaseOutcome, CaseReport};
use minijinja::context;
use serde::Serialize;

use super::{ReportError, format::CaseSummary};
use crate::cases::Cases;

/// One case as a `JUnit` test case (all in one suite, named after the case, with its golden as
/// `file`): `failure` for a failed comparison, `error` when the case could not be compared at all.
#[derive(Serialize)]
struct XmlCase<'a> {
    name: &'a str,
    image: &'a str,
    status: &'static str,
    message: Option<String>,
}

impl<'a> XmlCase<'a> {
    fn of(report: &'a CaseReport) -> Self {
        let status = match report.outcome {
            CaseOutcome::Error => "error",
            outcome if outcome.is_failure() => "failure",
            _ => "passed",
        };
        Self {
            name: &report.name,
            image: &report.golden.path,
            status,
            message: report
                .outcome
                .is_failure()
                .then(|| xml_text(CaseSummary(report).to_string())),
        }
    }
}

/// `text` without the control characters XML 1.0 forbids even as references (ANSI color codes
/// of a tool's output, `\0`), which would make every reader reject the whole file. Names and
/// paths cannot hold them; messages come from integrations and the OS.
fn xml_text(text: String) -> String {
    let is_forbidden = |c: char| c.is_control() && !matches!(c, '\t' | '\n' | '\r');
    if text.contains(is_forbidden) {
        text.chars().filter(|&c| !is_forbidden(c)).collect()
    } else {
        text
    }
}

impl super::ReportGenerator {
    /// Generates raw junit.xml file bytes: failed comparisons are `<failure>` nodes, cases that
    /// could not be compared `<error>` nodes.
    ///
    /// # Panics
    /// Panics if the bundled template cannot be retrieved (impossible in normal builds).
    ///
    /// # Errors
    /// Returns [`ReportError::Render`] if template rendering fails.
    pub fn generate_junit_xml(cases: &Cases) -> Result<String, ReportError> {
        let test_cases: Vec<_> = cases.reports().iter().map(XmlCase::of).collect();
        let count = |status| test_cases.iter().filter(|tc| tc.status == status).count();

        #[expect(
            clippy::expect_used,
            reason = "bundled templates are compile-time assets validated by the test suite"
        )]
        let tmpl = super::JINJA_ENV
            .get_template("junit.xml")
            .expect("bundled junit.xml template is registered");

        let ctx = context! {
            total_tests => test_cases.len(),
            failed_tests => count("failure"),
            error_tests => count("error"),
            test_cases => test_cases,
        };

        tmpl.render(ctx).map_err(|e| ReportError::Render {
            template: "junit.xml",
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
    use super::*;
    use crate::{
        cases::fixtures::{every_outcome, report},
        report::ReportGenerator,
    };

    /// The whole document for every outcome: failures and errors apart.
    #[test]
    fn test_generate_junit_xml_tells_failures_from_errors() {
        let cases = Cases::new("runs/latest", every_outcome());
        let xml = ReportGenerator::generate_junit_xml(&cases).unwrap();
        let lines: Vec<_> = xml
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        assert_eq!(
            lines,
            [
                r#"<?xml version="1.0" encoding="UTF-8"?>"#,
                r#"<testsuites name="gleon Tests" tests="7" failures="3" errors="1">"#,
                r#"<testsuite name="gleon" tests="7" failures="3" errors="1">"#,
                r#"<testcase name="test&#x2f;dimension_mismatch" classname="gleon" file="test&#x2f;dimension_mismatch.png">"#,
                r#"<failure message="Dimension Mismatch: golden is 10x10px, test image is 20x10px">Dimension Mismatch: golden is 10x10px, test image is 20x10px</failure>"#,
                "</testcase>",
                r#"<testcase name="test&#x2f;error" classname="gleon" file="test&#x2f;error.png">"#,
                r#"<error message="Error (image): candidate image: corrupt">Error (image): candidate image: corrupt</error>"#,
                "</testcase>",
                r#"<testcase name="test&#x2f;identical" classname="gleon" file="test&#x2f;identical.png">"#,
                "</testcase>",
                r#"<testcase name="test&#x2f;match" classname="gleon" file="test&#x2f;match.png">"#,
                "</testcase>",
                r#"<testcase name="test&#x2f;mismatch" classname="gleon" file="test&#x2f;mismatch.png">"#,
                r#"<failure message="Mismatch: 5.00% (5 of 100px) differ">Mismatch: 5.00% (5 of 100px) differ</failure>"#,
                "</testcase>",
                r#"<testcase name="test&#x2f;missing" classname="gleon" file="test&#x2f;missing.png">"#,
                r#"<failure message="Missing Baseline: no golden yet">Missing Baseline: no golden yet</failure>"#,
                "</testcase>",
                r#"<testcase name="test&#x2f;updated" classname="gleon" file="test&#x2f;updated.png">"#,
                "</testcase>",
                "</testsuite>",
                "</testsuites>",
            ]
        );
    }

    #[test]
    fn test_generate_junit_xml_escapes_messages() {
        let mut error = report("a", CaseOutcome::Error);
        error.message = Some("<boom> & \"quotes\" \u{1b}[31m red \u{1b}[0m\0".to_owned());
        let xml =
            ReportGenerator::generate_junit_xml(&Cases::new("runs/latest", vec![error])).unwrap();
        assert!(
            xml.contains("&lt;boom&gt; &amp; &quot;quotes&quot; [31m red [0m<"),
            "{xml}"
        );
        assert!(!xml.contains("<boom>"));
        assert!(
            !xml.chars().any(|c| c.is_control() && !c.is_whitespace()),
            "XML 1.0 forbids control characters: {xml:?}"
        );
    }
}
