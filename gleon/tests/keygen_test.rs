//! Tests for the keygen binary.
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
use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

use assert_cmd::Command;
use ed25519_dalek::SigningKey;
use gleon_core::license::{ExecutionContext, LicenseGate, LicenseValidity};
use predicates::prelude::*;

mod common;

#[test]
fn test_keygen_sign_happy_path_with_test_key() {
    let secret = [42u8; 32];
    let secret_hex = hex::encode(secret);
    let signing_key = SigningKey::from_bytes(&secret);
    let public_bytes = signing_key.verifying_key().to_bytes();
    let public_hex = hex::encode(public_bytes);

    let mut cmd = Command::cargo_bin("keygen").unwrap();
    let assert = cmd
        .args([
            "sign",
            "--kid",
            "255",
            "--org",
            "acme-corp",
            "--repo-pattern",
            "acme-corp/*",
            "--expires-days",
            "30",
            "--license-id",
            "lic_custom_123",
            "--test-pubkey",
            &public_hex,
        ])
        .env("GLEON_SIGNING_KEY", &secret_hex)
        .assert()
        .success();

    let token = String::from_utf8_lossy(&assert.get_output().stdout)
        .trim()
        .to_string();

    assert!(!token.is_empty());

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let valid_ctx = ExecutionContext::GenericCI {
        repo: "acme-corp/api".to_string(),
    };

    assert_eq!(
        LicenseGate::verify_key_with_public_key(&token, &valid_ctx, now, &public_bytes).unwrap(),
        LicenseValidity::Valid
    );
}

#[test]
fn test_keygen_sign_with_dummy_key_fails_self_check() {
    let secret = [99u8; 32];
    let secret_hex = hex::encode(secret);

    let mut cmd = Command::cargo_bin("keygen").unwrap();
    cmd.args([
        "sign",
        "--kid",
        "1",
        "--org",
        "acme",
        "--repo-pattern",
        "acme/*",
        "--expires-days",
        "30",
        "--license-id",
        "custom_id_123",
    ])
    .env("GLEON_SIGNING_KEY", &secret_hex)
    .assert()
    .failure()
    .stderr(predicate::str::contains(
        "Self-check failed: The generated public key does not match OFFICIAL_PUBLIC_KEYS",
    ));
}

#[test]
fn test_keygen_sign_audit_log_format() {
    let temp_dir = tempfile::tempdir().unwrap();
    let audit_file = temp_dir.path().join("audit.jsonl");

    let secret = [42u8; 32];
    let secret_hex = hex::encode(secret);
    let signing_key = SigningKey::from_bytes(&secret);
    let public_hex = hex::encode(signing_key.verifying_key().to_bytes());

    let mut cmd = Command::cargo_bin("keygen").unwrap();
    cmd.args([
        "sign",
        "--kid",
        "255",
        "--org",
        "audit-org",
        "--repo-pattern",
        "audit-org/*",
        "--expires-days",
        "15",
        "--license-id",
        "lic_audit_456",
        "--audit-log",
        audit_file.to_str().unwrap(),
        "--test-pubkey",
        &public_hex,
    ])
    .env("GLEON_SIGNING_KEY", &secret_hex)
    .assert()
    .success();

    assert!(audit_file.exists());
    let content = fs::read_to_string(&audit_file).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 1);

    let entry: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(entry["action"], "sign_license");
    assert_eq!(entry["v"], 1);
    assert_eq!(entry["kid"], 255);
    assert_eq!(entry["owner"], "audit-org");
    assert_eq!(entry["repo_pattern"], "audit-org/*");
    assert_eq!(entry["license_id"], "lic_audit_456");
}

#[test]
fn test_keygen_error_cases() {
    // Error 1: --expires-days 0
    let mut cmd1 = Command::cargo_bin("keygen").unwrap();
    cmd1.args([
        "sign",
        "--org",
        "test",
        "--repo-pattern",
        "*",
        "--expires-days",
        "0",
    ])
    .env("GLEON_SIGNING_KEY", hex::encode([42u8; 32]))
    .assert()
    .failure()
    .stderr(predicate::str::contains("must be greater than zero"));

    // Error 2: invalid secret key format
    let mut cmd2 = Command::cargo_bin("keygen").unwrap();
    cmd2.args([
        "sign",
        "--org",
        "test",
        "--repo-pattern",
        "*",
        "--expires-days",
        "10",
    ])
    .env("GLEON_SIGNING_KEY", "invalid_key_data")
    .assert()
    .failure()
    .stderr(predicate::str::contains("Invalid secret key format"));

    // Error 3: missing required args
    let mut cmd3 = Command::cargo_bin("keygen").unwrap();
    cmd3.args(["sign", "--expires-days", "10"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("required"));

    // Error 4: --expires-days > 3650
    let mut cmd4 = Command::cargo_bin("keygen").unwrap();
    cmd4.args([
        "sign",
        "--org",
        "test",
        "--repo-pattern",
        "*",
        "--expires-days",
        "4000",
    ])
    .env("GLEON_SIGNING_KEY", hex::encode([42u8; 32]))
    .assert()
    .failure()
    .stderr(predicate::str::contains("cannot exceed 3650 days"));

    // Error 5: invalid org characters (newline)
    let mut cmd5 = Command::cargo_bin("keygen").unwrap();
    cmd5.args([
        "sign",
        "--org",
        "test
newline",
        "--repo-pattern",
        "*",
        "--expires-days",
        "10",
    ])
    .env("GLEON_SIGNING_KEY", hex::encode([42u8; 32]))
    .assert()
    .failure()
    .stderr(predicate::str::contains("Invalid character in --org"));

    // Error 6: invalid license_id characters (spaces)
    let mut cmd6 = Command::cargo_bin("keygen").unwrap();
    cmd6.args([
        "sign",
        "--org",
        "test",
        "--license-id",
        "bad id with spaces",
        "--repo-pattern",
        "*",
        "--expires-days",
        "10",
    ])
    .env("GLEON_SIGNING_KEY", hex::encode([42u8; 32]))
    .assert()
    .failure()
    .stderr(predicate::str::contains(
        "Invalid character in --license-id",
    ));
}

#[test]
fn test_keygen_generate_keypair() {
    let temp_dir = tempfile::tempdir().unwrap();
    let key_file = temp_dir.path().join("secret.key");

    let mut cmd = Command::cargo_bin("keygen").unwrap();
    cmd.args(["generate-keypair", "--out", key_file.to_str().unwrap()])
        .assert()
        .success()
        .stderr(predicate::str::contains("Successfully generated keypair"));

    assert!(key_file.exists());
    let content = fs::read_to_string(&key_file).unwrap();
    let trimmed = content.trim();
    assert_eq!(trimmed.len(), 64);
    assert!(hex::decode(trimmed).is_ok());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::metadata(&key_file).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o600);
    }
}

#[test]
fn test_keygen_generate_keypair_write_failure() {
    let temp_dir = tempfile::tempdir().unwrap();
    let blocked_file = temp_dir.path().join("blocked");
    fs::write(&blocked_file, "blocking").unwrap();
    let invalid_key_file = blocked_file.join("sub").join("secret.key");

    let mut cmd = Command::cargo_bin("keygen").unwrap();
    cmd.args([
        "generate-keypair",
        "--out",
        invalid_key_file.to_str().unwrap(),
    ])
    .assert()
    .failure()
    .stderr(predicate::str::contains(
        "Failed to create parent directory",
    ));
}

#[test]
fn test_keygen_generate_keypair_already_exists_fails() {
    let temp_dir = tempfile::tempdir().unwrap();
    let existing_key = temp_dir.path().join("secret.key");
    fs::write(&existing_key, "existing-content").unwrap();

    let mut cmd = Command::cargo_bin("keygen").unwrap();
    cmd.args(["generate-keypair", "--out", existing_key.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Failed to open output file"));

    assert_eq!(
        fs::read_to_string(&existing_key).unwrap(),
        "existing-content"
    );
}

#[test]
fn test_keygen_secret_key_from_stdin_fails_self_check() {
    let secret = [66u8; 32];
    let secret_hex = hex::encode(secret);

    let mut cmd = Command::cargo_bin("keygen").unwrap();
    cmd.args([
        "sign",
        "--kid",
        "1",
        "--org",
        "stdin-corp",
        "--repo-pattern",
        "stdin-corp/*",
        "--expires-days",
        "20",
    ])
    .env("GLEON_SIGNING_KEY", "-")
    .write_stdin(secret_hex)
    .assert()
    .failure()
    .stderr(predicate::str::contains(
        "Self-check failed: The generated public key does not match OFFICIAL_PUBLIC_KEYS",
    ));
}

#[test]
fn test_gleon_binary_with_real_master_key_unblocks_private_ci() {
    let master_secret_hex = match std::env::var("GLEON_MASTER_SECRET_KEY") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => {
            eprintln!("Skipping e2e master key test: secret not present in open environment");
            return;
        }
    };

    let mut keygen_cmd = Command::cargo_bin("keygen").unwrap();
    let keygen_assert = keygen_cmd
        .args([
            "sign",
            "--org",
            "enterprise-customer",
            "--repo-pattern",
            "enterprise-customer/*",
            "--expires-days",
            "365",
        ])
        .env("GLEON_SIGNING_KEY", "-")
        .write_stdin(master_secret_hex)
        .assert()
        .success();

    let token = String::from_utf8_lossy(&keygen_assert.get_output().stdout)
        .trim()
        .to_string();

    let temp_dir = tempfile::tempdir().unwrap();
    let mut gleon_cmd = Command::cargo_bin("gleon").unwrap();
    gleon_cmd.current_dir(temp_dir.path());
    common::clear_env(&mut gleon_cmd);
    gleon_cmd.env("CI", "true");
    gleon_cmd.env("CI_PROJECT_PATH", "enterprise-customer/app");
    gleon_cmd.env("GLEON_LICENSE_KEY", &token);

    let assert = gleon_cmd.arg("--strict").arg("status").assert().success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);

    assert!(!stderr.contains("[GLEON COMPLIANCE ERROR] Execution blocked."));
}
