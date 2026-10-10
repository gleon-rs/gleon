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
        ApproveResult, approve_workspace, diff::DiffOptions, init_workspace, run_diff,
        stage_workspace,
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

/// The case report the gleon Flutter package wrote for a deleted golden of its example (macOS,
/// metrics off), verbatim.
const FLUTTER_MISSING_CASE: &str = r#"{
  "schema_version": 4,
  "name": "test/goldens/counter_three_taps",
  "golden": {
    "path": "test/goldens/counter_three_taps.png"
  },
  "candidate": {
    "sha256": "f80a2bb819104e3624d4e9a154cf10fdbd62b59fc96886c296836400572692a4",
    "width": 360,
    "height": 640
  },
  "source": {
    "tool": "gleon_flutter",
    "tool_version": "0.1.0",
    "renderer": "flutter-3.47.5"
  },
  "platform": {
    "os": "macos",
    "arch": "aarch64"
  },
  "test": {
    "name": "matches the golden after three taps"
  },
  "comparison": {
    "tolerance": {
      "kind": "ssim",
      "min_similarity": 0.73,
      "color_tolerance": 46.0
    },
    "masks": [
      {
        "x": 280,
        "y": 560,
        "width": 80,
        "height": 80
      }
    ],
    "policy_version": 2
  },
  "outcome": "missing",
  "regions": [],
  "artifacts": {
    "candidate": ".gleon/runs/latest/artifacts/macos-aarch64/test/goldens/counter_three_taps/candidate.png"
  },
  "timings_ms": {
    "total": 0.3
  },
  "run_id": "flutter-missing-macos",
  "recorded_at": "2026-10-02T06:48:23.128489Z"
}"#;

/// A real failing run of the Flutter example (a deleted golden, metrics off: the integration
/// records failures anyway), downloaded like a CI artifact: the HTML report shows the new
/// screenshot and `gleon approve` writes it as the golden.
#[test]
fn test_reports_and_approval_of_a_real_flutter_failure() {
    let temp = tempfile::tempdir().unwrap();
    let run = temp.path().join("download/latest");
    fs::create_dir_all(&run).unwrap();
    // The report as the integration wrote it, its candidate stored as `.png.bin` so the fixtures
    // hold no stray screenshot.
    let report = run.join("cases/macos-aarch64/test/goldens/counter_three_taps.json");
    fs::create_dir_all(report.parent().unwrap()).unwrap();
    fs::write(report, FLUTTER_MISSING_CASE).unwrap();
    let candidate =
        run.join("artifacts/macos-aarch64/test/goldens/counter_three_taps/candidate.png");
    fs::create_dir_all(candidate.parent().unwrap()).unwrap();
    fs::copy(
        fixtures().join("flutter_missing_candidate.png.bin"),
        &candidate,
    )
    .unwrap();
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

/// A real run of the Flutter example on macOS whose workspace names another
/// `fallback_platform` (`linux-x86_64`): the golden compared with the shared one, text ignored,
/// passed and kept its candidate. The reports say which golden was compared, and
/// approving by the shared golden's path (what the test printed) records this platform's own.
#[test]
fn test_a_real_run_against_the_fallback_seeds_per_platform_goldens() {
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
    assert_eq!(
        case.golden.path,
        "test/goldens/macos-aarch64/counter_initial.png"
    );
    assert_eq!(case.golden.compared(), "test/goldens/counter_initial.png");
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
        "the compared golden, not the own one that does not exist yet: {xml}"
    );
    assert!(
        !xml.contains("goldens&#x2f;macos-aarch64"),
        "the own golden does not exist yet: {xml}"
    );

    let approved = approve_workspace(
        &ctx,
        &[Path::new("test/goldens/counter_initial.png").to_path_buf()],
        std::slice::from_ref(&run),
        None,
    )
    .unwrap();
    assert_eq!(
        approved_names(&approved),
        ["macos-aarch64/test/goldens/counter_initial"]
    );
    assert_eq!(
        fs::read(root.join("test/goldens/macos-aarch64/counter_initial.png")).unwrap(),
        fs::read(run.join("artifacts/macos-aarch64/test/goldens/counter_initial/candidate.png"))
            .unwrap()
    );
    assert!(
        !root.join("test/goldens/counter_initial.png").exists(),
        "the shared golden is the fallback platform's"
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

/// Two platforms that ran apart into one `latest/` (the real Linux CI run and the real macOS run
/// against the fallback, each its own run id): both are read, each its own run, with a warning;
/// the reports name the platforms, `gleon approve` approves from both, and one run id reads one.
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

    let root = temp.path().join("app");
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), &root).unwrap();
    init_workspace(&ctx).unwrap();
    let approved = approve_workspace(&ctx, &[], std::slice::from_ref(&run), None).unwrap();
    assert_eq!(
        approved_names(&approved),
        ["macos-aarch64/test/goldens/counter_initial"],
        "the Linux run passed against its own goldens"
    );
    assert_eq!(approved.warnings, [warning]);
    assert!(
        root.join("test/goldens/macos-aarch64/counter_initial.png")
            .is_file()
    );

    let linux = gleon_core::case::RunId::new("36918116203-1").unwrap();
    let one = Cases::load(&run, Some(&linux)).unwrap();
    assert_eq!(one.reports().len(), 2);
    assert!(!one.spans_platforms());
    assert!(one.warnings().is_empty());
}

/// Two platforms of one integration run in one `latest/` (the real macOS run against the
/// fallback, and the same case recorded on Linux), each comparing the shared golden: approving
/// writes each platform's own golden.
#[test]
fn test_approve_writes_the_goldens_of_two_platforms_of_an_integration() {
    let temp = tempfile::tempdir().unwrap();
    let run = temp.path().join("latest");
    fs::create_dir_all(&run).unwrap();
    copy_run(&fixtures().join("cases/flutter-macos-fallback"), &run);
    let name = "test/goldens/counter_initial";
    let macos_report = run.join(format!("cases/macos-aarch64/{name}.json"));
    let mut linux: serde_json::Value =
        serde_json::from_slice(&fs::read(&macos_report).unwrap()).unwrap();
    linux["platform"] = serde_json::json!({"os": "linux", "arch": "x86_64"});
    linux["golden"]["path"] = "test/goldens/linux-x86_64/counter_initial.png".into();
    linux["artifacts"]["candidate"] =
        format!(".gleon/runs/latest/artifacts/linux-x86_64/{name}/candidate.png").into();
    let linux_report = run.join(format!("cases/linux-x86_64/{name}.json"));
    fs::create_dir_all(linux_report.parent().unwrap()).unwrap();
    fs::write(&linux_report, linux.to_string()).unwrap();
    let candidate = |key: &str| run.join(format!("artifacts/{key}/{name}/candidate.png"));
    fs::create_dir_all(candidate("linux-x86_64").parent().unwrap()).unwrap();
    fs::copy(candidate("macos-aarch64"), candidate("linux-x86_64")).unwrap();

    let root = temp.path().join("app");
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), &root).unwrap();
    init_workspace(&ctx).unwrap();
    let approved = approve_workspace(&ctx, &[], std::slice::from_ref(&run), None).unwrap();
    assert_eq!(
        approved_names(&approved),
        [
            "linux-x86_64/test/goldens/counter_initial",
            "macos-aarch64/test/goldens/counter_initial"
        ]
    );
    assert!(approved.warnings.is_empty(), "{:?}", approved.warnings);
    for key in ["linux-x86_64", "macos-aarch64"] {
        assert_eq!(
            fs::read(root.join(format!("test/goldens/{key}/counter_initial.png"))).unwrap(),
            fs::read(candidate(key)).unwrap(),
            "{key}"
        );
    }
    assert!(!root.join("test/goldens/counter_initial.png").exists());
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
