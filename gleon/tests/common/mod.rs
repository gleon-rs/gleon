//! Helpers shared by the integration tests.

use std::{fs, path::Path};

/// Copies the fixture config `tests/fixtures/<fixture>.yaml` (`config/…` or `platform/…`) to
/// `.gleon/gleon.yaml` of the workspace at `root`, creating `.gleon` when needed.
pub fn copy_config(root: &Path, fixture: &str) {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("{fixture}.yaml"));
    let gleon_dir = root.join(".gleon");
    fs::create_dir_all(&gleon_dir).expect("failed to create .gleon");
    fs::copy(&fixture, gleon_dir.join("gleon.yaml"))
        .unwrap_or_else(|error| panic!("copying {}: {error}", fixture.display()));
}

/// Variables of the environment that select runs, outputs, storage or the platform; CI exports
/// some job-wide (`GLEON_RUN_ID`). Tests remove them so each sees only what it sets.
/// (`GITHUB_EVENT_PATH` stays: on GitHub Actions the license check reads from it that the
/// repository is public, and a test that needs an event writes its own.)
#[allow(
    dead_code,
    reason = "each integration test binary uses its own subset of the helpers"
)]
pub const GLEON_ENV: [&str; 9] = [
    "GLEON_RUN_ID",
    "GLEON_ARTIFACTS_DIR",
    "GLEON_METRICS",
    "GLEON_STORAGE_URL",
    "GLEON_HTML_ARTIFACT_URL",
    "GLEON_REQUIRE_STORAGE",
    "GLEON_PLATFORM",
    "GLEON_FALLBACK_PLATFORM",
    "GLEON_OS",
];

/// The config of a goldens workspace as `gleon init` writes it for Flutter, shared with the
/// gleon-core tests (a path for [`copy_config`]).
#[allow(
    dead_code,
    reason = "each integration test binary uses its own subset of the helpers"
)]
pub const FLUTTER_GOLDENS: &str = "../../../gleon-core/tests/fixtures/config/flutter_goldens";
