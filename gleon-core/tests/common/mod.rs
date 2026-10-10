//! Helpers shared by the integration tests.

use std::{fs, path::Path};

/// Copies the fixture config `tests/fixtures/config/<name>.yaml` to `.gleon/gleon.yaml` of the
/// workspace at `root`, creating `.gleon` when needed.
pub fn copy_config(root: &Path, name: &str) {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/config")
        .join(format!("{name}.yaml"));
    let gleon_dir = root.join(".gleon");
    fs::create_dir_all(&gleon_dir).unwrap();
    fs::copy(&fixture, gleon_dir.join("gleon.yaml"))
        .unwrap_or_else(|error| panic!("copying {}: {error}", fixture.display()));
}
