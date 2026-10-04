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

use std::fs;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::tempdir;

const VALID_PNG_BYTES: &[u8] =
    include_bytes!("../../gleon-core/tests/fixtures/baseline_100x100.png");

#[test]
fn test_cli_clean_dry_run_and_execution() {
    let temp = tempdir().unwrap();
    let base_path = temp.path();

    let gleon_dir = base_path.join(".gleon");
    fs::create_dir_all(&gleon_dir).unwrap();
    let runs_dir = gleon_dir.join("runs");
    fs::create_dir_all(&runs_dir).unwrap();

    let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include:
      - "test/goldens/**/*.png"
    mode: pixel
"#;
    fs::write(gleon_dir.join("gleon.yaml"), config_yaml).unwrap();

    let goldens_dir = base_path.join("test").join("goldens");
    fs::create_dir_all(&goldens_dir).unwrap();
    let golden_file = goldens_dir.join("nav.png");
    fs::write(&golden_file, VALID_PNG_BYTES).unwrap();

    // 1. A plain clean removes the run cache only: the rules may match committed goldens (an
    // integration's workspace).
    Command::cargo_bin("gleon")
        .unwrap()
        .current_dir(base_path)
        .arg("clean")
        .assert()
        .success()
        .stderr(predicate::str::contains("Cleaned the .gleon/runs cache."))
        .stderr(predicate::str::contains("screenshot(s)").not());
    assert!(golden_file.exists());
    assert!(!runs_dir.exists());
    assert!(!base_path.join(".gitignore").exists());
    fs::create_dir_all(&runs_dir).unwrap();

    // 2. Run gleon clean --screenshots --dry-run
    let mut cmd_dry = Command::cargo_bin("gleon").unwrap();
    cmd_dry
        .current_dir(base_path)
        .args(["clean", "--screenshots", "--dry-run"])
        .assert()
        .success()
        .stderr(predicate::str::contains("[dry-run]"));

    assert!(golden_file.exists());

    // 3. Run gleon clean --screenshots
    let mut cmd_exec = Command::cargo_bin("gleon").unwrap();
    cmd_exec
        .current_dir(base_path)
        .args(["clean", "--screenshots"])
        .assert()
        .success()
        .stderr(predicate::str::contains("Removed 1 screenshot(s)"));

    assert!(!golden_file.exists());
    assert!(!runs_dir.exists());

    let gitignore = fs::read_to_string(base_path.join(".gitignore")).unwrap();
    assert!(gitignore.contains("test/goldens/**/*.png"));
}
