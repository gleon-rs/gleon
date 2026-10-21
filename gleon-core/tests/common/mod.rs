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

/// A `baselines: goldens` workspace at `root` (the Flutter template): a golden PNG under
/// `test/goldens/` and the case reports of a Flutter run in `.gleon/runs/latest/cases/`.
/// Returns the golden's path and the files under `.gleon/runs/` with their contents, to check
/// that a refused command left them alone.
#[allow(
    dead_code,
    reason = "each integration test binary uses its own subset of the helpers"
)]
pub fn goldens_workspace(root: &Path) -> (std::path::PathBuf, Vec<(std::path::PathBuf, Vec<u8>)>) {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    copy_config(root, "flutter_goldens");
    let golden = root.join("test/goldens/counter_initial.png");
    fs::create_dir_all(golden.parent().unwrap()).unwrap();
    fs::copy(fixtures.join("baseline_100x100.png"), &golden).unwrap();
    let runs = root.join(".gleon/runs/latest");
    let source = fixtures.join("cases/flutter-linux-x64");
    let mut files = Vec::new();
    for entry in walk(&source) {
        let target = runs.join(entry.strip_prefix(&source).unwrap());
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::copy(&entry, &target).unwrap();
        files.push((target.clone(), fs::read(&target).unwrap()));
    }
    assert!(!files.is_empty());
    (golden, files)
}

/// Every file below `dir`.
#[allow(
    dead_code,
    reason = "each integration test binary uses its own subset of the helpers"
)]
fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else {
            files.push(path);
        }
    }
    files
}

/// Asserts that `files` (from [`goldens_workspace`]) are unchanged.
#[allow(
    dead_code,
    reason = "each integration test binary uses its own subset of the helpers"
)]
pub fn assert_unchanged(files: &[(std::path::PathBuf, Vec<u8>)]) {
    for (path, bytes) in files {
        assert_eq!(&fs::read(path).unwrap(), bytes, "{}", path.display());
    }
}
