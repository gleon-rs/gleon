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
pub use pixel::{PixelAnalysis, PixelOptions, PixelRegions, TextAnalysis, TextTile};
pub use ssim::{Region, SsimAnalysis, SsimPolicy, SsimRegion};

use crate::config::Policy;

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
    /// [`Self::byte_len`] bytes.
    #[must_use]
    pub fn new(raw: &'a [u8], width: u32, height: u32) -> Option<Self> {
        Self::byte_len(width, height)
            .is_some_and(|len| u64::try_from(raw.len()) == Ok(len))
            .then_some(Self { raw, width, height })
    }

    /// The bytes of a `width` x `height` image, `width * height * 4`; `None` beyond `u64`.
    #[must_use]
    pub fn byte_len(width: u32, height: u32) -> Option<u64> {
        // Two `u32`s multiply within `u64`; the four bytes of a pixel may not.
        (u64::from(width) * u64::from(height)).checked_mul(4)
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

    /// The RGBA bytes of the pixel at (`x`, `y`).
    ///
    /// # Panics
    /// Panics if the pixel lies outside the image.
    #[must_use]
    pub fn rgba(self, x: usize, y: usize) -> [u8; 4] {
        let start = (y * self.width as usize + x) * 4;
        // One bounds check for the four bytes (measured faster than four indexings).
        let mut pixel = [0; 4];
        pixel.copy_from_slice(&self.raw[start..start + 4]);
        pixel
    }

    /// An owned copy, for the comparisons that paint over pixels (SSIM masks).
    ///
    /// # Panics
    /// Never: the length was checked when the view was made.
    #[must_use]
    pub(crate) fn to_image(self) -> RgbaImage {
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
    Pixel(PixelAnalysis),
    /// Tolerant (SSIM) mode; see [`ssim`] for the decision policy and [`SsimAnalysis`] for the
    /// meaning of each value.
    Ssim {
        /// What the policy measured.
        analysis: SsimAnalysis,
        /// The text regions, judged by their tiles, when a text tolerance applied to some.
        text: Option<TextAnalysis>,
    },
}

/// What a comparison keeps of the diff visualization of a failure ([`compare_images_as`]):
/// [`RgbaImage`] renders it, `()` renders none.
pub trait DiffImage: Sized {
    /// Whether a failure's diff is rendered at all.
    const RENDERED: bool;

    /// The diff of a failure, drawn by `draw` if this kind keeps one.
    fn of(draw: impl FnOnce() -> RgbaImage) -> Self;

    /// The diff of images of different sizes, drawn by `draw` (`None` when its canvas is over
    /// the decoding budget) if this kind keeps one.
    fn of_sizes(draw: impl FnOnce() -> Option<RgbaImage>) -> Option<Self>;
}

/// The diff rendered ([`compare_images`]).
impl DiffImage for RgbaImage {
    const RENDERED: bool = true;

    fn of(draw: impl FnOnce() -> Self) -> Self {
        draw()
    }

    fn of_sizes(draw: impl FnOnce() -> Option<Self>) -> Option<Self> {
        draw()
    }
}

/// No diff: for callers that only need the verdict and the measurement (`gleon status`), so a
/// failure costs no image of its size.
impl DiffImage for () {
    const RENDERED: bool = false;

    fn of(_: impl FnOnce() -> RgbaImage) -> Self {}

    fn of_sizes(_: impl FnOnce() -> Option<RgbaImage>) -> Option<Self> {
        None
    }
}

/// The result of comparing a baseline and an actual image, with the diff visualization of a
/// failure as `D` ([`DiffImage`]).
#[derive(Debug, Clone, PartialEq)]
pub enum ComparisonResult<D = RgbaImage> {
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
        diff_image: D,
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
        /// is over the decoding budget, or when `D` keeps no diff.
        diff_image: Option<D>,
    },
}

fn execute_pixel_comparison<D: DiffImage>(
    baseline: Pixels<'_>,
    actual: Pixels<'_>,
    max_diff_ratio: f64,
    options: PixelOptions,
    regions: &PixelRegions<'_>,
) -> ComparisonResult<D> {
    let compared = pixel::compare(baseline, actual, regions, &options);
    let analysis = compared.analysis;
    let strict_passes = if max_diff_ratio == 0.0 || analysis.checked_pixels == 0 {
        analysis.diff_pixels == 0
    } else {
        // Pixel counts here are always far below 2^52, so converting to `f64` is exact for
        // any realistic image size; the ratio itself is just a heuristic threshold comparison.
        #[expect(
            clippy::cast_precision_loss,
            reason = "counts are far below 2^52, so the f64 conversion is exact for any realistic input"
        )]
        let mismatch_ratio = analysis.diff_pixels as f64 / analysis.checked_pixels as f64;
        mismatch_ratio <= max_diff_ratio
    };
    let text_passes = compared.text_passes();

    let measurement = Measurement::Pixel(analysis);
    if strict_passes && text_passes {
        return ComparisonResult::Match { measurement };
    }
    // Only generate the diff image once we know there's actually a mismatch to report.
    ComparisonResult::Mismatch {
        measurement,
        diff_image: D::of(|| compared.diff_image(baseline, actual)),
    }
}

/// Compares a baseline and an actual image under `policy`.
///
/// `regions` lie inside the images ([`masking::resolve_zones`]). In pixel mode masked pixels are
/// neither compared nor counted and text is compared under its policy ([`pixel`]); in SSIM mode
/// text is judged by the same tiles, then masks and text regions are left out of both gates: they
/// take the baseline's pixels in the actual image, so they compare as equal while their
/// surroundings keep their real neighbors.
///
/// If dimensions do not match, returns `ComparisonResult::DimensionMismatch` with a diff of both
/// sizes, its masked pixels (in baseline pixels, clipped to the area both cover) unmarked; text
/// does not apply.
///
/// # Panics
/// Panics if a region of images of the same size reaches beyond them.
#[must_use]
pub fn compare_images<'a>(
    baseline: impl Into<Pixels<'a>>,
    actual: impl Into<Pixels<'a>>,
    policy: &Policy,
    regions: &PixelRegions<'_>,
) -> ComparisonResult {
    compare_images_as(baseline, actual, policy, regions)
}

/// [`compare_images`], keeping the diff visualization of a failure as `D`: `()` decides and
/// measures exactly the same but renders no diff, for callers that only need the verdict.
///
/// # Panics
/// Like [`compare_images`].
#[must_use]
pub fn compare_images_as<'a, D: DiffImage>(
    baseline: impl Into<Pixels<'a>>,
    actual: impl Into<Pixels<'a>>,
    policy: &Policy,
    regions: &PixelRegions<'_>,
) -> ComparisonResult<D> {
    let (baseline, actual) = (baseline.into(), actual.into());
    let (w1, h1) = baseline.dimensions();
    let (w2, h2) = actual.dimensions();

    if w1 != w2 || h1 != h2 {
        return ComparisonResult::DimensionMismatch {
            baseline_size: (w1, h1),
            actual_size: (w2, h2),
            diff_image: D::of_sizes(|| pixel::dimension_diff(baseline, actual, regions.masks)),
        };
    }

    match policy {
        Policy::Pixel {
            max_diff_ratio,
            options,
        } => execute_pixel_comparison(baseline, actual, *max_diff_ratio, *options, regions),
        Policy::Ssim(_) if !ssim::fits_analysis_budget(w1, h1) => {
            ComparisonResult::TooLarge { size: (w1, h1) }
        }
        Policy::Ssim(policy) => execute_ssim_comparison(baseline, actual, policy, regions),
    }
}

fn execute_ssim_comparison<D: DiffImage>(
    baseline: Pixels<'_>,
    actual: Pixels<'_>,
    policy: &SsimPolicy,
    regions: &PixelRegions<'_>,
) -> ComparisonResult<D> {
    let has_text = regions.text_tolerance.is_some() && !regions.text.is_empty();
    // Text is judged by its tiles as in pixel mode (masks winning); glyphs of another rasterizer
    // would fail the envelope, so text is left out of both gates like masks.
    let text = has_text.then(|| pixel::compare(baseline, actual, regions, &PixelOptions::STRICT));
    let text_regions: &[Region] = if has_text { regions.text } else { &[] };
    let excluded: Vec<Region> = regions.masks.iter().chain(text_regions).copied().collect();
    // Excluded pixels take the baseline's in a copy of the actual image, only when they differ:
    // equal there, while every other pixel keeps its real neighbors.
    let patched = masking::differ_inside(baseline, actual, &excluded).then(|| {
        let mut image = actual.to_image();
        masking::copy_regions(&mut image, baseline, &excluded);
        image
    });
    let analyzed_actual = patched.as_ref().map_or(actual, Pixels::from);
    let (analysis, failing_diff) =
        ssim::analyze_with(baseline, analyzed_actual, policy, D::RENDERED);
    let text_passes = text.as_ref().is_none_or(pixel::Compared::text_passes);
    let measurement = Measurement::Ssim {
        analysis,
        text: text.as_ref().and_then(|text| text.analysis.text),
    };
    if analysis.passed() && text_passes {
        return ComparisonResult::Match { measurement };
    }
    let diff_image = D::of(|| {
        let mut diff_image =
            failing_diff.unwrap_or_else(|| ssim::passing_diff(baseline, analyzed_actual));
        if let Some(text) = &text {
            text.paint_text(&mut diff_image, baseline);
        }
        diff_image
    });
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
    use crate::{config::Policy, pixel::PixelAnalysis};

    /// Exact: pixel mode, no share of differing pixels, strict options.
    const EXACT: Policy = pixel(0.0);
    /// SSIM at its defaults.
    const SSIM: Policy = Policy::Ssim(SsimPolicy {
        min_similarity: config::DEFAULT_MIN_SIMILARITY,
        color_tolerance: config::DEFAULT_COLOR_TOLERANCE,
    });

    /// Pixel mode with strict options and at most `max_diff_ratio` differing.
    const fn pixel(max_diff_ratio: f64) -> Policy {
        Policy::Pixel {
            max_diff_ratio,
            options: PixelOptions::STRICT,
        }
    }

    /// `result` with its diff visualization dropped, as [`compare_images_as::<()>`] returns it.
    fn without_diff(result: ComparisonResult) -> ComparisonResult<()> {
        match result {
            ComparisonResult::Match { measurement } => ComparisonResult::Match { measurement },
            ComparisonResult::Mismatch { measurement, .. } => ComparisonResult::Mismatch {
                measurement,
                diff_image: (),
            },
            ComparisonResult::TooLarge { size } => ComparisonResult::TooLarge { size },
            ComparisonResult::DimensionMismatch {
                baseline_size,
                actual_size,
                ..
            } => ComparisonResult::DimensionMismatch {
                baseline_size,
                actual_size,
                diff_image: None,
            },
        }
    }

    /// The length of a size never overflows: a size whose bytes `u64` cannot count has none.
    #[test]
    fn test_pixels_of_a_size_beyond_u64_have_no_length() {
        assert_eq!(Pixels::byte_len(2, 3), Some(24));
        assert_eq!(Pixels::byte_len(0, 7), Some(0));
        assert_eq!(Pixels::byte_len(u32::MAX, u32::MAX), None);
        assert_eq!(Pixels::new(&[], u32::MAX, u32::MAX), None);
        assert_eq!(
            Pixels::new(&[0; 24], 2, 3).map(Pixels::dimensions),
            Some((2, 3))
        );
        assert_eq!(Pixels::new(&[0; 23], 2, 3), None);
    }

    /// Keeping no diff, a comparison decides and measures exactly as with one, in both modes,
    /// with text and masks, and for another size; failures render nothing.
    #[test]
    fn test_compare_images_as_unit_decides_alike_and_renders_no_diff() {
        let (white, black) = (Rgba([255, 255, 255, 255]), Rgba([0, 0, 0, 255]));
        let baseline = ImageBuffer::from_pixel(48, 32, white);
        let text = [Region {
            x: 0,
            y: 0,
            width: 16,
            height: 16,
        }];
        let masks = [Region {
            x: 32,
            y: 0,
            width: 16,
            height: 8,
        }];
        let regions = PixelRegions {
            masks: &masks,
            text: &text,
            text_tolerance: Some(0.05),
        };
        // A dense change of the text, a masked change and a change elsewhere.
        let mut changed = baseline.clone();
        for x in 0..16 {
            changed.put_pixel(x, 0, black);
        }
        changed.put_pixel(40, 2, black);
        changed.put_pixel(40, 20, black);
        let wider = ImageBuffer::from_pixel(49, 32, white);

        let mut outcomes = Vec::new();
        for (actual, policy) in [
            (&baseline, EXACT),
            (&changed, EXACT),
            (&wider, EXACT),
            (&baseline, SSIM),
            (&changed, SSIM),
            (&wider, SSIM),
        ] {
            let rendered = compare_images(&baseline, actual, &policy, &regions);
            let skipped: ComparisonResult<()> =
                compare_images_as(&baseline, actual, &policy, &regions);
            outcomes.push(std::mem::discriminant(&skipped));
            assert_eq!(skipped, without_diff(rendered), "{policy:?}");
        }
        // Every outcome that renders a diff was exercised.
        assert!(matches!(
            compare_images_as::<()>(&baseline, &changed, &SSIM, &regions),
            ComparisonResult::Mismatch { .. }
        ));
        // Over the SSIM analysis budget nothing is analyzed, with a diff kept or not.
        let huge = ImageBuffer::from_pixel(4097, 4096, white);
        let none = PixelRegions::NONE;
        assert_eq!(
            compare_images_as::<()>(&huge, &huge, &SSIM, &none),
            without_diff(compare_images(&huge, &huge, &SSIM, &none))
        );
        assert!(outcomes.contains(&std::mem::discriminant(
            &ComparisonResult::<()>::DimensionMismatch {
                baseline_size: (0, 0),
                actual_size: (0, 0),
                diff_image: None,
            }
        )));
    }

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
        let compare = |actual: &RgbaImage, policy: Policy| {
            compare_images(&baseline, actual, &policy, &regions)
        };

        let mut tolerated = baseline.clone();
        tolerated.put_pixel(3, 3, Rgba([0, 0, 0, 255])); // 1 of 256 text pixels
        tolerated.put_pixel(20, 2, Rgba([0, 0, 0, 255])); // masked
        assert!(matches!(
            compare(&tolerated, EXACT),
            ComparisonResult::Match {
                measurement: Measurement::Pixel(PixelAnalysis {
                    checked_pixels: 128,
                    diff_pixels: 0,
                    tolerated_pixels: 0,
                    edge_pixels: 0,
                    text: Some(_),
                })
            }
        ));

        let mut strict = tolerated.clone();
        strict.put_pixel(20, 12, Rgba([0, 0, 0, 255]));
        assert!(matches!(
            compare(&strict, EXACT),
            ComparisonResult::Mismatch {
                measurement: Measurement::Pixel(PixelAnalysis { diff_pixels: 1, .. }),
                ..
            }
        ));

        let mut cluster = baseline.clone();
        for i in 0..16 {
            cluster.put_pixel(i, 0, Rgba([0, 0, 0, 255]));
        }
        let result = compare(&cluster, EXACT);
        assert!(
            matches!(
                &result,
                ComparisonResult::Mismatch {
                    measurement: Measurement::Pixel(PixelAnalysis {
                        diff_pixels: 0,
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
                    }),
                    ..
                }
            ),
            "{result:?}"
        );
        // Text without differing pixels has no worst tile.
        assert!(matches!(
            compare(&baseline, EXACT),
            ComparisonResult::Match {
                measurement: Measurement::Pixel(PixelAnalysis {
                    diff_pixels: 0,
                    text: Some(TextAnalysis {
                        worst_tile: None,
                        ..
                    }),
                    ..
                })
            }
        ));

        // SSIM: the masked change is painted over.
        let mut masked = baseline.clone();
        masked.put_pixel(20, 2, Rgba([0, 0, 0, 255]));
        assert!(matches!(
            compare(&masked, SSIM),
            ComparisonResult::Match { .. }
        ));
    }

    /// SSIM mode judges text by its tiles and leaves it out of both gates: glyphs of another
    /// rasterizer pass under a lenient text tolerance, a dense change fails under a strict one, and
    /// a change outside text still fails the SSIM policy.
    #[test]
    fn test_ssim_judges_text_by_tiles_and_leaves_it_out() {
        let white = Rgba([255, 255, 255, 255]);
        let baseline = ImageBuffer::from_pixel(48, 32, white);
        let text = [Region {
            x: 0,
            y: 0,
            width: 16,
            height: 16,
        }];
        let compare = |actual: &RgbaImage, text_tolerance| {
            compare_images(
                &baseline,
                actual,
                &SSIM,
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
                        analysis: SsimAnalysis {
                            failing_pixels: 0,
                            ..
                        },
                        text: Some(TextAnalysis {
                            diff_pixels: 16,
                            ..
                        }),
                    }
                }
            ),
            "text is out of the SSIM gates"
        );
        // A dense change fails its tiles: orange in the diff, unchanged text the faded baseline
        // as everywhere else in an SSIM diff.
        let strict = compare(&glyph, 0.05);
        assert!(
            matches!(
                &strict,
                ComparisonResult::Mismatch {
                    measurement: Measurement::Ssim { analysis: SsimAnalysis { failing_pixels: 0, .. }, text: Some(_), .. },
                    diff_image,
                } if *diff_image.get_pixel(5, 5) == Rgba([255, 165, 0, 255])
                    && *diff_image.get_pixel(12, 12) == Rgba([255, 255, 255, 255])
            ),
            "{strict:?}"
        );
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

    /// The pixels around an excluded region (text, a mask) are compared like any other: a 1px
    /// line recolored right below one still fails SSIM (an excluded region painted black would
    /// widen the envelope of its neighbors to every color).
    #[test]
    fn test_ssim_compares_the_pixels_next_to_excluded_regions() {
        let white = Rgba([255, 255, 255, 255]);
        let mut baseline = ImageBuffer::from_pixel(48, 32, white);
        let mut removed = baseline.clone();
        for x in 0..32 {
            baseline.put_pixel(x, 16, Rgba([200, 0, 0, 255]));
            removed.put_pixel(x, 16, Rgba([0, 0, 200, 255]));
        }
        // Without an excluded region the recolored line fails.
        assert!(matches!(
            compare_images(&baseline, &removed, &SSIM, &PixelRegions::NONE),
            ComparisonResult::Mismatch { .. }
        ));
        let above = [Region {
            x: 0,
            y: 0,
            width: 32,
            height: 16,
        }];
        for regions in [
            PixelRegions {
                masks: &[],
                text: &above,
                text_tolerance: Some(1.0),
            },
            PixelRegions {
                masks: &above,
                text: &[],
                text_tolerance: None,
            },
        ] {
            assert!(
                matches!(
                    compare_images(&baseline, &removed, &SSIM, &regions),
                    ComparisonResult::Mismatch { .. }
                ),
                "{regions:?}"
            );
        }
    }

    #[test]
    fn test_ssim_rejects_images_over_the_analysis_budget() {
        let big = RgbaImage::new(4097, 4096);
        assert_eq!(
            compare_images(&big, &big, &SSIM, &PixelRegions::NONE),
            ComparisonResult::TooLarge { size: (4097, 4096) }
        );
        // Pixel mode has no analysis workspace and keeps working at that size.
        assert_eq!(
            compare_images(&big, &big, &EXACT, &PixelRegions::NONE),
            ComparisonResult::Match {
                measurement: Measurement::Pixel(PixelAnalysis {
                    checked_pixels: 4097 * 4096,
                    diff_pixels: 0,
                    tolerated_pixels: 0,
                    edge_pixels: 0,
                    text: None
                })
            }
        );
    }

    #[test]
    fn test_empty_images_match_in_pixel_mode() {
        let empty = RgbaImage::new(0, 0);
        assert_eq!(
            compare_images(&empty, &empty, &EXACT, &PixelRegions::NONE),
            ComparisonResult::Match {
                measurement: Measurement::Pixel(PixelAnalysis {
                    checked_pixels: 0,
                    diff_pixels: 0,
                    tolerated_pixels: 0,
                    edge_pixels: 0,
                    text: None
                })
            }
        );
    }

    #[test]
    fn test_dimension_mismatch() {
        let img1 = ImageBuffer::from_pixel(100, 100, Rgba([255, 0, 0, 255]));
        let img2 = ImageBuffer::from_pixel(120, 100, Rgba([255, 0, 0, 255]));

        let result = compare_images(&img1, &img2, &EXACT, &PixelRegions::NONE);

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

        // The measurement is reported on a match too.
        let result = compare_images(&img1, &img2, &pixel(0.10), &PixelRegions::NONE);
        assert_eq!(
            result,
            ComparisonResult::Match {
                measurement: Measurement::Pixel(PixelAnalysis {
                    checked_pixels: 100,
                    diff_pixels: 5,
                    tolerated_pixels: 0,
                    edge_pixels: 0,
                    text: None
                })
            }
        );

        // With 2% threshold, it should mismatch
        let result2 = compare_images(&img1, &img2, &pixel(0.02), &PixelRegions::NONE);
        assert!(matches!(
            result2,
            ComparisonResult::Mismatch {
                measurement: Measurement::Pixel(PixelAnalysis {
                    checked_pixels: 100,
                    diff_pixels: 5,
                    tolerated_pixels: 0,
                    edge_pixels: 0,
                    text: None
                }),
                ..
            }
        ));
    }

    #[test]
    fn test_execute_pixel_comparison_u64_diff_count() {
        let img1 = ImageBuffer::from_pixel(2, 2, Rgba([255, 0, 0, 255]));
        let mut img2 = ImageBuffer::from_pixel(2, 2, Rgba([255, 0, 0, 255]));
        img2.put_pixel(0, 0, Rgba([0, 255, 0, 255]));

        let result = compare_images(&img1, &img2, &EXACT, &PixelRegions::NONE);
        assert_eq!(
            result,
            ComparisonResult::Mismatch {
                measurement: Measurement::Pixel(PixelAnalysis {
                    checked_pixels: 4,
                    diff_pixels: 1,
                    tolerated_pixels: 0,
                    edge_pixels: 0,
                    text: None
                }),
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

        let config = Policy::Ssim(SsimPolicy {
            min_similarity: 0.95,
            color_tolerance: 8.0,
        });

        // Imperceptible color drift is tolerated, and the match still reports its measurement.
        let result = compare_images(&img1, &img2, &config, &PixelRegions::NONE);
        assert!(
            matches!(
                result,
                ComparisonResult::Match {
                    measurement: Measurement::Ssim {
                        analysis: SsimAnalysis {
                            changed_pixels: 10_000,
                            failing_pixels: 0,
                            failing_region: None,
                            ..
                        },
                        ..
                    }
                }
            ),
            "{result:?}"
        );

        // A single saturated pixel on a flat area is a visible change and fails.
        img2.put_pixel(50, 50, Rgba([0, 255, 0, 255]));
        assert!(matches!(
            compare_images(&img1, &img2, &config, &PixelRegions::NONE),
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
        let result2 = compare_images(&img1, &img2, &config, &PixelRegions::NONE);
        assert!(matches!(
            result2,
            ComparisonResult::Mismatch {
                measurement: Measurement::Ssim { .. },
                ..
            }
        ));
    }
}
