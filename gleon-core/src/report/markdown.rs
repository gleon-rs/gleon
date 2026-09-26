//! Markdown report/PR-comment generation.

use gleon_engine::MismatchDetail;
use minijinja::context;
use serde::Serialize;

use super::{ImageUrlResolver, MarkdownReportOptions, RenderTarget};
use crate::results::{TestCaseResult, TestImageResult};

/// Displays a path using forward slashes regardless of platform, for embedding in
/// Markdown/URLs (e.g. `foo/bar.png` even on Windows).
struct PosixPathFormatter<'a>(&'a std::path::Path);
impl std::fmt::Display for PosixPathFormatter<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use std::fmt::Write;
        if self.0.as_os_str().is_empty() {
            return f.write_str(".");
        }
        let mut first = true;
        for comp in self.0.components() {
            match comp {
                std::path::Component::Normal(s) => {
                    if !first {
                        f.write_char('/')?;
                    }
                    f.write_str(&s.to_string_lossy())?;
                    first = false;
                }
                std::path::Component::ParentDir => {
                    if !first {
                        f.write_char('/')?;
                    }
                    f.write_str("..")?;
                    first = false;
                }
                std::path::Component::CurDir => {}
                std::path::Component::RootDir => {
                    f.write_char('/')?;
                    first = true;
                }
                std::path::Component::Prefix(prefix) => {
                    f.write_str(&prefix.as_os_str().to_string_lossy())?;
                    first = false;
                }
            }
        }
        Ok(())
    }
}

/// Displays a string with characters that would break a Markdown table cell
/// (`|`, backslash, backtick, brackets, newlines) escaped or replaced.
struct MarkdownEscape<'a>(&'a str);
impl std::fmt::Display for MarkdownEscape<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use std::fmt::Write;
        for c in self.0.chars() {
            match c {
                '|' => f.write_str("\\|")?,
                '\n' | '\r' => f.write_char(' ')?,
                '\\' => f.write_str("\\\\")?,
                '`' => f.write_str("\\`")?,
                '[' => f.write_str("\\[")?,
                ']' => f.write_str("\\]")?,
                _ => f.write_char(c)?,
            }
        }
        Ok(())
    }
}

/// Displays a string with characters that would break a Markdown inline code
/// span (`|`, backtick, newlines) escaped or replaced.
struct CodeSpanEscape<'a>(&'a str);
impl std::fmt::Display for CodeSpanEscape<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use std::fmt::Write;
        for c in self.0.chars() {
            match c {
                '|' => f.write_str("\\|")?,
                '`' => f.write_char('\'')?,
                '\n' | '\r' => f.write_char(' ')?,
                _ => f.write_char(c)?,
            }
        }
        Ok(())
    }
}

/// Renders an image cell: a signed URL from `resolver` if available, else a `base_url`-relative
/// link, else `N/A`.
struct ImgLinkFormatter<'a> {
    base_url: Option<&'a str>,
    path: Option<&'a std::path::Path>,
    resolver: Option<&'a ImageUrlResolver<'a>>,
}
impl std::fmt::Display for ImgLinkFormatter<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(p) = self.path {
            if let Some(signed_url) = self.resolver.and_then(|res_fn| res_fn(p)) {
                return write!(f, "[Image]({signed_url})");
            }
            // `base_image_url` is the published root of *repository-relative* screenshots.
            // An absolute path is a location on the machine that ran the tests, so joining it
            // would both 404 and leak the local directory layout into a public PR comment.
            if let Some(base) = self.base_url.filter(|_| p.is_relative()) {
                let base = base.trim_end_matches('/');
                return write!(f, "[Image]({}/{})", base, PosixPathFormatter(p));
            }
        }
        f.write_str("N/A")
    }
}

struct DeltaFormatter<'a>(&'a MismatchDetail);
impl std::fmt::Display for DeltaFormatter<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            MismatchDetail::Pixel { diff_count } => write!(f, "{diff_count} px"),
            MismatchDetail::Ssim {
                min_ssim,
                max_excess,
                ..
            } => {
                write!(f, "{min_ssim:.4} SSIM")?;
                if *max_excess > 0.0 {
                    write!(f, ", color +{max_excess:.0}")?;
                }
                Ok(())
            }
        }
    }
}

/// One rendered failure row, precomputed in Rust (rather than in the `pr_comment.md` template)
/// because `options.image_url_resolver` is a closure and can't cross into a minijinja context.
/// Carries both the image-url-table cells and the status-table cells; the template picks
/// whichever set applies via `has_image_urls`.
#[derive(Serialize)]
struct MarkdownRow {
    name: String,
    expected: String,
    actual: String,
    diff: String,
    delta: String,
    status: &'static str,
    error: String,
}

fn build_row(tc: &TestCaseResult, options: &MarkdownReportOptions) -> MarkdownRow {
    let res = &tc.result;

    let img = |path: &std::path::Path| {
        ImgLinkFormatter {
            base_url: options.base_image_url,
            path: Some(path),
            resolver: options.image_url_resolver,
        }
        .to_string()
    };
    let no_img = || {
        ImgLinkFormatter {
            base_url: None,
            path: None,
            resolver: None,
        }
        .to_string()
    };

    let (expected, actual, diff, delta) = match res {
        TestImageResult::Mismatch {
            detail,
            diff_path,
            baseline_path,
            actual_path,
            ..
        } => (
            img(baseline_path),
            img(actual_path),
            img(diff_path),
            DeltaFormatter(detail).to_string(),
        ),
        TestImageResult::DimensionMismatch {
            baseline_path,
            actual_path,
            ..
        } => (
            img(baseline_path),
            img(actual_path),
            no_img(),
            "Dim".to_string(),
        ),
        TestImageResult::MissingBaseline { .. } => (
            no_img(),
            img(res.relative_path()),
            no_img(),
            "Missing".to_string(),
        ),
        TestImageResult::DecodeError { .. } => (
            no_img(),
            img(res.relative_path()),
            no_img(),
            "Decode Error".to_string(),
        ),
        TestImageResult::IoError { .. } => (no_img(), no_img(), no_img(), "IO Error".to_string()),
        TestImageResult::EncodeError { actual_path, .. } => (
            no_img(),
            img(actual_path),
            no_img(),
            "Encode Error".to_string(),
        ),
        TestImageResult::Success { .. } => unreachable!(),
    };

    let (status, error) = match res {
        TestImageResult::Mismatch { detail, .. } => {
            ("Mismatch", DeltaFormatter(detail).to_string())
        }
        TestImageResult::DimensionMismatch { .. } => {
            ("Dimension Mismatch", "Dim mismatch".to_string())
        }
        TestImageResult::MissingBaseline { reason, .. } => {
            ("Missing Baseline", MarkdownEscape(reason).to_string())
        }
        TestImageResult::DecodeError { error, .. } => {
            ("Decode Error", MarkdownEscape(error).to_string())
        }
        TestImageResult::IoError { error, .. } => ("IO Error", MarkdownEscape(error).to_string()),
        TestImageResult::EncodeError { error, .. } => {
            ("Encode Error", MarkdownEscape(error).to_string())
        }
        TestImageResult::Success { .. } => unreachable!(),
    };

    MarkdownRow {
        name: CodeSpanEscape(&tc.name).to_string(),
        expected,
        actual,
        diff,
        delta,
        status,
        error,
    }
}

impl super::ReportGenerator {
    /// Maximum number of failure rows rendered in a PR comment table.
    pub const MAX_MARKDOWN_DIFF_ROWS: usize = 10;

    /// Renders a GitHub PR comment in Markdown from the failed test cases.
    /// Truncates the table to `MAX_MARKDOWN_DIFF_ROWS` rows.
    ///
    /// # Panics
    ///
    /// Panics if the bundled `pr_comment.md` template is missing from the registry or fails to
    /// render against the row context built here — both are build-time-class bugs (a broken
    /// bundled template or a Rust/template field mismatch) that the test suite catches
    /// immediately, not runtime conditions callers need to handle.
    #[must_use]
    pub fn render_pr_comment(
        test_cases: &[TestCaseResult],
        options: &MarkdownReportOptions,
    ) -> String {
        let failed_tests: Vec<_> = test_cases.iter().filter(|tc| !tc.passed()).collect();
        let total_failed = failed_tests.len();

        if total_failed == 0 {
            return "### ✅ Gleon Visual Regression: All tests passed!\n".to_string();
        }

        let rows: Vec<MarkdownRow> = failed_tests
            .iter()
            .take(Self::MAX_MARKDOWN_DIFF_ROWS)
            .map(|tc| build_row(tc, options))
            .collect();

        let has_image_urls = (options.base_image_url.is_some()
            || options.image_url_resolver.is_some())
            && rows
                .iter()
                .any(|r| r.expected != "N/A" || r.actual != "N/A" || r.diff != "N/A");

        let remaining = total_failed.saturating_sub(Self::MAX_MARKDOWN_DIFF_ROWS);

        let footer = match options.context {
            RenderTarget::GitHubActions => Self::FOOTER_GITHUB_ACTIONS,
            RenderTarget::LocalTerminal => Self::FOOTER_LOCAL_TERMINAL,
        };

        // Bundled template validated by the test suite; a syntax/context mismatch here would be
        // a build-time bug caught immediately, not a runtime condition callers need to handle.
        #[expect(
            clippy::expect_used,
            reason = "bundled templates are compile-time assets validated by the test suite"
        )]
        let tmpl = super::JINJA_ENV
            .get_template("pr_comment.md")
            .expect("bundled pr_comment.md template is registered");

        let ctx = context! {
            total_failed => total_failed,
            has_image_urls => has_image_urls,
            rows => rows,
            remaining => remaining,
            html_artifact_url => options.html_artifact_url,
            footer => footer,
        };

        #[expect(
            clippy::expect_used,
            reason = "bundled templates are compile-time assets validated by the test suite"
        )]
        tmpl.render(ctx)
            .expect("bundled pr_comment.md template renders against a well-formed context")
    }

    /// Generates a simple Markdown report summary string.
    #[must_use]
    pub fn generate_markdown(test_cases: &[TestCaseResult]) -> String {
        use std::fmt::Write;

        let total = test_cases.len();
        let failed = test_cases.iter().filter(|tc| !tc.passed()).count();

        let mut out = String::new();
        #[expect(
            clippy::expect_used,
            reason = "`fmt::Write` for `String` is infallible"
        )]
        writeln!(
            out,
            "# gleon Visual Regression Summary\n\n**Total Tests:** {total}\n**Failed:** {failed}\n"
        )
        .expect("write infallible");

        out.push_str("| Test Case | Screenshot | Status |\n|---|---|---|\n");

        for tc in test_cases {
            let res = &tc.result;
            let status = match res {
                TestImageResult::Success { .. } => "✅ Pass",
                TestImageResult::DecodeError { .. } => "❌ Decode Error",
                TestImageResult::IoError { .. } => "❌ IO Error",
                TestImageResult::EncodeError { .. } => "❌ Encode Error",
                TestImageResult::MissingBaseline { .. } => "❌ Missing Baseline",
                TestImageResult::DimensionMismatch { .. } => "❌ Dimension Mismatch",
                TestImageResult::Mismatch { .. } => "❌ Mismatch",
            };

            let path_fmt = PosixPathFormatter(res.relative_path());
            let path_str = path_fmt.to_string();
            #[expect(
                clippy::expect_used,
                reason = "`fmt::Write` for `String` is infallible"
            )]
            writeln!(
                out,
                "| {} | {} | {} |",
                MarkdownEscape(&tc.name),
                MarkdownEscape(&path_str),
                status
            )
            .expect("fmt::Write on String is infallible");
        }

        out
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
    fn test_delta_formatter_mentions_color_excess() {
        let detail = MismatchDetail::Ssim {
            ssim_score: 0.99,
            min_ssim: 0.97,
            max_excess: 146.4,
            region: None,
        };
        assert_eq!(
            DeltaFormatter(&detail).to_string(),
            "0.9700 SSIM, color +146"
        );
    }

    #[test]
    fn test_render_pr_comment_with_base_url_and_fallback() {
        let tc = TestCaseResult {
            name: "login_button".to_string(),
            result: TestImageResult::Mismatch {
                relative_path: PathBuf::from("login.png"),
                detail: MismatchDetail::Pixel { diff_count: 12 },
                diff_path: PathBuf::from("diffs/login.png"),
                baseline_path: PathBuf::from("goldens/login.png"),
                actual_path: PathBuf::from("actual/login.png"),
            },
        };
        let options = MarkdownReportOptions {
            context: RenderTarget::default(),
            base_image_url: Some("https://storage.cdn.com/run-1"),
            html_artifact_url: Some("https://github.com/org/repo/actions/runs/1/artifacts/2"),
            image_url_resolver: None,
        };
        let comment = ReportGenerator::render_pr_comment(&[tc], &options);
        assert!(comment.contains("`login_button`"));
        assert!(comment.contains("[Image](https://storage.cdn.com/run-1/goldens/login.png)"));
        assert!(comment.contains("12 px"));
    }

    #[test]
    fn test_render_pr_comment_never_joins_base_url_onto_absolute_local_paths() {
        // `base_image_url` describes where the *repository-relative* screenshots are published.
        // Joining it with an absolute runner path produced links like
        // `https://cdn/repo//Users/me/proj/.gleon/runs/latest/actual/x.png`, which 404 and leak
        // the local directory layout into the PR comment.
        let tc = TestCaseResult {
            name: "login".to_string(),
            result: TestImageResult::Mismatch {
                relative_path: PathBuf::from("test/login.png"),
                detail: MismatchDetail::Pixel { diff_count: 3 },
                diff_path: PathBuf::from("/Users/me/proj/.gleon/runs/latest/diffs/login.png"),
                baseline_path: PathBuf::from("/Users/me/proj/.gleon/blobs/sha256/abc"),
                actual_path: PathBuf::from("/Users/me/proj/.gleon/runs/latest/actual/login.png"),
            },
        };
        let options = MarkdownReportOptions {
            base_image_url: Some("https://cdn.example.com/run-1"),
            ..Default::default()
        };

        let comment = ReportGenerator::render_pr_comment(&[tc], &options);
        assert!(
            !comment.contains("/Users/me/proj"),
            "absolute local path leaked into the comment: {comment}"
        );
        assert!(
            comment.contains("| Test Name | Status | Error |"),
            "when all images are unpublishable, report must fall back to the status table: {comment}"
        );
    }

    #[test]
    fn test_render_pr_comment_still_joins_base_url_onto_relative_paths() {
        // Relative paths are exactly what `base_image_url` is for, so they must keep working.
        let tc = TestCaseResult {
            name: "login".to_string(),
            result: TestImageResult::Mismatch {
                relative_path: PathBuf::from("test/login.png"),
                detail: MismatchDetail::Pixel { diff_count: 3 },
                diff_path: PathBuf::from("diffs/login.png"),
                baseline_path: PathBuf::from("goldens/login.png"),
                actual_path: PathBuf::from("actual/login.png"),
            },
        };
        let options = MarkdownReportOptions {
            base_image_url: Some("https://cdn.example.com/run-1"),
            ..Default::default()
        };

        let comment = ReportGenerator::render_pr_comment(&[tc], &options);
        assert!(comment.contains("[Image](https://cdn.example.com/run-1/goldens/login.png)"));
        assert!(comment.contains("[Image](https://cdn.example.com/run-1/actual/login.png)"));
    }

    #[test]
    fn test_render_pr_comment_truncation() {
        let mut test_cases = Vec::new();
        for i in 0..15 {
            test_cases.push(TestCaseResult {
                name: format!("test_{i}"),
                result: TestImageResult::Mismatch {
                    relative_path: PathBuf::from(format!("{i}.png")),
                    detail: MismatchDetail::Pixel { diff_count: i + 1 },
                    diff_path: PathBuf::from(format!("diff_{i}.png")),
                    baseline_path: PathBuf::from(format!("base_{i}.png")),
                    actual_path: PathBuf::from(format!("act_{i}.png")),
                },
            });
        }
        let options = MarkdownReportOptions {
            context: RenderTarget::default(),
            base_image_url: None,
            html_artifact_url: Some("https://artifact.url/report.html"),
            image_url_resolver: None,
        };
        let comment = ReportGenerator::render_pr_comment(&test_cases, &options);
        assert!(comment.contains("Truncated 5 additional diffs"));
        assert!(comment.contains("https://artifact.url/report.html"));
    }

    #[test]
    fn test_render_pr_comment_all_variants_with_base_url() {
        let test_cases = vec![
            TestCaseResult {
                name: "tc1".to_string(),
                result: TestImageResult::DimensionMismatch {
                    relative_path: PathBuf::from("dim.png"),
                    actual_size: (100, 200),
                    baseline_size: (101, 200),
                    baseline_path: PathBuf::from("base.png"),
                    actual_path: PathBuf::from("act.png"),
                },
            },
            TestCaseResult {
                name: "tc2".to_string(),
                result: TestImageResult::MissingBaseline {
                    relative_path: PathBuf::from("miss.png"),
                    reason: "No baseline".to_string(),
                },
            },
            TestCaseResult {
                name: "tc3".to_string(),
                result: TestImageResult::DecodeError {
                    relative_path: PathBuf::from("err.png"),
                    error: "Corrupt".to_string(),
                },
            },
        ];
        let options = MarkdownReportOptions {
            context: RenderTarget::default(),
            base_image_url: Some("http://test.com"),
            html_artifact_url: None,
            image_url_resolver: None,
        };
        let out = ReportGenerator::render_pr_comment(&test_cases, &options);
        assert!(out.contains("`Dim`"));
        assert!(out.contains("`Missing`"));
        assert!(out.contains("`Decode Error`"));
    }

    #[test]
    fn test_render_pr_comment_all_variants_no_base_url() {
        let test_cases = vec![
            TestCaseResult {
                name: "tc1".to_string(),
                result: TestImageResult::DimensionMismatch {
                    relative_path: PathBuf::from("dim.png"),
                    actual_size: (100, 200),
                    baseline_size: (101, 200),
                    baseline_path: PathBuf::from("base.png"),
                    actual_path: PathBuf::from("act.png"),
                },
            },
            TestCaseResult {
                name: "tc2".to_string(),
                result: TestImageResult::MissingBaseline {
                    relative_path: PathBuf::from("miss.png"),
                    reason: "No baseline".to_string(),
                },
            },
            TestCaseResult {
                name: "tc3".to_string(),
                result: TestImageResult::DecodeError {
                    relative_path: PathBuf::from("err.png"),
                    error: "Corrupt".to_string(),
                },
            },
            TestCaseResult {
                name: "tc4".to_string(),
                result: TestImageResult::EncodeError {
                    relative_path: PathBuf::from("encode_err.png"),
                    actual_path: PathBuf::from("act.png"),
                    error: "io error".to_string(),
                },
            },
        ];
        let options = MarkdownReportOptions {
            context: RenderTarget::default(),
            base_image_url: None,
            html_artifact_url: None,
            image_url_resolver: None,
        };
        let out = ReportGenerator::render_pr_comment(&test_cases, &options);
        assert!(out.contains("Dimension Mismatch"));
        assert!(out.contains("Missing Baseline"));
        assert!(out.contains("Decode Error"));
    }

    #[test]
    fn test_markdown_escape_and_posix_branches() {
        let escaped = MarkdownEscape("a|b\\c\n\r`d`[e]").to_string();
        assert_eq!(escaped, "a\\|b\\\\c  \\`d\\`\\[e\\]");

        let p = PathBuf::from("foo/.././bar");
        assert_eq!(PosixPathFormatter(&p).to_string(), "foo/../bar");
    }

    #[test]
    fn test_posix_path_formatter_special_components() {
        let empty_path = PathBuf::from("");
        assert_eq!(PosixPathFormatter(&empty_path).to_string(), ".");

        let root_path = std::path::Path::new("/");
        assert_eq!(PosixPathFormatter(root_path).to_string(), "/");
    }

    #[test]
    fn test_posix_path_formatter_parent_and_curdir() {
        let p = std::path::Path::new("../goldens/./login.png");
        assert_eq!(PosixPathFormatter(p).to_string(), "../goldens/login.png");
    }

    #[test]
    fn test_posix_display_edge_cases() {
        let p = std::path::Path::new("C:\\.\\");
        let s = PosixPathFormatter(p).to_string();
        assert!(!s.is_empty());
    }

    #[test]
    fn test_posix_path_curdir_and_empty() {
        let empty_p = PathBuf::from("");
        let posix_empty = PosixPathFormatter(&empty_p).to_string();
        assert_eq!(posix_empty, ".");
    }

    #[test]
    fn test_render_pr_comment_pass_path() {
        let test_cases = vec![];
        let options = MarkdownReportOptions {
            context: RenderTarget::default(),
            base_image_url: None,
            html_artifact_url: None,
            image_url_resolver: None,
        };
        let comment = ReportGenerator::render_pr_comment(&test_cases, &options);
        assert!(comment.contains("All tests passed!"));
    }

    #[test]
    fn test_render_pr_comment_image_truncation_without_url() {
        let mut test_cases = Vec::new();
        for i in 0..15 {
            test_cases.push(TestCaseResult {
                name: format!("test_{i}"),
                result: TestImageResult::Mismatch {
                    relative_path: PathBuf::from(format!("{i}.png")),
                    detail: MismatchDetail::Pixel { diff_count: i + 1 },
                    diff_path: PathBuf::from(format!("diff_{i}.png")),
                    baseline_path: PathBuf::from(format!("base_{i}.png")),
                    actual_path: PathBuf::from(format!("act_{i}.png")),
                },
            });
        }
        let options = MarkdownReportOptions {
            context: RenderTarget::default(),
            base_image_url: Some("http://example.com"),
            html_artifact_url: None,
            image_url_resolver: None,
        };
        let comment = ReportGenerator::render_pr_comment(&test_cases, &options);
        assert!(comment.contains("Truncated 5 additional diffs"));
        assert!(
            comment
                .contains("Download the full HTML Report from GitHub Action Artifacts to inspect.")
        );
    }

    #[test]
    fn test_render_pr_comment_name_escaping_no_bracket_slashes() {
        let tc = TestCaseResult {
            name: "test`[foo]|bar".to_string(),
            result: TestImageResult::DecodeError {
                relative_path: PathBuf::from("err.png"),
                error: "Bad header".to_string(),
            },
        };
        let options = MarkdownReportOptions {
            context: RenderTarget::default(),
            base_image_url: None,
            html_artifact_url: None,
            image_url_resolver: None,
        };
        let comment = ReportGenerator::render_pr_comment(&[tc], &options);
        // Should contain `test'[foo]\|bar` (pipe escaped, brackets unescaped, backtick replaced)
        assert!(comment.contains("`test'[foo]\\|bar`"));
        assert!(!comment.contains("\\["));
    }

    #[test]
    fn test_render_pr_comment_with_image_url_resolver() {
        let tc = TestCaseResult {
            name: "login_btn".to_string(),
            result: TestImageResult::Mismatch {
                relative_path: PathBuf::from("login.png"),
                detail: MismatchDetail::Pixel { diff_count: 5 },
                diff_path: PathBuf::from("diffs/login.png"),
                baseline_path: PathBuf::from("goldens/login.png"),
                actual_path: PathBuf::from("actual/login.png"),
            },
        };
        let resolver = |p: &std::path::Path| {
            if p == std::path::Path::new("goldens/login.png") {
                Some("https://signed.com/golden.png?token=123".to_string())
            } else {
                None
            }
        };
        let options = MarkdownReportOptions {
            context: RenderTarget::default(),
            base_image_url: None,
            html_artifact_url: None,
            image_url_resolver: Some(&resolver),
        };
        let comment = ReportGenerator::render_pr_comment(&[tc], &options);
        assert!(comment.contains("[Image](https://signed.com/golden.png?token=123)"));
    }

    #[test]
    fn test_generate_markdown() {
        let tc = TestCaseResult {
            name: "billing".to_string(),
            result: TestImageResult::DecodeError {
                relative_path: PathBuf::from("corrupt.png"),
                error: "bad data".to_string(),
            },
        };
        let md = ReportGenerator::generate_markdown(&[tc]);
        assert!(md.contains("# gleon Visual Regression Summary"));
        assert!(md.contains("❌ Decode Error"));
        assert!(md.contains("billing"));
    }

    #[test]
    fn test_render_pr_comment_missing_and_decode_error() {
        let mut tests = Vec::new();
        tests.push(TestCaseResult {
            name: "missing".to_string(),
            result: TestImageResult::MissingBaseline {
                relative_path: PathBuf::from("missing.png"),
                reason: "not found".to_string(),
            },
        });
        tests.push(TestCaseResult {
            name: "corrupt".to_string(),
            result: TestImageResult::DecodeError {
                relative_path: PathBuf::from("corrupt.png"),
                error: "bad data".to_string(),
            },
        });
        tests.push(TestCaseResult {
            name: "pixel_small".to_string(),
            result: TestImageResult::Mismatch {
                relative_path: PathBuf::from("fb.png"),
                detail: MismatchDetail::Pixel { diff_count: 10 },
                diff_path: PathBuf::from("diff.png"),
                baseline_path: PathBuf::from("base.png"),
                actual_path: PathBuf::from("actual.png"),
            },
        });
        for i in 0..10 {
            tests.push(TestCaseResult {
                name: format!("mismatch_{i}"),
                result: TestImageResult::Mismatch {
                    relative_path: PathBuf::from(format!("{i}.png")),
                    detail: MismatchDetail::Pixel { diff_count: i + 1 },
                    diff_path: PathBuf::from(format!("diff_{i}.png")),
                    baseline_path: PathBuf::from(format!("base_{i}.png")),
                    actual_path: PathBuf::from(format!("actual_{i}.png")),
                },
            });
        }

        let opts = MarkdownReportOptions {
            context: RenderTarget::default(),
            base_image_url: Some("https://storage.url"),
            html_artifact_url: Some("https://artifact.url"),
            image_url_resolver: None,
        };
        let md = ReportGenerator::render_pr_comment(&tests, &opts);
        assert!(md.contains("`Missing`"));
        assert!(md.contains("`Decode Error`"));
        assert!(md.contains("10 px"));
        assert!(md.contains("https://artifact.url"));
    }

    #[test]
    fn test_execution_context_footer() {
        let tc = TestCaseResult {
            name: "fail".to_string(),
            result: TestImageResult::MissingBaseline {
                relative_path: PathBuf::from("a"),
                reason: "no baseline".to_string(),
            },
        };
        let tests = vec![tc];

        let opts_gh = MarkdownReportOptions {
            context: RenderTarget::GitHubActions,
            ..Default::default()
        };
        let md_gh = ReportGenerator::render_pr_comment(&tests, &opts_gh);
        assert!(md_gh.contains(ReportGenerator::FOOTER_GITHUB_ACTIONS));

        let opts_local = MarkdownReportOptions {
            context: RenderTarget::LocalTerminal,
            ..Default::default()
        };
        let md_local = ReportGenerator::render_pr_comment(&tests, &opts_local);
        assert!(md_local.contains(ReportGenerator::FOOTER_LOCAL_TERMINAL));
    }

    #[test]
    fn test_render_pr_comment_io_error() {
        let tests = vec![
            TestCaseResult {
                name: "io_error_test".to_string(),
                result: TestImageResult::IoError {
                    relative_path: PathBuf::from("io_error.png"),
                    error: "disk full".to_string(),
                },
            },
            TestCaseResult {
                name: "encode_error_test".to_string(),
                result: TestImageResult::EncodeError {
                    relative_path: PathBuf::from("encode_error.png"),
                    error: "bad dimensions".to_string(),
                    actual_path: PathBuf::from("actual_encode.png"),
                },
            },
            TestCaseResult {
                name: "ssim_test".to_string(),
                result: TestImageResult::Mismatch {
                    relative_path: PathBuf::from("ssim.png"),
                    actual_path: PathBuf::from("actual.png"),
                    baseline_path: PathBuf::from("baseline.png"),
                    diff_path: PathBuf::from("diff.png"),
                    detail: MismatchDetail::Ssim {
                        ssim_score: 0.95,
                        min_ssim: 0.95,
                        max_excess: 0.0,
                        region: None,
                    },
                },
            },
        ];

        let opts = MarkdownReportOptions::default();
        let md = ReportGenerator::render_pr_comment(&tests, &opts);
        assert!(md.contains("IO Error") || md.contains("IoError"));
        assert!(md.contains("Encode Error") || md.contains("EncodeError"));

        let opts_with_url = MarkdownReportOptions {
            base_image_url: Some("http://cdn.com"),
            ..Default::default()
        };
        let md_url = ReportGenerator::render_pr_comment(&tests, &opts_with_url);
        assert!(md_url.contains("IO Error"));
        assert!(md_url.contains("Encode Error"));
        assert!(md_url.contains("0.9500 SSIM"));
    }

    #[test]
    fn test_render_pr_comment_all_na_images_falls_back_to_status_table() {
        let tests = vec![TestCaseResult {
            name: "io_error_test".to_string(),
            result: TestImageResult::IoError {
                relative_path: PathBuf::from("io.png"),
                error: "permission denied".to_string(),
            },
        }];

        let opts_with_url = MarkdownReportOptions {
            base_image_url: Some("http://cdn.com"),
            ..Default::default()
        };
        let md = ReportGenerator::render_pr_comment(&tests, &opts_with_url);
        assert!(md.contains("| Test Name | Status | Error |"));
        assert!(md.contains("| `io_error_test` | IO Error | permission denied |"));
        assert!(!md.contains("| Expected | Actual | Diff |"));
    }
}
