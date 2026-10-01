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

//! Load time of a large manifest tree (`WorkspaceIndex::load`), measured on demand:
//! `cargo test --release -p gleon-core --test manifest_scale -- --ignored --nocapture`.

use std::{fs, time::Instant};

use gleon_core::manifest::WorkspaceIndex;

/// Manifests in the tree, spread over [`DIRS`] directories.
const MANIFESTS: usize = 50_000;
const DIRS: usize = 100;
/// Loads measured after one warm-up load.
const RUNS: usize = 5;

#[test]
#[cfg(not(miri))]
#[ignore = "benchmark: writes 50k files, run with --ignored --nocapture"]
fn test_loads_50k_manifests() {
    let temp = tempfile::tempdir().unwrap();
    let platform_dir = temp.path().join(".gleon/manifests/macos-aarch64");
    let per_dir = MANIFESTS / DIRS;
    for dir in 0..DIRS {
        let dir_path = platform_dir.join(format!("feature_{dir:03}"));
        fs::create_dir_all(&dir_path).unwrap();
        for file in 0..per_dir {
            let id = dir * per_dir + file;
            let manifest = format!(
                r#"{{"schema_version":1,"hash":"sha256:{id:064x}","phash":"dhash:{id:016x}","width":1080,"height":1920}}"#
            );
            fs::write(dir_path.join(format!("screen_{file:04}.json")), manifest).unwrap();
        }
    }

    let warm_up = WorkspaceIndex::load(&platform_dir).unwrap();
    assert_eq!(warm_up.len(), MANIFESTS);
    let mut timings: Vec<_> = (0..RUNS)
        .map(|_| {
            let started = Instant::now();
            let index = WorkspaceIndex::load(&platform_dir).unwrap();
            let elapsed = started.elapsed();
            assert_eq!(index.len(), MANIFESTS);
            elapsed
        })
        .collect();
    timings.sort();
    println!(
        "WorkspaceIndex::load of {MANIFESTS} manifests: min {:?}, median {:?}, max {:?} ({} cores)",
        timings[0],
        timings[RUNS / 2],
        timings[RUNS - 1],
        std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
    );
}
