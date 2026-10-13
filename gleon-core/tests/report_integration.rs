#![cfg(not(miri))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::pedantic,
    clippy::nursery,
    missing_docs,
    reason = "test code: panics are assertions, and pedantic/nursery style lints are not enforced in tests"
)]

mod common;

use std::{fs, path::Path};

use gleon_core::{
    case::{CaseOutcome, Metrics},
    cases::Cases,
    context::{ContextOptions, ResolvedContext},
    dashboard::RunHistoryEntry,
    ops::{
        ApproveError, ApproveResult, approve_workspace, diff::DiffOptions, init_workspace,
        run_diff, stage_workspace,
    },
    platform::PlatformKey,
    report::{MarkdownReportOptions, ReportGenerator},
};

fn fixtures() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Copies the directory tree `from` into `to`.
fn copy_tree(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            fs::create_dir_all(&target).unwrap();
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// The approved cases as `<platform>/<name>`.
fn approved_names(result: &ApproveResult) -> Vec<String> {
    result.approved.iter().map(ToString::to_string).collect()
}

/// The case reports of a real `flutter test` run in CI (the `metrics-linux-x64` artifact of the
/// gleon Flutter package's example, a copy of `.gleon/runs/latest`) render without `gleon diff`.
#[test]
fn test_reports_of_a_real_flutter_run() {
    let temp = tempfile::tempdir().unwrap();
    let downloaded = temp.path().join("metrics-linux-x64");
    fs::create_dir_all(&downloaded).unwrap();
    copy_tree(&fixtures().join("cases/flutter-linux-x64"), &downloaded);

    let cases = Cases::load(&downloaded, None).unwrap();
    assert_eq!(cases.run_id().unwrap().as_str(), "36918116203-1");
    assert!(cases.warnings().is_empty(), "{:?}", cases.warnings());
    let names: Vec<_> = cases.reports().iter().map(|r| r.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "test/goldens/counter_initial",
            "test/goldens/counter_three_taps"
        ]
    );
    let three_taps = &cases.reports()[1];
    assert_eq!(three_taps.outcome, CaseOutcome::Match);
    assert!(matches!(
        three_taps.metrics,
        Some(Metrics::Ssim { min_ssim, .. }) if (min_ssim - 0.754).abs() < 0.001
    ));

    ReportGenerator::generate_all(&downloaded, &cases).unwrap();
    let md = fs::read_to_string(downloaded.join("report.md")).unwrap();
    assert!(md.contains("**Total Tests:** 2\n**Failed:** 0"), "{md}");
    assert!(md.contains(
        "| test/goldens/counter_three_taps | test/goldens/counter_three_taps.png | ✅ Pass |"
    ));
    let xml = fs::read_to_string(downloaded.join("junit.xml")).unwrap();
    assert!(
        xml.contains(r#"tests="2" failures="0" errors="0""#),
        "{xml}"
    );
    assert!(!downloaded.join("report.html").exists(), "nothing failed");
    assert_eq!(
        ReportGenerator::render_pr_comment(&cases, &MarkdownReportOptions::default()),
        "### ✅ Gleon Visual Regression: All tests passed!\n"
    );

    let entry = RunHistoryEntry::from_cases(
        cases.recorded_at().unwrap(),
        "main",
        None,
        &cases,
        &PlatformKey::parse("linux-x86_64").unwrap(),
    );
    assert_eq!((entry.summary.total, entry.summary.failed), (2, 0));
    assert!(entry.failures.is_empty(), "passing tests are only counted");
}

/// A real failing run of the Flutter example (a deleted golden, metrics off: the integration
/// records failures anyway), downloaded like a CI artifact: the HTML report shows the new
/// screenshot and `gleon approve` writes it as the golden.
#[test]
fn test_reports_and_approval_of_a_real_flutter_failure() {
    let temp = tempfile::tempdir().unwrap();
    let run = temp.path().join("download/latest");
    fs::create_dir_all(&run).unwrap();
    // The report as the integration wrote it (macOS, metrics off), verbatim.
    copy_run(&fixtures().join("cases/flutter-macos-missing"), &run);
    let candidate =
        run.join("artifacts/macos-aarch64/test/goldens/counter_three_taps/candidate.png");
    let root = temp.path().join("app");
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), &root).unwrap();
    init_workspace(&ctx).unwrap();

    let cases = Cases::load(&run, None).unwrap();
    assert!(cases.warnings().is_empty(), "{:?}", cases.warnings());
    let [missing] = cases.reports() else {
        panic!("one case: {:?}", cases.reports());
    };
    assert_eq!(missing.outcome, CaseOutcome::Missing);
    let html = ReportGenerator::generate_html(&cases, &run)
        .unwrap()
        .unwrap();
    assert!(
        html.contains("Missing Baseline: the golden does not exist yet"),
        "{html}"
    );
    assert!(html.contains("New screenshot (360x640)"));
    assert!(html.contains(
        "src=\"artifacts&#x2f;macos-aarch64&#x2f;test&#x2f;goldens&#x2f;counter_three_taps&#x2f;candidate.png\""
    ));

    let approved = approve_workspace(&ctx, &[], std::slice::from_ref(&run), None).unwrap();
    assert_eq!(
        approved_names(&approved),
        ["macos-aarch64/test/goldens/counter_three_taps"]
    );
    assert_eq!(
        fs::read(root.join("test/goldens/counter_three_taps.png")).unwrap(),
        fs::read(candidate).unwrap()
    );
}

/// Copies the directory tree `from` into `to`, restoring `.png.bin` files (candidates kept out
/// of the fixtures' screenshots) as `.png`.
fn copy_run(from: &Path, to: &Path) {
    copy_tree(from, to);
    let mut dirs = vec![to.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else if let Some(png) = path.to_str().and_then(|p| p.strip_suffix(".bin")) {
                fs::rename(&path, png).unwrap();
            }
        }
    }
}

/// A run of the Flutter example on macOS whose workspace names another `fallback_platform`
/// (`linux-x86_64`): the golden compared with text ignored passed, and kept nothing. The reports
/// name the golden, and a pass has nothing to approve.
#[test]
fn test_a_run_on_another_platform_than_the_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let run = temp.path().join("download/metrics-macos-arm64");
    fs::create_dir_all(&run).unwrap();
    copy_run(&fixtures().join("cases/flutter-macos-fallback"), &run);
    let root = temp.path().join("app");
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), &root).unwrap();
    init_workspace(&ctx).unwrap();

    let cases = Cases::load(&run, None).unwrap();
    assert_eq!(cases.run_id().unwrap().as_str(), "fallback-fixture-1");
    let [case] = cases.reports() else {
        panic!("one case: {:?}", cases.reports());
    };
    assert_eq!(case.outcome, CaseOutcome::Match);
    assert_eq!(case.golden.path, "test/goldens/counter_initial.png");
    assert_eq!(case.golden.fallback.as_ref(), Some(&case.golden.path));
    ReportGenerator::generate_all(&run, &cases).unwrap();
    let xml = fs::read_to_string(run.join("junit.xml")).unwrap();
    assert!(
        xml.contains(r#"tests="1" failures="0" errors="0""#),
        "{xml}"
    );
    assert!(
        xml.contains(
            r#"<testcase name="test&#x2f;goldens&#x2f;counter_initial" classname="macos-aarch64" file="test&#x2f;goldens&#x2f;counter_initial.png">"#
        ),
        "{xml}"
    );

    let err = approve_workspace(&ctx, &[], std::slice::from_ref(&run), None).unwrap_err();
    assert!(
        matches!(err, ApproveError::NothingToApprove { .. }),
        "{err}"
    );
}

/// A `gleon diff` run with every kind of failure: the HTML report next to the case reports links
/// images that exist, the PR comment and `JUnit` tell failures from errors.
#[test]
fn test_reports_of_a_failing_diff_run_link_existing_images() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), root).unwrap();
    init_workspace(&ctx).unwrap();
    common::copy_config(root, "ssim_and_exact");
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), root).unwrap();
    fs::create_dir_all(root.join("shots")).unwrap();
    fs::create_dir_all(root.join("ssim")).unwrap();
    for (file, fixture) in [
        ("shots/pixel.png", "dashboard_baseline.png"),
        ("ssim/dashboard.png", "dashboard_baseline.png"),
        ("shots/size.png", "baseline_100x100.png"),
        ("shots/broken.png", "baseline_100x100.png"),
    ] {
        fs::copy(fixtures().join(fixture), root.join(file)).unwrap();
    }
    stage_workspace(&ctx, None).unwrap();

    for (file, fixture) in [
        ("shots/pixel.png", "dashboard_actual.png"),
        ("ssim/dashboard.png", "dashboard_actual.png"),
        ("shots/size.png", "200x100.png"),
        ("shots/broken.png", "corrupt.png"),
        ("shots/new.png", "baseline_100x100.png"),
    ] {
        fs::copy(fixtures().join(fixture), root.join(file)).unwrap();
    }
    let result = run_diff(&ctx, &DiffOptions::default()).unwrap();
    assert_eq!((result.total_tests, result.failed_tests), (5, 5));

    let latest = root.join(".gleon/runs/latest");
    let html = fs::read_to_string(latest.join("report.html")).unwrap();
    let sources: Vec<_> = html
        .split("src=\"")
        .skip(1)
        .map(|rest| rest.split('"').next().unwrap().replace("&#x2f;", "/"))
        .collect();
    // Pixel and SSIM mismatches and the dimension mismatch: golden, candidate and diff; the new
    // screenshot: its candidate.
    assert_eq!(sources.len(), 10, "{sources:?}");
    for source in &sources {
        assert!(!source.starts_with('/'), "relative links only: {source}");
        assert!(latest.join(source).is_file(), "{source} must exist");
    }
    // Images that fail to load are replaced by a script, never by inline handlers.
    assert!(html.contains("document.addEventListener('error'"));
    assert!(!html.contains("onerror="));

    let cases = Cases::load(&latest, None).unwrap();
    let md = ReportGenerator::render_pr_comment(&cases, &MarkdownReportOptions::default());
    assert!(md.contains("(5 diffs)"), "{md}");
    assert!(
        md.contains("| `shots/broken` | Error (image): candidate image:"),
        "{md}"
    );
    assert!(md.contains("| `shots/new` | Missing Baseline: "), "{md}");
    assert!(md.contains(
        "| `shots/size` | Dimension Mismatch: golden is 100x100px, test image is 200x100px |"
    ));
    assert!(
        md.contains("| `ssim/dashboard` | Mismatch: changed area at "),
        "{md}"
    );
    let first_row = md.lines().find(|line| line.starts_with("| `")).unwrap();
    assert!(first_row.contains("Mismatch: "), "mismatches first: {md}");
    let xml = fs::read_to_string(latest.join("junit.xml")).unwrap();
    assert!(
        xml.contains(r#"tests="5" failures="4" errors="1""#),
        "{xml}"
    );
}

/// The real Linux run and the same run on macOS in one `latest/` (a macOS host and a Linux
/// container on one checkout sharing `GLEON_RUN_ID`): one run of four cases, every golden once
/// per platform, and the reports name the platforms.
#[test]
fn test_reports_of_two_platforms_in_one_run() {
    let temp = tempfile::tempdir().unwrap();
    let run = temp.path().join("latest");
    fs::create_dir_all(&run).unwrap();
    copy_tree(&fixtures().join("cases/flutter-linux-x64"), &run);
    let linux = run.join("cases/linux-x86_64/test/goldens");
    let macos = run.join("cases/macos-aarch64/test/goldens");
    fs::create_dir_all(&macos).unwrap();
    for entry in fs::read_dir(&linux).unwrap() {
        let path = entry.unwrap().path();
        let mut report: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        report["platform"] = serde_json::json!({"os": "macos", "arch": "aarch64"});
        fs::write(macos.join(path.file_name().unwrap()), report.to_string()).unwrap();
    }

    let cases = Cases::load(&run, None).unwrap();
    assert_eq!(cases.run_id().unwrap().as_str(), "36918116203-1");
    assert!(cases.spans_platforms());
    let keys: Vec<_> = cases
        .reports()
        .iter()
        .map(|r| format!("{}/{}", r.platform_key().unwrap(), r.name))
        .collect();
    assert_eq!(
        keys,
        [
            "linux-x86_64/test/goldens/counter_initial",
            "macos-aarch64/test/goldens/counter_initial",
            "linux-x86_64/test/goldens/counter_three_taps",
            "macos-aarch64/test/goldens/counter_three_taps",
        ]
    );

    ReportGenerator::generate_all(&run, &cases).unwrap();
    let xml = fs::read_to_string(run.join("junit.xml")).unwrap();
    assert!(xml.contains(r#"tests="4" failures="0""#), "{xml}");
    for platform in ["linux-x86_64", "macos-aarch64"] {
        assert!(
            xml.contains(&format!(
                r#"<testcase name="test&#x2f;goldens&#x2f;counter_initial" classname="{platform}""#
            )),
            "{xml}"
        );
    }
    let md = fs::read_to_string(run.join("report.md")).unwrap();
    assert!(md.contains("**Total Tests:** 4\n**Failed:** 0"), "{md}");
    for platform in ["linux-x86_64", "macos-aarch64"] {
        assert!(
            md.contains(&format!(
                "| test/goldens/counter_three_taps ({platform}) | test/goldens/counter_three_taps.png | ✅ Pass |"
            )),
            "{md}"
        );
    }
}

/// Two platforms that ran apart into one `latest/` (the real Linux CI run and the macOS run on
/// another platform than the fallback, each its own run id): both are read, each its own run,
/// with a warning; the reports name the platforms, and one run id reads one.
#[test]
fn test_two_platforms_of_different_runs_in_one_latest() {
    let temp = tempfile::tempdir().unwrap();
    let run = temp.path().join("latest");
    fs::create_dir_all(&run).unwrap();
    copy_run(&fixtures().join("cases/flutter-linux-x64"), &run);
    copy_run(&fixtures().join("cases/flutter-macos-fallback"), &run);

    let cases = Cases::load(&run, None).unwrap();
    let keyed: Vec<_> = cases
        .keyed()
        .map(|(key, report)| format!("{key}/{}", report.name))
        .collect();
    assert_eq!(
        keyed,
        [
            "linux-x86_64/test/goldens/counter_initial",
            "macos-aarch64/test/goldens/counter_initial",
            "linux-x86_64/test/goldens/counter_three_taps",
        ]
    );
    assert_eq!(cases.run_id(), None, "two runs");
    let warning = "platforms come from different runs (linux-x86_64: 36918116203-1, \
                   macos-aarch64: fallback-fixture-1): set one GLEON_RUN_ID for every platform \
                   to read them as one run";
    assert_eq!(cases.warnings(), [warning]);

    ReportGenerator::generate_all(&run, &cases).unwrap();
    let md = fs::read_to_string(run.join("report.md")).unwrap();
    assert!(md.contains("**Total Tests:** 3\n**Failed:** 0"), "{md}");
    assert!(md.contains("platforms come from different runs"), "{md}");
    assert!(
        md.contains("| test/goldens/counter_initial (macos-aarch64) |"),
        "{md}"
    );
    let xml = fs::read_to_string(run.join("junit.xml")).unwrap();
    assert!(
        xml.contains(
            r#"<testcase name="test&#x2f;goldens&#x2f;counter_initial" classname="macos-aarch64""#
        ),
        "{xml}"
    );

    let linux = gleon_core::case::RunId::new("36918116203-1").unwrap();
    let one = Cases::load(&run, Some(&linux)).unwrap();
    assert_eq!(one.reports().len(), 2);
    assert!(!one.spans_platforms());
    assert!(one.warnings().is_empty());
}

/// Two platforms of an integration in one `latest/` (the real missing golden on macOS, and the
/// same report moved to Linux) in a workspace that tracks its goldens on macOS
/// (`macos_fallback.yaml`, read from disk): approving writes macOS's candidate and skips Linux's
/// with a warning.
#[test]
fn test_approve_writes_the_goldens_of_the_fallback_platform_only() {
    let temp = tempfile::tempdir().unwrap();
    let run = temp.path().join("latest");
    fs::create_dir_all(&run).unwrap();
    copy_run(&fixtures().join("cases/flutter-macos-missing"), &run);
    copy_run(&fixtures().join("cases/flutter-linux-missing"), &run);
    let name = "test/goldens/counter_three_taps";
    let root = temp.path().join("app");
    common::copy_config(&root, "macos_fallback");
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), &root).unwrap();
    init_workspace(&ctx).unwrap();

    let approved = approve_workspace(&ctx, &[], std::slice::from_ref(&run), None).unwrap();
    assert_eq!(
        approved_names(&approved),
        ["macos-aarch64/test/goldens/counter_three_taps"]
    );
    assert!(
        approved
            .warnings
            .last()
            .is_some_and(|skipped| skipped.contains("linux-x86_64/test/goldens/counter_three_taps")),
        "{:?}",
        approved.warnings
    );
    assert_eq!(
        fs::read(root.join(format!("{name}.png"))).unwrap(),
        fs::read(run.join(format!("artifacts/macos-aarch64/{name}/candidate.png"))).unwrap()
    );
    assert!(!root.join("test/goldens/linux-x86_64").exists());
}

/// Every committed case report fixture is a valid report of this schema version.
#[test]
fn test_case_report_fixtures_are_valid() {
    let mut files = vec![fixtures().join("cases")];
    let mut reports = 0;
    while let Some(path) = files.pop() {
        if path.is_dir() {
            files.extend(fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
        } else if path.extension().is_some_and(|ext| ext == "json") {
            let bytes = fs::read(&path).unwrap();
            gleon_core::case::CaseReport::parse(&bytes)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            reports += 1;
        }
    }
    assert!(reports > 0);
}
