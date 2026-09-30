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
use tempfile::TempDir;

fn init_temp_dir() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = Command::cargo_bin("gleon").unwrap();
    cmd.current_dir(dir.path()).arg("init").assert().success();
    dir
}

/// Creates an initialized temp workspace and copies the given fixture YAML into
/// `.gleon/gleon.yaml`, replacing the default config written by `gleon init`.
///
/// This avoids passing `--config /abs/path` to the binary, which would set the
/// scanner root to a system temp directory that macOS may pollute with stray
/// `.app` bundles (e.g. from the Simulator). With the config inside the temp dir,
/// workspace auto-discovery roots the scanner there — fully isolated.
fn init_with_config(fixture_yaml: impl AsRef<std::path::Path>) -> TempDir {
    let dir = init_temp_dir();
    std::fs::copy(
        fixture_yaml.as_ref(),
        dir.path().join(".gleon").join("gleon.yaml"),
    )
    .expect("failed to copy fixture config");
    dir
}

fn copy_dir_all(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    std::fs::create_dir_all(&dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_dir_all(entry.path(), dst.as_ref().join(entry.file_name()))?;
        } else {
            std::fs::copy(entry.path(), dst.as_ref().join(entry.file_name()))?;
        }
    }
    Ok(())
}

#[test]
fn test_help() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "Universal visual regression testing CLI",
        ));
    Ok(())
}

#[test]
fn test_no_arguments_shows_help() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.assert()
        .failure() // clap exits with 2 when required subcommand is missing
        .stderr(predicates::str::contains("Usage:"))
        .stderr(predicates::str::contains("Commands:"));
    Ok(())
}

#[test]
fn test_version() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::contains("gleon"));
    Ok(())
}

#[test]
fn test_init_command() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.current_dir(dir.path())
        .arg("init")
        .assert()
        .success()
        .stderr(predicates::str::contains("Initialized gleon workspace"));

    assert!(dir.path().join(".gleon").is_dir());
    assert!(dir.path().join(".gleon").join("gleon.yaml").is_file());
    Ok(())
}

#[test]
fn test_status_linux_chrome() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture_config = manifest_dir.join("tests/fixtures/platform/linux-chrome.yaml");
    let dir = init_with_config(&fixture_config);

    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.current_dir(dir.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "Nothing to report. Workspace is up to date.",
        ));
    Ok(())
}

#[test]
fn test_status_macos_opaque() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture_config = manifest_dir.join("tests/fixtures/platform/macos-opaque.yaml");
    let dir = init_with_config(&fixture_config);

    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.current_dir(dir.path()).arg("status").assert().success();
    Ok(())
}

#[test]
fn test_status_minimal_with_overrides() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture_config = manifest_dir.join("tests/fixtures/platform/minimal.yaml");
    let dir = init_with_config(&fixture_config);

    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.current_dir(dir.path())
        .arg("--os")
        .arg("windows")
        .arg("--arch")
        .arg("x86_64")
        .arg("status")
        .assert()
        .success();
    Ok(())
}

#[test]
fn test_status_opaque_conflict_error() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture_config = manifest_dir.join("tests/fixtures/platform/macos-opaque.yaml");

    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.current_dir(dir.path())
        .arg("--config")
        .arg(&fixture_config)
        .arg("--os")
        .arg("linux")
        .arg("status")
        .assert()
        .failure()
        .stderr(predicates::str::contains("opaque platform configuration"));
    Ok(())
}

#[test]
fn test_status_invalid_segment_error() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.current_dir(dir.path())
        .arg("--os")
        .arg("mac os")
        .arg("status")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Invalid character or pattern in platform segment",
        ));
    Ok(())
}

#[test]
fn test_status_reserved_label_key_error() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.current_dir(dir.path())
        .arg("--label")
        .arg("os=linux")
        .arg("status")
        .assert()
        .failure()
        .stderr(predicates::str::contains("Label key 'os' is reserved"));
    Ok(())
}

#[test]
fn test_stage_command() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.current_dir(dir.path())
        .arg("stage")
        .assert()
        .success()
        .stderr(predicates::str::contains("Already up to date."));
    Ok(())
}

#[test]
fn test_diff_command() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.current_dir(dir.path())
        .arg("diff")
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "Ran 0 test(s). Passed: 0, Failed: 0.",
        ));
    Ok(())
}

#[test]
fn test_test_placeholder() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.arg("test")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Subcommand 'test' is not implemented yet",
        ));
    Ok(())
}

#[test]
fn test_pull_placeholder() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.current_dir(dir.path())
        .env_remove("GLEON_STORAGE_URL")
        .arg("pull")
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "Operating in local mode. Cloud sync disabled. Please configure storage.",
        ));
    Ok(())
}

#[test]
fn test_push_placeholder() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.current_dir(dir.path())
        .env_remove("GLEON_STORAGE_URL")
        .arg("push")
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "Operating in local mode. Cloud sync disabled. Please configure storage.",
        ));
    Ok(())
}

#[test]
fn test_gc_local_mode() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.current_dir(dir.path())
        .env_remove("GLEON_STORAGE_URL")
        .arg("gc")
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "Operating in local mode. Cloud sync disabled. Please configure storage.",
        ));
    Ok(())
}

#[test]
fn test_gc_cli_lifecycle() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let remote_dir = tempfile::tempdir()?;
    let remote_url = url::Url::from_directory_path(remote_dir.path())
        .expect("valid directory path")
        .to_string();

    // 1. Run gc in dry-run mode (default 24h grace period)
    let mut cmd_dry = Command::cargo_bin("gleon")?;
    cmd_dry
        .current_dir(dir.path())
        .env("GLEON_STORAGE_URL", &remote_url)
        .args(["gc", "--dry-run"])
        .assert()
        .success()
        .stderr(predicates::str::contains("[DRY RUN]"));

    // 2. Run gc with grace-period < 24 without force -> fails with GracePeriodTooShort
    let mut cmd_zero_grace = Command::cargo_bin("gleon")?;
    cmd_zero_grace
        .current_dir(dir.path())
        .env("GLEON_STORAGE_URL", &remote_url)
        .args(["gc", "--grace-period-hours", "0"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Grace period must be at least 24 hours",
        ));

    // Also fails with force when grace-period < 24
    let mut cmd_force_short_grace = Command::cargo_bin("gleon")?;
    cmd_force_short_grace
        .current_dir(dir.path())
        .env("GLEON_STORAGE_URL", &remote_url)
        .args(["gc", "--force", "--grace-period-hours", "12"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Grace period must be at least 24 hours",
        ));

    // 3. Run gc without force in non-git directory -> fails with GitRequired
    let mut cmd_fail = Command::cargo_bin("gleon")?;
    cmd_fail
        .current_dir(dir.path())
        .env("GLEON_STORAGE_URL", &remote_url)
        .args(["gc", "--grace-period-hours", "24"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("requires a Git repository"));

    // 4. Run gc with --force and valid grace period (24 hours)
    let mut cmd_run = Command::cargo_bin("gleon")?;
    cmd_run
        .current_dir(dir.path())
        .env("GLEON_STORAGE_URL", &remote_url)
        .args(["gc", "--force", "--grace-period-hours", "24"])
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "No orphan blobs eligible for deletion",
        ));

    Ok(())
}

#[test]
fn test_gc_uninitialized_with_storage_fails() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.current_dir(dir.path())
        .env("GLEON_STORAGE_URL", "memory://")
        .arg("gc")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "gleon workspace is not initialized",
        ));
    Ok(())
}

#[test]
fn test_invalid_subcommand() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.arg("invalid-command").assert().failure();
    Ok(())
}

#[test]
fn test_verbose_flag_coverage() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture_config = manifest_dir.join("tests/fixtures/platform/minimal.yaml");
    let dir = init_with_config(&fixture_config);

    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.current_dir(dir.path())
        .arg("-v")
        .arg("status")
        .assert()
        .success()
        .stderr(predicates::str::contains("INFO"))
        .stderr(predicates::str::contains("gleon CLI starting up..."));
    Ok(())
}

#[test]
fn test_quiet_flag_coverage() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture_config = manifest_dir.join("tests/fixtures/platform/minimal.yaml");
    let dir = init_with_config(&fixture_config);

    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.current_dir(dir.path())
        .arg("-q")
        .arg("status")
        .assert()
        .success()
        .stderr(predicates::str::contains("gleon CLI starting up...").not());
    Ok(())
}

#[test]
fn test_conflicting_verbose_and_quiet() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.arg("-v").arg("-q").arg("status").assert().failure();
    Ok(())
}

#[test]
fn test_status_with_env_vars() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.current_dir(dir.path())
        .env("GLEON_OS", "linux")
        .env("GLEON_ARCH", "x86_64")
        .env("GLEON_RENDERER", "firefox")
        .env("GLEON_PLATFORM", "os=linux,arch=x86_64,renderer=firefox")
        .arg("status")
        .assert()
        .success();
    Ok(())
}

#[test]
fn test_status_cli_platform_success() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.current_dir(dir.path())
        .arg("--platform")
        .arg("custom-opaque")
        .arg("status")
        .assert()
        .success();
    Ok(())
}

#[test]
fn test_status_cli_platform_conflict() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.current_dir(dir.path())
        .arg("--platform")
        .arg("custom-opaque")
        .arg("--arch")
        .arg("x86_64")
        .arg("status")
        .assert()
        .failure()
        .stderr(predicates::str::contains("structured overrides"));
    Ok(())
}

#[test]
fn test_status_cli_platform_conflict_with_env_platform() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.current_dir(dir.path())
        .env("GLEON_PLATFORM", "os=linux,arch=x86_64")
        .arg("--platform")
        .arg("custom-opaque")
        .arg("status")
        .assert()
        .failure()
        .stderr(predicates::str::contains("opaque platform configuration"));
    Ok(())
}

#[test]
fn test_cli_diff_exit_code_on_match_and_mismatch() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let core_fixtures = manifest_dir
        .parent()
        .ok_or("No parent dir")?
        .join("gleon-core/tests/fixtures");

    // 1. gleon init
    let mut cmd_init = Command::cargo_bin("gleon")?;
    cmd_init
        .current_dir(dir.path())
        .arg("init")
        .assert()
        .success();

    // 2. Add fixture image and gleon.yaml
    let img_200 = std::fs::read(core_fixtures.join("200x100.png"))?;
    let img_100 = std::fs::read(core_fixtures.join("diff_16px_corners_100x100.png"))?;

    let billing_dir = dir.path().join("billing");
    std::fs::create_dir_all(&billing_dir)?;
    std::fs::write(billing_dir.join("form.png"), &img_200)?;

    let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include: "billing/**/*.png"
"#;
    std::fs::create_dir_all(dir.path().join(".gleon")).unwrap();
    std::fs::write(dir.path().join(".gleon").join("gleon.yaml"), config_yaml)?;

    // 3. gleon stage
    let mut cmd_stage = Command::cargo_bin("gleon")?;
    cmd_stage
        .current_dir(dir.path())
        .arg("stage")
        .assert()
        .success();

    // 4. gleon diff -> exit code 0 (match)
    let mut cmd_diff_match = Command::cargo_bin("gleon")?;
    cmd_diff_match
        .current_dir(dir.path())
        .arg("diff")
        .assert()
        .code(0);

    // 5. Overwrite screenshot with different image
    std::fs::write(billing_dir.join("form.png"), &img_100)?;

    // 6. gleon diff on mismatch -> returns exit code 1
    let mut cmd_diff_mismatch = Command::cargo_bin("gleon")?;
    cmd_diff_mismatch
        .current_dir(dir.path())
        .arg("diff")
        .assert()
        .code(1);

    Ok(())
}

#[test]
fn test_stage_already_up_to_date_message() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let core_fixtures = manifest_dir
        .parent()
        .ok_or("No parent dir")?
        .join("gleon-core/tests/fixtures");

    let mut cmd_init = Command::cargo_bin("gleon")?;
    cmd_init
        .current_dir(dir.path())
        .arg("init")
        .assert()
        .success();

    let img_200 = std::fs::read(core_fixtures.join("200x100.png"))?;
    let billing_dir = dir.path().join("billing");
    std::fs::create_dir_all(&billing_dir)?;
    std::fs::write(billing_dir.join("form.png"), &img_200)?;

    let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include: "billing/**/*.png"
"#;
    std::fs::create_dir_all(dir.path().join(".gleon")).unwrap();
    std::fs::write(dir.path().join(".gleon").join("gleon.yaml"), config_yaml)?;

    // First stage
    let mut cmd_stage1 = Command::cargo_bin("gleon")?;
    cmd_stage1
        .current_dir(dir.path())
        .arg("stage")
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "Staged 1 screenshot(s) across 1 test case(s).",
        ));

    // Second stage on unchanged screenshots: outputs Already up to date.
    let mut cmd_stage2 = Command::cargo_bin("gleon")?;
    cmd_stage2
        .current_dir(dir.path())
        .arg("stage")
        .assert()
        .success()
        .stderr(predicates::str::contains("Already up to date."));

    Ok(())
}

#[test]
fn test_pull_and_push_no_storage_configured() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();

    // Pull without GLEON_STORAGE_URL
    let mut cmd_pull = Command::cargo_bin("gleon")?;
    cmd_pull
        .current_dir(dir.path())
        .env_remove("GLEON_STORAGE_URL")
        .arg("pull")
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "Operating in local mode. Cloud sync disabled. Please configure storage.",
        ));

    // Push without GLEON_STORAGE_URL
    let mut cmd_push = Command::cargo_bin("gleon")?;
    cmd_push
        .current_dir(dir.path())
        .env_remove("GLEON_STORAGE_URL")
        .arg("push")
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "Operating in local mode. Cloud sync disabled. Please configure storage.",
        ));

    // Diff --auto-pull without GLEON_STORAGE_URL
    let mut cmd_diff = Command::cargo_bin("gleon")?;
    cmd_diff
        .current_dir(dir.path())
        .env_remove("GLEON_STORAGE_URL")
        .arg("diff")
        .arg("--auto-pull")
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "Ran 0 test(s). Passed: 0, Failed: 0.",
        ));

    Ok(())
}

#[test]
fn test_sync_fails_and_clears_spinner() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let mut cmd = Command::cargo_bin("gleon")?;

    cmd.current_dir(dir.path())
        .env("GLEON_STORAGE_URL", "s3://non-existent-bucket-123456/gleon")
        .arg("pull")
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "No baseline blobs found to pull.",
        ));

    Ok(())
}

#[test]
fn test_pull_and_push_with_file_storage_url() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let remote_dir = tempfile::tempdir()?;
    let remote_url = url::Url::from_directory_path(remote_dir.path())
        .unwrap()
        .to_string();

    // 1. Stage a screenshot first
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let core_fixtures = manifest_dir
        .parent()
        .ok_or("No parent dir")?
        .join("gleon-core/tests/fixtures");

    let img_200 = std::fs::read(core_fixtures.join("200x100.png"))?;
    let billing_dir = dir.path().join("billing");
    std::fs::create_dir_all(&billing_dir)?;
    std::fs::write(billing_dir.join("form.png"), &img_200)?;

    let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include: "billing/**/*.png"
"#;
    std::fs::create_dir_all(dir.path().join(".gleon")).unwrap();
    std::fs::write(dir.path().join(".gleon").join("gleon.yaml"), config_yaml)?;

    let mut cmd_stage = Command::cargo_bin("gleon")?;
    cmd_stage
        .current_dir(dir.path())
        .arg("stage")
        .assert()
        .success();

    // 2. Push with storage URL
    let mut cmd_push = Command::cargo_bin("gleon")?;
    cmd_push
        .current_dir(dir.path())
        .env("GLEON_STORAGE_URL", &remote_url)
        .arg("push")
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "Uploaded 1 missing baseline blob(s) to storage",
        ));

    // 3. Pull in fresh workspace with copied manifests (simulating git pull)
    let fresh_dir = tempfile::tempdir()?;
    let mut cmd_init2 = Command::cargo_bin("gleon")?;
    cmd_init2
        .current_dir(fresh_dir.path())
        .arg("init")
        .assert()
        .success();

    let manifests_src = dir.path().join(".gleon").join("manifests");
    let manifests_dst = fresh_dir.path().join(".gleon").join("manifests");
    if manifests_src.exists() {
        copy_dir_all(&manifests_src, &manifests_dst)?;
    }

    let mut cmd_pull = Command::cargo_bin("gleon")?;
    cmd_pull
        .current_dir(fresh_dir.path())
        .env("GLEON_STORAGE_URL", &remote_url)
        .arg("pull")
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "Downloaded 1 missing baseline blob(s) from storage",
        ));

    // 4. Pull again, should be up to date
    let mut cmd_pull2 = Command::cargo_bin("gleon")?;
    cmd_pull2
        .current_dir(fresh_dir.path())
        .env("GLEON_STORAGE_URL", &remote_url)
        .arg("pull")
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "All 1 baseline blob(s) are already up to date locally.",
        ));

    // 5. Push again, should be up to date
    let mut cmd_push2 = Command::cargo_bin("gleon")?;
    cmd_push2
        .current_dir(dir.path())
        .env("GLEON_STORAGE_URL", &remote_url)
        .arg("push")
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "All 1 baseline blob(s) are already present in remote storage.",
        ));

    Ok(())
}

#[test]
fn test_status_json_flag() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let mut cmd_status = Command::cargo_bin("gleon")
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::NotFound, e))?;
    cmd_status
        .current_dir(dir.path())
        .arg("status")
        .arg("--json")
        .assert()
        .success()
        .stdout(predicates::str::contains("\"added\":"));

    Ok(())
}

#[test]
fn test_stage_path_filter() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let mut cmd_stage = Command::cargo_bin("gleon")
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::NotFound, e))?;
    cmd_stage
        .current_dir(dir.path())
        .arg("stage")
        .arg("non_existent_folder")
        .assert()
        .success()
        .stderr(predicates::str::contains("Already up to date"));

    Ok(())
}

#[test]
fn test_dotenv_loading_integration() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();

    // Copy our real .env fixtures into the .gleon folder of the temp workspace
    let fixtures_env = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("gleon-core")
        .join("tests")
        .join("fixtures")
        .join("env");

    std::fs::copy(
        fixtures_env.join(".env"),
        dir.path().join(".gleon").join(".env"),
    )?;
    std::fs::copy(
        fixtures_env.join(".env.local"),
        dir.path().join(".gleon").join(".env.local"),
    )?;

    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.current_dir(dir.path())
        .arg("--verbose")
        .arg("diff")
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "Loaded 2 environment variable(s)",
        ));

    Ok(())
}

/// `gleon diff` always writes its JSON report to `.gleon/runs/latest/gleon-report.json`;
/// `gleon report <format>` must default `--report` to that exact path so it works right after a
/// `diff` run without the caller repeating the path back.
#[test]
fn test_cli_report_defaults_to_the_diff_output_path() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let core_fixtures = manifest_dir
        .parent()
        .ok_or("No parent dir")?
        .join("gleon-core/tests/fixtures");

    let img_200 = std::fs::read(core_fixtures.join("200x100.png"))?;
    let img_100 = std::fs::read(core_fixtures.join("diff_16px_corners_100x100.png"))?;
    let billing_dir = dir.path().join("billing");
    std::fs::create_dir_all(&billing_dir)?;
    std::fs::write(billing_dir.join("form.png"), &img_200)?;

    let config_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include: "billing/**/*.png"
"#;
    std::fs::write(dir.path().join(".gleon").join("gleon.yaml"), config_yaml)?;

    Command::cargo_bin("gleon")?
        .current_dir(dir.path())
        .arg("stage")
        .assert()
        .success();

    // Change the screenshot after staging so `diff` reports a failure, not a no-op pass — a
    // passing report renders no per-test rows and couldn't tell "found the default path" apart
    // from "silently found nothing".
    std::fs::write(billing_dir.join("form.png"), &img_100)?;

    Command::cargo_bin("gleon")?
        .current_dir(dir.path())
        .arg("diff")
        .assert()
        .code(1);

    // No `--report` passed: must find `.gleon/runs/latest/gleon-report.json` on its own.
    Command::cargo_bin("gleon")?
        .current_dir(dir.path())
        .args(["report", "markdown"])
        .assert()
        .success()
        .stdout(predicates::str::contains("form"));

    Ok(())
}

#[test]
fn test_cli_report_markdown_stdout_and_file() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let json_report_path = dir.path().join("gleon-report.json");
    let out_report_path = dir.path().join("out-report.md");

    // Write sample json report
    let sample_json = r#"[
        {
            "name": "login_button",
            "result": {
                "Mismatch": {
                    "relative_path": "login.png",
                    "detail": { "Pixel": { "diff_count": 42 } },
                    "diff_path": "diffs/login.png",
                    "baseline_path": "goldens/login.png",
                    "actual_path": "actual/login.png"
                }
            }
        },
        {
            "name": "missing_test",
            "result": {
                "MissingBaseline": {
                    "relative_path": "footer.png",
                    "reason": "Missing baseline blob"
                }
            }
        },
        {
            "name": "corrupt_test",
            "result": {
                "DecodeError": {
                    "relative_path": "sidebar.png",
                    "error": "corrupt png file"
                }
            }
        }
    ]"#;
    std::fs::write(&json_report_path, sample_json)?;

    // Test stdout output
    let mut cmd_stdout = Command::cargo_bin("gleon")?;
    cmd_stdout
        .current_dir(dir.path())
        .arg("report")
        .arg("markdown")
        .arg("--report")
        .arg(&json_report_path)
        .assert()
        .success()
        .stdout(predicates::str::contains("login_button"))
        .stdout(predicates::str::contains("42 px"))
        .stdout(predicates::str::contains("Missing baseline blob"))
        .stdout(predicates::str::contains("corrupt png file"));

    // Test --out file output
    let mut cmd_out = Command::cargo_bin("gleon")?;
    cmd_out
        .current_dir(dir.path())
        .arg("report")
        .arg("markdown")
        .arg("--report")
        .arg(&json_report_path)
        .arg("--out")
        .arg(&out_report_path)
        .assert()
        .success();

    let out_content = std::fs::read_to_string(out_report_path)?;
    assert!(out_content.contains("login_button"));
    assert!(out_content.contains("42 px"));
    assert!(out_content.contains("Missing baseline blob"));
    assert!(out_content.contains("corrupt png file"));

    Ok(())
}

#[test]
fn test_cli_report_invalid_json() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let json_report_path = dir.path().join("invalid.json");
    std::fs::write(&json_report_path, "{}")?; // Empty object, not an array

    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.current_dir(dir.path())
        .arg("report")
        .arg("markdown")
        .arg("--report")
        .arg(&json_report_path)
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Failed to parse report JSON from",
        ));

    Ok(())
}

#[test]
fn test_cli_report_unsupported_format() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let json_report_path = dir.path().join("valid.json");
    std::fs::write(&json_report_path, "[]")?;

    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.current_dir(dir.path())
        .arg("report")
        .arg("unsupported-format")
        .arg("--report")
        .arg(&json_report_path)
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "error: invalid value 'unsupported-format' for '<FORMAT>'",
        ));

    Ok(())
}

#[test]
fn test_cli_report_with_base_url() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let json_report_path = dir.path().join("gleon-report.json");
    let sample_json = r#"[
        {
            "name": "login_button",
            "result": {
                "Mismatch": {
                    "relative_path": "login.png",
                    "detail": { "Pixel": { "diff_count": 42 } },
                    "diff_path": "diffs/login.png",
                    "baseline_path": "goldens/login.png",
                    "actual_path": "actual/login.png"
                }
            }
        }
    ]"#;
    std::fs::write(&json_report_path, sample_json)?;

    let mut cmd_stdout = Command::cargo_bin("gleon")?;
    cmd_stdout
        .current_dir(dir.path())
        .env("GLEON_STORAGE_URL", "https://example.com/bucket")
        .arg("report")
        .arg("markdown")
        .arg("--report")
        .arg(&json_report_path)
        .assert()
        .success()
        .stdout(predicates::str::contains("login_button"))
        .stdout(predicates::str::contains("https://example.com"));

    Ok(())
}

#[test]
fn test_cli_report_invalid_pr_number() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let json_report_path = dir.path().join("gleon-report.json");
    std::fs::write(&json_report_path, "[]")?;

    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.current_dir(dir.path())
        .arg("report")
        .arg("markdown")
        .arg("--report")
        .arg(&json_report_path)
        .arg("--pr-number")
        .arg("0")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "PR number must be greater than 0",
        ));

    Ok(())
}

#[test]
fn test_cli_report_valid_pr_number_and_html_url() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let json_report_path = dir.path().join("gleon-report.json");
    let mut items = Vec::new();
    for i in 0..11 {
        items.push(format!(
            r#"{{
                "name": "test_{i}",
                "result": {{
                    "DecodeError": {{
                        "relative_path": "test_{i}.png",
                        "error": "corrupt"
                    }}
                }}
            }}"#
        ));
    }
    let sample_json = format!("[{}]", items.join(","));
    std::fs::write(&json_report_path, sample_json)?;

    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.current_dir(dir.path())
        .env(
            "GLEON_HTML_ARTIFACT_URL",
            "https://github.com/actions/artifact",
        )
        .arg("report")
        .arg("markdown")
        .arg("--report")
        .arg(&json_report_path)
        .arg("--pr-number")
        .arg("42")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "https://github.com/actions/artifact",
        ))
        .stdout(predicates::str::contains("Truncated 1 additional diffs"));

    Ok(())
}

#[test]
#[cfg(not(miri))]
fn test_cli_report_with_s3_storage_pre_signed_urls() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let json_report_path = dir.path().join("gleon-report.json");
    let sample_json = r#"[
        {
            "name": "mismatch_test",
            "result": {
                "Mismatch": {
                    "relative_path": "login.png",
                    "detail": { "Pixel": { "diff_count": 42 } },
                    "diff_path": "diffs/login.png",
                    "baseline_path": ".gleon/blobs/sha256/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "actual_path": "actual/login.png"
                }
            }
        },
        {
            "name": "dimension_test",
            "result": {
                "DimensionMismatch": {
                    "relative_path": "header.png",
                    "actual_size": [100, 200],
                    "baseline_size": [100, 201],
                    "baseline_path": ".gleon/blobs/sha256/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    "actual_path": "actual/header.png"
                }
            }
        },
        {
            "name": "missing_test",
            "result": {
                "MissingBaseline": {
                    "relative_path": "footer.png",
                    "reason": "Missing baseline blob"
                }
            }
        },
        {
            "name": "corrupt_test",
            "result": {
                "DecodeError": {
                    "relative_path": "sidebar.png",
                    "error": "corrupt png file"
                }
            }
        }
    ]"#;
    std::fs::write(&json_report_path, sample_json)?;

    let mut cmd = Command::cargo_bin("gleon")?;
    cmd.current_dir(dir.path())
        .env("GLEON_STORAGE_URL", "s3://my-test-bucket/gleon")
        .env("GLEON_AWS_ACCESS_KEY_ID", "key")
        .env("GLEON_AWS_SECRET_ACCESS_KEY", "secret")
        .env("GLEON_AWS_REGION", "us-east-1")
        .arg("report")
        .arg("markdown")
        .arg("--report")
        .arg(&json_report_path)
        .assert()
        .success()
        // Baselines are signed by their content-addressed key, the same one `push` uploads to.
        .stdout(predicates::str::contains("my-test-bucket"))
        .stdout(predicates::str::contains("X-Amz-Signature"))
        .stdout(predicates::str::contains(
            "blobs/sha256/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ))
        // `actual`/`diff` only ever exist on the runner, so they must not be signed into
        // dead links pointing at keys that were never uploaded.
        .stdout(predicates::str::contains("actual/login.png?X-Amz").not())
        .stdout(predicates::str::contains("diffs/login.png?X-Amz").not());

    Ok(())
}

#[test]
fn test_approve_command() {
    let dir = init_temp_dir();
    let base_path = dir.path();
    let fixtures_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("get parent dir")
        .join("gleon-core")
        .join("tests")
        .join("fixtures");

    // Create actual screenshots to approve
    let actual_dir = base_path
        .join(".gleon")
        .join("runs")
        .join("latest")
        .join("actual");
    let login_dir = actual_dir.join("login");
    std::fs::create_dir_all(&login_dir).expect("create_dir_all login");
    std::fs::copy(
        fixtures_dir.join("baseline_100x100.png"),
        login_dir.join("button.png"),
    )
    .expect("copy baseline screenshot");

    // Run approve command
    let mut cmd = Command::cargo_bin("gleon").expect("cargo_bin gleon");
    cmd.current_dir(base_path)
        .arg("approve")
        .assert()
        .success()
        .stderr(predicates::str::contains("Approved 1 screenshot(s)"));

    // Check that manifest and blob are created
    let manifests_dir = base_path.join(".gleon").join("manifests");
    let mut found_button_json = false;
    if let Ok(entries) = std::fs::read_dir(&manifests_dir) {
        for entry in entries.flatten() {
            if entry.file_type().unwrap().is_dir() {
                let button_json = entry.path().join("login").join("button.json");
                if button_json.exists() {
                    found_button_json = true;
                    let manifest_content =
                        std::fs::read_to_string(&button_json).expect("read_to_string button.json");
                    assert!(manifest_content.contains("sha256:"));
                }
            }
        }
    }
    assert!(
        found_button_json,
        "Expected to find login/button.json manifest"
    );
}

/// Subcommand 'test' must not report success: a green pipeline for a command that did
/// nothing is worse than a red one, because CI treats it as a passing visual-regression gate.
#[test]
fn test_test_subcommand_unimplemented() {
    let dir = init_temp_dir();
    let mut cmd = Command::cargo_bin("gleon").unwrap();
    cmd.current_dir(dir.path())
        .arg("test")
        .assert()
        .failure()
        .stderr(predicate::str::contains("not implemented"));
}

/// `--auto-pull` is advertised by the CLI, so it must actually run a pull before diffing
/// rather than being silently ignored.
#[test]
fn test_diff_auto_pull_runs_pull_first() {
    let dir = init_temp_dir();

    let mut cmd = Command::cargo_bin("gleon").unwrap();
    let assert = cmd
        .current_dir(dir.path())
        .args(["diff", "--auto-pull"])
        .assert();

    // No storage configured -> pull reports local mode, then the diff itself proceeds.
    assert.stderr(predicate::str::contains("Running blob pull..."));
}
