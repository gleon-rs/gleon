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

/// Variables of the environment that select runs or outputs; CI exports `GLEON_RUN_ID` job-wide.
const GLEON_ENV: [&str; 5] = [
    "GLEON_RUN_ID",
    "GLEON_ARTIFACTS_DIR",
    "GLEON_METRICS",
    "GLEON_STORAGE_URL",
    "GLEON_HTML_ARTIFACT_URL",
];

/// The `gleon` binary without [`GLEON_ENV`], so each test sees only what it sets.
fn gleon() -> Command {
    let mut cmd = Command::cargo_bin("gleon").unwrap();
    for var in GLEON_ENV {
        cmd.env_remove(var);
    }
    cmd
}

fn init_temp_dir() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = gleon();
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
    let mut cmd = gleon();
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
    let mut cmd = gleon();
    cmd.assert()
        .failure() // clap exits with 2 when required subcommand is missing
        .stderr(predicates::str::contains("Usage:"))
        .stderr(predicates::str::contains("Commands:"));
    Ok(())
}

#[test]
fn test_version() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = gleon();
    cmd.arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::contains("gleon"));
    Ok(())
}

#[test]
fn test_init_command() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let mut cmd = gleon();
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

    let mut cmd = gleon();
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

    let mut cmd = gleon();
    cmd.current_dir(dir.path()).arg("status").assert().success();
    Ok(())
}

#[test]
fn test_status_minimal_with_overrides() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture_config = manifest_dir.join("tests/fixtures/platform/minimal.yaml");
    let dir = init_with_config(&fixture_config);

    let mut cmd = gleon();
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

    let mut cmd = gleon();
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
    let mut cmd = gleon();
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
    let mut cmd = gleon();
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
    let mut cmd = gleon();
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
    let mut cmd = gleon();
    // A run that compares nothing is no pass.
    cmd.current_dir(dir.path())
        .arg("diff")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "no screenshots match the `screenshots` rules",
        ));
    Ok(())
}

#[test]
fn test_pull_placeholder() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let mut cmd = gleon();
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
    let mut cmd = gleon();
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
    let mut cmd = gleon();
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
    let mut cmd_dry = gleon();
    cmd_dry
        .current_dir(dir.path())
        .env("GLEON_STORAGE_URL", &remote_url)
        .args(["gc", "--dry-run"])
        .assert()
        .success()
        .stderr(predicates::str::contains("[DRY RUN]"));

    // 2. Run gc with grace-period < 24 without force -> fails with GracePeriodTooShort
    let mut cmd_zero_grace = gleon();
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
    let mut cmd_force_short_grace = gleon();
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
    let mut cmd_fail = gleon();
    cmd_fail
        .current_dir(dir.path())
        .env("GLEON_STORAGE_URL", &remote_url)
        .args(["gc", "--grace-period-hours", "24"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("requires a Git repository"));

    // 4. Run gc with --force and valid grace period (24 hours)
    let mut cmd_run = gleon();
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
    let mut cmd = gleon();
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
    let mut cmd = gleon();
    cmd.arg("invalid-command").assert().failure();
    Ok(())
}

#[test]
fn test_verbose_flag_coverage() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture_config = manifest_dir.join("tests/fixtures/platform/minimal.yaml");
    let dir = init_with_config(&fixture_config);

    let mut cmd = gleon();
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

    let mut cmd = gleon();
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
    let mut cmd = gleon();
    cmd.arg("-v").arg("-q").arg("status").assert().failure();
    Ok(())
}

#[test]
fn test_status_with_env_vars() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let mut cmd = gleon();
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
    let mut cmd = gleon();
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
    let mut cmd = gleon();
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
    let mut cmd = gleon();
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
    let mut cmd_init = gleon();
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
    let mut cmd_stage = gleon();
    cmd_stage
        .current_dir(dir.path())
        .arg("stage")
        .assert()
        .success();

    // 4. gleon diff -> exit code 0 (match)
    let mut cmd_diff_match = gleon();
    cmd_diff_match
        .current_dir(dir.path())
        .arg("diff")
        .assert()
        .code(0);

    // 5. Overwrite screenshot with different image
    std::fs::write(billing_dir.join("form.png"), &img_100)?;

    // 6. gleon diff on mismatch -> returns exit code 1
    let mut cmd_diff_mismatch = gleon();
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

    let mut cmd_init = gleon();
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
    let mut cmd_stage1 = gleon();
    cmd_stage1
        .current_dir(dir.path())
        .arg("stage")
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "Staged 1 screenshot(s) across 1 test case(s).",
        ));

    // Second stage on unchanged screenshots: outputs Already up to date.
    let mut cmd_stage2 = gleon();
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
    let mut cmd_pull = gleon();
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
    let mut cmd_push = gleon();
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
    let mut cmd_diff = gleon();
    cmd_diff
        .current_dir(dir.path())
        .env_remove("GLEON_STORAGE_URL")
        .arg("diff")
        .arg("--auto-pull")
        .assert()
        .failure()
        .stderr(predicates::str::contains(
            "Operating in local mode. Cloud sync disabled. Please configure storage.",
        ))
        .stderr(predicates::str::contains("no screenshots match"));

    Ok(())
}

#[test]
fn test_sync_fails_and_clears_spinner() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let mut cmd = gleon();

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

    let mut cmd_stage = gleon();
    cmd_stage
        .current_dir(dir.path())
        .arg("stage")
        .assert()
        .success();

    // 2. Push with storage URL
    let mut cmd_push = gleon();
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
    let mut cmd_init2 = gleon();
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

    let mut cmd_pull = gleon();
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
    let mut cmd_pull2 = gleon();
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
    let mut cmd_push2 = gleon();
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

    let mut cmd = gleon();
    cmd.current_dir(dir.path())
        .arg("--verbose")
        .arg("status")
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "Loaded 2 environment variable(s)",
        ));

    Ok(())
}

/// A case report of the Flutter integration for the golden `<name>.png` with `outcome` and
/// `extra` fields, in run `run-1`.
fn case(name: &str, outcome: &str, extra: serde_json::Value) -> serde_json::Value {
    let mut report = serde_json::json!({
        "schema_version": 2,
        "name": name,
        "golden": {"path": format!("{name}.png"), "sha256": "1".repeat(64)},
        "candidate": {"sha256": "0".repeat(64)},
        "source": {"tool": "gleon_flutter", "tool_version": "0.1.0"},
        "platform": {"os": "linux", "arch": "x86_64"},
        "comparison": {"tolerance": {"kind": "exact"}, "masks": [], "policy_version": 2},
        "outcome": outcome,
        "regions": [],
        "timings_ms": {"total": 1.0},
        "run_id": "run-1",
        "recorded_at": "2026-10-01T12:00:00Z"
    });
    for (key, value) in extra.as_object().unwrap() {
        report[key] = value.clone();
    }
    if outcome == "missing" {
        report["golden"]["sha256"] = serde_json::Value::Null;
    }
    report
}

/// Writes `reports` into `<dir>/.gleon/runs/latest/cases/` and returns that directory.
fn write_cases(dir: &std::path::Path, reports: &[serde_json::Value]) -> std::path::PathBuf {
    let cases = dir.join(".gleon/runs/latest/cases");
    for report in reports {
        let file = cases.join(format!("{}.json", report["name"].as_str().unwrap()));
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, report.to_string()).unwrap();
    }
    cases
}

fn mismatch(name: &str, diff_pixels: u64) -> serde_json::Value {
    case(
        name,
        "mismatch",
        serde_json::json!({
            "metrics": {"kind": "pixel", "total_pixels": 1000, "diff_pixels": diff_pixels,
                        "diff_ratio": diff_pixels as f64 / 1000.0, "headroom": -1.0},
        }),
    )
}

fn with_blob(mut report: serde_json::Value, digit: char) -> serde_json::Value {
    report["golden"]["blob"] = format!("sha256:{}", digit.to_string().repeat(64)).into();
    report
}

/// `gleon report <format>` reads the case reports `gleon diff` just wrote without arguments.
#[test]
fn test_cli_report_defaults_to_the_latest_run() -> Result<(), Box<dyn std::error::Error>> {
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

    gleon()
        .current_dir(dir.path())
        .arg("stage")
        .assert()
        .success();

    // Change the screenshot after staging so `diff` reports a failure, not a no-op pass.
    std::fs::write(billing_dir.join("form.png"), &img_100)?;

    gleon().current_dir(dir.path()).arg("diff").assert().code(1);

    // From a subdirectory too: the workspace is resolved like for every command.
    gleon()
        .current_dir(&billing_dir)
        .args(["report", "markdown"])
        .assert()
        .success()
        .stdout(predicates::str::contains("billing/form"))
        .stdout(predicates::str::contains("Dimension Mismatch"));

    Ok(())
}

#[test]
fn test_cli_report_markdown_stdout_and_file() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let cases = write_cases(
        dir.path(),
        &[
            mismatch("login_button", 42),
            case(
                "missing_test",
                "missing",
                serde_json::json!({"message": "no golden yet"}),
            ),
            case(
                "corrupt_test",
                "error",
                serde_json::json!({"error_kind": "image", "message": "corrupt png file"}),
            ),
        ],
    );
    let out_report_path = dir.path().join("out-report.md");

    gleon()
        .current_dir(dir.path())
        .args(["report", "markdown", "--from"])
        .arg(cases.parent().unwrap())
        .assert()
        .success()
        .stdout(predicates::str::contains("login_button"))
        .stdout(predicates::str::contains("(42 of 1000px) differ"))
        .stdout(predicates::str::contains("Missing Baseline: no golden yet"))
        .stdout(predicates::str::contains("Error (image): corrupt png file"));

    gleon()
        .current_dir(dir.path())
        .args(["report", "markdown", "--out"])
        .arg(&out_report_path)
        .assert()
        .success();
    let out_content = std::fs::read_to_string(out_report_path)?;
    assert!(out_content.contains("login_button"));
    assert!(out_content.contains("corrupt png file"));

    gleon()
        .current_dir(dir.path())
        .args(["report", "junit"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            r#"tests="3" failures="2" errors="1""#,
        ));
    Ok(())
}

#[test]
fn test_cli_report_without_or_with_broken_case_reports() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    gleon()
        .current_dir(dir.path())
        .args(["report", "markdown"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("No case reports found"));

    // A broken report is skipped with a warning; the others still make the report.
    let cases = write_cases(dir.path(), &[mismatch("a", 1)]);
    std::fs::write(cases.join("broken.json"), "{}")?;
    gleon()
        .current_dir(dir.path())
        .args(["report", "markdown"])
        .assert()
        .success()
        .stdout(predicates::str::contains("`a`"))
        .stdout(predicates::str::contains(
            "skipped 1 invalid case report(s), e.g. broken.json",
        ));

    gleon()
        .current_dir(dir.path())
        .env("GLEON_RUN_ID", "a:b")
        .args(["report", "markdown"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("invalid GLEON_RUN_ID"));
    Ok(())
}

#[test]
fn test_cli_report_unsupported_format() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let mut cmd = gleon();
    cmd.current_dir(dir.path())
        .arg("report")
        .arg("unsupported-format")
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
    write_cases(dir.path(), &[with_blob(mismatch("login_button", 42), 'a')]);

    gleon()
        .current_dir(dir.path())
        .env("GLEON_STORAGE_URL", "https://example.com/bucket")
        .args(["report", "markdown"])
        .assert()
        .success()
        .stdout(predicates::str::contains("login_button"))
        .stdout(predicates::str::contains(format!(
            "[Image](https://example.com/bucket/blobs/sha256/{})",
            "a".repeat(64)
        )));

    Ok(())
}

#[test]
fn test_cli_report_invalid_pr_number() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let mut cmd = gleon();
    cmd.current_dir(dir.path())
        .args(["report", "markdown", "--pr-number", "0"])
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
    let reports: Vec<_> = (0..11)
        .map(|i| mismatch(&format!("test_{i:02}"), i + 1))
        .collect();
    write_cases(dir.path(), &reports);

    let mut cmd = gleon();
    cmd.current_dir(dir.path())
        .env(
            "GLEON_HTML_ARTIFACT_URL",
            "https://github.com/actions/artifact",
        )
        .args(["report", "markdown", "--pr-number", "42"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "https://github.com/actions/artifact",
        ))
        .stdout(predicates::str::contains("Truncated 1 additional diffs"))
        .stdout(predicates::str::contains("/gleon approve"));

    Ok(())
}

#[test]
#[cfg(not(miri))]
fn test_cli_report_with_s3_storage_pre_signed_urls() -> Result<(), Box<dyn std::error::Error>> {
    let dir = init_temp_dir();
    let mut dimension = case(
        "dimension_test",
        "dimension_mismatch",
        serde_json::json!({"message": "golden is 100x201px, test image is 100x200px"}),
    );
    dimension = with_blob(dimension, 'b');
    write_cases(
        dir.path(),
        &[
            with_blob(mismatch("mismatch_test", 42), 'a'),
            dimension,
            case("missing_test", "missing", serde_json::json!({})),
        ],
    );

    let mut cmd = gleon();
    cmd.current_dir(dir.path())
        .env("GLEON_STORAGE_URL", "s3://my-test-bucket/gleon")
        .env("GLEON_AWS_ACCESS_KEY_ID", "key")
        .env("GLEON_AWS_SECRET_ACCESS_KEY", "secret")
        .env("GLEON_AWS_REGION", "us-east-1")
        .args(["report", "markdown"])
        .assert()
        .success()
        // Baselines are signed by their content-addressed key, the same one `push` uploads to.
        .stdout(predicates::str::contains("my-test-bucket"))
        .stdout(predicates::str::contains("X-Amz-Signature"))
        .stdout(predicates::str::contains(format!(
            "blobs/sha256/{}",
            "a".repeat(64)
        )))
        .stdout(predicates::str::contains(format!(
            "blobs/sha256/{}",
            "b".repeat(64)
        )))
        // Candidates and diffs only ever exist on the runner.
        .stdout(predicates::str::contains("runs/latest").not());

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

    // A screenshot without a baseline: `gleon diff` keeps it as the candidate.
    let login_dir = base_path.join("login");
    std::fs::create_dir_all(&login_dir).expect("create_dir_all login");
    std::fs::copy(
        fixtures_dir.join("baseline_100x100.png"),
        login_dir.join("button.png"),
    )
    .expect("copy screenshot");
    std::fs::write(
        base_path.join(".gleon/gleon.yaml"),
        "required_version: \">=0.1.0\"\nscreenshots:\n  - include: \"login/*.png\"\n",
    )
    .unwrap();
    gleon().current_dir(base_path).arg("diff").assert().code(1);
    std::fs::write(base_path.join(".gleon/runs/latest/cases/broken.json"), "{").unwrap();

    // The run id names the workspace run, so an invalid one is an error.
    gleon()
        .current_dir(base_path)
        .env("GLEON_RUN_ID", "a:b")
        .arg("approve")
        .assert()
        .failure()
        .stderr(predicates::str::contains("invalid GLEON_RUN_ID"));
    let mut cmd = gleon();
    cmd.current_dir(base_path)
        .arg("approve")
        .assert()
        .success()
        .stderr(predicates::str::contains("Approved 1 screenshot(s)"))
        .stderr(predicates::str::contains(
            "skipped 1 invalid case report(s)",
        ));

    gleon()
        .current_dir(base_path)
        .arg("diff")
        .assert()
        .success();
}

/// `gleon test` runs the command as one run: it records the run, hands the command its run id
/// and exits with the command's code. The command here is `gleon diff` itself, so the test runs
/// without a shell on every OS.
#[test]
fn test_test_runs_the_command_as_one_run() {
    let dir = init_temp_dir();
    let gleon_bin = assert_cmd::cargo::cargo_bin("gleon");
    copy_fixture("baseline_100x100.png", &dir.path().join("shots/new.png"));
    std::fs::write(
        dir.path().join(".gleon/gleon.yaml"),
        "required_version: \">=0.1.0\"\nscreenshots:\n  - include: \"shots/*.png\"\n",
    )
    .unwrap();

    gleon()
        .current_dir(dir.path())
        .arg("test")
        .arg("--")
        .arg(&gleon_bin)
        .arg("diff")
        .assert()
        .code(1);
    let latest = dir.path().join(".gleon/runs/latest");
    let run: serde_json::Value =
        serde_json::from_slice(&std::fs::read(latest.join("run.json")).unwrap()).unwrap();
    let run_id = run["run_id"].as_str().unwrap();
    assert!(run_id.starts_with("run-"), "{run}");
    assert_eq!(run["command"][1], "diff");
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(latest.join("cases/shots/new.json")).unwrap())
            .unwrap();
    assert_eq!(report["run_id"], run_id);

    // A run id given by CI is kept.
    gleon()
        .current_dir(dir.path())
        .env("GLEON_RUN_ID", "ci-42")
        .args(["test", "--"])
        .arg(&gleon_bin)
        .arg("status")
        .assert()
        .success();
    let run: serde_json::Value =
        serde_json::from_slice(&std::fs::read(latest.join("run.json")).unwrap()).unwrap();
    assert_eq!(run["run_id"], "ci-42");

    // The command's own exit code (a usage error of clap), and 127 for none, like a shell.
    gleon()
        .current_dir(dir.path())
        .args(["test", "--"])
        .arg(&gleon_bin)
        .arg("--no-such-flag")
        .assert()
        .code(2);
    gleon()
        .current_dir(dir.path())
        .args(["test", "--", "gleon-no-such-program"])
        .assert()
        .code(127)
        .stderr(predicate::str::contains(
            "`gleon-no-such-program` was not found",
        ));

    // Only the process names a run: a `GLEON_RUN_ID` in `.gleon/.env` would stamp every run.
    std::fs::write(dir.path().join(".gleon/.env"), "GLEON_RUN_ID=from-dotenv\n").unwrap();
    gleon()
        .current_dir(dir.path())
        .args(["test", "--"])
        .arg(&gleon_bin)
        .arg("--version")
        .assert()
        .success();
    let run: serde_json::Value =
        serde_json::from_slice(&std::fs::read(latest.join("run.json")).unwrap()).unwrap();
    assert_ne!(run["run_id"], "from-dotenv");
}

/// `--from` paths are relative to where the command runs, like every path argument; `approve`
/// takes several runs (one per CI job).
#[test]
fn test_from_paths_are_relative_to_the_working_directory() {
    let dir = init_temp_dir();
    let sub = dir.path().join("packages/app");
    std::fs::create_dir_all(&sub).unwrap();
    let run = sub.join("dl/linux/latest");
    let case = mismatch("a", 1);
    let file = run.join("cases/a.json");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, case.to_string()).unwrap();

    for command in [["report", "markdown"], ["dashboard", "--push"]] {
        let assert = gleon()
            .current_dir(&sub)
            .args(command)
            .args(["--from", "dl/linux/latest"])
            .assert();
        if command[0] == "report" {
            assert.success().stdout(predicates::str::contains("`a`"));
        } else {
            // Read from the copy; only the missing storage stops it.
            assert
                .failure()
                .stderr(predicates::str::contains("Storage not configured"));
        }
    }
    // A run directory that does not exist is no empty run, and the caller's `GLEON_RUN_ID`
    // (invalid here) does not apply to copies.
    gleon()
        .current_dir(&sub)
        .env("GLEON_RUN_ID", "a:b")
        .args([
            "approve",
            "--from",
            "dl/linux/latest",
            "--from",
            "dl/macso/latest",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("'dl/macso/latest' is no run"));
    gleon()
        .current_dir(&sub)
        .env("GLEON_RUN_ID", "a:b")
        .args(["approve", "--from", "dl/linux/latest"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("candidate"));
    gleon()
        .current_dir(&sub)
        .args(["report", "markdown", "--from", "dl/linux"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("pass the `latest` directory"));
}

/// Outside a workspace (a monorepo root) the command runs with its run id, without a run file;
/// a command killed by a signal ends `gleon test` by the same signal (gleon became the command).
#[test]
fn test_test_outside_a_workspace_and_signals() {
    let dir = tempfile::tempdir().unwrap();
    let gleon_bin = assert_cmd::cargo::cargo_bin("gleon");
    gleon()
        .current_dir(dir.path())
        .arg("test")
        .arg("--")
        .arg(&gleon_bin)
        .arg("--version")
        .assert()
        .success()
        .stderr(predicates::str::contains("No gleon workspace"));
    assert!(!dir.path().join(".gleon").exists());

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;

        let output = gleon()
            .current_dir(dir.path())
            .args(["test", "--", "sh", "-c", "kill -TERM $$"])
            .output()
            .unwrap();
        assert_eq!(output.status.signal(), Some(15), "{output:?}");
    }
}

/// A PNG fixture of gleon-core (`tests/fixtures/<name>`).
fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../gleon-core/tests/fixtures")
        .join(name)
}

/// Copies the PNG fixture `name` to `path`, creating its folder.
fn copy_fixture(name: &str, path: &std::path::Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::copy(fixture(name), path).unwrap();
}

/// Invalid run settings and a missing workspace fail `gleon diff` before it compares anything.
#[test]
fn test_diff_rejects_invalid_settings() {
    let dir = init_temp_dir();
    for (var, value) in [("GLEON_ARTIFACTS_DIR", "/tmp"), ("GLEON_RUN_ID", "a:b")] {
        gleon()
            .current_dir(dir.path())
            .env(var, value)
            .arg("diff")
            .assert()
            .failure()
            .stderr(predicate::str::contains("Error running visual diff"));
    }
    let outside = tempfile::tempdir().unwrap();
    gleon()
        .current_dir(outside.path())
        .arg("diff")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Error running visual diff"));
}

/// `--auto-pull` is advertised by the CLI, so it must actually run a pull before diffing
/// rather than being silently ignored.
#[test]
fn test_diff_auto_pull_runs_pull_first() {
    let dir = init_temp_dir();

    let mut cmd = gleon();
    let assert = cmd
        .current_dir(dir.path())
        .args(["diff", "--auto-pull"])
        .assert();

    // No storage configured -> pull reports local mode, then the diff itself runs (and finds
    // no screenshots in the empty workspace).
    assert
        .failure()
        .stderr(predicate::str::contains("Running blob pull..."))
        .stderr(predicate::str::contains("no screenshots match"));
}

/// The whole loop a developer runs, through the binary: a test run fails, its reports show the
/// failures, approving them makes the next run pass. `gleon diff` is the test command, so it runs
/// without a shell on every OS.
#[test]
fn test_test_report_approve_end_to_end() {
    let dir = init_temp_dir();
    let root = dir.path();
    let gleon_bin = assert_cmd::cargo::cargo_bin("gleon");
    std::fs::write(
        root.join(".gleon/gleon.yaml"),
        "required_version: \">=0.1.0\"\nscreenshots:\n  - include: \"shots/*.png\"\n    mode: pixel\n    diff: { threshold: 0.0 }\n",
    )
    .unwrap();
    copy_fixture("baseline_100x100.png", &root.join("shots/changed.png"));
    gleon().current_dir(root).arg("stage").assert().success();
    copy_fixture(
        "diff_16px_corners_100x100.png",
        &root.join("shots/changed.png"),
    );
    copy_fixture("200x100.png", &root.join("shots/new.png"));
    let run = || {
        let mut test = gleon();
        test.current_dir(root)
            .args(["test", "--"])
            .arg(&gleon_bin)
            .arg("diff");
        test
    };

    run().assert().code(1);
    let out = root.join("out");
    gleon()
        .current_dir(root)
        .args(["report", "junit", "--out", "out/junit.xml"])
        .assert()
        .success();
    let junit = std::fs::read_to_string(out.join("junit.xml")).unwrap();
    assert!(
        junit.contains(r#"tests="2" failures="2" errors="0""#),
        "{junit}"
    );
    gleon()
        .current_dir(root)
        .args(["report", "html", "--out", "out/report.html"])
        .assert()
        .success();
    let html = std::fs::read_to_string(out.join("report.html")).unwrap();
    let sources: Vec<_> = html
        .split("src=\"")
        .skip(1)
        .map(|rest| rest.split('"').next().unwrap().replace("&#x2f;", "/"))
        .collect();
    // The mismatch: golden, candidate and diff; the new screenshot: its candidate.
    assert_eq!(sources.len(), 4, "{sources:?}");
    for source in &sources {
        assert!(out.join(source).is_file(), "{source} must exist");
    }
    gleon()
        .current_dir(root)
        .args(["report", "markdown"])
        .assert()
        .success()
        .stdout(predicate::str::contains("| `shots/changed` | Mismatch: "))
        .stdout(predicate::str::contains(
            "| `shots/new` | Missing Baseline: ",
        ));

    gleon()
        .current_dir(root)
        .arg("approve")
        .assert()
        .success()
        .stderr(predicate::str::contains("Approved 2 screenshot(s)"));
    run().assert().success();
}
