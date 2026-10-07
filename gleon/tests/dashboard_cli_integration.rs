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

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::tempdir;

fn core_fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../gleon-core/tests/fixtures")
}

/// Puts the case reports of the real Flutter run of the fixtures into the workspace as the run
/// `run_id`, recorded `offset_secs` after the fixture run (later runs get later offsets).
fn record_run(workspace: &Path, run_id: &str, offset_secs: i64) {
    let from = core_fixtures().join("cases/flutter-linux-x64/cases/linux-x86_64/test/goldens");
    let to = workspace.join(".gleon/runs/latest/cases/linux-x86_64/test/goldens");
    std::fs::create_dir_all(&to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let mut report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(entry.path()).unwrap()).unwrap();
        report["run_id"] = run_id.into();
        let recorded_at =
            chrono::DateTime::parse_from_rfc3339(report["recorded_at"].as_str().unwrap()).unwrap()
                + chrono::TimeDelta::seconds(offset_secs);
        report["recorded_at"] = recorded_at.to_rfc3339().into();
        std::fs::write(to.join(entry.file_name()), report.to_string()).unwrap();
    }
}

fn dashboard(workspace: &Path) -> Command {
    let mut cmd = Command::cargo_bin("gleon").unwrap();
    cmd.current_dir(workspace)
        .env_remove("GLEON_RUN_ID")
        .env_remove("GLEON_STORAGE_URL")
        .env_remove("GLEON_ARTIFACTS_DIR")
        .arg("dashboard");
    cmd
}

fn history(workspace: &Path) -> gleon_core::dashboard::DashboardHistory {
    let raw = std::fs::read_to_string(workspace.join(".gleon/history.json")).unwrap();
    gleon_core::dashboard::DashboardHistory::parse_or_empty(&raw, "test").unwrap()
}

#[test]
fn test_cli_dashboard_end_to_end() {
    let temp = tempdir().unwrap();
    let workspace = temp.path();

    // 1. Uninitialized workspace fails
    dashboard(workspace)
        .assert()
        .failure()
        .stderr(predicate::str::contains("Workspace not initialized"));

    // 2. Initialize the workspace; without case reports there is no run to add.
    Command::cargo_bin("gleon")
        .unwrap()
        .current_dir(workspace)
        .arg("init")
        .assert()
        .success();
    dashboard(workspace)
        .assert()
        .failure()
        .stderr(predicate::str::contains("No case reports found"));

    // 3. The run of a Flutter test run, without `gleon diff`
    record_run(workspace, "r1", 0);
    dashboard(workspace)
        .assert()
        .success()
        .stdout(predicate::str::contains("dashboard.html"))
        .stderr(predicate::str::contains("Dashboard compiled successfully"));
    let html_content = std::fs::read_to_string(workspace.join(".gleon/dashboard.html")).unwrap();
    assert!(html_content.contains("Gleon Regression History"));
    assert!(html_content.contains("2</strong> / 2 passed"));
    let first = history(workspace);
    assert_eq!(first.runs.len(), 1);
    assert_eq!(first.runs[0].platforms.len(), 1);
    assert_eq!(first.runs[0].platforms[0], "linux-x86_64");

    // 4. Test --truncate-history 0 fails validation
    dashboard(workspace)
        .args(["--truncate-history", "0"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("zero"));

    // 5. Every run is recorded once; --truncate-history 2 keeps the newest two by time (the ids
    //    sort the other way)
    for (run_id, offset_secs) in [("r4", 1), ("r3", 2), ("r2", 3), ("r2", 3)] {
        record_run(workspace, run_id, offset_secs);
        dashboard(workspace)
            .args(["--truncate-history", "2"])
            .assert()
            .success();
    }
    let ids: Vec<_> = history(workspace)
        .runs
        .into_iter()
        .map(|run| run.id)
        .collect();
    assert_eq!(ids, ["r3/linux-x86_64", "r2/linux-x86_64"]);

    // 6. Test --push fails fast without storage configuration
    dashboard(workspace)
        .arg("--push")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Storage not configured"));

    // 7. Test --push with remote storage
    let remote_dir = temp.path().join("remote_bucket");
    std::fs::create_dir_all(&remote_dir).unwrap();
    dashboard(workspace)
        .env(
            "GLEON_STORAGE_URL",
            format!("file://{}", remote_dir.display()),
        )
        .arg("--push")
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "Successfully uploaded history.json and dashboard.html",
        ));
    assert!(remote_dir.join("history.json").is_file());
    assert!(remote_dir.join("dashboard.html").is_file());

    // 8. --from pointing to no run, or to a run without reports, fails fast
    dashboard(workspace)
        .args(["--from", "no_such_dir/latest"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("'no_such_dir/latest' is no run"));
    std::fs::create_dir_all(workspace.join("empty/latest/cases")).unwrap();
    dashboard(workspace)
        .args(["--from", "empty/latest"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("No case reports found"));

    // 9. A nested subdirectory resolves the run of the workspace
    let subfolder = workspace.join("packages").join("app");
    std::fs::create_dir_all(&subfolder).unwrap();
    dashboard(&subfolder)
        .assert()
        .success()
        .stdout(predicate::str::contains("dashboard.html"));
}

#[test]
fn test_cli_dashboard_large_history_merge_and_truncate() {
    let temp = tempdir().unwrap();
    let workspace = temp.path();
    Command::cargo_bin("gleon")
        .unwrap()
        .current_dir(workspace)
        .arg("init")
        .assert()
        .success();
    record_run(workspace, "ci-201", 1);
    std::fs::copy(
        core_fixtures().join("sample_history.json"),
        workspace.join(".gleon/history.json"),
    )
    .unwrap();
    assert_eq!(history(workspace).runs.len(), 200);

    dashboard(workspace)
        .args(["--truncate-history", "50"])
        .assert()
        .success();

    // The 200 runs plus the new one, truncated to the newest 50.
    let truncated = history(workspace);
    assert_eq!(truncated.runs.len(), 50);
    assert!(
        truncated
            .runs
            .iter()
            .any(|run| run.id == "ci-201/linux-x86_64")
    );
}
