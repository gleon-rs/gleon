//! Helpers shared by the integration tests.

use std::{fs, path::Path};

/// Copies the fixture config `tests/fixtures/<fixture>.yaml` (`config/…` or `platform/…`) to
/// `.gleon/gleon.yaml` of the workspace at `root`, creating `.gleon` when needed.
#[allow(
    dead_code,
    reason = "each integration test binary uses its own subset of the helpers"
)]
pub fn copy_config(root: &Path, fixture: &str) {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("{fixture}.yaml"));
    let gleon_dir = root.join(".gleon");
    fs::create_dir_all(&gleon_dir).expect("failed to create .gleon");
    fs::copy(&fixture, gleon_dir.join("gleon.yaml"))
        .unwrap_or_else(|error| panic!("copying {}: {error}", fixture.display()));
}

/// Variables of the environment that select runs, outputs, storage, the platform, the branch or
/// the license check; CI exports some job-wide (`GLEON_RUN_ID`), a developer's shell others.
/// Tests remove them so each sees only what it sets.
/// (`GITHUB_EVENT_PATH` stays: on GitHub Actions the license check reads from it that the
/// repository is public, and a test that needs an event writes its own.)
#[allow(
    dead_code,
    reason = "each integration test binary uses its own subset of the helpers"
)]
pub const GLEON_ENV: [&str; 17] = [
    "GLEON_RUN_ID",
    "GLEON_ARTIFACTS_DIR",
    "GLEON_METRICS",
    "GLEON_STORAGE_URL",
    "GLEON_CONCURRENCY",
    "GLEON_HTML_ARTIFACT_URL",
    "GLEON_REQUIRE_STORAGE",
    "GLEON_REQUIRE_CANONICAL",
    "GLEON_PLATFORM",
    "GLEON_FALLBACK_PLATFORM",
    "GLEON_OS",
    "GLEON_ARCH",
    "GLEON_RENDERER",
    "GLEON_BRANCH",
    "GLEON_STRICT",
    "GLEON_LICENSE_KEY",
    "GLEON_PROJECT_PATH",
];

/// The config of a goldens workspace as `gleon init` writes it for Flutter, shared with the
/// gleon-core tests (a path for [`copy_config`]).
#[allow(
    dead_code,
    reason = "each integration test binary uses its own subset of the helpers"
)]
pub const FLUTTER_GOLDENS: &str = "../../../gleon-core/tests/fixtures/config/flutter_goldens";

/// Clears the environment of `cmd` but for where an instrumented run (`cargo llvm-cov`) collects
/// profiles: without it the child writes its profile into its working directory, and its
/// coverage is lost.
#[allow(
    dead_code,
    reason = "each integration test binary uses its own subset of the helpers"
)]
pub fn clear_env(cmd: &mut assert_cmd::Command) {
    cmd.env_clear();
    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        cmd.env("LLVM_PROFILE_FILE", profile);
    }
}
