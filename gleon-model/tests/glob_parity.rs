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

//! `GlobPattern` (the small `glob` crate, kept out of the native library's size) matches like
//! `globset` did with `literal_separator` and `case_insensitive`, on every pattern the docs, the
//! fixtures and the tests use and on the paths a workspace holds.

use gleon_model::config::GlobPattern;

const PATTERNS: &[&str] = &[
    "*",
    "*.png",
    "**",
    "**/*.png",
    "**/clock*.png",
    "**/goldens/*.png",
    "**/masked/**",
    "a/*.png",
    "build/**",
    "example/**",
    "goldens/**/a.png",
    "login/*.png",
    "masked/**",
    "node_modules/**",
    "settings/**/*.png",
    "shots/*.png",
    "src/test.png",
    "target/**",
    "test/**/*.png",
    "test/[!a]*.png",
    "test/[a-c]*.png",
    "test/goldens/**/*.png",
    "test/goldens/clock?.png",
    "test/pic.png",
    "test/*/a.png",
];

const PATHS: &[&str] = &[
    "",
    ".hidden/a.png",
    "a.png",
    "a/b.png",
    "build",
    "build/a.png",
    "example/test/goldens/a.png",
    "goldens/a.png",
    "goldens/x/a.png",
    "login/a.png",
    "masked/a.png",
    "node_modules",
    "node_modules/x/a.png",
    "settings/a.png",
    "settings/x/y/a.png",
    "shots/a.png",
    "shots/x/a.png",
    "src/test.png",
    "Test/Goldens/A.PNG",
    "test",
    "test/.a.png",
    "test/a.png",
    "test/b.png",
    "test/d.png",
    "test/goldens",
    "test/goldens/a.png",
    "test/goldens/clock.png",
    "test/goldens/clock1.png",
    "test/goldens/macos-aarch64/a.png",
    "test/goldens/x/y/a.png",
    "test/pic.png",
    "test/x/a.png",
    "x/masked/y/a.png",
];

#[test]
fn test_glob_patterns_match_like_globset() {
    let mut differences = Vec::new();
    for pattern in PATTERNS {
        let ours = GlobPattern::new(pattern).unwrap();
        let theirs = globset::GlobBuilder::new(pattern)
            .literal_separator(true)
            .case_insensitive(true)
            .build()
            .unwrap()
            .compile_matcher();
        for path in PATHS {
            let (ours, theirs) = (ours.is_match(path), theirs.is_match(path));
            if ours != theirs {
                differences.push(format!("{pattern:?} on {path:?}: {ours}, globset {theirs}"));
            }
        }
    }
    assert!(differences.is_empty(), "{}", differences.join("\n"));
}

/// Both reject an unclosed character class.
#[test]
fn test_invalid_patterns_are_errors() {
    for pattern in ["test/[a-z", "["] {
        assert!(GlobPattern::new(pattern).is_err(), "{pattern}");
        assert!(globset::Glob::new(pattern).is_err(), "{pattern}");
    }
}

/// Where `globset` silently read a pattern differently (`**` inside a segment as `*`, `{a,b}`
/// alternatives, a trailing `/` that never matches a file), the pattern is a config error now.
#[test]
fn test_former_globset_readings_are_errors() {
    for pattern in [
        "a**",
        "**a.png",
        "a/**b",
        "{a,b}.png",
        "test/{x}/*.png",
        "build/",
        "a/**/",
    ] {
        assert!(GlobPattern::new(pattern).is_err(), "{pattern}");
    }
}
