//! Implementation of the `gleon report` subcommand.

use anyhow::{Context, Result, anyhow};
use gleon_core::report::{MarkdownReportOptions, ReportGenerator};

use crate::{
    cli::ReportFormat,
    commands::{RunSource, load_cases, report_failure},
    exit_code::ExitCode,
};

/// Runs the `gleon report` subcommand on the case reports of the run of `source`.
///
/// Returns [`ExitCode::Success`] once the report has been generated (and written or printed), or
/// [`ExitCode::Failure`] for any error along the way (bad arguments, no case reports, template
/// rendering, or I/O) — every subcommand reports failures the same way.
pub async fn run_report(
    env: &dyn gleon_core::env::EnvProvider,
    storage_cfg: Option<gleon_core::storage::StorageConfig>,
    format: ReportFormat,
    source: &RunSource,
    pr_number: Option<u64>,
    out: Option<&std::path::Path>,
) -> ExitCode {
    match run_report_inner(env, storage_cfg, format, source, pr_number, out).await {
        Ok(()) => ExitCode::Success,
        Err(e) => report_failure("Error generating report", &*e),
    }
}

async fn run_report_inner(
    env: &dyn gleon_core::env::EnvProvider,
    storage_cfg: Option<gleon_core::storage::StorageConfig>,
    format: ReportFormat,
    source: &RunSource,
    pr_number: Option<u64>,
    out: Option<&std::path::Path>,
) -> Result<()> {
    if let Some(pr) = pr_number {
        if pr == 0 {
            return Err(anyhow!("PR number must be greater than 0"));
        }
        tracing::info!("Report target PR: #{}", pr);
    }

    tracing::debug!("Generating report in '{format:?}' format");

    let cases = load_cases(source)?;

    let mut base_image_url = None;
    let mut signed_urls = std::collections::HashMap::new();

    // Only the PR comment links baselines.
    if let Some(cfg) = storage_cfg
        .as_ref()
        .filter(|_| format == ReportFormat::Markdown)
    {
        if cfg.url.starts_with("https://") || cfg.url.starts_with("http://") {
            base_image_url = Some(cfg.url.as_str());
        }

        if let Ok(adapter) = gleon_core::storage::ObjectStoreAdapter::from_config(cfg) {
            let expires_in = std::time::Duration::from_hours(168);
            signed_urls = ReportGenerator::sign_image_urls(&adapter, &cases, expires_in).await;
        }
    }

    // Only the PR comment links goldens: no event payload or path work for other formats.
    let golden_base_url = (format == ReportFormat::Markdown)
        .then(|| golden_base_url(env, source.workspace_root()))
        .flatten();
    let artifact_env = env.get_var("GLEON_HTML_ARTIFACT_URL");
    let html_artifact_url = artifact_env.as_deref().filter(|s| !s.is_empty());

    let is_ci = env.get_var("GITHUB_ACTIONS").is_some() || pr_number.is_some();
    let context = if is_ci {
        gleon_core::report::RenderTarget::GitHubActions
    } else {
        gleon_core::report::RenderTarget::LocalTerminal
    };

    let has_signed_urls = !signed_urls.is_empty();
    let resolver = |hash: &gleon_core::manifest::ImageHash| signed_urls.get(hash).cloned();
    let options = MarkdownReportOptions {
        context,
        base_image_url,
        html_artifact_url,
        golden_base_url: golden_base_url.as_deref(),
        image_url_resolver: if has_signed_urls {
            Some(&resolver)
        } else {
            None
        },
    };

    let report_content = match format {
        ReportFormat::Markdown => ReportGenerator::render_pr_comment(&cases, &options),
        ReportFormat::Html => {
            // Image links are relative to where the page goes (the working directory for stdout).
            let report_dir = out
                .and_then(std::path::Path::parent)
                .unwrap_or_else(|| std::path::Path::new(""));
            ReportGenerator::generate_html(&cases, report_dir)
                .with_context(|| "Failed to generate HTML report")?
                .unwrap_or_else(|| "<html><body>All tests passed!</body></html>".to_string())
        }
        // JUnit XML strictly conforms to the standard CI runner schema (Jenkins, GitLab CI,
        // GitHub Actions test-reporters), omitting non-standard visual image links.
        ReportFormat::Junit => ReportGenerator::generate_junit_xml(&cases)
            .with_context(|| "Failed to generate JUnit XML report")?,
        // The case reports of the run (`case.v5.json` each), for scripts and CI tooling.
        ReportFormat::Json => serde_json::to_string_pretty(cases.reports())
            .with_context(|| "Failed to serialize report to JSON")?,
    };

    if let Some(out_path) = out {
        let parent = out_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "Failed to create parent directory for report output '{}'",
                parent.display()
            )
        })?;
        gleon_core::io::save_file_atomically(out_path, report_content.as_bytes())
            .with_context(|| format!("Failed to write output to '{}'", out_path.display()))?;
        tracing::info!("Generated the report at {}", out_path.display());
    } else {
        println!("{report_content}");
    }

    Ok(())
}

/// The URL under which the files of the workspace at `workspace_root` are seen in its GitHub
/// repository, when running on GitHub Actions:
/// `$GITHUB_SERVER_URL/$GITHUB_REPOSITORY/blob/<commit>/<workspace dir in $GITHUB_WORKSPACE>`.
///
/// The commit is the head of the pull request ([`pull_request_head`]), which stays in the base
/// repository for good (forks included); its golden is the one the PR compared, unless the base
/// branch changed the same golden. Elsewhere it is `GITHUB_SHA`. (`GITHUB_SHA` of a pull request
/// is its merge commit, made again whenever the base moves, so links to it go stale.) `None`
/// without the variables, for a run without a workspace (`--from`), or for a workspace outside
/// the checkout.
fn golden_base_url(
    env: &dyn gleon_core::env::EnvProvider,
    workspace_root: Option<&std::path::Path>,
) -> Option<String> {
    let var = |name| env.get_var(name).filter(|value| !value.trim().is_empty());
    let (server, repository, checkout) = (
        var("GITHUB_SERVER_URL")?,
        var("GITHUB_REPOSITORY")?,
        var("GITHUB_WORKSPACE")?,
    );
    let sha = pull_request_head(env).or_else(|| var("GITHUB_SHA"))?;
    let (owner, repo) = repository
        .split_once('/')
        .filter(|(owner, repo)| !owner.is_empty() && !repo.is_empty() && !repo.contains('/'))?;
    let root = gleon_core::paths::resolved(workspace_root?);
    let checkout = gleon_core::paths::resolved(std::path::Path::new(&checkout));
    let dir = root.strip_prefix(&checkout).ok()?;
    let mut url = url::Url::parse(&server)
        .ok()
        .filter(|url| matches!(url.scheme(), "https" | "http"))?;
    {
        let mut segments = url.path_segments_mut().ok()?;
        segments
            .pop_if_empty()
            .extend([owner, repo, "blob", sha.as_str()]);
        // Both paths are resolved: only plain names lie between them.
        for component in dir.components() {
            segments.push(component.as_os_str().to_str()?);
        }
    }
    Some(url.into())
}

/// The head commit of the pull request in the event payload of the job (`GITHUB_EVENT_PATH`):
/// `pull_request.head.sha` when it is a commit id (hex), else `None` (another event, or a payload
/// that cannot be read).
fn pull_request_head(env: &dyn gleon_core::env::EnvProvider) -> Option<String> {
    let path = env.get_var("GITHUB_EVENT_PATH")?;
    let payload = std::fs::read(&path)
        .map_err(|e| tracing::debug!("Cannot read the event payload '{path}': {e}"))
        .ok()?;
    let event: serde_json::Value = serde_json::from_slice(&payload)
        .map_err(|e| tracing::debug!("Cannot parse the event payload '{path}': {e}"))
        .ok()?;
    let sha = event.pointer("/pull_request/head/sha")?.as_str()?;
    let is_commit = matches!(sha.len(), 40 | 64) && sha.bytes().all(|b| b.is_ascii_hexdigit());
    is_commit.then(|| sha.to_owned())
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
    use super::*;

    struct DummyEnv;
    impl gleon_core::env::EnvProvider for DummyEnv {
        fn get_var(&self, _key: &str) -> Option<String> {
            None
        }
    }

    /// A copy of `runs/latest` with one mismatch report (its candidate image listed as an
    /// artifact) in a temporary directory.
    fn copy_of_run(temp: &tempfile::TempDir) -> RunSource {
        let cases = temp.path().join("runs/latest/cases");
        let report = serde_json::json!({
            "schema_version": 5,
            "name": "test/enc",
            "golden": {"path": "test/enc.png", "sha256": "1".repeat(64)},
            "candidate": {"sha256": "0".repeat(64)},
            "source": {"tool": "gleon_flutter", "tool_version": "0.1.0"},
            "platform": {"os": "macos", "arch": "aarch64"},
            "comparison": {"tolerance": {"kind": "exact"}, "masks": [], "policy_version": 2},
            "outcome": "mismatch",
            "metrics": {"kind": "pixel", "total_pixels": 4, "diff_pixels": 1, "diff_ratio": 0.25, "headroom": -0.25},
            "regions": [],
            "artifacts": {"candidate": ".gleon/runs/latest/artifacts/macos-aarch64/test/enc/candidate.png"},
            "timings_ms": {"total": 1.0},
            "recorded_at": "2026-10-01T12:00:00Z"
        });
        std::fs::create_dir_all(cases.join("macos-aarch64/test")).unwrap();
        std::fs::write(
            cases.join("macos-aarch64/test/enc.json"),
            report.to_string(),
        )
        .unwrap();
        RunSource::Copy(temp.path().join("runs/latest"))
    }

    #[tokio::test]
    async fn test_run_report_creates_nested_parent_dir() {
        let temp = tempfile::tempdir().unwrap();
        let cases = copy_of_run(&temp);
        let nested_out = temp.path().join("nested").join("sub").join("output.md");

        let res = run_report(
            &DummyEnv,
            None,
            ReportFormat::Markdown,
            &cases,
            None,
            Some(&nested_out),
        )
        .await;

        assert_eq!(res, ExitCode::Success);
        let md = std::fs::read_to_string(nested_out).unwrap();
        assert!(md.contains("test/enc"), "{md}");
    }

    #[tokio::test]
    async fn test_run_report_formats() {
        let temp = tempfile::tempdir().unwrap();
        let cases = copy_of_run(&temp);
        for (format, expected) in [
            (
                ReportFormat::Junit,
                "<failure message=\"Mismatch: 25.00% (1 of 4px) differ\"",
            ),
            (ReportFormat::Html, "test&#x2f;enc"),
            (ReportFormat::Json, "\"schema_version\": 5"),
        ] {
            let out = temp.path().join(format!("out.{format:?}"));
            let res = run_report(&DummyEnv, None, format, &cases, Some(7), Some(&out)).await;
            assert_eq!(res, ExitCode::Success, "{format:?}");
            let content = std::fs::read_to_string(&out).unwrap();
            assert!(content.contains(expected), "{format:?}: {content}");
        }

        // To stdout, the HTML links its images from the working directory.
        let res = run_report(&DummyEnv, None, ReportFormat::Html, &cases, None, None).await;
        assert_eq!(res, ExitCode::Success);
        let res = run_report(
            &DummyEnv,
            None,
            ReportFormat::Markdown,
            &cases,
            Some(0),
            None,
        )
        .await;
        assert_eq!(res, ExitCode::Failure, "PR #0 is rejected");
    }

    struct MapEnv(std::collections::HashMap<&'static str, String>);
    impl gleon_core::env::EnvProvider for MapEnv {
        fn get_var(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    /// The GitHub Actions variables of a job whose checkout is `checkout`.
    fn github(checkout: &std::path::Path) -> MapEnv {
        MapEnv(std::collections::HashMap::from([
            ("GITHUB_SERVER_URL", "https://github.com".to_owned()),
            ("GITHUB_REPOSITORY", "o/r".to_owned()),
            ("GITHUB_SHA", "abc123".to_owned()),
            ("GITHUB_WORKSPACE", checkout.to_str().unwrap().to_owned()),
        ]))
    }

    #[test]
    fn test_golden_base_url_needs_github_and_a_workspace_inside_the_checkout() {
        let temp = tempfile::tempdir().unwrap();
        let package = temp.path().join("packages/app");
        std::fs::create_dir_all(&package).unwrap();
        let env = github(temp.path());
        assert_eq!(
            golden_base_url(&env, Some(&package)).as_deref(),
            Some("https://github.com/o/r/blob/abc123/packages/app")
        );
        assert_eq!(
            golden_base_url(&env, Some(temp.path())).as_deref(),
            Some("https://github.com/o/r/blob/abc123")
        );
        assert_eq!(golden_base_url(&env, None), None, "a copy of a run");
        let elsewhere = tempfile::tempdir().unwrap();
        assert_eq!(golden_base_url(&env, Some(elsewhere.path())), None);
        let mut without_sha = github(temp.path());
        without_sha.0.insert("GITHUB_SHA", " ".to_owned());
        assert_eq!(golden_base_url(&without_sha, Some(temp.path())), None);
        assert_eq!(golden_base_url(&DummyEnv, Some(temp.path())), None);
        // An event payload that cannot be read or parsed names no pull request: `GITHUB_SHA`.
        let broken = temp.path().join("broken.json");
        std::fs::write(&broken, "{").unwrap();
        for payload in [temp.path().join("missing.json"), broken] {
            let mut unread = github(temp.path());
            unread
                .0
                .insert("GITHUB_EVENT_PATH", payload.to_str().unwrap().to_owned());
            assert_eq!(
                golden_base_url(&unread, Some(temp.path())).as_deref(),
                Some("https://github.com/o/r/blob/abc123"),
                "{}",
                payload.display()
            );
        }
        // A repository that is not `owner/name`, or a server that is not a web URL: no link.
        for (var, value) in [
            ("GITHUB_REPOSITORY", "o"),
            ("GITHUB_REPOSITORY", "o/"),
            ("GITHUB_REPOSITORY", "o/r/x"),
            ("GITHUB_SERVER_URL", "file:///etc"),
        ] {
            let mut odd = github(temp.path());
            odd.0.insert(var, value.to_owned());
            assert_eq!(
                golden_base_url(&odd, Some(temp.path())),
                None,
                "{var}={value}"
            );
        }

        // In a pull request, its head commit (stable), not the merge commit of `GITHUB_SHA`.
        let head = "a".repeat(40);
        let event = temp.path().join("event.json");
        std::fs::write(
            &event,
            serde_json::json!({"pull_request": {"head": {"sha": head}}}).to_string(),
        )
        .unwrap();
        let mut pull_request = github(temp.path());
        pull_request
            .0
            .insert("GITHUB_EVENT_PATH", event.to_str().unwrap().to_owned());
        assert_eq!(
            golden_base_url(&pull_request, Some(&package)),
            Some(format!("https://github.com/o/r/blob/{head}/packages/app"))
        );
        // Another event, or a head that is no commit id: `GITHUB_SHA`.
        for payload in [
            serde_json::json!({"push": {}}),
            serde_json::json!({"pull_request": {"head": {"sha": "../main"}}}),
        ] {
            std::fs::write(&event, payload.to_string()).unwrap();
            assert_eq!(
                golden_base_url(&pull_request, Some(temp.path())).as_deref(),
                Some("https://github.com/o/r/blob/abc123")
            );
        }
    }

    #[tokio::test]
    async fn test_run_report_fails_without_case_reports() {
        let temp = tempfile::tempdir().unwrap();
        let empty = RunSource::Own(temp.path().join("runs/latest"));
        let res = run_report(&DummyEnv, None, ReportFormat::Markdown, &empty, None, None).await;
        assert_eq!(res, ExitCode::Failure, "nothing recorded is no pass");

        std::fs::create_dir_all(temp.path().join("runs/latest/cases")).unwrap();
        std::fs::write(temp.path().join("runs/latest/cases/broken.json"), "{").unwrap();
        let res = run_report(&DummyEnv, None, ReportFormat::Markdown, &empty, None, None).await;
        assert_eq!(res, ExitCode::Failure, "only broken reports are no reports");
    }
}
