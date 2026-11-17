//! Generates HTML, `JUnit` XML, and Markdown reports from the case reports of a run
//! ([`crate::cases::Cases`]).
//!
//! Split by output format across private submodules: `html` (`report.html`), `xml`
//! (`junit.xml`), and `markdown` (PR-comment/summary Markdown), plus `presign` (signing remote
//! storage URLs for a Markdown PR comment's baselines), sharing the path and outcome formatting
//! helpers in `format` and the bundled-template registry defined here. Each submodule contributes
//! its generator methods via its own `impl ReportGenerator` block.

mod format;
mod html;
mod markdown;
mod presign;
mod xml;

use std::sync::LazyLock;

pub use format::{CaseSummary, case_name};

use crate::{cases::Cases, manifest::ImageHash};

pub(crate) static JINJA_ENV: LazyLock<minijinja::Environment<'static>> = LazyLock::new(|| {
    let mut env = minijinja::Environment::new();
    for (name, source) in [
        ("report.html", include_str!("../templates/report.html")),
        ("junit.xml", include_str!("../templates/junit.xml")),
        ("pr_comment.md", include_str!("../templates/pr_comment.md")),
        (
            "dashboard.html",
            include_str!("../templates/dashboard.html"),
        ),
    ] {
        // Bundled templates are compiled into the binary and validated by the test suite; a
        // syntax error here would be a build-time bug caught immediately, not a runtime
        // condition callers need to handle.
        #[expect(
            clippy::expect_used,
            reason = "bundled templates are compile-time assets validated by the test suite"
        )]
        env.add_template(name, source)
            .expect("bundled templates are valid minijinja syntax");
    }
    env
});

/// Renders the bundled template `template` ([`JINJA_ENV`]) with `context`.
///
/// # Errors
/// Returns [`ReportError::Render`] naming the template if it cannot be rendered.
pub(crate) fn render(
    template: &'static str,
    context: impl Into<minijinja::Value>,
) -> Result<String, ReportError> {
    JINJA_ENV
        .get_template(template)
        .and_then(|bundled| bundled.render(context))
        .map_err(|source| ReportError::Render { template, source })
}

/// Errors that can occur during report generation.
#[derive(Debug, thiserror::Error)]
pub enum ReportError {
    /// Template rendering failed.
    #[error("Template rendering failed for '{template}'")]
    Render {
        /// Name of the template that failed.
        template: &'static str,
        /// The underlying minijinja error message.
        #[source]
        source: minijinja::Error,
    },

    /// IO error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Where the PR comment is being rendered, used to pick an appropriate footer.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RenderTarget {
    /// Rendered for a local terminal invocation (e.g. `gleon diff`).
    #[default]
    LocalTerminal,
    /// Rendered for a GitHub Actions workflow run.
    GitHubActions,
}

/// Options controlling how [`ReportGenerator::render_pr_comment`] links images and
/// which footer it appends.
#[derive(Default)]
pub struct MarkdownReportOptions<'a> {
    /// Public URL of the remote storage, under which baselines live at `blobs/<scheme>/<value>`;
    /// used for a baseline without a signed URL.
    pub base_image_url: Option<&'a str>,
    /// URL of the full HTML report artifact, linked when rows are truncated.
    pub html_artifact_url: Option<&'a str>,
    /// Signed URLs of baselines ([`ReportGenerator::sign_image_urls`]), tried before
    /// `base_image_url`.
    pub signed_urls: Option<&'a std::collections::HashMap<ImageHash, String>>,
    /// URL under which the files of the workspace are seen in its repository at the tested
    /// commit (`https://github.com/<owner>/<repo>/blob/<sha>[/<workspace dir>]`): the baseline
    /// link of a case whose golden is a file of the repository (an integration's golden, no
    /// blob).
    pub golden_base_url: Option<&'a str>,
    /// Where the comment is being rendered, selecting the footer text.
    pub context: RenderTarget,
}

/// Generates HTML, `JUnit` XML, and Markdown reports from test results.
pub struct ReportGenerator;

impl ReportGenerator {
    /// Footer appended to PR comments rendered for GitHub Actions.
    pub const FOOTER_GITHUB_ACTIONS: &'static str = "\n---\n*Reply with `/gleon approve` to update baseline images for this PR (see repository README for workflow setup instructions).*\n";
    /// Footer appended to PR comments rendered for a local terminal.
    pub const FOOTER_LOCAL_TERMINAL: &'static str =
        "\n---\n*Run `gleon approve` to accept failed screenshots as new baselines locally.*\n";

    /// The files [`Self::generate_all`] writes into a runs directory.
    pub const OUTPUT_FILES: [&'static str; 3] = ["report.md", "junit.xml", "report.html"];

    /// Generates the markdown, `JUnit` XML and HTML reports of `cases` inside `runs_dir`
    /// ([`Self::OUTPUT_FILES`]); without failures there is no HTML report.
    ///
    /// # Errors
    ///
    /// Returns `ReportError` if any of the underlying report generation steps
    /// fail (template rendering) or if writing a report file to `runs_dir`
    /// fails.
    pub fn generate_all(runs_dir: &std::path::Path, cases: &Cases) -> Result<(), ReportError> {
        // Regenerated by every run: atomic, without the cost of a flush to disk; an HTML report
        // of an earlier run with failures goes.
        let write = |name: &str, content: Option<&str>| {
            gleon_model::fs::write_or_remove(
                &runs_dir.join(name),
                content.map(str::as_bytes),
                gleon_model::fs::Durability::Atomic,
            )
        };
        let [md_name, xml_name, html_name] = Self::OUTPUT_FILES;
        write(md_name, Some(&Self::generate_markdown(cases)))?;
        write(xml_name, Some(&Self::generate_junit_xml(cases)?))?;
        write(html_name, Self::generate_html(cases, runs_dir)?.as_deref())?;
        Ok(())
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

    #[test]
    fn test_report_error_display() {
        let report_err = ReportError::Io(std::io::Error::other("test io"));
        assert_eq!(report_err.to_string(), "IO error: test io");
        let missing = render("missing.html", minijinja::context! {}).unwrap_err();
        assert!(matches!(
            missing,
            ReportError::Render {
                template: "missing.html",
                ..
            }
        ));
    }

    #[test]
    fn test_generate_all_removes_stale_report_html_when_no_diffs() {
        let temp = tempfile::tempdir().unwrap();
        let runs_dir = temp.path();
        let stale_html = runs_dir.join("report.html");
        std::fs::write(&stale_html, b"<html>stale</html>").unwrap();
        assert!(stale_html.exists());

        ReportGenerator::generate_all(runs_dir, &Cases::new(runs_dir, Vec::new())).unwrap();
        assert!(!stale_html.exists());
    }
}
