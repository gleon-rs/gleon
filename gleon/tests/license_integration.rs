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

use assert_cmd::Command;
use predicates::prelude::*;

mod common;

#[test]
fn test_cli_blocks_execution_in_private_ci_without_valid_license_or_timestamp() {
    // This test simulates running the gleon CLI in a private CI environment.
    // Because GLEON_BUILD_TIMESTAMP is not set to a valid future timestamp (or > 0)
    // during local `cargo test`, `is_valid_official_build` will evaluate to false.
    // In a generic private CI context, this must trigger an exit 42 with compliance error.

    let mut cmd = Command::cargo_bin("gleon").unwrap();
    common::clear_env(&mut cmd);

    // Mock Generic CI environment (private repository)
    cmd.env("CI", "true");
    cmd.env("CI_PROJECT_PATH", "myorg/private-repo");

    // Explicitly zero out the timestamp in case the local environment leaked it
    cmd.env("GLEON_BUILD_TIMESTAMP", "0");

    // Provide a dummy invalid license key
    cmd.env("GLEON_LICENSE_KEY", "invalid_base64_or_signature");

    // Attempt to run the CLI (e.g. status)
    let assert = cmd.arg("--strict").arg("status").assert();

    // The CLI should fail with exit code 42 due to UnofficialBuildInPrivateCI
    // or ExpiredUnlicensedBinary
    assert.failure().code(42).stderr(predicate::str::contains(
        "[GLEON COMPLIANCE ERROR] Execution blocked.",
    ));
}

#[test]
fn test_cli_allows_execution_in_local_dev() {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut cmd = Command::cargo_bin("gleon").unwrap();
    cmd.current_dir(temp_dir.path());
    common::clear_env(&mut cmd);
    cmd.env("PATH", std::env::var("PATH").unwrap_or_default());

    let assert = cmd.arg("status").assert().failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(!stderr.contains("[GLEON COMPLIANCE ERROR]"));
    assert!(stderr.contains("gleon workspace is not initialized"));
}

#[test]
fn test_cli_allows_execution_in_public_github_actions() {
    let temp_dir = tempfile::tempdir().unwrap();
    let event_path = temp_dir.path().join("event.json");
    std::fs::write(&event_path, r#"{"repository":{"private":false}}"#).unwrap();

    let mut cmd = Command::cargo_bin("gleon").unwrap();
    cmd.current_dir(temp_dir.path());
    common::clear_env(&mut cmd);
    cmd.env("PATH", std::env::var("PATH").unwrap_or_default());
    cmd.env("GITHUB_ACTIONS", "true");
    cmd.env("GITHUB_REPOSITORY", "open-source/public-repo");
    cmd.env("GITHUB_EVENT_PATH", event_path.to_str().unwrap());

    let assert = cmd.arg("status").assert().failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(!stderr.contains("[GLEON COMPLIANCE ERROR]"));
    assert!(stderr.contains("gleon workspace is not initialized"));
}
