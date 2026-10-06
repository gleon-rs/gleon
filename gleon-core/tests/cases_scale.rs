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

//! Time of reading and clearing a large run of two platforms (`Cases::load`,
//! `cases::remove_reports_of`), measured on demand:
//! `cargo test --release -p gleon-core --test cases_scale -- --ignored --nocapture`.

use std::{fs, path::Path, time::Instant};

use gleon_core::{cases::Cases, platform::PlatformKey};

/// Case reports in the run, half on each platform, spread over [`DIRS`] directories.
const REPORTS: usize = 50_000;
const DIRS: usize = 100;
const PLATFORMS: [(&str, &str, &str); 2] = [
    ("linux-x86_64", "linux", "x86_64"),
    ("macos-aarch64", "macos", "aarch64"),
];
/// Loads measured after one warm-up load.
const RUNS: usize = 5;

#[test]
#[cfg(not(miri))]
#[ignore = "benchmark: writes 50k files, run with --ignored --nocapture"]
fn test_loads_and_clears_50k_case_reports_of_two_platforms() {
    let temp = tempfile::tempdir().unwrap();
    let runs_latest = temp.path().join(".gleon/runs/latest");
    // The real Flutter fixture (~2 KB), a pass: no images.
    let template: serde_json::Value = serde_json::from_slice(
        &fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join(
            "tests/fixtures/cases/flutter-linux-x64/cases/linux-x86_64/test/goldens/counter_three_taps.json",
        ))
        .unwrap(),
    )
    .unwrap();
    for i in 0..REPORTS {
        let (key, os, arch) = PLATFORMS[i % PLATFORMS.len()];
        let name = format!("test/dir_{:03}/golden_{:05}", i % DIRS, i / PLATFORMS.len());
        let mut report = template.clone();
        report["name"] = name.clone().into();
        report["platform"] = serde_json::json!({"os": os, "arch": arch});
        let file = runs_latest
            .join("cases")
            .join(key)
            .join(format!("{name}.json"));
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, report.to_string()).unwrap();
    }

    let warm_up = Cases::load(&runs_latest, None).unwrap();
    assert_eq!(warm_up.reports().len(), REPORTS);
    assert!(warm_up.spans_platforms());
    assert!(warm_up.warnings().is_empty(), "{:?}", warm_up.warnings());
    let mut timings: Vec<_> = (0..RUNS)
        .map(|_| {
            let started = Instant::now();
            let cases = Cases::load(&runs_latest, None).unwrap();
            let elapsed = started.elapsed();
            assert_eq!(cases.reports().len(), REPORTS);
            elapsed
        })
        .collect();
    timings.sort();
    println!(
        "Cases::load of {REPORTS} case reports on {} platforms: min {:?}, median {:?}, max {:?} \
         ({} cores)",
        PLATFORMS.len(),
        timings[0],
        timings[RUNS / 2],
        timings[RUNS - 1],
        std::thread::available_parallelism().map_or(1, usize::from)
    );

    // `gleon diff` clears its platform's reports before it runs: here the integration's.
    let linux = PlatformKey::parse(PLATFORMS[0].0).unwrap();
    let started = Instant::now();
    gleon_core::cases::remove_reports_of(&runs_latest, "gleon_flutter", &linux).unwrap();
    println!(
        "remove_reports_of {} case reports of one platform: {:?}",
        REPORTS / PLATFORMS.len(),
        started.elapsed()
    );
    let rest = Cases::load(&runs_latest, None).unwrap();
    assert_eq!(rest.reports().len(), REPORTS / PLATFORMS.len());
    assert!(!rest.spans_platforms());
}
