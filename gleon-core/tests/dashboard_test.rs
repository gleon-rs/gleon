//! Integration tests for Dashboard Compiler and History Tracker.

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

use std::{num::NonZeroUsize, path::Path};

use gleon_core::{
    case::{CaseOutcome, Metrics, RunId},
    cases::Cases,
    context::{ContextOptions, ResolvedContext},
    dashboard::{
        DashboardCompiler, DashboardError, DashboardHistory, DashboardOptions, RunHistoryEntry,
    },
    paths::GleonPaths,
    platform::PlatformKey,
    storage::{ObjectStoreAdapter, StorageConfig},
};

fn key(key: &str) -> PlatformKey {
    PlatformKey::parse(key).unwrap()
}

fn fixtures() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// The real Flutter run of the fixtures as the run `run_id`, its second case (SSIM) turned into
/// a mismatch below its similarity threshold: one passing and one failing test.
fn run_with_a_failure(run_id: &str) -> Cases {
    run_with_a_failure_at(run_id, chrono::TimeDelta::zero())
}

/// [`run_with_a_failure`] recorded `offset` after the fixture run, so runs can be ordered in time.
fn run_with_a_failure_at(run_id: &str, offset: chrono::TimeDelta) -> Cases {
    let mut reports = Cases::load(&fixtures().join("cases/flutter-linux-x64"), None)
        .unwrap()
        .reports()
        .to_vec();
    let failing = &mut reports[1];
    failing.outcome = CaseOutcome::Mismatch;
    if let Some(Metrics::Ssim {
        min_ssim, headroom, ..
    }) = &mut failing.metrics
    {
        *min_ssim = 0.164;
        headroom.similarity = 0.164 - 0.73;
    }
    let temp = tempfile::tempdir().unwrap();
    for report in &mut reports {
        report.run_id = Some(RunId::new(run_id).unwrap());
        report.recorded_at += offset;
        let file = temp
            .path()
            .join("cases")
            .join(report.platform_key().unwrap())
            .join(format!("{}.json", report.name));
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, serde_json::to_vec(&*report).unwrap()).unwrap();
    }
    Cases::load(temp.path(), None).unwrap()
}

#[tokio::test]
async fn test_dashboard_compilation_and_history_lifecycle() {
    let temp = tempfile::tempdir().unwrap();
    let base_dir = temp.path();
    let paths = GleonPaths::new(base_dir);

    let remote_dir = temp.path().join("remote_store");
    std::fs::create_dir_all(&remote_dir).unwrap();
    let storage_cfg = StorageConfig::new(format!("file://{}", remote_dir.display()));

    let options = ContextOptions {
        branch: Some("feature/checkout".to_string()),
        ..Default::default()
    };
    let ctx = ResolvedContext::from_options(&options, base_dir).unwrap();

    // Run 1: Compile and push to remote storage with truncate limit 2
    let dash_opts = DashboardOptions {
        truncate_limit: NonZeroUsize::new(2),
        push_to_storage: true,
        ..Default::default()
    };
    let res1 = DashboardCompiler::execute(
        &paths,
        &ctx,
        &run_with_a_failure("ci-1"),
        &dash_opts,
        Some(&storage_cfg),
    )
    .await
    .unwrap();

    assert_eq!(res1.total_runs, 1);
    assert!(res1.pushed);
    assert!(paths.history_file().is_file());
    assert!(paths.dashboard_file().is_file());

    // Verify dashboard HTML content and visual assets
    let html1 = std::fs::read_to_string(paths.dashboard_file()).unwrap();
    assert!(html1.contains("<!DOCTYPE html>"));
    assert!(html1.contains("Gleon Regression History"));
    assert!(html1.contains("<svg class=\"svg-chart\""));
    assert!(html1.contains("feature&#x2f;checkout"));
    assert!(html1.contains("test&#x2f;goldens&#x2f;counter_three_taps"));
    assert!(
        !html1.contains("counter_initial"),
        "passes are only counted"
    );
    assert!(html1.contains("Min local SSIM: 0.1640"));
    assert!(html1.contains("linux-x86_64"), "the platform of the cases");
    assert!(html1.contains("0.0%")); // Run pass rate (0 of 1 runs passed)
    assert!(html1.contains("50.0%")); // Test pass rate (1 of 2 tests passed)
    assert!(html1.contains("1</strong> / 2 passed"));

    // Verify history.json on disk
    let history_json1 = std::fs::read_to_string(paths.history_file()).unwrap();
    let history1 = DashboardHistory::parse_or_empty(&history_json1, "test").unwrap();
    assert_eq!(history1.schema_version, 3);
    assert_eq!(history1.runs[0].failures[0].platform, "linux-x86_64");
    assert_eq!(history1.runs.len(), 1);
    let run = &history1.runs[0];
    assert_eq!(run.id, "ci-1/linux-x86_64");
    assert_eq!(run.platforms, [key("linux-x86_64")]);
    assert_eq!(run.run_id.as_ref().map(RunId::as_str), Some("ci-1"));
    assert_eq!((run.summary.total, run.summary.failed), (2, 1));
    assert_eq!(
        run.timestamp.to_rfc3339(),
        "2026-10-01T20:00:32.111489108+00:00",
        "the time of the newest case"
    );

    // Verify remote storage received both assets
    let adapter = ObjectStoreAdapter::from_config(&storage_cfg).unwrap();
    for name in ["history.json", "dashboard.html"] {
        let object = adapter.get_object(name).await.unwrap();
        assert!(object.is_some_and(|o| !o.bytes.is_empty()), "{name}");
    }

    // Runs 2 and 3 on main; the third truncates the history to 2 runs.
    let ctx_main = ResolvedContext::from_options(
        &ContextOptions {
            branch: Some("main".to_string()),
            ..Default::default()
        },
        base_dir,
    )
    .unwrap();
    // Later runs, so truncation keeps the newest by time (not by id).
    for (run_id, minutes, total_runs) in [("ci-3", 1, 2), ("ci-2", 2, 2)] {
        let res = DashboardCompiler::execute(
            &paths,
            &ctx_main,
            &run_with_a_failure_at(run_id, chrono::TimeDelta::minutes(minutes)),
            &dash_opts,
            Some(&storage_cfg),
        )
        .await
        .unwrap();
        assert_eq!(res.total_runs, total_runs, "{run_id}");
    }
    let history3 = DashboardHistory::parse_or_empty(
        &std::fs::read_to_string(paths.history_file()).unwrap(),
        "test",
    )
    .unwrap();
    let ids: Vec<_> = history3.runs.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(
        ids,
        ["ci-3/linux-x86_64", "ci-2/linux-x86_64"],
        "oldest first"
    );
}

/// The history of `tests/fixtures/sample_history.json`: 200 runs on two branches and three
/// platforms, every 25th with a failure of each kind.
fn sample_history() -> DashboardHistory {
    use gleon_core::{
        case::{CaseErrorKind, SsimHeadroom},
        dashboard::{RunSummary, TestHistoryEntry},
    };

    // The platform is the run's (see below).
    let failure = |name: &str, outcome, message: Option<&str>| TestHistoryEntry {
        name: name.to_owned(),
        platform: key("linux-x86_64"),
        outcome,
        error_kind: (outcome == CaseOutcome::Error).then_some(CaseErrorKind::Image),
        metrics: None,
        message: message.map(str::to_owned),
    };
    let failures = [
        TestHistoryEntry {
            metrics: Some(Metrics::Pixel {
                total_pixels: 60_000,
                diff_pixels: 128,
                tolerated_pixels: 0,
                edge_pixels: 0,
                diff_ratio: 128.0 / 60_000.0,
                headroom: -128.0 / 60_000.0,
                text: None,
            }),
            ..failure(
                "cart/checkout",
                CaseOutcome::Mismatch,
                Some("0.2133% (128 of 60000px) differ"),
            )
        },
        TestHistoryEntry {
            metrics: Some(Metrics::Ssim {
                min_ssim: 0.5,
                mean_ssim: 0.99,
                max_excess: 0.0,
                peak_excess: 3.5,
                changed_pixels: 120,
                changed_region: None,
                failing_pixels: 40,
                failing_region: None,
                headroom: SsimHeadroom {
                    similarity: -0.3,
                    color: 4.5,
                },
                text: None,
            }),
            ..failure(
                "cart/header",
                CaseOutcome::Mismatch,
                Some("min local SSIM 0.500"),
            )
        },
        failure(
            "cart/item",
            CaseOutcome::DimensionMismatch,
            Some("golden is 100x60px, test image is 100x61px"),
        ),
        failure(
            "profile/avatar",
            CaseOutcome::Error,
            Some("candidate image: invalid PNG signature"),
        ),
        failure(
            "settings/theme",
            CaseOutcome::Missing,
            Some("no baseline is staged for this screenshot"),
        ),
    ];
    let start = chrono::DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let runs = (1..=200_i64)
        .map(|i| {
            let platform = ["macos-aarch64", "linux-x86_64", "windows-x86_64"]
                [usize::try_from(i % 3).unwrap()];
            let failures = if i % 25 == 0 {
                failures
                    .iter()
                    .map(|failure| TestHistoryEntry {
                        platform: key(platform),
                        ..failure.clone()
                    })
                    .collect()
            } else {
                Vec::new()
            };
            RunHistoryEntry {
                id: format!("run-{i:04}"),
                timestamp: start + chrono::TimeDelta::hours(i),
                branch: ["main", "feature/checkout"][usize::try_from(i % 2).unwrap()].to_owned(),
                platforms: vec![key(platform)],
                run_id: None,
                commit_sha: (i % 25 == 0).then(|| format!("{i:040x}")),
                summary: RunSummary {
                    total: 40,
                    failed: failures.len(),
                },
                failures,
            }
        })
        .collect();
    DashboardHistory {
        runs,
        ..DashboardHistory::new()
    }
}

/// The committed fixture is exactly [`sample_history`], so it is changed by changing the code.
#[test]
fn test_history_fixture_is_generated() {
    let raw = std::fs::read_to_string(fixtures().join("sample_history.json")).unwrap();
    assert_eq!(
        DashboardHistory::parse_or_empty(&raw, "test").unwrap(),
        sample_history(),
        "regenerate: cargo test -p gleon-core --test dashboard_test -- --ignored \
         regenerate_history_fixture"
    );
}

#[test]
#[ignore = "rewrites tests/fixtures/sample_history.json from sample_history()"]
fn regenerate_history_fixture() {
    let json = serde_json::to_string_pretty(&sample_history()).unwrap() + "\n";
    std::fs::write(fixtures().join("sample_history.json"), json).unwrap();
}

/// The committed `history.json` of 200 runs reads, takes one more run, drops the oldest and
/// reads back the same.
#[test]
fn test_history_fixture_round_trip_and_truncation() {
    let mut history = sample_history();
    let cases = run_with_a_failure("ci-new");
    let entry = RunHistoryEntry::from_cases(
        cases.recorded_at().unwrap(),
        "main",
        None,
        &cases,
        &key("linux-x86_64"),
    );
    assert_eq!(entry.id, "ci-new/linux-x86_64");
    history.append_run(entry.clone(), NonZeroUsize::new(200));
    assert_eq!(history.runs.len(), 200);
    assert_eq!(history.runs[0].id, "run-0002", "the oldest run is dropped");
    assert_eq!(history.runs.last(), Some(&entry));
    assert_eq!(entry.failures.len(), 1, "only failures are kept");

    let json = serde_json::to_string_pretty(&history).unwrap();
    assert_eq!(
        DashboardHistory::parse_or_empty(&json, "test").unwrap(),
        history
    );
    let html = DashboardCompiler::compile_dashboard(&history).unwrap();
    assert!(html.contains("dimension_mismatch"));
    assert!(html.contains("error (image)"));
    assert!(html.contains("Last 30 runs"));
}

#[tokio::test]
async fn test_dashboard_custom_output_path() {
    let temp = tempfile::tempdir().unwrap();
    let base_dir = temp.path();
    let paths = GleonPaths::new(base_dir);
    let custom_html_out = base_dir.join("custom_reports").join("dash.html");

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_dir).unwrap();
    let opts = DashboardOptions {
        out_html: Some(&custom_html_out),
        ..Default::default()
    };

    let res = DashboardCompiler::execute(&paths, &ctx, &run_with_a_failure("ci"), &opts, None)
        .await
        .unwrap();

    assert_eq!(res.html_path, custom_html_out);
    assert!(custom_html_out.is_file());
}

#[tokio::test]
async fn test_dashboard_push_without_storage_fails_fast() {
    let temp = tempfile::tempdir().unwrap();
    let paths = GleonPaths::new(temp.path());
    let opts = DashboardOptions {
        push_to_storage: true,
        ..Default::default()
    };

    let err = DashboardCompiler::execute(
        &paths,
        &ResolvedContext::default(),
        &run_with_a_failure("ci"),
        &opts,
        None,
    )
    .await;
    assert!(matches!(err, Err(DashboardError::StorageNotConfigured)));
}

/// `gleon dashboard` on a joint run of two platforms (one run id) while its platforms arrive: the
/// entry of the first platform is replaced by the entry of both, which names both.
#[tokio::test]
async fn test_dashboard_of_a_joint_run() {
    let temp = tempfile::tempdir().unwrap();
    let paths = GleonPaths::new(temp.path());
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), temp.path()).unwrap();
    let run = temp.path().join("download/latest");
    let linux = fixtures().join("cases/flutter-linux-x64/cases/linux-x86_64/test/goldens");
    let platform_dir = |key: &str| run.join("cases").join(key).join("test/goldens");
    // The macOS half of the run first: the Linux reports, recorded on macOS.
    std::fs::create_dir_all(platform_dir("macos-aarch64")).unwrap();
    for entry in std::fs::read_dir(&linux).unwrap() {
        let path = entry.unwrap().path();
        let mut report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        report["platform"] = serde_json::json!({"os": "macos", "arch": "aarch64"});
        std::fs::write(
            platform_dir("macos-aarch64").join(path.file_name().unwrap()),
            report.to_string(),
        )
        .unwrap();
    }
    let record = || async {
        DashboardCompiler::execute(
            &paths,
            &ctx,
            &Cases::load(&run, None).unwrap(),
            &DashboardOptions::default(),
            None,
        )
        .await
        .unwrap();
        DashboardHistory::parse_or_empty(
            &std::fs::read_to_string(paths.history_file()).unwrap(),
            "test",
        )
        .unwrap()
    };
    let first = record().await;
    assert_eq!(first.runs.len(), 1);
    assert_eq!(first.runs[0].id, "36918116203-1/macos-aarch64");

    std::fs::create_dir_all(platform_dir("linux-x86_64")).unwrap();
    for entry in std::fs::read_dir(&linux).unwrap() {
        let path = entry.unwrap().path();
        std::fs::copy(
            &path,
            platform_dir("linux-x86_64").join(path.file_name().unwrap()),
        )
        .unwrap();
    }
    let joint = record().await;
    assert_eq!(joint.runs.len(), 1, "{:?}", joint.runs);
    let entry = &joint.runs[0];
    assert_eq!(entry.id, "36918116203-1/linux-x86_64,macos-aarch64");
    assert_eq!(entry.platforms, [key("linux-x86_64"), key("macos-aarch64")]);
    assert_eq!((entry.summary.total, entry.summary.failed), (4, 0));
    let html = std::fs::read_to_string(paths.dashboard_file()).unwrap();
    assert!(
        html.contains(r#"data-platforms="linux-x86_64 macos-aarch64""#),
        "{html}"
    );
}
