//! Unified visual regression diff engine.
//!
//! Shared by the gleon CLI (`gleon-core`) and the gleon Flutter package (via `gleon-ffi`), so
//! both entry points produce identical verdicts for the same inputs and configuration.

pub mod config;
pub mod decode;
pub mod masking;
pub mod phash;
pub mod pixel;
pub mod ssim;

use image::RgbaImage;
pub use phash::{calculate_hamming_distance, compute_phash};
pub use pixel::compare_pixels;
pub use ssim::{Region, SsimAnalysis, SsimPolicy};

use crate::config::{DiffConfig, Mode};

/// Detailed breakdown of a mismatch between baseline and actual images.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum MismatchDetail {
    /// Pixel difference count.
    Pixel {
        /// Number of mismatched pixels.
        diff_count: u64,
    },
    /// Tolerant (SSIM) policy failure; see [`ssim`] for the decision policy.
    Ssim {
        /// Mean local SSIM over the whole (half-resolution) image; diagnostic only.
        ssim_score: f64,
        /// Lowest local SSIM, the value gated by `min_similarity`.
        min_ssim: f64,
        /// Largest deviation beyond the local envelope (8-bit channel units), gated by
        /// `color_tolerance`.
        max_excess: f64,
        /// Bounding box of the changed pixels that failed the policy.
        region: Option<Region>,
    },
}

/// Short reason used by every report format, e.g. `"42 pixels"` or
/// `"min local SSIM 0.9955, colors exceed tolerance by 146.0 at (32, 19) 26x15px"`.
impl std::fmt::Display for MismatchDetail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::Pixel { diff_count } => write!(f, "{diff_count} pixels"),
            Self::Ssim {
                min_ssim,
                max_excess,
                region,
                ..
            } => {
                write!(f, "min local SSIM {min_ssim:.4}")?;
                if max_excess > 0.0 {
                    write!(f, ", colors exceed tolerance by {max_excess:.1}")?;
                }
                region.map_or(Ok(()), |r| {
                    write!(f, " at ({}, {}) {}x{}px", r.x, r.y, r.width, r.height)
                })
            }
        }
    }
}

/// The result of comparing a baseline and an actual image.
#[derive(Debug, Clone, PartialEq)]
pub enum ComparisonResult {
    /// The images match within the configured tolerance thresholds.
    Match,
    /// The images differ.
    Mismatch {
        /// Detailed information about the mismatch.
        detail: MismatchDetail,
        /// The generated visualization diff image.
        diff_image: RgbaImage,
    },
    /// SSIM mode only: the images exceed [`ssim::MAX_ANALYSIS_PIXELS`], so they were not analyzed
    /// (the analysis workspace is bounded separately from the decoder budget).
    TooLarge {
        /// Dimensions of both images.
        size: (u32, u32),
    },
    /// The images have different dimensions.
    DimensionMismatch {
        /// Dimensions of the baseline image.
        baseline_size: (u32, u32),
        /// Dimensions of the actual image.
        actual_size: (u32, u32),
    },
}

fn execute_pixel_comparison(
    baseline: &RgbaImage,
    actual: &RgbaImage,
    threshold: f64,
) -> ComparisonResult {
    let total_pixels = u64::from(baseline.width()) * u64::from(baseline.height());
    if total_pixels == 0 {
        return ComparisonResult::Match;
    }

    // Count mismatched pixels first without allocating a diff image buffer, so the common
    // "images match" case avoids the allocation entirely, regardless of threshold mode.
    let diff_count = pixel::count_mismatched_pixels(baseline, actual);

    let is_match = if threshold == 0.0 {
        diff_count == 0
    } else {
        // Pixel counts here are always far below 2^52, so converting to `f64` is exact for
        // any realistic image size; the ratio itself is just a heuristic threshold comparison.
        #[expect(
            clippy::cast_precision_loss,
            reason = "counts are far below 2^52, so the f64 conversion is exact for any realistic input"
        )]
        let mismatch_ratio = diff_count as f64 / total_pixels as f64;
        mismatch_ratio <= threshold
    };

    if is_match {
        return ComparisonResult::Match;
    }

    // Only generate the diff image once we know there's actually a mismatch to report.
    let (_, diff_image) = compare_pixels(baseline, actual);
    ComparisonResult::Mismatch {
        detail: MismatchDetail::Pixel { diff_count },
        diff_image,
    }
}

/// Compares a baseline and an actual image using the configured mode and thresholds.
///
/// If dimensions do not match, returns `ComparisonResult::DimensionMismatch`.
#[must_use]
pub fn compare_images(
    baseline: &RgbaImage,
    actual: &RgbaImage,
    mode: Mode,
    config: &DiffConfig,
) -> ComparisonResult {
    let w1 = baseline.width();
    let h1 = baseline.height();
    let w2 = actual.width();
    let h2 = actual.height();

    if w1 != w2 || h1 != h2 {
        return ComparisonResult::DimensionMismatch {
            baseline_size: (w1, h1),
            actual_size: (w2, h2),
        };
    }

    match mode {
        Mode::Pixel => execute_pixel_comparison(baseline, actual, config.threshold),
        Mode::Ssim if !ssim::fits_analysis_budget(w1, h1) => {
            ComparisonResult::TooLarge { size: (w1, h1) }
        }
        Mode::Ssim => {
            let analysis = ssim::analyze(
                baseline,
                actual,
                &SsimPolicy {
                    min_similarity: config.min_similarity,
                    color_tolerance: config.color_tolerance,
                },
            );
            match analysis.diff_image {
                None => ComparisonResult::Match,
                Some(diff_image) => ComparisonResult::Mismatch {
                    detail: MismatchDetail::Ssim {
                        ssim_score: analysis.mean_ssim,
                        min_ssim: analysis.min_ssim,
                        max_excess: analysis.max_excess,
                        region: analysis.failing_region,
                    },
                    diff_image,
                },
            }
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
    use image::{ImageBuffer, Rgba};

    use super::*;
    use crate::config::DiffConfig;

    #[test]
    fn test_ssim_rejects_images_over_the_analysis_budget() {
        let big = RgbaImage::new(4097, 4096);
        assert_eq!(
            compare_images(&big, &big, Mode::Ssim, &DiffConfig::default()),
            ComparisonResult::TooLarge { size: (4097, 4096) }
        );
        // Pixel mode has no analysis workspace and keeps working at that size.
        assert_eq!(
            compare_images(&big, &big, Mode::Pixel, &DiffConfig::default()),
            ComparisonResult::Match
        );
    }

    #[test]
    fn test_empty_images_match_in_pixel_mode() {
        let empty = RgbaImage::new(0, 0);
        assert_eq!(
            compare_images(&empty, &empty, Mode::Pixel, &DiffConfig::default()),
            ComparisonResult::Match
        );
    }

    #[test]
    fn test_ssim_detail_display_names_the_failing_gate_and_region() {
        let detail = MismatchDetail::Ssim {
            ssim_score: 0.999,
            min_ssim: 0.9955,
            max_excess: 146.0,
            region: Some(Region {
                x: 32,
                y: 19,
                width: 26,
                height: 15,
            }),
        };
        assert_eq!(
            detail.to_string(),
            "min local SSIM 0.9955, colors exceed tolerance by 146.0 at (32, 19) 26x15px"
        );
    }

    #[test]
    fn test_dimension_mismatch() {
        let img1 = ImageBuffer::from_pixel(100, 100, Rgba([255, 0, 0, 255]));
        let img2 = ImageBuffer::from_pixel(120, 100, Rgba([255, 0, 0, 255]));

        let config = DiffConfig::default();
        let result = compare_images(&img1, &img2, Mode::Pixel, &config);

        assert!(matches!(
            result,
            ComparisonResult::DimensionMismatch {
                baseline_size: (100, 100),
                actual_size: (120, 100)
            }
        ));
    }

    #[test]
    fn test_compare_images_pixel_match_with_tolerance() {
        let img1 = ImageBuffer::from_pixel(10, 10, Rgba([255, 0, 0, 255]));
        let mut img2 = ImageBuffer::from_pixel(10, 10, Rgba([255, 0, 0, 255]));
        // Make 5 pixels different out of 100 (5% difference)
        for i in 0..5 {
            (&mut *img2)[(i * 4)..(i * 4 + 4)].copy_from_slice(&[0, 255, 0, 255]);
        }

        // With 10% threshold, it should match
        let config = DiffConfig {
            threshold: 0.10,
            ..Default::default()
        };

        let result = compare_images(&img1, &img2, Mode::Pixel, &config);
        assert_eq!(result, ComparisonResult::Match);

        // With 2% threshold, it should mismatch
        let config2 = DiffConfig {
            threshold: 0.02,
            ..Default::default()
        };
        let result2 = compare_images(&img1, &img2, Mode::Pixel, &config2);
        assert!(matches!(
            result2,
            ComparisonResult::Mismatch {
                detail: MismatchDetail::Pixel { diff_count: 5 },
                ..
            }
        ));
    }

    #[test]
    fn test_execute_pixel_comparison_u64_diff_count() {
        let img1 = ImageBuffer::from_pixel(2, 2, Rgba([255, 0, 0, 255]));
        let mut img2 = ImageBuffer::from_pixel(2, 2, Rgba([255, 0, 0, 255]));
        img2.put_pixel(0, 0, Rgba([0, 255, 0, 255]));

        let config = DiffConfig {
            threshold: 0.0,
            ..Default::default()
        };

        let result = compare_images(&img1, &img2, Mode::Pixel, &config);
        assert_eq!(
            result,
            ComparisonResult::Mismatch {
                detail: MismatchDetail::Pixel { diff_count: 1 },
                diff_image: compare_pixels(&img1, &img2).1,
            }
        );
    }

    #[test]
    #[cfg(not(miri))]
    fn test_compare_images_ssim_match() {
        let img1 = ImageBuffer::from_pixel(100, 100, Rgba([255, 0, 0, 255]));
        let mut img2 = ImageBuffer::from_pixel(100, 100, Rgba([254, 0, 0, 255]));

        let config = DiffConfig {
            min_similarity: 0.95,
            ..Default::default()
        };

        // Imperceptible color drift is tolerated.
        let result = compare_images(&img1, &img2, Mode::Ssim, &config);
        assert_eq!(result, ComparisonResult::Match);

        // A single saturated pixel on a flat area is a visible change and fails.
        img2.put_pixel(50, 50, Rgba([0, 255, 0, 255]));
        assert!(matches!(
            compare_images(&img1, &img2, Mode::Ssim, &config),
            ComparisonResult::Mismatch {
                detail: MismatchDetail::Ssim { .. },
                ..
            }
        ));

        // A large change should mismatch
        let half_bytes = 50 * 100 * 4;
        (&mut *img2)[..half_bytes]
            .as_chunks_mut::<4>()
            .0
            .fill([0, 255, 0, 255]);
        let result2 = compare_images(&img1, &img2, Mode::Ssim, &config);
        assert!(matches!(
            result2,
            ComparisonResult::Mismatch {
                detail: MismatchDetail::Ssim { .. },
                ..
            }
        ));
    }
}
