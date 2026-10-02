//! Pixel-by-pixel image comparison, with masked and text regions.
//!
//! Every pixel of a frame is in one class: masked (never compared, not counted), text (compared
//! under a [`TextPolicy`]: a per-channel color tolerance, and a share of differing pixels in every
//! [`TEXT_TILE`]-pixel square tile of a text region) or strict (compared byte for byte). Masks win over text. Text is
//! what operating systems draw differently (their font engines, hinting); everything else of a
//! test frame renders the same everywhere.

use image::RgbaImage;
use rayon::prelude::*;

use crate::ssim::Region;

/// Side of the square tiles a text region is judged in: every square of this side inside the
/// region, wherever it starts.
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

/// A tile of a text region with its pixels and the text pixels beyond the color tolerance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextTile {
    /// The tile, inside its text region ([`TEXT_TILE`] square, or the region's size where it is
    /// smaller).
    pub region: Region,
    /// Its pixels: text, and masked ones (which count as equal).
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

/// What [`compare`] measured.
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

/// A pixel comparison: what it measured, and how to draw its diff without comparing again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compared {
    /// What the comparison measured.
    pub analysis: PixelAnalysis,
    /// The class of every pixel (row-major); `None` when every pixel was compared strictly.
    classes: Option<Vec<Class>>,
}

impl Compared {
    /// The diff visualization of the compared `baseline` and `actual`: strict differences
    /// magenta, text beyond its tolerance orange, everything else the darkened baseline.
    ///
    /// # Panics
    /// Panics if the images are not the compared ones (another size).
    #[must_use]
    pub fn diff_image(&self, baseline: &RgbaImage, actual: &RgbaImage) -> RgbaImage {
        let Some(classes) = &self.classes else {
            return compare_pixels(baseline, actual).1;
        };
        assert_eq!(
            classes.len(),
            baseline.width() as usize * baseline.height() as usize,
            "Image dimensions must match the comparison"
        );
        let mut diff = baseline.clone();
        let raw: &mut [u8] = diff.as_mut();
        raw.as_chunks_mut::<4>()
            .0
            .par_iter_mut()
            .zip(classes.par_iter())
            .for_each(|(pixel, class)| {
                *pixel = match class {
                    Class::StrictDiff => MAGENTA,
                    Class::TextDiff => ORANGE,
                    Class::Strict | Class::Text | Class::Masked => {
                        let [r, g, b, a] = *pixel;
                        [r / 2, g / 2, b / 2, a]
                    }
                };
            });
        diff
    }
}

/// Compares `baseline` and `actual` (of the same size) pixel by pixel under `regions`.
///
/// # Panics
/// Panics if the images differ in size or a region reaches beyond them.
#[must_use]
pub fn compare(baseline: &RgbaImage, actual: &RgbaImage, regions: &PixelRegions<'_>) -> Compared {
    if regions.is_none() {
        let checked_pixels = u64::from(baseline.width()) * u64::from(baseline.height());
        return Compared {
            analysis: PixelAnalysis {
                checked_pixels,
                diff_pixels: count_mismatched_pixels(baseline, actual),
                text: None,
            },
            classes: None,
        };
    }
    let (classes, analysis) = classify(baseline, actual, regions);
    Compared {
        analysis,
        classes: Some(classes),
    }
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
    let (image_width, image_height) = baseline.dimensions();
    let inside = |region: &Region| {
        region
            .x
            .checked_add(region.width)
            .is_some_and(|right| right <= image_width)
            && region
                .y
                .checked_add(region.height)
                .is_some_and(|bottom| bottom <= image_height)
    };
    // Every region, on both axes, before any pixel is touched.
    assert!(
        regions.masks.iter().chain(regions.text).all(inside),
        "a region reaches beyond the {image_width}x{image_height} image"
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
                reason = "the regions were checked to lie inside the image"
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

/// The tile of `text` with the largest share of differing text pixels (the first of equal ones):
/// every [`TEXT_TILE`]-pixel square inside a region (the region's own size when it is smaller),
/// so a cluster is judged whole wherever it falls and no thin strip at a region's edge makes a
/// tile of its own. Masked pixels of a tile count as equal, so a mask over most of a tile cannot
/// turn one noisy pixel into a large share; tiles without text pixels are left out.
///
/// The column sums of the tile's rows slide down a region and the tile slides along them, so
/// this takes one pass over each region. `text` lies inside the image ([`classify`]).
fn worst_tile(classes: &[Class], width: usize, text: &[Region]) -> Option<TextTile> {
    let mut worst: Option<TextTile> = None;
    // Per column of a region: its text and differing text pixels in the tile's rows.
    let mut columns = Vec::new();
    for region in text {
        let (tile_width, tile_height) = (TEXT_TILE.min(region.width), TEXT_TILE.min(region.height));
        let row = |y: u32| {
            let start = y as usize * width + region.x as usize;
            #[expect(
                clippy::expect_used,
                reason = "classify checked that the regions lie inside the image"
            )]
            classes
                .get(start..start + region.width as usize)
                .expect("a text region lies inside the image")
        };
        let mut consider = |x: u32, y: u32, (text_pixels, diff_pixels): (u64, u64)| {
            let tile = TextTile {
                region: Region {
                    x,
                    y,
                    width: tile_width,
                    height: tile_height,
                },
                pixels: u64::from(tile_width) * u64::from(tile_height),
                diff_pixels,
            };
            // Shares compared exactly: a / b > c / d as a * d > c * b.
            let is_worse = text_pixels > 0
                && worst.is_none_or(|worst| {
                    tile.diff_pixels * worst.pixels > worst.diff_pixels * tile.pixels
                });
            if is_worse {
                worst = Some(tile);
            }
        };
        columns.clear();
        columns.resize(region.width as usize, (0, 0));
        for y in region.y..region.y + tile_height {
            add_row(&mut columns, row(y), true);
        }
        for top in region.y..=region.y + region.height - tile_height {
            if top > region.y {
                add_row(&mut columns, row(top - 1), false);
                add_row(&mut columns, row(top + tile_height - 1), true);
            }
            let mut sum = columns
                .iter()
                .take(tile_width as usize)
                .fold((0, 0), |(pixels, diff), column| {
                    (pixels + column.0, diff + column.1)
                });
            consider(region.x, top, sum);
            let slides = columns.iter().zip(columns.iter().skip(tile_width as usize));
            for (left, (leaving, entering)) in (region.x + 1..).zip(slides) {
                sum = (
                    sum.0 - leaving.0 + entering.0,
                    sum.1 - leaving.1 + entering.1,
                );
                consider(left, top, sum);
            }
        }
    }
    worst
}

/// Adds (or, without `add`, removes) the text and differing text pixels of `row` to the sums of
/// its `columns` (`row` is as long as `columns`: a row of the region).
fn add_row(columns: &mut [(u64, u64)], row: &[Class], add: bool) {
    for (column, class) in columns.iter_mut().zip(row) {
        let (pixels, diff) = match class {
            Class::Text => (1, 0),
            Class::TextDiff => (1, 1),
            Class::Strict | Class::StrictDiff | Class::Masked => continue,
        };
        *column = if add {
            (column.0 + pixels, column.1 + diff)
        } else {
            (column.0 - pixels, column.1 - diff)
        };
    }
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

        let analysis = compare(&baseline, &actual, &regions).analysis;
        // 64x32 = 2048 pixels; text 24x16 = 384, of which 4x4 masked; masks 12x4 = 48.
        assert_eq!(analysis.checked_pixels, 2048 - 48 - (384 - 16));
        assert_eq!(analysis.diff_pixels, 1);
        let text = analysis.text.unwrap();
        assert_eq!((text.pixels, text.diff_pixels), (384 - 16, 1));
        let worst = text.worst_tile.unwrap();
        // Its 16 masked pixels count as equal.
        assert_eq!(worst.region, region(8, 0, 16, 16));
        assert_eq!((worst.pixels, worst.diff_pixels), (256, 1));

        let diff = compare(&baseline, &actual, &regions).diff_image(&baseline, &actual);
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
        let analysis = compare(&baseline, &actual, &strict).analysis;
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

        let clustered = compare(&baseline, &cluster, &regions)
            .analysis
            .text
            .unwrap();
        let spread = compare(&baseline, &noise, &regions).analysis.text.unwrap();
        assert_eq!((clustered.diff_pixels, spread.diff_pixels), (40, 40));
        let ratio = |text: TextAnalysis| text.worst_tile.unwrap().diff_ratio();
        assert!(ratio(clustered) > TEXT.max_diff_ratio, "{clustered:?}");
        assert!(ratio(spread) <= TEXT.max_diff_ratio, "{spread:?}");
        // A whole-region share would not tell them apart: 40 of 2560 pixels pass either way.
    }

    /// Tiles lie inside their region; a tile that is all mask does not count.
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
        let worst = compare(&baseline, &actual, &regions)
            .analysis
            .text
            .unwrap()
            .worst_tile
            .unwrap();
        // The first tile around the pixel that still lies inside the region.
        assert_eq!(worst.region, region(26, 26, 16, 16));
        assert_eq!((worst.pixels, worst.diff_pixels), (256, 1));

        let all_masked = PixelRegions {
            masks: &text,
            ..regions
        };
        let text = compare(&baseline, &actual, &all_masked)
            .analysis
            .text
            .unwrap();
        assert_eq!((text.pixels, text.worst_tile), (0, None));
    }

    /// A cluster is judged the same wherever it falls in the text: tiles anchored to the region
    /// would cut a 6x6 changed glyph straddling their corners into four 3x3 pieces (3.5% each)
    /// and pass it, while the same glyph inside one tile fails (14%).
    #[test]
    fn test_a_cluster_fails_wherever_it_falls() {
        let baseline = ImageBuffer::from_pixel(64, 32, WHITE);
        let text = [region(0, 0, 64, 32)];
        let regions = PixelRegions {
            masks: &[],
            text: &text,
            text_policy: Some(TEXT),
        };
        let ratio_at = |left: u32, top: u32| {
            let mut actual = baseline.clone();
            for (x, y) in (0..36).map(|i| (left + i % 6, top + i / 6)) {
                actual.put_pixel(x, y, BLACK);
            }
            let text = compare(&baseline, &actual, &regions).analysis.text.unwrap();
            text.worst_tile.unwrap().diff_ratio()
        };

        assert_eq!(ratio_at(16, 8), 36.0 / 256.0);
        assert_eq!(ratio_at(13, 13), 36.0 / 256.0, "straddling tile corners");
    }

    /// Noise at the edge of a region is judged in a whole tile: a 1px strip left over at the
    /// bottom of a 17px line would make two noisy pixels 12.5% of a "tile".
    #[test]
    fn test_region_edges_get_whole_tiles() {
        let baseline = ImageBuffer::from_pixel(32, 17, WHITE);
        let mut actual = baseline.clone();
        actual.put_pixel(0, 16, BLACK);
        actual.put_pixel(8, 16, BLACK);
        let text = [region(0, 0, 32, 17)];
        let regions = PixelRegions {
            masks: &[],
            text: &text,
            text_policy: Some(TEXT),
        };
        let worst = compare(&baseline, &actual, &regions)
            .analysis
            .text
            .unwrap()
            .worst_tile
            .unwrap();
        assert_eq!((worst.pixels, worst.diff_pixels), (256, 2), "{worst:?}");

        // A region smaller than a tile is one tile of its own size.
        let small = [region(4, 4, 10, 6)];
        let mut actual = ImageBuffer::from_pixel(32, 17, WHITE);
        actual.put_pixel(5, 5, BLACK);
        let worst = compare(
            &ImageBuffer::from_pixel(32, 17, WHITE),
            &actual,
            &PixelRegions {
                text: &small,
                ..regions
            },
        )
        .analysis
        .text
        .unwrap()
        .worst_tile
        .unwrap();
        assert_eq!(worst.region, small[0]);
        assert_eq!((worst.pixels, worst.diff_pixels), (60, 1));
    }

    /// A mask over most of a tile leaves few text pixels; they are judged in the whole tile, so
    /// two noisy pixels next to the mask stay noise (2 of 256), not 2 of the 16 unmasked ones.
    #[test]
    fn test_masked_pixels_of_a_tile_count_as_equal() {
        let baseline = ImageBuffer::from_pixel(16, 16, WHITE);
        let mut actual = baseline.clone();
        actual.put_pixel(3, 15, BLACK);
        actual.put_pixel(9, 15, BLACK);
        let (text, masks) = ([region(0, 0, 16, 16)], [region(0, 0, 16, 15)]);
        let regions = PixelRegions {
            masks: &masks,
            text: &text,
            text_policy: Some(TEXT),
        };
        let text = compare(&baseline, &actual, &regions).analysis.text.unwrap();
        assert_eq!((text.pixels, text.diff_pixels), (16, 2));
        let worst = text.worst_tile.unwrap();
        assert_eq!((worst.pixels, worst.diff_pixels), (256, 2));
        assert!(worst.diff_ratio() <= TEXT.max_diff_ratio);
    }

    /// A region beyond the image is rejected on either axis (callers clip them first), before
    /// any pixel is compared.
    #[test]
    fn test_regions_beyond_the_image_panic_on_both_axes() {
        let image = ImageBuffer::from_pixel(8, 8, WHITE);
        for text in [
            region(4, 0, 5, 1),
            region(0, 4, 1, 5),
            region(0, u32::MAX, 1, 2),
        ] {
            let text = [text];
            let regions = PixelRegions {
                masks: &[],
                text: &text,
                text_policy: Some(TEXT),
            };
            let panic = std::panic::catch_unwind(|| compare(&image, &image, &regions));
            assert!(panic.is_err(), "{text:?}");
        }
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
