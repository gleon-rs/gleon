//! A golden PNG against raw candidate pixels of any size and length: an error or a verdict, never
//! a panic (the integrations pass these straight from a capture).
#![no_main]

use gleon_model::{
    compare::{Candidate, compare},
    tolerance::Tolerance,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: (&[u8], u8, u8, &[u8])| {
    let (golden, width, height, pixels) = input;
    let candidate = Candidate::Rgba {
        width: u32::from(width),
        height: u32::from(height),
        pixels,
    };
    for tolerance in [
        Tolerance::Exact {},
        // Every pixel option reads neighbors: 0x0, 1x1 and 1xN images included.
        Tolerance::Pixel {
            max_diff_ratio: 0.0,
            channel_tolerance: 4,
            anti_alias: true,
            edge_threshold: 64,
        },
        Tolerance::Ssim {
            min_similarity: 0.9,
            color_tolerance: 8.0,
        },
    ] {
        let _ = compare(golden, candidate, &tolerance, &[], None);
    }
});
