//! The comparison of a golden with a candidate: decoding with the engine's resource limits,
//! masks, the engine under a [`Tolerance`], [`Metrics`] and the diff PNG.
//!
//! `gleon diff`, `gleon status` and the integrations (`gleon-ffi`) all compare through here, so a
//! pair of images gets one verdict and one message wherever it is compared.

use gleon_engine::{
    ComparisonResult, PixelRegions, Pixels, Region, compare_images,
    config::Zone,
    decode::{DecodeError, decode_rgba, fits_budget},
    masking::{apply_masks, resolve_zones},
};
use image::{
    ExtendedColorType, ImageEncoder, RgbaImage,
    codecs::png::{CompressionType, FilterType, PngEncoder},
};

use crate::{
    case::{CaseErrorKind, Metrics, RegionMetrics, text},
    tolerance::{TextTolerance, Tolerance},
};

/// A candidate image: PNG bytes, or the raw straight (not premultiplied) RGBA8 pixels of a
/// capture the integration did not encode, row by row.
#[derive(Debug, Clone, Copy)]
pub enum Candidate<'a> {
    /// PNG bytes.
    Png(&'a [u8]),
    /// `width * height * 4` bytes of straight RGBA.
    Rgba {
        /// Width in pixels.
        width: u32,
        /// Height in pixels.
        height: u32,
        /// The pixels.
        pixels: &'a [u8],
    },
}

impl<'a> Candidate<'a> {
    /// The candidate as PNG: the given bytes, or the raw pixels encoded (`None` for pixels over
    /// the decoding budget, which no comparison could read back, of the wrong length, or an
    /// encoder failure).
    #[must_use]
    pub fn to_png(&self) -> Option<std::borrow::Cow<'_, [u8]>> {
        match *self {
            Self::Png(png) => Some(std::borrow::Cow::Borrowed(png)),
            Self::Rgba {
                width,
                height,
                pixels,
            } => {
                // Encoded in place: a copy of the pixels would cost as much as a frame.
                if !fits_budget(width, height) || !is_rgba_len(width, height, pixels.len()) {
                    return None;
                }
                encode(pixels, width, height)
                    .ok()
                    .map(std::borrow::Cow::Owned)
            }
        }
    }

    /// The pixels, within the engine's decoding budget: a PNG decoded into `decoded`, raw pixels
    /// as they are (never copied).
    fn pixels<'s>(self, decoded: &'s mut Option<RgbaImage>) -> Result<Pixels<'s>, CompareError>
    where
        'a: 's,
    {
        match self {
            Self::Png(png) => {
                let image = decode_rgba(png).map_err(CompareError::Candidate)?;
                Ok(Pixels::from(&*decoded.insert(image)))
            }
            Self::Rgba {
                width,
                height,
                pixels,
            } => {
                if !fits_budget(width, height) {
                    return Err(CompareError::Candidate(DecodeError::TooLarge {
                        width,
                        height,
                    }));
                }
                Pixels::new(pixels, width, height).ok_or(CompareError::CandidatePixels {
                    width,
                    height,
                    len: pixels.len(),
                })
            }
        }
    }
}

/// Whether `len` bytes are the straight RGBA8 pixels of a `width` x `height` image.
#[must_use]
pub fn is_rgba_len(width: u32, height: u32, len: usize) -> bool {
    u64::try_from(len).ok() == Some(u64::from(width) * u64::from(height) * 4)
}

/// The text of a candidate, judged by its tiles under its own tolerance (in SSIM mode left out
/// of both gates).
#[derive(Debug, Clone, Copy)]
pub struct Text<'a> {
    /// Text regions in candidate pixels; [`compare`] clips them to the image.
    pub regions: &'a [Region],
    /// Their tolerance.
    pub tolerance: TextTolerance,
}

/// What comparing a golden with a candidate found.
#[derive(Debug, Clone, PartialEq)]
pub enum Compared {
    /// Within the tolerance.
    Match {
        /// Whole-image metrics, reported for matches too (headroom to the tolerance).
        metrics: Metrics,
        /// The compared regions: the whole image, then the worst tile of text (if any).
        regions: Vec<RegionMetrics>,
    },
    /// Beyond the tolerance.
    Mismatch {
        /// Whole-image metrics.
        metrics: Metrics,
        /// The compared regions: the whole image, then the worst tile of text (if any).
        regions: Vec<RegionMetrics>,
        /// The PNG-encoded diff visualization.
        diff_png: Vec<u8>,
    },
    /// Different image sizes; only the area both cover was compared, for the diff.
    DimensionMismatch {
        /// Width and height of the golden.
        golden: (u32, u32),
        /// Width and height of the candidate.
        candidate: (u32, u32),
        /// The PNG-encoded diff of both sizes; `None` when its canvas is over the decoding
        /// budget.
        diff_png: Option<Vec<u8>>,
    },
}

/// A comparison and the masks that reached beyond the images.
#[derive(Debug, Clone, PartialEq)]
pub struct Comparison {
    /// What the comparison found.
    pub compared: Compared,
    /// Masks that reached beyond the images and were clipped (none for images of different
    /// sizes, which are not masked).
    pub clamped_masks: usize,
}

/// Why two images could not be compared: never a pass.
#[derive(Debug, thiserror::Error)]
pub enum CompareError {
    /// The golden is no valid image within the decoding budget.
    #[error("golden image: {0}")]
    Golden(#[source] DecodeError),
    /// The candidate is no valid image within the decoding budget.
    #[error("candidate image: {0}")]
    Candidate(#[source] DecodeError),
    /// The images are over the SSIM analysis budget.
    #[error("{}", text::too_large_for_ssim(*width, *height))]
    TooLarge {
        /// Width of the images.
        width: u32,
        /// Height of the images.
        height: u32,
    },
    /// Raw candidate pixels of another length than their size says (a bug of the integration).
    #[error("candidate pixels: {len} bytes for {width}x{height} RGBA")]
    CandidatePixels {
        /// Declared width.
        width: u32,
        /// Declared height.
        height: u32,
        /// Bytes given.
        len: usize,
    },
    /// The diff visualization could not be encoded.
    #[error("cannot encode the diff image: {0}")]
    Diff(#[source] image::ImageError),
    /// The engine measured in another mode than the tolerance asked for (a bug).
    #[error("internal error: the engine measurement does not match the tolerance")]
    Internal,
}

impl CompareError {
    /// The class of this error in a case report.
    #[must_use]
    pub const fn kind(&self) -> CaseErrorKind {
        match self {
            Self::Internal => CaseErrorKind::Internal,
            Self::CandidatePixels { .. } => CaseErrorKind::InvalidInput,
            Self::Golden(_) | Self::Candidate(_) | Self::TooLarge { .. } | Self::Diff(_) => {
                CaseErrorKind::Image
            }
        }
    }
}

/// Two decoded images with the masks applied.
#[derive(Debug)]
pub struct Decoded {
    /// The golden.
    pub golden: RgbaImage,
    /// The candidate.
    pub candidate: RgbaImage,
    /// Masks that reached beyond the images and were clipped.
    pub clamped_masks: usize,
}

/// Decodes the PNGs `golden` and `candidate` with the engine's resource limits and masks both.
///
/// The masks apply only to images of the same size: images of different sizes are never alike,
/// and a mask means the same pixels only on the same size.
///
/// # Errors
/// Returns [`CompareError::Golden`] or [`CompareError::Candidate`] for an image that cannot be
/// decoded within the budget.
pub fn decode_masked(
    golden: &[u8],
    candidate: &[u8],
    masks: &[Zone],
) -> Result<Decoded, CompareError> {
    let mut golden = decode_rgba(golden).map_err(CompareError::Golden)?;
    let mut candidate = decode_rgba(candidate).map_err(CompareError::Candidate)?;
    let clamped_masks = if !masks.is_empty() && golden.dimensions() == candidate.dimensions() {
        apply_masks(&mut candidate, masks);
        apply_masks(&mut golden, masks)
    } else {
        0
    };
    Ok(Decoded {
        golden,
        candidate,
        clamped_masks,
    })
}

/// Compares the PNG `golden` with `candidate` under `tolerance`, with `masks` and `text`.
///
/// Masks apply to images of the same size (like [`decode_masked`]): in pixel and exact mode their
/// pixels are neither compared nor counted, in SSIM mode they are painted black in both. Text
/// regions are judged by their tiles under their tolerance, masks winning (in SSIM mode painted
/// black for both gates too).
///
/// # Errors
/// Returns [`CompareError`] for an image that cannot be decoded, raw pixels of the wrong length,
/// images over the SSIM analysis budget, a diff that cannot be encoded, or an engine measurement
/// of another mode (a bug).
pub fn compare(
    golden: &[u8],
    candidate: Candidate<'_>,
    tolerance: &Tolerance,
    masks: &[Zone],
    text: Option<Text<'_>>,
) -> Result<Comparison, CompareError> {
    let golden = decode_rgba(golden).map_err(CompareError::Golden)?;
    let mut decoded = None;
    let candidate = candidate.pixels(&mut decoded)?;
    let (width, height) = golden.dimensions();
    let same_size = golden.dimensions() == candidate.dimensions();
    let (masks, clamped_masks) = if same_size {
        resolve_zones(masks, width, height)
    } else {
        // Only for the diff of both sizes (in golden pixels): no warning, the sizes are the
        // finding.
        (resolve_zones(masks, width, height).0, 0)
    };
    let (mode, config) = tolerance.engine_config();
    let text = text.filter(|text| same_size && !text.regions.is_empty());
    let text_regions: Vec<Region> = text
        .iter()
        .flat_map(|text| text.regions)
        .filter_map(|region| clip(region, width, height))
        .collect();
    let text_tolerance = text.map(|text| text.tolerance);
    let regions = PixelRegions {
        masks: &masks,
        text: &text_regions,
        text_tolerance: text_tolerance.map(|text| text.0),
    };
    let measured = |measurement| {
        Metrics::from_measurement(&measurement, tolerance, text_tolerance.as_ref())
            .map(|metrics| {
                let regions =
                    RegionMetrics::of(metrics, &measurement, tolerance, text_tolerance.as_ref());
                (metrics, regions)
            })
            .ok_or(CompareError::Internal)
    };
    let compared = match compare_images(&golden, candidate, mode, &config, &regions) {
        ComparisonResult::Match { measurement } => {
            let (metrics, regions) = measured(measurement)?;
            Compared::Match { metrics, regions }
        }
        ComparisonResult::Mismatch {
            measurement,
            diff_image,
        } => {
            let (metrics, regions) = measured(measurement)?;
            Compared::Mismatch {
                metrics,
                regions,
                diff_png: encode_png(&diff_image).map_err(CompareError::Diff)?,
            }
        }
        ComparisonResult::DimensionMismatch {
            baseline_size,
            actual_size,
            diff_image,
        } => Compared::DimensionMismatch {
            golden: baseline_size,
            candidate: actual_size,
            diff_png: diff_image
                .map(|diff| encode_png(&diff))
                .transpose()
                .map_err(CompareError::Diff)?,
        },
        ComparisonResult::TooLarge {
            size: (width, height),
        } => return Err(CompareError::TooLarge { width, height }),
    };
    Ok(Comparison {
        compared,
        clamped_masks,
    })
}

/// `region` clipped to a `width` x `height` image; `None` if nothing of it is inside.
fn clip(region: &Region, width: u32, height: u32) -> Option<Region> {
    let (x, y) = (region.x.min(width), region.y.min(height));
    let clipped = Region {
        x,
        y,
        width: region.x.saturating_add(region.width).min(width) - x,
        height: region.y.saturating_add(region.height).min(height) - y,
    };
    (clipped.width > 0 && clipped.height > 0).then_some(clipped)
}

/// Encodes `image` as PNG.
///
/// # Errors
/// Returns the encoder's error.
pub fn encode_png(image: &RgbaImage) -> Result<Vec<u8>, image::ImageError> {
    encode(image.as_raw(), image.width(), image.height())
}

/// Encodes the straight RGBA8 `pixels` of a `width` x `height` image as PNG with fast compression
/// and adaptive filters (`Up` alone encodes a diff in half the time but makes it 1.3-3x larger,
/// written twice per failure): the one PNG encoder of the comparisons (the candidate kept for
/// `gleon approve`, the diff, [`encode_png`]).
fn encode(pixels: &[u8], width: u32, height: u32) -> Result<Vec<u8>, image::ImageError> {
    let mut png = Vec::new();
    PngEncoder::new_with_quality(&mut png, CompressionType::Fast, FilterType::Adaptive)
        .write_image(pixels, width, height, ExtendedColorType::Rgba8)
        .map(|()| png)
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
    use gleon_engine::{Measurement, config::Dimension};
    use image::{ImageBuffer, Rgba};

    use super::*;
    use crate::tolerance::TextTolerance;

    fn png(width: u32, height: u32, paint: impl Fn(u32, u32) -> Rgba<u8>) -> Vec<u8> {
        encode_png(&ImageBuffer::from_fn(width, height, paint)).unwrap()
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

    fn compared(golden: &[u8], candidate: &[u8], tolerance: &Tolerance) -> Compared {
        compare(golden, Candidate::Png(candidate), tolerance, &[], None)
            .unwrap()
            .compared
    }

    #[test]
    fn test_exact_match_reports_metrics() {
        let a = png(10, 10, |_, _| RED);
        assert_eq!(
            compared(&a, &a, &EXACT),
            Compared::Match {
                metrics: Metrics::Pixel {
                    total_pixels: 100,
                    diff_pixels: 0,
                    tolerated_pixels: 0,
                    edge_pixels: 0,
                    diff_ratio: 0.0,
                    headroom: 0.0,
                    text: None,
                },
                regions: vec![RegionMetrics::whole_image(Metrics::Pixel {
                    total_pixels: 100,
                    diff_pixels: 0,
                    tolerated_pixels: 0,
                    edge_pixels: 0,
                    diff_ratio: 0.0,
                    headroom: 0.0,
                    text: None,
                })],
            }
        );
    }

    #[test]
    fn test_exact_single_pixel_mismatch_has_diff() {
        let a = png(10, 10, |_, _| RED);
        let b = png(10, 10, one_blue_pixel);
        assert!(matches!(
            compared(&a, &b, &EXACT),
            Compared::Mismatch {
                metrics: Metrics::Pixel { diff_pixels: 1, headroom, .. },
                diff_png,
                ..
            } if headroom == -0.01 && image::load_from_memory(&diff_png).is_ok()
        ));
    }

    #[test]
    fn test_pixel_threshold_tolerates_small_change() {
        let a = png(10, 10, |_, _| RED);
        let b = png(10, 10, one_blue_pixel);
        let tolerance = Tolerance::pixel(0.05);
        assert!(matches!(
            compared(&a, &b, &tolerance),
            Compared::Match {
                metrics: Metrics::Pixel { diff_pixels: 1, .. },
                ..
            }
        ));
    }

    #[test]
    fn test_ssim_reports_policy_metrics() {
        let a = png(64, 64, |_, _| RED);
        let b = png(64, 64, |x, _| if x < 32 { BLUE } else { RED });
        assert!(matches!(
            compared(&a, &b, &SSIM),
            Compared::Mismatch {
                metrics: Metrics::Ssim {
                    peak_excess,
                    failing_region: Some(region),
                    changed_pixels: 2048,
                    headroom,
                    ..
                },
                ..
            } if peak_excess > 100.0 && headroom.color < 0.0 && region.width == 32
        ));
    }

    #[test]
    fn test_masks_hide_changed_region_and_count_clipped_ones() {
        let a = png(10, 10, |_, _| RED);
        let b = png(10, 10, |x, y| if x < 2 && y < 2 { BLUE } else { RED });
        let mask = |x, width| Zone {
            x,
            y: 0,
            width: Dimension::Pixels(width),
            height: Dimension::Pixels(2),
        };
        let inside = compare(&a, Candidate::Png(&b), &EXACT, &[mask(0, 2)], None).unwrap();
        assert!(matches!(inside.compared, Compared::Match { .. }));
        assert_eq!(inside.clamped_masks, 0);
        let clipped = compare(
            &a,
            Candidate::Png(&b),
            &EXACT,
            &[mask(0, 2), mask(9, 5)],
            None,
        )
        .unwrap();
        assert!(matches!(clipped.compared, Compared::Match { .. }));
        assert_eq!(clipped.clamped_masks, 1);
    }

    /// Images of different sizes: their sizes are the finding (no clamped-mask warning), and
    /// the diff shows both, masked pixels unmarked.
    #[test]
    fn test_dimension_mismatch_reports_sizes_with_a_diff_of_both() {
        let a = png(10, 10, |_, _| RED);
        let b = png(12, 10, |x, y| {
            if (x, y) == (1, 1) || (x, y) == (8, 8) {
                Rgba([0, 0, 0, 255])
            } else {
                RED
            }
        });
        let mask = Zone {
            x: 0,
            y: 0,
            width: Dimension::Percent(50.0),
            height: Dimension::Pixels(20),
        };
        let comparison = compare(&a, Candidate::Png(&b), &EXACT, &[mask], None).unwrap();
        let diff = |png: &[u8]| decode_rgba(png).unwrap();
        assert!(
            matches!(
                &comparison,
                Comparison {
                    compared: Compared::DimensionMismatch {
                        golden: (10, 10),
                        candidate: (12, 10),
                        diff_png: Some(png),
                    },
                    clamped_masks: 0,
                } if diff(png).dimensions() == (12, 10)
                    // Masked: the darkened golden; unmasked: magenta; the candidate's own
                    // columns: green stripes.
                    && diff(png).get_pixel(1, 1).0 == [127, 0, 0, 255]
                    && diff(png).get_pixel(8, 8).0 == [255, 0, 255, 255]
                    && diff(png).get_pixel(10, 0).0 == [0, 200, 83, 255]
            ),
            "{comparison:?}"
        );
    }

    /// Raw pixels compare like the PNG they encode to; pixels of the wrong length are the
    /// integration's bug.
    #[test]
    fn test_raw_candidates_compare_like_their_png() {
        let a = png(10, 10, |_, _| RED);
        let b = ImageBuffer::from_fn(10, 10, one_blue_pixel);
        let raw = Candidate::Rgba {
            width: 10,
            height: 10,
            pixels: b.as_raw(),
        };
        let from_png = compare(
            &a,
            Candidate::Png(&encode_png(&b).unwrap()),
            &EXACT,
            &[],
            None,
        );
        assert_eq!(
            compare(&a, raw, &EXACT, &[], None).unwrap(),
            from_png.unwrap()
        );
        assert_eq!(raw.to_png().unwrap().as_ref(), encode_png(&b).unwrap());

        let short = Candidate::Rgba {
            width: 10,
            height: 10,
            pixels: &b.as_raw()[..12],
        };
        let err = compare(&a, short, &EXACT, &[], None).unwrap_err();
        assert_eq!(err.kind(), CaseErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "candidate pixels: 12 bytes for 10x10 RGBA");
        assert!(short.to_png().is_none());

        // Surplus bytes are as wrong as missing ones.
        let mut surplus = b.as_raw().clone();
        surplus.push(0);
        let long = Candidate::Rgba {
            width: 10,
            height: 10,
            pixels: &surplus,
        };
        assert_eq!(
            compare(&a, long, &EXACT, &[], None).unwrap_err().kind(),
            CaseErrorKind::InvalidInput
        );
        assert!(long.to_png().is_none());

        // A size over the decoding budget is refused before its pixels are looked at.
        let huge = Candidate::Rgba {
            width: 20_000,
            height: 1,
            pixels: &[],
        };
        assert!(matches!(
            compare(&a, huge, &EXACT, &[], None),
            Err(CompareError::Candidate(DecodeError::TooLarge {
                width: 20_000,
                height: 1
            }))
        ));
        let wide = vec![0; 16_385 * 4];
        let over_budget = Candidate::Rgba {
            width: 16_385,
            height: 1,
            pixels: &wide,
        };
        assert!(over_budget.to_png().is_none(), "never kept as a candidate");
        assert_eq!(Candidate::Png(&a).to_png().unwrap().as_ref(), &a[..]);
    }

    /// Text regions (clipped to the image) are compared under their tolerance, the rest
    /// strictly; the worst tile becomes a `text` region of the report.
    #[test]
    fn test_text_regions_and_their_metrics() {
        let a = png(32, 16, |_, _| RED);
        let mut b = ImageBuffer::from_pixel(32, 16, RED);
        b.put_pixel(3, 3, BLUE);
        let text = Text {
            regions: &[Region {
                x: 0,
                y: 0,
                width: 16,
                height: 40,
            }],
            tolerance: TextTolerance(0.1),
        };
        let raw = Candidate::Rgba {
            width: 32,
            height: 16,
            pixels: b.as_raw(),
        };
        let Compared::Match { metrics, regions } =
            compare(&a, raw, &EXACT, &[], Some(text)).unwrap().compared
        else {
            panic!("text noise within its tolerance matches");
        };
        assert!(matches!(
            metrics,
            Metrics::Pixel {
                total_pixels: 256,
                diff_pixels: 0,
                text: Some(crate::case::TextMetrics {
                    pixels: 256,
                    diff_pixels: 1,
                    ..
                }),
                ..
            }
        ));
        assert_eq!(regions[1].kind, crate::case::RegionKind::Text);
        assert_eq!(
            regions[1].rect,
            Some(Region {
                x: 0,
                y: 0,
                width: 16,
                height: 16
            })
        );

        // Without text the same dot fails exact.
        assert!(matches!(
            compare(&a, raw, &EXACT, &[], None).unwrap().compared,
            Compared::Mismatch { .. }
        ));
    }

    #[test]
    fn test_ssim_over_analysis_budget_is_an_error() {
        let big = png(4097, 4096, |_, _| RED);
        let err = compare(&big, Candidate::Png(&big), &SSIM, &[], None).unwrap_err();
        assert!(matches!(
            err,
            CompareError::TooLarge {
                width: 4097,
                height: 4096
            }
        ));
        assert_eq!(err.kind(), CaseErrorKind::Image);
        assert!(err.to_string().contains("SSIM analysis budget"), "{err}");
    }

    #[test]
    fn test_corrupt_images_name_the_image() {
        let a = png(4, 4, |_, _| RED);
        for (golden, candidate, prefix) in [
            (&b"garbage"[..], &a[..], "golden image: "),
            (&a[..], &b"garbage"[..], "candidate image: "),
        ] {
            let err = compare(golden, Candidate::Png(candidate), &EXACT, &[], None).unwrap_err();
            assert_eq!(err.kind(), CaseErrorKind::Image);
            assert!(err.to_string().starts_with(prefix), "{err}");
        }
        let masked = decode_masked(&a, b"garbage", &[]).unwrap_err();
        assert!(matches!(masked, CompareError::Candidate(_)));
    }

    /// Masks mean the same pixels only on images of the same size: others stay unmasked.
    #[test]
    fn test_decode_masked_leaves_images_of_other_sizes_alone() {
        let (a, b) = (png(4, 4, |_, _| RED), png(5, 4, |_, _| RED));
        let mask = Zone {
            x: 0,
            y: 0,
            width: Dimension::Pixels(10),
            height: Dimension::Pixels(1),
        };
        let decoded = decode_masked(&a, &b, &[mask]).unwrap();
        assert_eq!(decoded.clamped_masks, 0);
        assert_eq!(*decoded.golden.get_pixel(0, 0), RED);
    }

    #[test]
    fn test_a_measurement_of_another_mode_is_an_internal_error() {
        let pixel = Measurement::Pixel {
            checked_pixels: 1,
            diff_count: 0,
            tolerated_count: 0,
            edge_count: 0,
            text: None,
        };
        assert!(Metrics::from_measurement(&pixel, &SSIM, None).is_none());
        assert_eq!(CompareError::Internal.kind(), CaseErrorKind::Internal);
        assert!(
            CompareError::Internal
                .to_string()
                .starts_with("internal error")
        );
    }
}
