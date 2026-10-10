//! Unified visual regression diff engine.
//!
//! Shared by the gleon CLI (`gleon-core`) and the gleon Flutter package (via `gleon-ffi`), so
//! both entry points produce identical verdicts for the same inputs and configuration.

pub mod config;
pub mod decode;
pub mod masking;
mod par;
#[cfg(feature = "phash")]
pub mod phash;
pub mod pixel;
pub mod ssim;

use image::RgbaImage;
#[cfg(feature = "phash")]
pub use phash::compute_phash;
pub use pixel::{PixelOptions, PixelRegions, TextAnalysis, TextTile};
pub use ssim::{Region, SsimAnalysis, SsimPolicy, SsimRegion};

use crate::config::{DiffConfig, Mode};

/// Version of the engine's tolerant decisions: the SSIM policy, the text tiles.
///
/// Bumped whenever verdicts can change for the same inputs. 3: a text tile always counts
/// [`pixel::TEXT_TILE`] squared pixels, so a thin or edge-clipped text region no longer turns one
/// differing pixel into a large share. 4: SSIM mode judges text regions by their tiles too and
/// leaves them out of both of its gates, like masks.
pub const POLICY_VERSION: u32 = 4;

/// The straight (not premultiplied) RGBA8 pixels of an image, row by row, borrowed: a decoded
/// golden (`&RgbaImage`) or the raw capture of an integration, compared without a copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pixels<'a> {
    raw: &'a [u8],
    width: u32,
    height: u32,
}

impl<'a> Pixels<'a> {
    /// The `width` x `height` image of `raw`, or `None` unless `raw` holds exactly its
    /// `width * height * 4` bytes.
    #[must_use]
    pub fn new(raw: &'a [u8], width: u32, height: u32) -> Option<Self> {
        (u64::try_from(raw.len()).ok() == Some(u64::from(width) * u64::from(height) * 4))
            .then_some(Self { raw, width, height })
    }

    /// The bytes, `width * height * 4` of them.
    #[must_use]
    pub const fn raw(self) -> &'a [u8] {
        self.raw
    }

    /// Width and height in pixels.
    #[must_use]
    pub const fn dimensions(self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// An owned copy, for the comparisons that paint over pixels (SSIM masks).
    ///
    /// # Panics
    /// Never: the length was checked when the view was made.
    #[must_use]
    pub fn to_image(self) -> RgbaImage {
        #[expect(
            clippy::expect_used,
            reason = "the length was checked when the view was made"
        )]
        RgbaImage::from_raw(self.width, self.height, self.raw.to_vec())
            .expect("the view holds width * height * 4 bytes")
    }
}

impl<'a> From<&'a RgbaImage> for Pixels<'a> {
    fn from(image: &'a RgbaImage) -> Self {
        Self {
            raw: image.as_raw(),
            width: image.width(),
            height: image.height(),
        }
    }
}

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
        /// Pixels compared strictly (neither masked nor text under a text tolerance), the base of
        /// the threshold.
        checked_pixels: u64,
        /// Strictly compared pixels whose RGBA bytes differ (beyond the [`PixelOptions`]).
        diff_count: u64,
        /// Differing pixels the [`PixelOptions`] let pass (channel tolerance, anti-aliasing,
        /// edges), counted as equal.
        tolerated_count: u64,
        /// The text regions, when a text tolerance applied to some.
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
        /// The policy's metrics inside `changed_region`.
        changed_local: Option<SsimRegion>,
        /// The policy's metrics inside `failing_region`.
        failing_local: Option<SsimRegion>,
        /// The text regions, judged by their tiles, when a text tolerance applied to some.
        text: Option<TextAnalysis>,
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
            changed_local: analysis.changed_local,
            failing_local: analysis.failing_local,
            text: None,
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
        /// The visualization of both sizes ([`pixel::dimension_diff`]); `None` when its canvas
        /// is over the decoding budget.
        diff_image: Option<RgbaImage>,
    },
}

fn execute_pixel_comparison(
    baseline: Pixels<'_>,
    actual: Pixels<'_>,
    config: &DiffConfig,
    regions: &PixelRegions<'_>,
) -> ComparisonResult {
    let threshold = config.threshold;
    let compared = pixel::compare(baseline, actual, regions, &PixelOptions::from(config));
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
    let text_passes = compared.text_passes();

    let measurement = Measurement::Pixel {
        checked_pixels: analysis.checked_pixels,
        diff_count: analysis.diff_pixels,
        tolerated_count: analysis.tolerated_pixels + analysis.edge_pixels,
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
/// text is judged by the same tiles, then masks and text regions are painted black in both images
/// for both gates.
///
/// If dimensions do not match, returns `ComparisonResult::DimensionMismatch` with a diff of both
/// sizes (masks and text do not apply: they name pixels of one size).
///
/// # Panics
/// Panics if a region reaches beyond the images.
#[must_use]
pub fn compare_images<'a>(
    baseline: impl Into<Pixels<'a>>,
    actual: impl Into<Pixels<'a>>,
    mode: Mode,
    config: &DiffConfig,
    regions: &PixelRegions<'_>,
) -> ComparisonResult {
    let (baseline, actual) = (baseline.into(), actual.into());
    let (w1, h1) = baseline.dimensions();
    let (w2, h2) = actual.dimensions();

    if w1 != w2 || h1 != h2 {
        return ComparisonResult::DimensionMismatch {
            baseline_size: (w1, h1),
            actual_size: (w2, h2),
            diff_image: pixel::dimension_diff(baseline, actual),
        };
    }

    match mode {
        Mode::Pixel => execute_pixel_comparison(baseline, actual, config, regions),
        Mode::Ssim if !ssim::fits_analysis_budget(w1, h1) => {
            ComparisonResult::TooLarge { size: (w1, h1) }
        }
        Mode::Ssim => execute_ssim_comparison(baseline, actual, config, regions),
    }
}

fn execute_ssim_comparison(
    baseline: Pixels<'_>,
    actual: Pixels<'_>,
    config: &DiffConfig,
    regions: &PixelRegions<'_>,
) -> ComparisonResult {
    let policy = SsimPolicy {
        min_similarity: config.min_similarity,
        color_tolerance: config.color_tolerance,
    };
    let has_text = regions.text_tolerance.is_some() && !regions.text.is_empty();
    // Text is judged by its tiles as in pixel mode (masks winning), then painted out of both
    // gates with the masks: glyphs of another rasterizer would fail the envelope.
    let text = has_text.then(|| pixel::compare(baseline, actual, regions, &PixelOptions::STRICT));
    let text_regions: &[Region] = if has_text { regions.text } else { &[] };
    let painted: Vec<Region> = regions.masks.iter().chain(text_regions).copied().collect();
    // Copies only here: the regions are painted black in both images.
    let masked = (!painted.is_empty()).then(|| {
        let paint = |pixels: Pixels<'_>| {
            let mut image = pixels.to_image();
            masking::paint_black(&mut image, &painted);
            image
        };
        (paint(baseline), paint(actual))
    });
    let (analysed_baseline, analysed_actual) =
        masked.as_ref().map_or((baseline, actual), |(b, a)| {
            (Pixels::from(b), Pixels::from(a))
        });
    let analysis = ssim::analyze(analysed_baseline, analysed_actual, &policy);
    let text_passes = text.as_ref().is_none_or(pixel::Compared::text_passes);
    let mut measurement = Measurement::from(&analysis);
    if let Measurement::Ssim {
        text: measured_text,
        ..
    } = &mut measurement
    {
        *measured_text = text.as_ref().and_then(|text| text.analysis.text);
    }
    if analysis.passed() && text_passes {
        return ComparisonResult::Match { measurement };
    }
    let mut diff_image = analysis
        .diff_image
        .unwrap_or_else(|| ssim::passing_diff(analysed_baseline, analysed_actual));
    if let Some(text) = &text {
        text.paint_text(&mut diff_image, baseline);
    }
    ComparisonResult::Mismatch {
        measurement,
        diff_image,
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
            text_tolerance: Some(0.05),
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
                    tolerated_count: 0,
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
            matches!(
                &result,
                ComparisonResult::Mismatch {
                    measurement: Measurement::Pixel {
                        diff_count: 0,
                        text: Some(TextAnalysis {
                            worst_tile: Some(TextTile {
                                region: Region {
                                    x: 0,
                                    y: 0,
                                    width: 16,
                                    height: 16
                                },
                                pixels: 256,
                                diff_pixels: 16,
                            }),
                            ..
                        }),
                        ..
                    },
                    ..
                }
            ),
            "{result:?}"
        );
        // Text without differing pixels has no worst tile.
        assert!(matches!(
            compare(&baseline, Mode::Pixel),
            ComparisonResult::Match {
                measurement: Measurement::Pixel {
                    diff_count: 0,
                    text: Some(TextAnalysis {
                        worst_tile: None,
                        ..
                    }),
                    ..
                }
            }
        ));

        // SSIM: the masked change is painted over.
        let mut masked = baseline.clone();
        masked.put_pixel(20, 2, Rgba([0, 0, 0, 255]));
        assert!(matches!(
            compare(&masked, Mode::Ssim),
            ComparisonResult::Match { .. }
        ));
    }

    /// SSIM mode judges text by its tiles and leaves it out of both gates: glyphs of another
    /// rasterizer pass under a lenient text tolerance, a dense change fails under a strict one, and
    /// a change outside text still fails the SSIM policy.
    #[test]
    fn test_ssim_judges_text_by_tiles_and_paints_it_out() {
        let white = Rgba([255, 255, 255, 255]);
        let baseline = ImageBuffer::from_pixel(48, 32, white);
        let text = [Region {
            x: 0,
            y: 0,
            width: 16,
            height: 16,
        }];
        let ssim = DiffConfig::default();
        let compare = |actual: &RgbaImage, text_tolerance| {
            compare_images(
                &baseline,
                actual,
                Mode::Ssim,
                &ssim,
                &PixelRegions {
                    masks: &[],
                    text: &text,
                    text_tolerance: Some(text_tolerance),
                },
            )
        };
        // A changed glyph: 4x4 black pixels inside the text region (16 of 256, 6.25%).
        let mut glyph = baseline.clone();
        for (x, y) in (4..8).flat_map(|x| (4..8).map(move |y| (x, y))) {
            glyph.put_pixel(x, y, Rgba([0, 0, 0, 255]));
        }
        assert!(
            matches!(
                compare(&glyph, 1.0),
                ComparisonResult::Match {
                    measurement: Measurement::Ssim {
                        failing_pixels: 0,
                        text: Some(TextAnalysis {
                            diff_pixels: 16,
                            ..
                        }),
                        ..
                    }
                }
            ),
            "text is out of the SSIM gates"
        );
        let ComparisonResult::Mismatch {
            measurement:
                Measurement::Ssim {
                    failing_pixels: 0,
                    text: Some(_),
                    ..
                },
            diff_image,
        } = compare(&glyph, 0.05)
        else {
            panic!("a dense text change fails its tiles");
        };
        assert_eq!(*diff_image.get_pixel(5, 5), Rgba([255, 165, 0, 255]));
        // Unchanged text is the faded baseline, as everywhere else in an SSIM diff.
        assert_eq!(*diff_image.get_pixel(12, 12), Rgba([255, 255, 255, 255]));
        // Outside text the SSIM policy applies as before.
        let mut outside = baseline.clone();
        for (x, y) in (30..34).flat_map(|x| (20..24).map(move |y| (x, y))) {
            outside.put_pixel(x, y, Rgba([0, 0, 0, 255]));
        }
        assert!(matches!(
            compare(&outside, 1.0),
            ComparisonResult::Mismatch {
                measurement: Measurement::Ssim { .. },
                ..
            }
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
                    tolerated_count: 0,
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
                    tolerated_count: 0,
                    text: None
                }
            }
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
                actual_size: (120, 100),
                diff_image: Some(ref diff),
            } if diff.dimensions() == (120, 100)
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
                    tolerated_count: 0,
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
                    tolerated_count: 0,
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
                    tolerated_count: 0,
                    text: None
                },
                diff_image: ImageBuffer::from_fn(2, 2, |x, y| {
                    if (x, y) == (0, 0) {
                        Rgba([255, 0, 255, 255])
                    } else {
                        Rgba([127, 0, 0, 255])
                    }
                }),
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
