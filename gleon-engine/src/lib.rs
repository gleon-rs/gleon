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
pub use pixel::{PixelRegions, TextAnalysis, TextPolicy, TextTile, compare_pixels};
pub use ssim::{Region, SsimAnalysis, SsimPolicy};

use crate::config::{DiffConfig, Mode};

/// What a comparison measured, reported for matches and mismatches alike, so callers can see how
/// much headroom a passing comparison had.
///
/// Results are recorded as `gleon_model::case::Metrics` in case reports (the CLI and the
/// integrations alike).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Measurement {
    /// Pixel (and exact) mode: the differing pixels among those compared strictly, and the text
    /// regions.
    Pixel {
        /// Pixels compared strictly (neither masked nor text under a text policy), the base of
        /// the threshold.
        checked_pixels: u64,
        /// Strictly compared pixels whose RGBA bytes differ.
        diff_count: u64,
        /// The text regions, when a text policy applied to some.
        text: Option<TextAnalysis>,
    },
    /// Tolerant (SSIM) mode; see [`ssim`] for the decision policy and [`SsimAnalysis`] for the
    /// meaning of each value.
    Ssim {
        /// Mean local SSIM over the whole (half-resolution) image; diagnostic only.
        mean_ssim: f64,
        /// Lowest local SSIM, the value gated by `min_similarity`.
        min_ssim: f64,
        /// Largest deviation beyond `color_tolerance` among failing envelope regions (0 unless
        /// the envelope gate failed).
        max_excess: f64,
        /// Largest envelope deviation over all changed pixels, `color_tolerance` not subtracted.
        peak_excess: f64,
        /// Number of pixels whose RGBA bytes differ.
        changed_pixels: u64,
        /// Bounding box of the changed pixels.
        changed_region: Option<Region>,
        /// Number of pixels failing the policy.
        failing_pixels: u64,
        /// Bounding box of the changed pixels that failed the policy.
        failing_region: Option<Region>,
    },
}

impl From<&SsimAnalysis> for Measurement {
    fn from(analysis: &SsimAnalysis) -> Self {
        Self::Ssim {
            mean_ssim: analysis.mean_ssim,
            min_ssim: analysis.min_ssim,
            max_excess: analysis.max_excess,
            peak_excess: analysis.peak_excess,
            changed_pixels: analysis.changed_pixels,
            changed_region: analysis.changed_region,
            failing_pixels: analysis.failing_pixels,
            failing_region: analysis.failing_region,
        }
    }
}

/// Short reason used by every report format, e.g. `"42 pixels"` or
/// `"min local SSIM 0.9955, colors exceed tolerance by 146.0 at (32, 19) 26x15px"`.
impl std::fmt::Display for Measurement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::Pixel {
                diff_count, text, ..
            } => {
                write!(f, "{diff_count} pixels")?;
                match text.and_then(|text| text.worst_tile) {
                    Some(TextTile {
                        region: r,
                        pixels,
                        diff_pixels,
                    }) if diff_pixels > 0 => write!(
                        f,
                        ", text {diff_pixels} of {pixels} pixels in the tile at ({}, {}) {}x{}px",
                        r.x, r.y, r.width, r.height
                    ),
                    _ => Ok(()),
                }
            }
            Self::Ssim {
                min_ssim,
                max_excess,
                failing_region: region,
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
    Match {
        /// What the comparison measured (headroom to the thresholds).
        measurement: Measurement,
    },
    /// The images differ.
    Mismatch {
        /// What the comparison measured, including why it failed.
        measurement: Measurement,
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
    regions: &PixelRegions<'_>,
) -> ComparisonResult {
    let compared = pixel::compare(baseline, actual, regions);
    let analysis = compared.analysis;
    let strict_passes = if threshold == 0.0 || analysis.checked_pixels == 0 {
        analysis.diff_pixels == 0
    } else {
        // Pixel counts here are always far below 2^52, so converting to `f64` is exact for
        // any realistic image size; the ratio itself is just a heuristic threshold comparison.
        #[expect(
            clippy::cast_precision_loss,
            reason = "counts are far below 2^52, so the f64 conversion is exact for any realistic input"
        )]
        let mismatch_ratio = analysis.diff_pixels as f64 / analysis.checked_pixels as f64;
        mismatch_ratio <= threshold
    };
    // Every tile of text within the share: a dense cluster (a changed word) fails, noise spread
    // over the text passes.
    let text_passes = analysis
        .text
        .and_then(|text| text.worst_tile)
        .zip(regions.text_policy)
        .is_none_or(|(tile, policy)| tile.diff_ratio() <= policy.max_diff_ratio);

    let measurement = Measurement::Pixel {
        checked_pixels: analysis.checked_pixels,
        diff_count: analysis.diff_pixels,
        text: analysis.text,
    };
    if strict_passes && text_passes {
        return ComparisonResult::Match { measurement };
    }
    // Only generate the diff image once we know there's actually a mismatch to report.
    ComparisonResult::Mismatch {
        measurement,
        diff_image: compared.diff_image(baseline, actual),
    }
}

/// Compares a baseline and an actual image using the configured mode and thresholds.
///
/// `regions` lie inside the images ([`masking::resolve_zones`]). In pixel mode masked pixels are
/// neither compared nor counted and text is compared under its policy ([`pixel`]); in SSIM mode
/// masked pixels are painted black in both images first and text regions do not apply.
///
/// If dimensions do not match, returns `ComparisonResult::DimensionMismatch`.
///
/// # Panics
/// Panics if a region reaches beyond the images.
#[must_use]
pub fn compare_images(
    baseline: &RgbaImage,
    actual: &RgbaImage,
    mode: Mode,
    config: &DiffConfig,
    regions: &PixelRegions<'_>,
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
        Mode::Pixel => execute_pixel_comparison(baseline, actual, config.threshold, regions),
        Mode::Ssim if !ssim::fits_analysis_budget(w1, h1) => {
            ComparisonResult::TooLarge { size: (w1, h1) }
        }
        Mode::Ssim => {
            let masked = |image: &RgbaImage| {
                let mut image = image.clone();
                masking::paint_black(&mut image, regions.masks);
                image
            };
            let (baseline, actual) = if regions.masks.is_empty() {
                (
                    std::borrow::Cow::Borrowed(baseline),
                    std::borrow::Cow::Borrowed(actual),
                )
            } else {
                (
                    std::borrow::Cow::Owned(masked(baseline)),
                    std::borrow::Cow::Owned(masked(actual)),
                )
            };
            let analysis = ssim::analyze(
                &baseline,
                &actual,
                &SsimPolicy {
                    min_similarity: config.min_similarity,
                    color_tolerance: config.color_tolerance,
                },
            );
            let measurement = Measurement::from(&analysis);
            analysis
                .diff_image
                .map_or(ComparisonResult::Match { measurement }, |diff_image| {
                    ComparisonResult::Mismatch {
                        measurement,
                        diff_image,
                    }
                })
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

    /// Pixel mode passes a frame whose text differs within its policy and fails a strict
    /// difference or a dense text cluster; masked pixels are neither compared nor counted, and
    /// SSIM mode paints them black.
    #[test]
    fn test_compare_images_with_regions() {
        let white = Rgba([255, 255, 255, 255]);
        let baseline = ImageBuffer::from_pixel(32, 16, white);
        let text = [Region {
            x: 0,
            y: 0,
            width: 16,
            height: 16,
        }];
        let masks = [Region {
            x: 16,
            y: 0,
            width: 16,
            height: 8,
        }];
        let regions = PixelRegions {
            masks: &masks,
            text: &text,
            text_policy: Some(TextPolicy {
                color_tolerance: 8.0,
                max_diff_ratio: 0.05,
            }),
        };
        let exact = DiffConfig {
            threshold: 0.0,
            ..DiffConfig::default()
        };
        let compare =
            |actual: &RgbaImage, mode| compare_images(&baseline, actual, mode, &exact, &regions);

        let mut tolerated = baseline.clone();
        tolerated.put_pixel(3, 3, Rgba([0, 0, 0, 255])); // 1 of 256 text pixels
        tolerated.put_pixel(20, 2, Rgba([0, 0, 0, 255])); // masked
        assert!(matches!(
            compare(&tolerated, Mode::Pixel),
            ComparisonResult::Match {
                measurement: Measurement::Pixel {
                    checked_pixels: 128,
                    diff_count: 0,
                    text: Some(_),
                }
            }
        ));

        let mut strict = tolerated.clone();
        strict.put_pixel(20, 12, Rgba([0, 0, 0, 255]));
        assert!(matches!(
            compare(&strict, Mode::Pixel),
            ComparisonResult::Mismatch {
                measurement: Measurement::Pixel { diff_count: 1, .. },
                ..
            }
        ));

        let mut cluster = baseline.clone();
        for i in 0..16 {
            cluster.put_pixel(i, 0, Rgba([0, 0, 0, 255]));
        }
        let result = compare(&cluster, Mode::Pixel);
        assert!(
            matches!(&result, ComparisonResult::Mismatch { measurement, .. }
                if measurement.to_string()
                    == "0 pixels, text 16 of 256 pixels in the tile at (0, 0) 16x16px"),
            "{result:?}"
        );
        // Text without differing pixels says nothing about it.
        assert!(matches!(
            compare(&baseline, Mode::Pixel),
            ComparisonResult::Match { measurement } if measurement.to_string() == "0 pixels"
        ));

        // SSIM: the masked change is painted over, text regions do not apply.
        let mut masked = baseline.clone();
        masked.put_pixel(20, 2, Rgba([0, 0, 0, 255]));
        assert!(matches!(
            compare(&masked, Mode::Ssim),
            ComparisonResult::Match { .. }
        ));
    }

    #[test]
    fn test_ssim_rejects_images_over_the_analysis_budget() {
        let big = RgbaImage::new(4097, 4096);
        assert_eq!(
            compare_images(
                &big,
                &big,
                Mode::Ssim,
                &DiffConfig::default(),
                &PixelRegions::NONE
            ),
            ComparisonResult::TooLarge { size: (4097, 4096) }
        );
        // Pixel mode has no analysis workspace and keeps working at that size.
        assert_eq!(
            compare_images(
                &big,
                &big,
                Mode::Pixel,
                &DiffConfig::default(),
                &PixelRegions::NONE
            ),
            ComparisonResult::Match {
                measurement: Measurement::Pixel {
                    checked_pixels: 4097 * 4096,
                    diff_count: 0,
                    text: None
                }
            }
        );
    }

    #[test]
    fn test_empty_images_match_in_pixel_mode() {
        let empty = RgbaImage::new(0, 0);
        assert_eq!(
            compare_images(
                &empty,
                &empty,
                Mode::Pixel,
                &DiffConfig::default(),
                &PixelRegions::NONE
            ),
            ComparisonResult::Match {
                measurement: Measurement::Pixel {
                    checked_pixels: 0,
                    diff_count: 0,
                    text: None
                }
            }
        );
    }

    #[test]
    fn test_ssim_detail_display_names_the_failing_gate_and_region() {
        let detail = Measurement::Ssim {
            mean_ssim: 0.999,
            min_ssim: 0.9955,
            max_excess: 146.0,
            peak_excess: 154.0,
            changed_pixels: 390,
            changed_region: None,
            failing_pixels: 390,
            failing_region: Some(Region {
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
        let result = compare_images(&img1, &img2, Mode::Pixel, &config, &PixelRegions::NONE);

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

        // The measurement is reported on a match too.
        let result = compare_images(&img1, &img2, Mode::Pixel, &config, &PixelRegions::NONE);
        assert_eq!(
            result,
            ComparisonResult::Match {
                measurement: Measurement::Pixel {
                    checked_pixels: 100,
                    diff_count: 5,
                    text: None
                }
            }
        );

        // With 2% threshold, it should mismatch
        let config2 = DiffConfig {
            threshold: 0.02,
            ..Default::default()
        };
        let result2 = compare_images(&img1, &img2, Mode::Pixel, &config2, &PixelRegions::NONE);
        assert!(matches!(
            result2,
            ComparisonResult::Mismatch {
                measurement: Measurement::Pixel {
                    checked_pixels: 100,
                    diff_count: 5,
                    text: None
                },
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

        let result = compare_images(&img1, &img2, Mode::Pixel, &config, &PixelRegions::NONE);
        assert_eq!(
            result,
            ComparisonResult::Mismatch {
                measurement: Measurement::Pixel {
                    checked_pixels: 4,
                    diff_count: 1,
                    text: None
                },
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

        // Imperceptible color drift is tolerated, and the match still reports its measurement.
        let result = compare_images(&img1, &img2, Mode::Ssim, &config, &PixelRegions::NONE);
        assert!(
            matches!(
                result,
                ComparisonResult::Match {
                    measurement: Measurement::Ssim {
                        changed_pixels: 10_000,
                        failing_pixels: 0,
                        failing_region: None,
                        ..
                    }
                }
            ),
            "{result:?}"
        );

        // A single saturated pixel on a flat area is a visible change and fails.
        img2.put_pixel(50, 50, Rgba([0, 255, 0, 255]));
        assert!(matches!(
            compare_images(&img1, &img2, Mode::Ssim, &config, &PixelRegions::NONE),
            ComparisonResult::Mismatch {
                measurement: Measurement::Ssim { .. },
                ..
            }
        ));

        // A large change should mismatch
        let half_bytes = 50 * 100 * 4;
        (&mut *img2)[..half_bytes]
            .as_chunks_mut::<4>()
            .0
            .fill([0, 255, 0, 255]);
        let result2 = compare_images(&img1, &img2, Mode::Ssim, &config, &PixelRegions::NONE);
        assert!(matches!(
            result2,
            ComparisonResult::Mismatch {
                measurement: Measurement::Ssim { .. },
                ..
            }
        ));
    }
}
