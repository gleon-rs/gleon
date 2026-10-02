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

//! Real text: renders of the Flutter package's `Caption` test widget (`test/helpers/caption.dart`
//! there) with Roboto, captured on macOS, and the text regions the package reported for each.
//! Every change a user can see must fail under the default text tolerance, on the shared
//! comparison pipeline the integrations and `gleon diff` use. Renders of the same golden on other
//! operating systems (benign noise that must pass) belong here once CI has captured them.
//!
//! Run `cargo test -p gleon-model --test text_corpus -- --nocapture` to print the metrics table.

use std::path::PathBuf;

use gleon_engine::Region;
use gleon_model::{
    case::Metrics,
    compare::{Candidate, Compared, Text, compare},
    tolerance::{TextTolerance, Tolerance},
};

/// The default text tolerance of the Flutter package.
const TEXT: TextTolerance = TextTolerance {
    color_tolerance: 24.0,
    max_diff_ratio: 0.1,
};

/// A render and the text regions (`[x, y, width, height]`) reported for it: one per line.
struct Render {
    name: &'static str,
    regions: &'static [[u32; 4]],
}

const HEADING: [u32; 4] = [9, 21, 71, 29];
const DIGITS: [u32; 4] = [33, 92, 84, 22];
/// The paragraph's three lines when its layout is unchanged.
const PARAGRAPH: [[u32; 4]; 3] = [[10, 45, 220, 18], [10, 59, 192, 18], [10, 73, 133, 18]];
const UNCHANGED: &[[u32; 4]] = &[DIGITS, PARAGRAPH[0], PARAGRAPH[1], PARAGRAPH[2], HEADING];

/// The golden itself.
const GOLDEN: Render = Render {
    name: "golden",
    regions: UNCHANGED,
};

/// Changes a user sees: each must fail.
const REGRESSIONS: [Render; 7] = [
    // A digit of the same width: no layout changes.
    Render {
        name: "digit",
        regions: UNCHANGED,
    },
    // "Submit" -> "Cancel" inside the paragraph.
    Render {
        name: "word",
        regions: UNCHANGED,
    },
    // #000 -> #444.
    Render {
        name: "color",
        regions: UNCHANGED,
    },
    // Weight 400 -> 700 and 400 -> 500 of the paragraph.
    Render {
        name: "bold",
        regions: &[
            DIGITS,
            [10, 45, 195, 18],
            [10, 59, 201, 18],
            [10, 73, 162, 18],
            HEADING,
        ],
    },
    Render {
        name: "medium",
        regions: &[
            DIGITS,
            [10, 45, 193, 18],
            [10, 59, 199, 18],
            [10, 73, 161, 18],
            HEADING,
        ],
    },
    // The paragraph moved by a whole pixel.
    Render {
        name: "shift",
        regions: &[
            DIGITS,
            [11, 45, 191, 18],
            [11, 59, 198, 18],
            [11, 73, 159, 18],
            HEADING,
        ],
    },
    // A 1px frame drawn tightly around the digits.
    Render {
        name: "frame",
        regions: UNCHANGED,
    },
];

fn read(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/text")
        .join(format!("{name}.png"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Compares `render` with the golden, exactly outside its text regions.
fn judge(render: &Render, text: TextTolerance) -> Compared {
    let regions: Vec<Region> = render
        .regions
        .iter()
        .map(|&[x, y, width, height]| Region {
            x,
            y,
            width,
            height,
        })
        .collect();
    compare(
        &read(GOLDEN.name),
        Candidate::Png(&read(render.name)),
        &Tolerance::Exact {},
        &[],
        Some(Text {
            regions: &regions,
            tolerance: text,
        }),
    )
    .unwrap()
    .compared
}

/// `(strict differing pixels, worst tile share)` of a comparison.
fn measured(compared: &Compared) -> (u64, f64) {
    match compared {
        Compared::Match { metrics, .. } | Compared::Mismatch { metrics, .. } => match metrics {
            Metrics::Pixel {
                diff_pixels, text, ..
            } => (
                *diff_pixels,
                text.map_or(0.0, |text| text.worst_tile_diff_ratio),
            ),
            Metrics::Ssim { .. } => panic!("pixel metrics expected"),
        },
        Compared::DimensionMismatch { .. } => panic!("same sizes expected"),
    }
}

#[test]
fn test_the_golden_matches_itself() {
    assert!(matches!(judge(&GOLDEN, TEXT), Compared::Match { .. }));
}

#[test]
fn test_every_visible_change_fails() {
    println!("render  strict px  worst tile");
    for render in &REGRESSIONS {
        let compared = judge(render, TEXT);
        let (strict, worst) = measured(&compared);
        println!("{:<7} {strict:>9}  {:>9.2}%", render.name, worst * 100.0);
        assert!(
            matches!(compared, Compared::Mismatch { .. }),
            "{} passed: {strict} strict pixels, worst tile {worst}",
            render.name
        );
    }
}
