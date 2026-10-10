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
//! there) with Roboto, captured on macOS, and the text regions the package reported for each, on
//! the shared comparison pipeline the integrations and `gleon diff` use.
//!
//! The default text tolerance lets text never fail (other operating systems rasterize glyphs
//! differently, by about 40% of a tile), so it catches every change of layout and none of text
//! alone: that is its documented trade-off. A tolerance of 10% catches every change, on the
//! operating system the golden was recorded on.
//!
//! Run `cargo test -p gleon-model --test text_corpus -- --nocapture` to print the metrics table.

use std::path::PathBuf;

use gleon_engine::Region;
use gleon_model::{
    case::Metrics,
    compare::{Candidate, Compared, Text, compare},
    tolerance::{TextTolerance, Tolerance},
};

/// A tolerance that compares text, for goldens of the same operating system.
const STRICT: TextTolerance = TextTolerance(0.1);

/// Renders that change only text: the default tolerance lets them pass.
const TEXT_ONLY: [&str; 4] = ["digit", "word", "color", "frame"];

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
    judge_under(render, &Tolerance::Exact {}, text)
}

/// Compares `render` with the golden under `tolerance` outside its text regions.
fn judge_under(render: &Render, tolerance: &Tolerance, text: TextTolerance) -> Compared {
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
        tolerance,
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
    for tolerance in [TextTolerance::DEFAULT, STRICT] {
        assert!(matches!(judge(&GOLDEN, tolerance), Compared::Match { .. }));
    }
}

/// Every change a user can see fails while text is compared.
#[test]
fn test_every_visible_change_fails_while_text_is_compared() {
    println!("render  strict px  worst tile");
    for render in &REGRESSIONS {
        let compared = judge(render, STRICT);
        let (strict, worst) = measured(&compared);
        println!("{:<7} {strict:>9}  {:>9.2}%", render.name, worst * 100.0);
        assert!(
            matches!(compared, Compared::Mismatch { .. }),
            "{} passed: {strict} strict pixels, worst tile {worst}",
            render.name
        );
    }
}

/// By default text never fails: changes of text alone pass, changes of layout still fail on the
/// pixels around the text.
#[test]
fn test_the_default_compares_layout_not_text() {
    for render in &REGRESSIONS {
        let compared = judge(render, TextTolerance::DEFAULT);
        let is_text_only = TEXT_ONLY.contains(&render.name);
        assert_eq!(
            matches!(compared, Compared::Match { .. }),
            is_text_only,
            "{}: {:?}",
            render.name,
            measured(&compared)
        );
    }
}

/// SSIM judges text by the same tiles and leaves it out of both gates (policy 4): the matrix of
/// the exact comparison holds under the default SSIM tolerance too.
#[test]
fn test_ssim_with_text_follows_the_same_matrix() {
    let ssim = Tolerance::Ssim {
        min_similarity: 0.8,
        color_tolerance: 8.0,
    };
    for tolerance in [TextTolerance::DEFAULT, STRICT] {
        assert!(matches!(
            judge_under(&GOLDEN, &ssim, tolerance),
            Compared::Match { .. }
        ));
    }
    println!("render  ssim, text ignored  ssim, text <= 10%");
    for render in &REGRESSIONS {
        let ignored = judge_under(render, &ssim, TextTolerance::DEFAULT);
        let strict = judge_under(render, &ssim, STRICT);
        let verdict = |compared: &Compared| match compared {
            Compared::Match { .. } => "pass",
            _ => "FAIL",
        };
        println!(
            "{:<7} {:>18}  {:>17}",
            render.name,
            verdict(&ignored),
            verdict(&strict)
        );
        assert!(
            matches!(strict, Compared::Mismatch { .. }),
            "{} passes SSIM with text compared",
            render.name
        );
        assert_eq!(
            matches!(ignored, Compared::Match { .. }),
            TEXT_ONLY.contains(&render.name),
            "{} under SSIM with text ignored",
            render.name
        );
    }
}
