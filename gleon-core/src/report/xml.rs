//! `JUnit` XML report generation.

use gleon_engine::MismatchDetail;
use minijinja::context;
use serde::{
    Serialize, Serializer,
    ser::{SerializeSeq, SerializeStruct},
};

use super::{ReportError, format::FormattedPath};
use crate::results::{TestCaseResult, TestImageResult};

/// Lazy view prepending a static prefix (`"Decode error: "`, `"IO error: "`, ...) to a failure
/// message, shared by every `TestImageResult` variant whose XML `failure_message` is just
/// `"{prefix}: {message}"`.
struct XmlPrefixedMessage<'a>(&'static str, &'a str);

impl Serialize for XmlPrefixedMessage<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(&format_args!("{}: {}", self.0, self.1))
    }
}

struct XmlDimensionMismatchView((u32, u32), (u32, u32));

impl std::fmt::Display for XmlDimensionMismatchView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Dimension mismatch (Baseline: {}x{}, Actual: {}x{})",
            (self.0).0,
            (self.0).1,
            (self.1).0,
            (self.1).1
        )
    }
}

impl Serialize for XmlDimensionMismatchView {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

struct XmlMismatchMessageView<'a>(&'a MismatchDetail);

impl std::fmt::Display for XmlMismatchMessageView<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Visual mismatch detected ({})", self.0)
    }
}

impl Serialize for XmlMismatchMessageView<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

// Lazy view for XML image result
struct XmlTestImageResultView<'a>(&'a TestImageResult);

impl Serialize for XmlTestImageResultView<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("XmlTestImageResult", 3)?;
        state.serialize_field(
            "name",
            &FormattedPath {
                path: self.0.relative_path(),
                report_dir: None,
            },
        )?;

        match self.0 {
            TestImageResult::Success { .. } => {
                state.serialize_field("status", "Success")?;
                state.serialize_field("failure_message", &None::<String>)?;
            }
            TestImageResult::DecodeError { error, .. } => {
                state.serialize_field("status", "DecodeError")?;
                state.serialize_field(
                    "failure_message",
                    &Some(XmlPrefixedMessage("Decode error", error)),
                )?;
            }
            TestImageResult::IoError { error, .. } => {
                state.serialize_field("status", "IoError")?;
                state.serialize_field(
                    "failure_message",
                    &Some(XmlPrefixedMessage("IO error", error)),
                )?;
            }
            TestImageResult::EncodeError { error, .. } => {
                state.serialize_field("status", "EncodeError")?;
                state.serialize_field(
                    "failure_message",
                    &Some(XmlPrefixedMessage("Encode error", error)),
                )?;
            }
            TestImageResult::MissingBaseline { reason, .. } => {
                state.serialize_field("status", "MissingBaseline")?;
                state.serialize_field(
                    "failure_message",
                    &Some(XmlPrefixedMessage("Missing baseline", reason)),
                )?;
            }
            TestImageResult::DimensionMismatch {
                baseline_size,
                actual_size,
                ..
            } => {
                state.serialize_field("status", "DimensionMismatch")?;
                state.serialize_field(
                    "failure_message",
                    &Some(XmlDimensionMismatchView(*baseline_size, *actual_size)),
                )?;
            }
            TestImageResult::Mismatch { detail, .. } => {
                state.serialize_field("status", "Mismatch")?;
                state.serialize_field("failure_message", &Some(XmlMismatchMessageView(detail)))?;
            }
        }
        state.end()
    }
}

// Lazy view for XML Test Case
struct XmlTestCaseView<'a>(&'a TestCaseResult);

impl Serialize for XmlTestCaseView<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // For JUnit compatibility, we serialize the single result as a 1-element list
        struct ResultsSeq<'a>(&'a TestImageResult);
        impl Serialize for ResultsSeq<'_> {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                let mut seq = serializer.serialize_seq(Some(1))?;
                seq.serialize_element(&XmlTestImageResultView(self.0))?;
                seq.end()
            }
        }

        let mut state = serializer.serialize_struct("XmlTestCase", 3)?;
        state.serialize_field("name", &self.0.name)?;
        state.serialize_field("results", &ResultsSeq(&self.0.result))?;

        let failures = i32::from(!matches!(self.0.result, TestImageResult::Success { .. }));
        state.serialize_field("failures", &failures)?;

        state.end()
    }
}

// Lazy view for all test cases
struct XmlTestCasesView<'a>(&'a [TestCaseResult]);

impl Serialize for XmlTestCasesView<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for tc in self.0 {
            seq.serialize_element(&XmlTestCaseView(tc))?;
        }
        seq.end()
    }
}

impl super::ReportGenerator {
    /// Generates raw junit.xml file bytes mapping failures and decode/dimension errors to
    /// `<failure>` nodes.
    ///
    /// # Panics
    /// Panics if the bundled template cannot be retrieved (impossible in normal builds).
    ///
    /// # Errors
    /// Returns [`ReportError::Render`] if template rendering fails.
    pub fn generate_junit_xml(test_cases: &[TestCaseResult]) -> Result<String, ReportError> {
        let total_tests = test_cases.len();
        let failed_tests = test_cases.iter().filter(|tc| !tc.passed()).count();

        #[expect(
            clippy::expect_used,
            reason = "bundled templates are compile-time assets validated by the test suite"
        )]
        let tmpl = super::JINJA_ENV
            .get_template("junit.xml")
            .expect("bundled junit.xml template is registered");

        let ctx = context! {
            total_tests => total_tests,
            failed_tests => failed_tests,
            test_cases => XmlTestCasesView(test_cases),
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
    use std::path::PathBuf;

    use super::*;
    use crate::report::ReportGenerator;

    #[test]
    fn test_generate_junit_xml() {
        let tc1 = TestCaseResult {
            name: "billing".to_string(),
            result: TestImageResult::Mismatch {
                relative_path: PathBuf::from("form.png"),
                detail: MismatchDetail::Pixel { diff_count: 5 },
                diff_path: PathBuf::from("diff.png"),
                baseline_path: PathBuf::from("baseline.png"),
                actual_path: PathBuf::from("actual.png"),
            },
        };
        let tc2 = TestCaseResult {
            name: "billing".to_string(),
            result: TestImageResult::Mismatch {
                relative_path: PathBuf::from("ssim_form.png"),
                detail: MismatchDetail::Ssim {
                    ssim_score: 0.9412,
                    min_ssim: 0.9412,
                    max_excess: 0.0,
                    region: None,
                },
                diff_path: PathBuf::from("diff.png"),
                baseline_path: PathBuf::from("baseline.png"),
                actual_path: PathBuf::from("actual.png"),
            },
        };
        let tc3 = TestCaseResult {
            name: "billing".to_string(),
            result: TestImageResult::EncodeError {
                relative_path: PathBuf::from("encode_form.png"),
                actual_path: PathBuf::from("act.png"),
                error: "io error".to_string(),
            },
        };
        let xml =
            ReportGenerator::generate_junit_xml(&[tc1, tc2, tc3]).expect("Render should succeed");
        assert!(xml.contains("<failure message=\"Visual mismatch detected (5 pixels)\">Visual mismatch detected (5 pixels)</failure>"));
        assert!(xml.contains("<failure message=\"Visual mismatch detected (min local SSIM 0.9412)\">Visual mismatch detected (min local SSIM 0.9412)</failure>"));
        assert!(xml.contains(
            "<failure message=\"Encode error: io error\">Encode error: io error</failure>"
        ));
        assert!(xml.contains("classname=\"billing\""));
        assert!(xml.contains("name=\"form.png\""));
        assert!(xml.contains("name=\"encode_form.png\""));
    }

    #[test]
    fn test_generate_junit_xml_io_and_encode_errors() {
        let tests = vec![
            TestCaseResult {
                name: "io_fail".to_string(),
                result: TestImageResult::IoError {
                    relative_path: PathBuf::from("io.png"),
                    error: "disk error".to_string(),
                },
            },
            TestCaseResult {
                name: "encode_fail".to_string(),
                result: TestImageResult::EncodeError {
                    relative_path: PathBuf::from("enc.png"),
                    error: "bad data".to_string(),
                    actual_path: PathBuf::from("actual.png"),
                },
            },
        ];
        let xml = ReportGenerator::generate_junit_xml(&tests).unwrap();
        assert!(xml.contains("io_fail"));
        assert!(xml.contains("encode_fail"));
    }

    #[test]
    fn test_generate_junit_xml_all_variants() {
        let tests = vec![
            TestCaseResult {
                name: "mismatch".to_string(),
                result: TestImageResult::Mismatch {
                    relative_path: PathBuf::from("rel.png"),
                    actual_path: PathBuf::from("actual.png"),
                    baseline_path: PathBuf::from("baseline.png"),
                    diff_path: PathBuf::from("diff.png"),
                    detail: MismatchDetail::Pixel { diff_count: 5 },
                },
            },
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
                name: "pass".to_string(),
                result: TestImageResult::Success {
                    relative_path: PathBuf::from("rel.png"),
                },
            },
        ];

        let xml = ReportGenerator::generate_junit_xml(&tests).unwrap();
        assert!(xml.contains("mismatch"));
        assert!(xml.contains("dim_mismatch"));
        assert!(xml.contains("missing"));
        assert!(xml.contains("decode"));
        assert!(xml.contains("io"));
        assert!(xml.contains("encode"));
    }
}
