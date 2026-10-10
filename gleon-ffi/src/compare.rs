//! Comparing a golden with a candidate: the shared pipeline of `gleon_model::compare`, timed.
//!
//! The engine runs on the calling thread (`gleon-engine` without its `parallel` feature): the test
//! runners of the integrations (`flutter test`, Playwright) parallelize across worker processes,
//! each loading its own copy of the library, so a thread pool per process would only multiply
//! threads (workers x cores) and add a hand-off per pass.

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use gleon_engine::config::Zone;
use gleon_model::{
    compare::{Candidate, Comparison, Text},
    tolerance::Tolerance,
};

use crate::error::Failure;

/// A comparison and the time it took.
#[derive(Debug)]
pub struct Timed {
    /// What the comparison found, or why the images could not be compared (never a pass).
    pub comparison: Result<Comparison, Failure>,
    /// Time spent decoding, comparing and encoding the diff.
    pub native: Duration,
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
) -> Timed {
    let started = Instant::now();
    let comparison = gleon_model::compare::compare(golden, candidate, tolerance, masks, text)
        .map_err(|error| Failure::new(error.kind().into(), error.to_string()));
    Timed {
        comparison,
        native: started.elapsed(),
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
    use gleon_model::compare::{Compared, encode_png};
    use image::{ImageBuffer, Rgba};

    use super::*;
    use crate::error::ErrorKind;

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
            compare(&a, Candidate::Png(&a), &EXACT, &[mask], None).comparison,
            Ok(Comparison {
                compared: Compared::Match { .. },
                clamped_masks: 1,
            })
        ));
        assert!(matches!(
            compare(&a, Candidate::Png(&png(12, 10)), &EXACT, &[], None).comparison,
            Ok(Comparison {
                compared: Compared::DimensionMismatch {
                    golden: (10, 10),
                    candidate: (12, 10),
                    diff_png: Some(_),
                },
                ..
            })
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
                compare(golden, Candidate::Png(candidate), &EXACT, &[], None).comparison,
                Err(Failure { kind: ErrorKind::Image, message }) if message.contains(needle)
            ));
        }
    }
}
