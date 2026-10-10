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

//! Timings of the shared comparison pipeline on real Flutter goldens (the example app's counter,
//! 360x640, recorded with real fonts) and on a phone-sized frame tiled from it (1080x2560): the
//! work of one `gleon_golden` call without the file system.
//!
//! `cargo test` runs every case once; the timings are ignored by default, run them in release for
//! numbers that mean something:
//! `cargo test --release -p gleon-model --test perf -- --ignored --nocapture`.

use std::{
    hint::black_box,
    path::PathBuf,
    time::{Duration, Instant},
};

use gleon_engine::{Region, decode::decode_rgba};
use gleon_model::{
    compare::{Candidate, Compared, Text, compare, encode_png},
    tolerance::{TextTolerance, Tolerance},
};
use image::{RgbaImage, imageops};

const EXACT: Tolerance = Tolerance::Exact {};
const SSIM: Tolerance = Tolerance::Ssim {
    min_similarity: 0.99,
    color_tolerance: 8.0,
};
/// Every pixel option on.
const OPTIONS: Tolerance = Tolerance::Pixel {
    max_diff_ratio: 0.0,
    channel_tolerance: 8,
    anti_alias: true,
    edge_threshold: 64,
};
/// The anti-aliasing detection and the edge mask without a channel tolerance: the most work
/// per differing pixel (nothing short-cuts them).
const AA_EDGES: Tolerance = Tolerance::Pixel {
    max_diff_ratio: 0.0,
    channel_tolerance: 0,
    anti_alias: true,
    edge_threshold: 64,
};

fn fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/flutter")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// `image` tiled 3x4 times: a phone-sized frame with the same content.
fn tiled(image: &RgbaImage) -> RgbaImage {
    let (width, height) = image.dimensions();
    let mut frame = RgbaImage::new(width * 3, height * 4);
    for row in 0..4 {
        for column in 0..3 {
            imageops::replace(
                &mut frame,
                image,
                i64::from(column * width),
                i64::from(row * height),
            );
        }
    }
    frame
}

/// Text regions like the package reports for a screen: lines of 18 px every 60 px.
fn lines(width: u32, height: u32) -> Vec<Region> {
    (0..height / 60)
        .map(|line| Region {
            x: 16,
            y: line * 60 + 20,
            width: width - 32,
            height: 18,
        })
        .collect()
}

/// The median time of `runs` calls of `f`.
fn median(runs: usize, mut f: impl FnMut()) -> Duration {
    f();
    let mut times: Vec<Duration> = (0..runs)
        .map(|_| {
            let started = Instant::now();
            f();
            started.elapsed()
        })
        .collect();
    times.sort_unstable();
    times[runs / 2]
}

/// A named case and one run of it.
type Case<'a> = (&'static str, Box<dyn Fn() + 'a>);

struct Frame {
    name: &'static str,
    golden: Vec<u8>,
    same: RgbaImage,
    changed: RgbaImage,
    /// Every pixel differs (the colors inverted).
    inverted: RgbaImage,
}

impl Frame {
    fn new(name: &'static str, golden: RgbaImage, changed: RgbaImage) -> Self {
        let mut inverted = golden.clone();
        for pixel in inverted.pixels_mut() {
            let [r, g, b, a] = pixel.0;
            pixel.0 = [255 - r, 255 - g, 255 - b, a];
        }
        Self {
            name,
            golden: encode_png(&golden).unwrap(),
            same: golden,
            changed,
            inverted,
        }
    }
}

fn raw(image: &RgbaImage) -> Candidate<'_> {
    Candidate::Rgba {
        width: image.width(),
        height: image.height(),
        pixels: image.as_raw(),
    }
}

/// Every case once, so the timings below never rot unnoticed (`cargo test` runs this).
#[test]
fn perf_cases_run() {
    run_cases(1, false);
}

#[test]
#[ignore = "timings: run in release with --ignored --nocapture"]
fn perf_of_one_comparison() {
    run_cases(0, true);
}

/// Runs every case `runs` times (0: enough for a stable median per frame), printing the median
/// when `prints`.
fn run_cases(runs: usize, prints: bool) {
    let initial = decode_rgba(&fixture("counter_initial.png")).unwrap();
    let taps = decode_rgba(&fixture("counter_three_taps.png")).unwrap();
    let frames = [
        Frame::new("360x640", initial.clone(), taps.clone()),
        Frame::new("1080x2560", tiled(&initial), tiled(&taps)),
    ];
    if prints {
        eprintln!("{:<10} {:<34} {:>10}", "frame", "case", "median");
    }
    for frame in &frames {
        let (width, height) = frame.same.dimensions();
        let regions = lines(width, height);
        let text = Some(Text {
            regions: &regions,
            tolerance: TextTolerance::OWN_PLATFORM,
        });
        let other_png = encode_png(&frame.same).unwrap();
        let runs = match runs {
            0 if width > 1000 => 15,
            0 => 51,
            runs => runs,
        };
        let compared = |candidate: Candidate<'_>, tolerance: &Tolerance, text| {
            black_box(compare(&frame.golden, candidate, tolerance, &[], text).unwrap());
        };
        let cases: [Case<'_>; 12] = [
            (
                "decode golden",
                Box::new(|| {
                    black_box(decode_rgba(&frame.golden).unwrap());
                }),
            ),
            (
                "raw pass, exact",
                Box::new(|| compared(raw(&frame.same), &EXACT, None)),
            ),
            (
                "raw pass, exact + text",
                Box::new(|| compared(raw(&frame.same), &EXACT, text)),
            ),
            (
                "raw fail, exact + text",
                Box::new(|| compared(raw(&frame.changed), &EXACT, text)),
            ),
            (
                "png same pixels, exact",
                Box::new(|| compared(Candidate::Png(&other_png), &EXACT, None)),
            ),
            (
                "raw pass, pixel options",
                Box::new(|| compared(raw(&frame.same), &OPTIONS, None)),
            ),
            (
                "raw fail, pixel options",
                Box::new(|| compared(raw(&frame.changed), &OPTIONS, None)),
            ),
            (
                "raw full change, aa + edges",
                Box::new(|| compared(raw(&frame.inverted), &AA_EDGES, None)),
            ),
            (
                "raw pass, ssim",
                Box::new(|| compared(raw(&frame.same), &SSIM, None)),
            ),
            (
                "raw pass, ssim + text",
                Box::new(|| compared(raw(&frame.same), &SSIM, text)),
            ),
            (
                "raw fail, ssim + text",
                Box::new(|| compared(raw(&frame.changed), &SSIM, text)),
            ),
            (
                "raw fail, ssim",
                Box::new(|| compared(raw(&frame.changed), &SSIM, None)),
            ),
        ];
        for (case, run) in &cases {
            let time = median(runs, run);
            if prints {
                eprintln!(
                    "{:<10} {:<34} {:>8.3} ms",
                    frame.name,
                    case,
                    time.as_secs_f64() * 1000.0
                );
            }
        }
        assert!(matches!(
            compare(&frame.golden, raw(&frame.changed), &EXACT, &[], text)
                .unwrap()
                .compared,
            Compared::Mismatch { .. }
        ));
    }
}
