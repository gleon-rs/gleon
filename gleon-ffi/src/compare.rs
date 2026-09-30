//! Decoding and comparing two PNG-encoded images with the engine.

#![forbid(unsafe_code)]

use std::time::{Duration, Instant};

use gleon_engine::{
    ComparisonResult, Measurement, compare_images,
    config::Zone,
    decode::{DecodeError, decode_rgba},
    masking::apply_masks,
};
use gleon_model::{case::Metrics, tolerance::Tolerance};
use image::{ImageFormat, RgbaImage};

use crate::error::{ErrorKind, Failure};

/// Result of comparing two PNGs.
#[derive(Debug)]
pub enum Comparison {
    /// Within the tolerance.
    Match {
        /// Whole-image metrics, reported for matches too (headroom to the tolerance).
        metrics: Metrics,
        /// Masks that reached beyond the image and were clipped.
        clamped_masks: usize,
        /// Time spent decoding and comparing.
        native: Duration,
    },
    /// Beyond the tolerance.
    Mismatch {
        /// Whole-image metrics.
        metrics: Metrics,
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

fn decode(label: &str, bytes: &[u8]) -> Result<RgbaImage, Failure> {
    decode_rgba(bytes)
        .map_err(|e: DecodeError| Failure::new(ErrorKind::Image, format!("{label} image: {e}")))
}

/// Encodes `image` as PNG.
///
/// # Errors
/// Returns the encoder's message.
pub fn encode_png(image: &RgbaImage) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
        .map(|()| bytes)
        .map_err(|e| format!("failed to encode diff image: {e}"))
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

/// Compares the PNGs `golden` and `candidate` under `tolerance`.
///
/// Masks are applied to both images before comparing, exactly like `gleon diff`.
#[must_use]
pub fn compare(
    golden: &[u8],
    candidate: &[u8],
    tolerance: &Tolerance,
    masks: &[Zone],
) -> Comparison {
    limit_engine_threads();
    let image_error = |message: String| Failure::new(ErrorKind::Image, message);
    let run = || -> Result<Comparison, Failure> {
        let (mode, config) = tolerance.engine_config();
        let started = Instant::now();
        let mut golden_img = decode("golden", golden)?;
        let mut candidate_img = decode("candidate", candidate)?;
        let golden_size = golden_img.dimensions();
        let candidate_size = candidate_img.dimensions();
        let clamped_masks = if !masks.is_empty() && golden_size == candidate_size {
            apply_masks(&mut candidate_img, masks);
            apply_masks(&mut golden_img, masks)
        } else {
            0
        };
        let total_pixels = u64::from(golden_size.0) * u64::from(golden_size.1);
        let metrics_of = |measurement| metrics_of(&measurement, tolerance, total_pixels);
        match compare_images(&golden_img, &candidate_img, mode, &config) {
            ComparisonResult::Match { measurement } => Ok(Comparison::Match {
                metrics: metrics_of(measurement)?,
                clamped_masks,
                native: started.elapsed(),
            }),
            ComparisonResult::TooLarge {
                size: (width, height),
            } => Err(image_error(format!(
                "{width}x{height} exceeds the SSIM analysis budget of {} pixels; use exact or \
                 pixel mode or a smaller capture",
                gleon_engine::ssim::MAX_ANALYSIS_PIXELS
            ))),
            ComparisonResult::DimensionMismatch { .. } => Ok(Comparison::DimensionMismatch {
                golden: golden_size,
                candidate: candidate_size,
                native: started.elapsed(),
            }),
            ComparisonResult::Mismatch {
                measurement,
                diff_image,
            } => Ok(Comparison::Mismatch {
                metrics: metrics_of(measurement)?,
                diff_png: encode_png(&diff_image).map_err(image_error)?,
                clamped_masks,
                native: started.elapsed(),
            }),
        }
    };
    run().unwrap_or_else(Comparison::Error)
}

/// The metrics of `measurement`; an internal failure if the engine measured in another mode than
/// `tolerance` asked for (a bug).
fn metrics_of(
    measurement: &Measurement,
    tolerance: &Tolerance,
    total_pixels: u64,
) -> Result<Metrics, Failure> {
    Metrics::from_measurement(measurement, tolerance, total_pixels).ok_or_else(|| {
        Failure::new(
            ErrorKind::Internal,
            "internal error: the engine measurement does not match the tolerance",
        )
    })
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
    use image::{ImageBuffer, Rgba};

    use super::*;

    pub(crate) fn png(width: u32, height: u32, paint: impl Fn(u32, u32) -> Rgba<u8>) -> Vec<u8> {
        let img: RgbaImage = ImageBuffer::from_fn(width, height, paint);
        encode_png(&img).unwrap()
    }

    const RED: Rgba<u8> = Rgba([255, 0, 0, 255]);
    const BLUE: Rgba<u8> = Rgba([0, 0, 255, 255]);
    const EXACT: Tolerance = Tolerance::Exact {};
    const SSIM: Tolerance = Tolerance::Ssim {
        min_similarity: 0.8,
        color_tolerance: 8.0,
    };

    fn one_blue_pixel(x: u32, y: u32) -> Rgba<u8> {
        if (x, y) == (3, 3) { BLUE } else { RED }
    }

    /// The metrics of a match or mismatch.
    fn metrics(comparison: &Comparison) -> Option<Metrics> {
        match comparison {
            Comparison::Match { metrics, .. } | Comparison::Mismatch { metrics, .. } => {
                Some(*metrics)
            }
            Comparison::DimensionMismatch { .. } | Comparison::Error(_) => None,
        }
    }

    #[test]
    fn test_exact_match_reports_metrics() {
        let a = png(10, 10, |_, _| RED);
        let comparison = compare(&a, &a, &EXACT, &[]);
        assert!(matches!(comparison, Comparison::Match { .. }));
        assert_eq!(
            metrics(&comparison),
            Some(Metrics::Pixel {
                total_pixels: 100,
                diff_pixels: 0,
                diff_ratio: 0.0,
                headroom: 0.0
            })
        );
    }

    #[test]
    fn test_exact_single_pixel_mismatch_has_diff() {
        let a = png(10, 10, |_, _| RED);
        let b = png(10, 10, one_blue_pixel);
        let comparison = compare(&a, &b, &EXACT, &[]);
        assert!(matches!(&comparison, Comparison::Mismatch { diff_png, .. }
            if image::load_from_memory(diff_png).is_ok()));
        assert!(matches!(
            metrics(&comparison),
            Some(Metrics::Pixel { diff_pixels: 1, headroom, .. }) if headroom == -0.01
        ));
    }

    #[test]
    fn test_pixel_threshold_tolerates_small_change() {
        let a = png(10, 10, |_, _| RED);
        let b = png(10, 10, one_blue_pixel);
        let tolerance = Tolerance::Pixel {
            max_diff_ratio: 0.05,
        };
        assert!(matches!(
            compare(&a, &b, &tolerance, &[]),
            Comparison::Match {
                metrics: Metrics::Pixel { diff_pixels: 1, .. },
                ..
            }
        ));
    }

    #[test]
    fn test_ssim_reports_policy_metrics() {
        let a = png(64, 64, |_, _| RED);
        let b = png(64, 64, |x, _| if x < 32 { BLUE } else { RED });
        let comparison = compare(&a, &b, &SSIM, &[]);
        assert!(matches!(comparison, Comparison::Mismatch { .. }));
        assert!(matches!(
            metrics(&comparison),
            Some(Metrics::Ssim {
                peak_excess,
                failing_region: Some(region),
                changed_pixels: 2048,
                headroom,
                ..
            }) if peak_excess > 100.0 && headroom.color < 0.0 && region.width == 32
        ));
    }

    #[test]
    fn test_masks_hide_changed_region() {
        let a = png(10, 10, |_, _| RED);
        let b = png(10, 10, |x, y| if x < 2 && y < 2 { BLUE } else { RED });
        let mask = |x, width| Zone {
            x,
            y: 0,
            width: Dimension::Pixels(width),
            height: Dimension::Pixels(2),
        };
        assert!(matches!(
            compare(&a, &b, &EXACT, &[mask(0, 2)]),
            Comparison::Match {
                clamped_masks: 0,
                ..
            }
        ));
        assert!(matches!(
            compare(&a, &b, &EXACT, &[mask(0, 2), mask(9, 5)]),
            Comparison::Match {
                clamped_masks: 1,
                ..
            }
        ));
    }

    #[test]
    fn test_dimension_mismatch_reports_sizes_with_masks() {
        let a = png(10, 10, |_, _| RED);
        let b = png(12, 10, |_, _| RED);
        let mask = Zone {
            x: 0,
            y: 0,
            width: Dimension::Percent(50.0),
            height: Dimension::Pixels(2),
        };
        let comparison = compare(&a, &b, &EXACT, &[mask]);
        assert!(matches!(
            comparison,
            Comparison::DimensionMismatch {
                golden: (10, 10),
                candidate: (12, 10),
                ..
            }
        ));
        assert_eq!(metrics(&comparison), None);
    }

    #[test]
    fn test_ssim_over_analysis_budget_is_an_error() {
        let big = png(4097, 4096, |_, _| RED);
        let comparison = compare(&big, &big, &SSIM, &[]);
        assert_eq!(metrics(&comparison), None);
        assert!(matches!(
            comparison,
            Comparison::Error(Failure { kind: ErrorKind::Image, message })
                if message.contains("SSIM analysis budget")
        ));
    }

    #[test]
    fn test_corrupt_images_are_errors_not_passes() {
        let a = png(4, 4, |_, _| RED);
        for (golden, candidate, needle) in [
            (&b"garbage"[..], &a[..], "golden image"),
            (&a[..], &b"garbage"[..], "candidate image"),
        ] {
            assert!(matches!(
                compare(golden, candidate, &EXACT, &[]),
                Comparison::Error(Failure { kind: ErrorKind::Image, message })
                    if message.contains(needle)
            ));
        }
    }

    #[test]
    fn test_a_measurement_of_another_mode_is_an_internal_error() {
        let pixel = Measurement::Pixel { diff_count: 0 };
        assert!(metrics_of(&pixel, &EXACT, 1).is_ok());
        assert!(matches!(
            metrics_of(&pixel, &SSIM, 1),
            Err(Failure { kind: ErrorKind::Internal, message }) if message.contains("internal error")
        ));
    }
}
