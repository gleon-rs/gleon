//! Pixel-by-pixel image comparison, with masked and text regions.
//!
//! Every pixel of a frame is in one class: masked (never compared, not counted), text (compared
//! under a [`TextPolicy`]: a per-channel color tolerance, and a share of differing pixels in each
//! [`TEXT_TILE`]-pixel tile) or strict (compared byte for byte). Masks win over text. Text is
//! what operating systems draw differently (their font engines, hinting); everything else of a
//! test frame renders the same everywhere.

use image::RgbaImage;
use rayon::prelude::*;

use crate::ssim::Region;

/// Side of the square tiles a text region is judged in.
///
/// A share of differing pixels per tile reacts to how they cluster: a changed word is a dense
/// cluster, rasterization noise is spread thin, and a share of a whole long paragraph would
/// dilute the changed word.
pub const TEXT_TILE: u32 = 16;

/// How much text may differ.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextPolicy {
    /// Largest difference of any channel (8-bit units) for a text pixel to count as equal.
    pub color_tolerance: f64,
    /// Largest share of differing text pixels in any tile.
    pub max_diff_ratio: f64,
}

/// The rectangles of a frame the pixel comparison treats apart; masks win over text.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PixelRegions<'a> {
    /// Pixels never compared nor counted (`ignoreRegions`, mask rules).
    pub masks: &'a [Region],
    /// Text, compared under [`Self::text_policy`]; strictly without one.
    pub text: &'a [Region],
    /// The tolerance of text.
    pub text_policy: Option<TextPolicy>,
}

impl PixelRegions<'_> {
    /// No masks and no text: every pixel is compared strictly.
    pub const NONE: PixelRegions<'static> = PixelRegions {
        masks: &[],
        text: &[],
        text_policy: None,
    };

    /// Whether every pixel is compared strictly.
    #[must_use]
    pub const fn is_none(&self) -> bool {
        self.masks.is_empty() && (self.text.is_empty() || self.text_policy.is_none())
    }
}

/// A tile of a text region with its text pixels (masked ones left out) and those beyond the
/// color tolerance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextTile {
    /// The tile, inside its text region (smaller at the region's right and bottom edges).
    pub region: Region,
    /// Its text pixels.
    pub pixels: u64,
    /// Its text pixels beyond the color tolerance.
    pub diff_pixels: u64,
}

impl TextTile {
    /// The share of differing pixels.
    #[must_use]
    pub fn diff_ratio(&self) -> f64 {
        // Pixel counts of a tile are tiny, so the conversion is exact.
        #[expect(
            clippy::cast_precision_loss,
            reason = "a tile has at most TEXT_TILE squared pixels"
        )]
        let ratio = self.diff_pixels as f64 / self.pixels as f64;
        ratio
    }
}

/// What the text regions measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextAnalysis {
    /// Text pixels (masked ones left out).
    pub pixels: u64,
    /// Text pixels beyond the color tolerance.
    pub diff_pixels: u64,
    /// The tile with the largest share of differing pixels (the first of equal ones); `None`
    /// without text pixels.
    pub worst_tile: Option<TextTile>,
}

/// What [`analyze`] measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelAnalysis {
    /// Pixels compared strictly: neither masked nor text under a text policy.
    pub checked_pixels: u64,
    /// Strictly compared pixels whose RGBA bytes differ.
    pub diff_pixels: u64,
    /// The text regions, when a text policy applied to some.
    pub text: Option<TextAnalysis>,
}

/// The class of a pixel in [`classify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Strict,
    StrictDiff,
    Text,
    TextDiff,
    Masked,
}

/// Compares `baseline` and `actual` (of the same size) pixel by pixel under `regions`.
///
/// # Panics
/// Panics if the images differ in size or a region reaches beyond them.
#[must_use]
pub fn analyze(
    baseline: &RgbaImage,
    actual: &RgbaImage,
    regions: &PixelRegions<'_>,
) -> PixelAnalysis {
    if regions.is_none() {
        let checked_pixels = u64::from(baseline.width()) * u64::from(baseline.height());
        return PixelAnalysis {
            checked_pixels,
            diff_pixels: count_mismatched_pixels(baseline, actual),
            text: None,
        };
    }
    classify(baseline, actual, regions).1
}

/// The diff visualization of `baseline` and `actual` under `regions`: strict differences
/// magenta, text beyond its tolerance orange, everything else the darkened baseline.
///
/// # Panics
/// Panics if the images differ in size or a region reaches beyond them.
#[must_use]
pub fn diff_image(
    baseline: &RgbaImage,
    actual: &RgbaImage,
    regions: &PixelRegions<'_>,
) -> RgbaImage {
    if regions.is_none() {
        return compare_pixels(baseline, actual).1;
    }
    let (classes, _) = classify(baseline, actual, regions);
    let mut diff = baseline.clone();
    for (pixel, class) in diff.pixels_mut().zip(classes) {
        pixel.0 = match class {
            Class::StrictDiff => MAGENTA,
            Class::TextDiff => ORANGE,
            Class::Strict | Class::Text | Class::Masked => {
                let [r, g, b, a] = pixel.0;
                [r / 2, g / 2, b / 2, a]
            }
        };
    }
    diff
}

const MAGENTA: [u8; 4] = [255, 0, 255, 255];
const ORANGE: [u8; 4] = [255, 165, 0, 255];

/// The class of every pixel (row-major) and the analysis.
fn classify(
    baseline: &RgbaImage,
    actual: &RgbaImage,
    regions: &PixelRegions<'_>,
) -> (Vec<Class>, PixelAnalysis) {
    assert_eq!(
        baseline.dimensions(),
        actual.dimensions(),
        "Image dimensions must match for a pixel comparison"
    );
    let width = baseline.width() as usize;
    let mut classes = vec![Class::Strict; width * baseline.height() as usize];
    let mut mark = |region: &Region, class: Class| {
        for row in classes
            .chunks_exact_mut(width)
            .skip(region.y as usize)
            .take(region.height as usize)
        {
            #[expect(
                clippy::expect_used,
                reason = "regions lie inside the image (documented panic)"
            )]
            row.get_mut(region.x as usize..(region.x + region.width) as usize)
                .expect("a region lies inside the image")
                .fill(class);
        }
    };
    let text = regions.text_policy.filter(|_| !regions.text.is_empty());
    if text.is_some() {
        regions
            .text
            .iter()
            .for_each(|region| mark(region, Class::Text));
    }
    regions
        .masks
        .iter()
        .for_each(|region| mark(region, Class::Masked));

    let tolerance = text.map_or(0.0, |policy| policy.color_tolerance);
    let mut analysis = PixelAnalysis {
        checked_pixels: 0,
        diff_pixels: 0,
        text: None,
    };
    let (mut text_pixels, mut text_diff) = (0, 0);
    let pairs = baseline
        .as_raw()
        .as_chunks::<4>()
        .0
        .iter()
        .zip(actual.as_raw().as_chunks::<4>().0);
    for (class, (expected, found)) in classes.iter_mut().zip(pairs) {
        match class {
            Class::Strict => {
                analysis.checked_pixels += 1;
                if expected != found {
                    analysis.diff_pixels += 1;
                    *class = Class::StrictDiff;
                }
            }
            Class::Text => {
                text_pixels += 1;
                let beyond = expected
                    .iter()
                    .zip(found)
                    .any(|(e, f)| f64::from(e.abs_diff(*f)) > tolerance);
                if beyond {
                    text_diff += 1;
                    *class = Class::TextDiff;
                }
            }
            Class::StrictDiff | Class::TextDiff | Class::Masked => {}
        }
    }
    if text.is_some() {
        analysis.text = Some(TextAnalysis {
            pixels: text_pixels,
            diff_pixels: text_diff,
            worst_tile: worst_tile(&classes, width, regions.text),
        });
    }
    (classes, analysis)
}

/// The tile of `text` (tiles counted from each region's corner) with the largest share of
/// differing text pixels; tiles without text pixels (masked) are left out.
fn worst_tile(classes: &[Class], width: usize, text: &[Region]) -> Option<TextTile> {
    let mut worst: Option<TextTile> = None;
    for region in text {
        for y in (region.y..region.y + region.height).step_by(TEXT_TILE as usize) {
            for x in (region.x..region.x + region.width).step_by(TEXT_TILE as usize) {
                let tile = Region {
                    x,
                    y,
                    width: TEXT_TILE.min(region.x + region.width - x),
                    height: TEXT_TILE.min(region.y + region.height - y),
                };
                let (mut pixels, mut diff_pixels) = (0, 0);
                let rows = classes
                    .chunks_exact(width)
                    .skip(tile.y as usize)
                    .take(tile.height as usize);
                for row in rows {
                    let columns = row
                        .get(tile.x as usize..(tile.x + tile.width) as usize)
                        .unwrap_or_default();
                    for class in columns {
                        match class {
                            Class::Text => pixels += 1,
                            Class::TextDiff => {
                                pixels += 1;
                                diff_pixels += 1;
                            }
                            Class::Strict | Class::StrictDiff | Class::Masked => {}
                        }
                    }
                }
                let tile = TextTile {
                    region: tile,
                    pixels,
                    diff_pixels,
                };
                let is_worse =
                    pixels > 0 && worst.is_none_or(|worst| tile.diff_ratio() > worst.diff_ratio());
                if is_worse {
                    worst = Some(tile);
                }
            }
        }
    }
    worst
}

/// Compares two images of the same dimensions pixel-by-pixel.
///
/// Returns the number of mismatched pixels and a composite diff image
/// where matching areas are darkened and mismatched areas are painted magenta.
///
/// # Panics
/// Panics if `baseline` and `actual` do not have identical dimensions.
#[must_use]
pub fn compare_pixels(baseline: &RgbaImage, actual: &RgbaImage) -> (u64, RgbaImage) {
    assert_eq!(
        baseline.dimensions(),
        actual.dimensions(),
        "Image dimensions must match for compare_pixels: baseline={:?}, actual={:?}",
        baseline.dimensions(),
        actual.dimensions()
    );

    let width = baseline.width();
    let height = baseline.height();

    let baseline_raw = baseline.as_raw();
    let actual_raw = actual.as_raw();

    let mut diff_raw = vec![0u8; baseline_raw.len()];

    let b_chunks = baseline_raw.par_chunks_exact(4);
    let a_chunks = actual_raw.par_chunks_exact(4);
    let d_chunks = diff_raw.par_chunks_exact_mut(4);

    let diff_count: u64 = b_chunks
        .zip(a_chunks)
        .zip(d_chunks)
        .map(|((b_chunk, a_chunk), d_chunk)| {
            if b_chunk == a_chunk {
                // Darken matching pixel: divide R, G, B by 2, keep A
                d_chunk[0] = b_chunk[0] / 2;
                d_chunk[1] = b_chunk[1] / 2;
                d_chunk[2] = b_chunk[2] / 2;
                d_chunk[3] = b_chunk[3];
                0u64
            } else {
                // Magenta: [255, 0, 255, 255]
                d_chunk.copy_from_slice(&[255, 0, 255, 255]);
                1u64
            }
        })
        .sum();

    // `diff_raw` is allocated above as exactly `baseline_raw.len()` bytes, which is always
    // `width * height * 4` for a valid `RgbaImage`, so `from_raw` can never return `None`.
    #[expect(
        clippy::expect_used,
        reason = "`diff_raw` is allocated as exactly `width * height * 4` bytes"
    )]
    let diff_image = RgbaImage::from_raw(width, height, diff_raw)
        .expect("invariant: diff_raw length must be exactly width * height * 4");

    (diff_count, diff_image)
}

/// Counts the number of mismatched pixels without allocating a diff image.
///
/// # Panics
/// Panics if `baseline` and `actual` do not have identical dimensions.
#[must_use]
pub fn count_mismatched_pixels(baseline: &RgbaImage, actual: &RgbaImage) -> u64 {
    assert_eq!(
        baseline.dimensions(),
        actual.dimensions(),
        "Image dimensions must match for count_mismatched_pixels: baseline={:?}, actual={:?}",
        baseline.dimensions(),
        actual.dimensions()
    );

    let baseline_raw = baseline.as_raw();
    let actual_raw = actual.as_raw();

    // Fast path: reinterpret the byte slices as u32 for cheaper, word-sized
    // equality checks. RgbaImage guarantees the raw length is a multiple of 4.
    // `try_cast_slice` never panics on misaligned input; it simply returns
    // Err, in which case we fall back to the byte-chunk comparison below.
    if let (Ok(b_u32), Ok(a_u32)) = (
        bytemuck::try_cast_slice::<u8, u32>(baseline_raw),
        bytemuck::try_cast_slice::<u8, u32>(actual_raw),
    ) {
        b_u32
            .par_iter()
            .zip(a_u32.par_iter())
            .filter(|(b, a)| b != a)
            .count() as u64
    } else {
        // Fallback for unaligned slice buffers
        let b_chunks = baseline_raw.par_chunks_exact(4);
        let a_chunks = actual_raw.par_chunks_exact(4);
        b_chunks.zip(a_chunks).filter(|(b, a)| b != a).count() as u64
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

    #[test]
    fn test_compare_pixels_identical() {
        let img1 = ImageBuffer::from_pixel(10, 10, Rgba([255, 0, 0, 255]));
        let img2 = ImageBuffer::from_pixel(10, 10, Rgba([255, 0, 0, 255]));

        let (diff_count, diff_img) = compare_pixels(&img1, &img2);
        assert_eq!(diff_count, 0);
        // Matching pixels should be darkened: 255 / 2 = 127
        assert_eq!(*diff_img.get_pixel(0, 0), Rgba([127, 0, 0, 255]));

        assert_eq!(count_mismatched_pixels(&img1, &img2), 0);
    }

    #[test]
    fn test_compare_pixels_mismatch() {
        let img1 = ImageBuffer::from_pixel(10, 10, Rgba([255, 0, 0, 255]));
        let mut img2 = ImageBuffer::from_pixel(10, 10, Rgba([255, 0, 0, 255]));
        img2.put_pixel(5, 5, Rgba([0, 255, 0, 255]));

        let (diff_count, diff_img) = compare_pixels(&img1, &img2);
        assert_eq!(diff_count, 1);
        // The mismatched pixel should be magenta
        assert_eq!(*diff_img.get_pixel(5, 5), Rgba([255, 0, 255, 255]));
        // The matching pixel should be darkened
        assert_eq!(*diff_img.get_pixel(0, 0), Rgba([127, 0, 0, 255]));

        assert_eq!(count_mismatched_pixels(&img1, &img2), 1);
    }

    const WHITE: Rgba<u8> = Rgba([255, 255, 255, 255]);
    const GRAY: Rgba<u8> = Rgba([235, 235, 235, 255]);
    const BLACK: Rgba<u8> = Rgba([0, 0, 0, 255]);

    fn region(x: u32, y: u32, width: u32, height: u32) -> Region {
        Region {
            x,
            y,
            width,
            height,
        }
    }

    const TEXT: TextPolicy = TextPolicy {
        color_tolerance: 24.0,
        max_diff_ratio: 0.1,
    };

    /// Masks win over text, text under a policy is apart from the strict pixels, and the strict
    /// pixels are compared byte for byte.
    #[test]
    fn test_classes_and_mask_priority() {
        let baseline = ImageBuffer::from_pixel(64, 32, WHITE);
        let mut actual = baseline.clone();
        actual.put_pixel(1, 1, BLACK); // masked
        actual.put_pixel(14, 1, BLACK); // text, beyond the tolerance
        actual.put_pixel(15, 1, GRAY); // text, within the tolerance
        actual.put_pixel(40, 20, Rgba([255, 255, 255, 254])); // strict: alpha counts
        let (masks, text) = ([region(0, 0, 12, 4)], [region(8, 0, 24, 16)]);
        let regions = PixelRegions {
            masks: &masks,
            text: &text,
            text_policy: Some(TEXT),
        };

        let analysis = analyze(&baseline, &actual, &regions);
        // 64x32 = 2048 pixels; text 24x16 = 384, of which 4x4 masked; masks 12x4 = 48.
        assert_eq!(analysis.checked_pixels, 2048 - 48 - (384 - 16));
        assert_eq!(analysis.diff_pixels, 1);
        let text = analysis.text.unwrap();
        assert_eq!((text.pixels, text.diff_pixels), (384 - 16, 1));
        let worst = text.worst_tile.unwrap();
        assert_eq!(worst.region, region(8, 0, 16, 16));
        assert_eq!((worst.pixels, worst.diff_pixels), (256 - 16, 1));

        let diff = diff_image(&baseline, &actual, &regions);
        assert_eq!(diff.get_pixel(14, 1).0, ORANGE);
        assert_eq!(diff.get_pixel(40, 20).0, MAGENTA);
        assert_eq!(
            diff.get_pixel(1, 1).0,
            [127, 127, 127, 255],
            "masked: darkened"
        );
        assert_eq!(
            diff.get_pixel(11, 1).0,
            [127, 127, 127, 255],
            "tolerated: darkened"
        );

        // Without a policy text is compared strictly.
        let strict = PixelRegions {
            text_policy: None,
            ..regions
        };
        let analysis = analyze(&baseline, &actual, &strict);
        assert_eq!(analysis.checked_pixels, 2048 - 48);
        assert_eq!(analysis.diff_pixels, 3);
        assert_eq!(analysis.text, None);
    }

    /// The same number of differing text pixels fails as a dense cluster (a changed word) and
    /// passes spread over a long line (rasterization noise).
    #[test]
    fn test_text_tiles_tell_a_cluster_from_noise() {
        let baseline = ImageBuffer::from_pixel(160, 16, WHITE);
        let text = [region(0, 0, 160, 16)];
        let regions = PixelRegions {
            masks: &[],
            text: &text,
            text_policy: Some(TEXT),
        };
        let mut cluster = baseline.clone();
        let mut noise = baseline.clone();
        for i in 0..40 {
            cluster.put_pixel(i % 8, i / 8, BLACK);
            noise.put_pixel(i * 4, (i * 7) % 16, BLACK);
        }

        let clustered = analyze(&baseline, &cluster, &regions).text.unwrap();
        let spread = analyze(&baseline, &noise, &regions).text.unwrap();
        assert_eq!((clustered.diff_pixels, spread.diff_pixels), (40, 40));
        let ratio = |text: TextAnalysis| text.worst_tile.unwrap().diff_ratio();
        assert!(ratio(clustered) > TEXT.max_diff_ratio, "{clustered:?}");
        assert!(ratio(spread) <= TEXT.max_diff_ratio, "{spread:?}");
        // A whole-region share would not tell them apart: 40 of 2560 pixels pass either way.
    }

    /// Tiles start at each region's corner and shrink at its edges; a tile that is all mask
    /// does not count.
    #[test]
    fn test_tiles_follow_their_region() {
        let baseline = ImageBuffer::from_pixel(64, 64, WHITE);
        let mut actual = baseline.clone();
        actual.put_pixel(41, 41, BLACK);
        let (text, masks) = ([region(5, 5, 40, 40)], [region(5, 5, 16, 16)]);
        let regions = PixelRegions {
            masks: &masks,
            text: &text,
            text_policy: Some(TEXT),
        };
        let worst = analyze(&baseline, &actual, &regions)
            .text
            .unwrap()
            .worst_tile
            .unwrap();
        assert_eq!(worst.region, region(37, 37, 8, 8));
        assert_eq!((worst.pixels, worst.diff_pixels), (64, 1));

        let all_masked = PixelRegions {
            masks: &text,
            ..regions
        };
        let text = analyze(&baseline, &actual, &all_masked).text.unwrap();
        assert_eq!((text.pixels, text.worst_tile), (0, None));
    }

    #[test]
    #[should_panic(expected = "Image dimensions must match")]
    fn test_compare_pixels_unequal_dimensions_panics() {
        let img1 = ImageBuffer::from_pixel(10, 10, Rgba([255, 0, 0, 255]));
        let img2 = ImageBuffer::from_pixel(20, 10, Rgba([255, 0, 0, 255]));
        let _ = compare_pixels(&img1, &img2);
    }

    #[test]
    #[should_panic(expected = "Image dimensions must match")]
    fn test_count_mismatched_pixels_unequal_dimensions_panics() {
        let img1 = ImageBuffer::from_pixel(10, 10, Rgba([255, 0, 0, 255]));
        let img2 = ImageBuffer::from_pixel(10, 20, Rgba([255, 0, 0, 255]));
        let _ = count_mismatched_pixels(&img1, &img2);
    }
}
