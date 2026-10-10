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
