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

//! `gleon report` on the case reports of a real Flutter run (a downloaded CI artifact), outside
//! any workspace and without `gleon diff`.

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::tempdir;

/// The `metrics-linux-x64` CI artifact of the Flutter example (a copy of `.gleon/runs/latest`).
fn flutter_run() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../gleon-core/tests/fixtures/cases/flutter-linux-x64")
}

#[test]
fn test_cli_report_supports_all_formats() {
    let temp = tempdir().unwrap();
    let run = flutter_run();
    let report = |format: &str| {
        let mut cmd = Command::cargo_bin("gleon").unwrap();
        cmd.current_dir(temp.path())
            .env_remove("GLEON_RUN_ID")
            .env_remove("GLEON_STORAGE_URL")
            .args(["report", format, "--from"])
            .arg(&run);
        cmd
    };

    report("markdown")
        .assert()
        .success()
        .stdout(predicate::str::contains("All tests passed!"));

    let html_out = temp.path().join("report.html");
    report("html").arg("-o").arg(&html_out).assert().success();
    assert!(
        std::fs::read_to_string(&html_out)
            .unwrap()
            .contains("All tests passed!")
    );

    for format in ["junit", "junit.xml", "xml"] {
        report(format)
            .assert()
            .success()
            .stdout(predicate::str::contains(
                r#"tests="2" failures="0" errors="0""#,
            ))
            .stdout(predicate::str::contains(
                "test&#x2f;goldens&#x2f;counter_three_taps",
            ));
    }

    let json = report("json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let reports: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(reports.as_array().unwrap().len(), 2);
    assert_eq!(reports[1]["outcome"], "match");
    assert_eq!(reports[1]["run_id"], "36918116203-1");

    // The caller's run id names the caller's run, not a downloaded one.
    report("markdown")
        .env("GLEON_RUN_ID", "another-run")
        .assert()
        .success()
        .stdout(predicate::str::contains("All tests passed!"));
}
