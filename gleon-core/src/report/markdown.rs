//! Markdown report/PR-comment generation.

use gleon_model::{case::CaseReport, platform::PlatformKey};
use minijinja::context;
use serde::Serialize;

use super::{
    MarkdownReportOptions, RenderTarget,
    format::{CaseSummary, case_name, status},
};
use crate::cases::Cases;

/// Displays a string with characters that would break a Markdown table cell
/// (`|`, backslash, backtick, brackets, newlines) escaped or replaced, and `<`/`>` as entities
/// (GitHub would drop `<word>` as an HTML tag).
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
                '<' => f.write_str("&lt;")?,
                '>' => f.write_str("&gt;")?,
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

/// The image link of a case's baseline: a signed URL from the resolver if available, else the
/// blob under `base_image_url`. Only baselines are content-addressed and uploaded; candidates and
/// diffs never leave the machine that ran the tests.
fn baseline_cell(report: &CaseReport, options: &MarkdownReportOptions) -> Option<String> {
    let blob = report.golden.blob.as_ref()?;
    let url = options
        .image_url_resolver
        .and_then(|resolve| resolve(blob))
        .or_else(|| {
            options.base_image_url.map(|base| {
                format!(
                    "{}/blobs/{}/{}",
                    base.trim_end_matches('/'),
                    blob.scheme(),
                    blob.value()
                )
            })
        })?;
    Some(format!("[Image]({url})"))
}

/// A warning as a Markdown blockquote: code spans stay code, `<` and `>` stay text (GitHub would
/// drop them as HTML tags).
fn quote(warning: &str) -> String {
    format!(
        "> ⚠️ {}\n",
        warning
            .replace(['\n', '\r'], " ")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    )
}

/// One rendered failure row, precomputed in Rust (rather than in the `pr_comment.md` template)
/// because `options.image_url_resolver` is a closure and can't cross into a minijinja context.
#[derive(Serialize)]
struct MarkdownRow {
    name: String,
    /// The baseline link, when the baseline is a blob in reachable storage.
    baseline: Option<String>,
    /// Status and reason ([`CaseSummary`]).
    result: String,
}

fn build_row(
    cases: &Cases,
    key: &PlatformKey,
    report: &CaseReport,
    options: &MarkdownReportOptions,
) -> MarkdownRow {
    MarkdownRow {
        name: CodeSpanEscape(&case_name(cases, key, report)).to_string(),
        baseline: baseline_cell(report, options),
        result: MarkdownEscape(&CaseSummary(report).to_string()).to_string(),
    }
}

impl super::ReportGenerator {
    /// Maximum number of failure rows rendered in a PR comment table.
    pub const MAX_MARKDOWN_DIFF_ROWS: usize = 10;

    /// Renders a GitHub PR comment in Markdown from the failed cases, most telling first
    /// ([`Cases::failures_by_severity`]), and the warnings of the selection. Truncates the table
    /// to `MAX_MARKDOWN_DIFF_ROWS` rows.
    ///
    /// # Panics
    ///
    /// Panics if the bundled `pr_comment.md` template is missing from the registry or fails to
    /// render against the row context built here — both are build-time-class bugs (a broken
    /// bundled template or a Rust/template field mismatch) that the test suite catches
    /// immediately, not runtime conditions callers need to handle.
    #[must_use]
    pub fn render_pr_comment(cases: &Cases, options: &MarkdownReportOptions) -> String {
        let failures = cases.failures_by_severity();
        let total_failed = failures.len();
        let warnings: String = cases.warnings().iter().map(|w| quote(w)).collect();

        if total_failed == 0 {
            let passed = "### ✅ Gleon Visual Regression: All tests passed!\n";
            return if warnings.is_empty() {
                passed.to_owned()
            } else {
                format!("{passed}\n{warnings}")
            };
        }

        let rows: Vec<MarkdownRow> = failures
            .into_iter()
            .take(Self::MAX_MARKDOWN_DIFF_ROWS)
            .map(|(key, report)| build_row(cases, key, report, options))
            .collect();

        let has_baselines = rows.iter().any(|row| row.baseline.is_some());

        let remaining = total_failed.saturating_sub(Self::MAX_MARKDOWN_DIFF_ROWS);

        let footer = match options.context {
            RenderTarget::GitHubActions => Self::FOOTER_GITHUB_ACTIONS,
            RenderTarget::LocalTerminal => Self::FOOTER_LOCAL_TERMINAL,
        };
        let footer = if warnings.is_empty() {
            footer.to_owned()
        } else {
            format!("{warnings}{footer}")
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
            has_baselines => has_baselines,
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

    /// Generates a Markdown summary of every case, with the warnings of the selection.
    #[must_use]
    pub fn generate_markdown(cases: &Cases) -> String {
        use std::fmt::Write;

        let total = cases.reports().len();
        let failed = cases.failures().count();

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
        for warning in cases.warnings() {
            out.push_str(&quote(warning));
            out.push('\n');
        }

        out.push_str("| Test Case | Screenshot | Status |\n|---|---|---|\n");

        for (key, report) in cases.keyed() {
            let mark = if report.outcome.is_failure() {
                "❌"
            } else {
                "✅"
            };
            #[expect(
                clippy::expect_used,
                reason = "`fmt::Write` for `String` is infallible"
            )]
            writeln!(
                out,
                "| {} | {} | {mark} {} |",
                MarkdownEscape(&case_name(cases, key, report)),
                MarkdownEscape(report.golden.compared()),
                status(report.outcome)
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
    use gleon_model::{case::CaseOutcome, platform::PlatformConfig};

    use super::*;
    use crate::{
        cases::fixtures::{every_outcome, report, report_on},
        manifest::ImageHash,
        report::ReportGenerator,
    };

    fn digest() -> String {
        "d".repeat(64)
    }

    /// A mismatch of `name` whose baseline is the blob `sha256:ddd…`.
    fn with_blob(name: &str) -> CaseReport {
        let mut report = report(name, CaseOutcome::Mismatch);
        report.golden.blob = Some(ImageHash::new("sha256", digest()).unwrap());
        report
    }

    /// The whole comment for every outcome, warnings included: what a reviewer reads.
    #[test]
    fn test_render_pr_comment_every_outcome() {
        let cases = Cases::new("runs/latest", every_outcome()).with_warnings(vec![
            "2 case report(s) without a run id: use `gleon test -- <command>`".to_owned(),
        ]);
        let options = MarkdownReportOptions {
            context: RenderTarget::GitHubActions,
            ..Default::default()
        };
        assert_eq!(
            ReportGenerator::render_pr_comment(&cases, &options),
            format!(
                "### ❌ Gleon Visual Regression Failure (4 diffs)\n\
                 \n\
                 | Test Name | Result |\n\
                 | :--- | :--- |\n\
                 | `test/mismatch` | Mismatch: 5.00% (5 of 100px) differ |\n\
                 | `test/dimension_mismatch` | Dimension Mismatch: golden is 10x10px, test image is 20x10px |\n\
                 | `test/error` | Error (image): candidate image: corrupt |\n\
                 | `test/missing` | Missing Baseline: no golden yet |\n\
                 \n\
                 > ⚠️ 2 case report(s) without a run id: use `gleon test -- &lt;command&gt;`\n\
                 {}",
                ReportGenerator::FOOTER_GITHUB_ACTIONS
            )
        );
    }

    #[test]
    fn test_render_pr_comment_links_baselines_under_the_base_url() {
        let options = MarkdownReportOptions {
            base_image_url: Some("https://storage.cdn.com/gleon/"),
            html_artifact_url: Some("https://github.com/org/repo/actions/runs/1/artifacts/2"),
            ..Default::default()
        };
        let mut error = report("error", CaseOutcome::Error);
        error.message = Some("the baseline blob is not in .gleon/blobs".to_owned());
        let cases = Cases::new("runs/latest", vec![with_blob("login_button"), error]);
        let comment = ReportGenerator::render_pr_comment(&cases, &options);
        assert!(
            comment.contains(&format!(
                "| Test Name | Baseline | Result |\n\
                 | :--- | :---: | :--- |\n\
                 | `login_button` | [Image](https://storage.cdn.com/gleon/blobs/sha256/{}) | Mismatch: 5.00% (5 of 100px) differ |\n\
                 | `error` | N/A | Error (image): the baseline blob is not in .gleon/blobs |\n",
                digest()
            )),
            "every row tells why it failed: {comment}"
        );
        assert!(
            !comment.contains("runs/latest"),
            "candidates and diffs never leave the runner: {comment}"
        );
    }

    #[test]
    fn test_render_pr_comment_prefers_signed_urls() {
        let resolver =
            |hash: &ImageHash| Some(format!("https://signed.com/{}?token=1", hash.value()));
        let options = MarkdownReportOptions {
            base_image_url: Some("https://storage.cdn.com"),
            image_url_resolver: Some(&resolver),
            ..Default::default()
        };
        let cases = Cases::new("runs/latest", vec![with_blob("a")]);
        let comment = ReportGenerator::render_pr_comment(&cases, &options);
        assert!(comment.contains(&format!("[Image](https://signed.com/{}?token=1)", digest())));
        assert!(!comment.contains("storage.cdn.com"));
    }

    #[test]
    fn test_render_pr_comment_truncation_keeps_the_most_telling() {
        let mut reports: Vec<_> = (0..15)
            .map(|i| report(&format!("missing_{i:02}"), CaseOutcome::Missing))
            .collect();
        reports.push(report("zz_mismatch", CaseOutcome::Mismatch));
        let options = MarkdownReportOptions {
            html_artifact_url: Some("https://artifact.url/report.html"),
            ..Default::default()
        };
        let comment =
            ReportGenerator::render_pr_comment(&Cases::new("runs/latest", reports), &options);
        assert!(comment.contains("Truncated 6 additional diffs"));
        assert!(comment.contains("https://artifact.url/report.html"));
        assert!(
            comment.contains("zz_mismatch"),
            "a mismatch outranks missing goldens"
        );
        assert!(comment.contains("missing_08") && !comment.contains("missing_09"));
    }

    #[test]
    fn test_render_pr_comment_pass_and_footers() {
        let passing = Cases::new("runs/latest", vec![report("a", CaseOutcome::Match)]);
        assert_eq!(
            ReportGenerator::render_pr_comment(&passing, &MarkdownReportOptions::default()),
            "### ✅ Gleon Visual Regression: All tests passed!\n"
        );
        let warned = passing.with_warnings(vec!["mixed".to_owned()]);
        assert_eq!(
            ReportGenerator::render_pr_comment(&warned, &MarkdownReportOptions::default()),
            "### ✅ Gleon Visual Regression: All tests passed!\n\n> ⚠️ mixed\n"
        );

        let failing = Cases::new("runs/latest", vec![report("a", CaseOutcome::Mismatch)]);
        let md_local =
            ReportGenerator::render_pr_comment(&failing, &MarkdownReportOptions::default());
        assert!(md_local.contains(ReportGenerator::FOOTER_LOCAL_TERMINAL));
    }

    #[test]
    fn test_render_pr_comment_escapes_names_and_messages() {
        let mut error = report("a", CaseOutcome::Error);
        error.name = "test'[foo]|bar".to_owned();
        error.message = Some("a | b [c]\nd `e` <placeholder>".to_owned());
        let md = ReportGenerator::render_pr_comment(
            &Cases::new("runs/latest", vec![error]),
            &MarkdownReportOptions::default(),
        );
        assert!(md.contains("`test'[foo]\\|bar`"), "{md}");
        assert!(
            md.contains("a \\| b \\[c\\] d \\`e\\` &lt;placeholder&gt;"),
            "{md}"
        );
    }

    #[test]
    fn test_generate_markdown_lists_every_case_and_the_warnings() {
        let cases = Cases::new("runs/latest", every_outcome())
            .with_warnings(vec!["use `gleon test -- <command>`".to_owned()]);
        let md = ReportGenerator::generate_markdown(&cases);
        assert_eq!(
            md,
            "# gleon Visual Regression Summary\n\n**Total Tests:** 7\n**Failed:** 4\n\n\
             > ⚠️ use `gleon test -- &lt;command&gt;`\n\n\
             | Test Case | Screenshot | Status |\n|---|---|---|\n\
             | test/dimension_mismatch | test/dimension_mismatch.png | ❌ Dimension Mismatch |\n\
             | test/error | test/error.png | ❌ Error |\n\
             | test/identical | test/identical.png | ✅ Pass |\n\
             | test/match | test/match.png | ✅ Pass |\n\
             | test/mismatch | test/mismatch.png | ❌ Mismatch |\n\
             | test/missing | test/missing.png | ❌ Missing Baseline |\n\
             | test/updated | test/updated.png | ✅ Pass |\n"
        );
    }

    /// A run of two platforms names the platform of each case in the comment and the summary.
    #[test]
    fn test_markdown_names_the_platforms_of_a_joint_run() {
        let platform = |key: &str| PlatformConfig::Opaque(key.to_owned());
        let cases = Cases::new(
            "runs/latest",
            vec![
                report_on("a", CaseOutcome::Mismatch, platform("macos-aarch64")),
                report_on("a", CaseOutcome::Match, platform("linux-x86_64")),
            ],
        );
        let comment = ReportGenerator::render_pr_comment(&cases, &MarkdownReportOptions::default());
        assert!(
            comment.contains("| `a (macos-aarch64)` | Mismatch: 5.00% (5 of 100px) differ |"),
            "{comment}"
        );
        assert!(!comment.contains("linux-x86_64"), "{comment}");
        let md = ReportGenerator::generate_markdown(&cases);
        assert!(
            md.ends_with(
                "| a (linux-x86_64) | a.png | ✅ Pass |\n\
                 | a (macos-aarch64) | a.png | ❌ Mismatch |\n"
            ),
            "{md}"
        );
    }
}
