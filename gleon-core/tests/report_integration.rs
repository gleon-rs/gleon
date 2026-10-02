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

use std::{fs, path::Path};

use gleon_core::{
    case::{CaseOutcome, Metrics},
    cases::Cases,
    context::{ContextOptions, ResolvedContext},
    dashboard::RunHistoryEntry,
    ops::{approve_workspace, diff::DiffOptions, init_workspace, run_diff, stage_workspace},
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
        "run",
        cases.recorded_at().unwrap(),
        "main",
        "linux-x86_64",
        None,
        &cases,
    );
    assert_eq!((entry.summary.total, entry.summary.failed), (2, 0));
    assert!(entry.failures.is_empty(), "passing tests are only counted");
}

/// The case report the gleon Flutter package wrote for a deleted golden of its example (macOS,
/// metrics off), verbatim.
const FLUTTER_MISSING_CASE: &str = r#"{
  "schema_version": 2,
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
    "candidate": ".gleon/runs/latest/artifacts/test/goldens/counter_three_taps/candidate.png"
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
    let report = run.join("cases/test/goldens/counter_three_taps.json");
    fs::create_dir_all(report.parent().unwrap()).unwrap();
    fs::write(report, FLUTTER_MISSING_CASE).unwrap();
    let candidate = run.join("artifacts/test/goldens/counter_three_taps/candidate.png");
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
        "src=\"artifacts&#x2f;test&#x2f;goldens&#x2f;counter_three_taps&#x2f;candidate.png\""
    ));

    let approved = approve_workspace(&ctx, &[], std::slice::from_ref(&run), None).unwrap();
    assert_eq!(
        approved.approved_test_cases,
        ["test/goldens/counter_three_taps"]
    );
    assert_eq!(
        fs::read(root.join("test/goldens/counter_three_taps.png")).unwrap(),
        fs::read(run.join("artifacts/test/goldens/counter_three_taps/candidate.png")).unwrap()
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
    fs::write(
        root.join(".gleon/gleon.yaml"),
        r#"
required_version: ">=0.1.0"
screenshots:
  - include: "ssim/*.png"
    mode: ssim
  - include: "shots/*.png"
    mode: pixel
    diff: { threshold: 0.0 }
"#,
    )
    .unwrap();
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
    // Pixel and SSIM mismatches: golden, candidate and diff; the dimension mismatch: two; the new
    // screenshot: its candidate.
    assert_eq!(sources.len(), 9, "{sources:?}");
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

/// A demo page kept with the fixtures: images that fail to load are replaced by a script, never
/// by inline handlers.
#[test]
fn test_fallback_demo_fixture_avoids_inline_handlers() {
    let fallback = fs::read_to_string(fixtures().join("report_output/fallback_demo.html")).unwrap();
    assert!(fallback.contains("document.addEventListener('error'"));
    assert!(!fallback.contains("onerror="));
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

/// Load time of a large run (`Cases::load`), measured on demand:
/// `cargo test --release -p gleon-core --test report_integration -- --ignored --nocapture`.
///
/// 50k case reports of the real Flutter fixture (~2 KB each) in 100 directories, M3 Max (14
/// cores), macOS on APFS, 3 reader threads (`manifest::index::READ_THREADS`): 0.91 s (50k manifests: 0.6 s).
#[test]
#[ignore = "benchmark: writes 50k files, run with --ignored --nocapture"]
fn test_loads_50k_case_reports() {
    const REPORTS: usize = 50_000;
    let temp = tempfile::tempdir().unwrap();
    let runs_latest = temp.path().join(".gleon/runs/latest");
    let template: serde_json::Value = serde_json::from_slice(
        &fs::read(
            fixtures().join("cases/flutter-linux-x64/cases/test/goldens/counter_three_taps.json"),
        )
        .unwrap(),
    )
    .unwrap();
    for i in 0..REPORTS {
        let name = format!("test/dir_{:03}/golden_{i:05}", i % 100);
        let mut report = template.clone();
        report["name"] = name.clone().into();
        let file = runs_latest.join("cases").join(format!("{name}.json"));
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, report.to_string()).unwrap();
    }
    let _warm_up = Cases::load(&runs_latest, None).unwrap();
    let started = std::time::Instant::now();
    let cases = Cases::load(&runs_latest, None).unwrap();
    println!(
        "loaded {} case reports in {:?}",
        cases.reports().len(),
        started.elapsed()
    );
    assert_eq!(cases.reports().len(), REPORTS);
}
