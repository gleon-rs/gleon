//! Comparing two PNG-encoded images: the shared pipeline of `gleon_model::compare`, run
//! single-threaded and timed.

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use gleon_engine::config::Zone;
use gleon_model::{
    case::{Metrics, RegionMetrics},
    compare::{Candidate, CompareError, Compared, Text},
    tolerance::Tolerance,
};

use crate::error::{ErrorKind, Failure};

/// Result of comparing two PNGs.
#[derive(Debug)]
pub enum Comparison {
    /// Within the tolerance.
    Match {
        /// Whole-image metrics, reported for matches too (headroom to the tolerance).
        metrics: Metrics,
        /// The compared regions (the whole image, then the worst tile of text).
        regions: Vec<RegionMetrics>,
        /// Masks that reached beyond the image and were clipped.
        clamped_masks: usize,
        /// Time spent decoding and comparing.
        native: Duration,
    },
    /// Beyond the tolerance.
    Mismatch {
        /// Whole-image metrics.
        metrics: Metrics,
        /// The compared regions (the whole image, then the worst tile of text).
        regions: Vec<RegionMetrics>,
        /// PNG-encoded diff visualization.
        diff_png: Vec<u8>,
        /// Masks that reached beyond the image and were clipped.
        clamped_masks: usize,
        /// Time spent decoding, comparing and encoding the diff.
        native: Duration,
    },
    /// Different image sizes; no pixel comparison was attempted.
    DimensionMismatch {
        /// Width and height of the golden.
        golden: (u32, u32),
        /// Width and height of the candidate.
        candidate: (u32, u32),
        /// Time spent decoding.
        native: Duration,
    },
    /// Invalid input (a corrupt PNG, an image over the SSIM budget); never a pass.
    Error(Failure),
}

/// Runs the engine single-threaded inside this library.
///
/// The test runners of the integrations (`flutter test`, Playwright) run their workers as
/// processes, each loading its own copy of the library, and parallelize across them; an all-core
/// rayon pool per process would multiply threads (workers x cores) and thrash. An in-process
/// parallel runner would need the pool size as a session input instead. The CLI keeps its
/// parallel global pool. Only the first initialization of the process-wide pool can succeed, so a
/// failure means it is already set up.
fn limit_engine_threads() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let _already_initialized = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build_global();
    });
}

/// Compares the PNG `golden` with `candidate` under `tolerance` with `masks` and `text`, exactly
/// like `gleon diff` (`gleon_model::compare::compare`).
#[must_use]
pub fn compare(
    golden: &[u8],
    candidate: Candidate<'_>,
    tolerance: &Tolerance,
    masks: &[Zone],
    text: Option<Text<'_>>,
) -> Comparison {
    limit_engine_threads();
    let started = Instant::now();
    let compared = gleon_model::compare::compare(golden, candidate, tolerance, masks, text);
    let native = started.elapsed();
    match compared {
        Ok(comparison) => {
            let clamped_masks = comparison.clamped_masks;
            match comparison.compared {
                Compared::Match { metrics, regions } => Comparison::Match {
                    metrics,
                    regions,
                    clamped_masks,
                    native,
                },
                Compared::Mismatch {
                    metrics,
                    regions,
                    diff_png,
                } => Comparison::Mismatch {
                    metrics,
                    regions,
                    diff_png,
                    clamped_masks,
                    native,
                },
                Compared::DimensionMismatch { golden, candidate } => {
                    Comparison::DimensionMismatch {
                        golden,
                        candidate,
                        native,
                    }
                }
            }
        }
        Err(error) => {
            let kind = match error {
                CompareError::Internal => ErrorKind::Internal,
                CompareError::CandidatePixels { .. } => ErrorKind::InvalidInput,
                _ => ErrorKind::Image,
            };
            Comparison::Error(Failure::new(kind, error.to_string()))
        }
    }
}

#[cfg(all(test, not(miri)))]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::pedantic,
    clippy::nursery,
    reason = "test code: panics are assertions, and pedantic/nursery style lints are not enforced in tests"
)]
mod tests {
    use gleon_engine::config::Dimension;
    use gleon_model::compare::encode_png;
    use image::{ImageBuffer, Rgba};

    use super::*;

    const RED: Rgba<u8> = Rgba([255, 0, 0, 255]);
    const EXACT: Tolerance = Tolerance::Exact {};

    fn png(width: u32, height: u32) -> Vec<u8> {
        encode_png(&ImageBuffer::from_pixel(width, height, RED)).unwrap()
    }

    /// The shared comparison (tested in `gleon_model::compare`) reaches the integration with
    /// its clipped masks and timing.
    #[test]
    fn test_comparisons_carry_clipped_masks() {
        let a = png(10, 10);
        let mask = Zone {
            x: 9,
            y: 0,
            width: Dimension::Pixels(5),
            height: Dimension::Pixels(2),
        };
        assert!(matches!(
            compare(&a, Candidate::Png(&a), &EXACT, &[mask], None),
            Comparison::Match {
                clamped_masks: 1,
                ..
            }
        ));
        assert!(matches!(
            compare(&a, Candidate::Png(&png(12, 10)), &EXACT, &[], None),
            Comparison::DimensionMismatch {
                golden: (10, 10),
                candidate: (12, 10),
                ..
            }
        ));
    }

    #[test]
    fn test_corrupt_images_are_errors_not_passes() {
        let a = png(4, 4);
        for (golden, candidate, needle) in [
            (&b"garbage"[..], &a[..], "golden image"),
            (&a[..], &b"garbage"[..], "candidate image"),
        ] {
            assert!(matches!(
                compare(golden, Candidate::Png(candidate), &EXACT, &[], None),
                Comparison::Error(Failure { kind: ErrorKind::Image, message })
                    if message.contains(needle)
            ));
        }
    }
}
