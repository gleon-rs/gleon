//! Pixel-by-pixel image comparison, with masked and text regions.
//!
//! Every pixel of a frame is in one class: masked (never compared, not counted), text (compared
//! byte for byte, and passing while no [`TEXT_TILE`]-pixel square tile of a text region has more
//! than the text tolerance's share of differing pixels) or strict (compared byte for byte). Masks
//! win over text. Text is what operating systems draw differently (their font engines, hinting);
//! everything else of a test frame renders the same everywhere.

use image::RgbaImage;

use crate::{Pixels, par, ssim::Region};

/// Side of the square tiles a text region is judged in: every square of this side inside the
/// region, wherever it starts.
///
/// A share of differing pixels per tile reacts to how they cluster: a changed word is a dense
/// cluster, rasterization noise is spread thin, and a share of a whole long paragraph would
/// dilute the changed word.
pub const TEXT_TILE: u32 = 16;

/// The rectangles of a frame the pixel comparison treats apart; masks win over text.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PixelRegions<'a> {
    /// Pixels never compared nor counted (`ignoreRegions`, mask rules).
    pub masks: &'a [Region],
    /// Text, compared under [`Self::text_tolerance`]; strictly without one.
    pub text: &'a [Region],
    /// The largest share of differing pixels in any tile of text, `[0, 1]` (1: text never
    /// fails).
    pub text_tolerance: Option<f64>,
}

impl PixelRegions<'_> {
    /// No masks and no text: every pixel is compared strictly.
    pub const NONE: PixelRegions<'static> = PixelRegions {
        masks: &[],
        text: &[],
        text_tolerance: None,
    };

    /// Whether every pixel is compared strictly.
    #[must_use]
    pub const fn is_none(&self) -> bool {
        self.masks.is_empty() && (self.text.is_empty() || self.text_tolerance.is_none())
    }
}

/// A tile of a text region with its pixels and the text pixels that differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextTile {
    /// The tile, inside its text region ([`TEXT_TILE`] square, or the region's size where it is
    /// smaller).
    pub region: Region,
    /// Its pixels: text, and masked ones (which count as equal).
    pub pixels: u64,
    /// Its text pixels that differ.
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
    /// Text pixels that differ.
    pub diff_pixels: u64,
    /// The tile with the largest share of differing pixels (the first of equal ones); `None`
    /// when no text pixel differs.
    pub worst_tile: Option<TextTile>,
}

/// What [`compare`] measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelAnalysis {
    /// Pixels compared strictly: neither masked nor text under a text tolerance.
    pub checked_pixels: u64,
    /// Strictly compared pixels whose RGBA bytes differ.
    pub diff_pixels: u64,
    /// The text regions, when a text tolerance applied to some.
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
    /// Whether a tile of text has more differing pixels than the text tolerance allows.
    text_fails: bool,
}

impl Compared {
    /// Whether the text is within its tolerance (no text: it is).
    #[must_use]
    pub const fn text_passes(&self) -> bool {
        !self.text_fails
    }

    /// The diff visualization of the compared `baseline` and `actual`: strict differences
    /// magenta, differing text orange when the text failed its tolerance, everything else (text
    /// within its tolerance too) the darkened baseline.
    ///
    /// # Panics
    /// Panics if the images are not the compared ones (another size).
    #[must_use]
    pub fn diff_image<'a>(
        &self,
        baseline: impl Into<Pixels<'a>>,
        actual: impl Into<Pixels<'a>>,
    ) -> RgbaImage {
        let (baseline, actual) = (baseline.into(), actual.into());
        let (width, height) = same_size(baseline, actual);
        let pixels_per_row = (width as usize).max(1);
        if let Some(classes) = &self.classes {
            assert_eq!(
                classes.len(),
                width as usize * height as usize,
                "Image dimensions must match the comparison"
            );
        }
        let mut diff = vec![0u8; baseline.raw().len()];
        par::for_each_chunk_mut(&mut diff, pixels_per_row * 4, |row, out| {
            let start = row * pixels_per_row;
            let rows = start * 4..(start + pixels_per_row) * 4;
            let (expected, found) = (&baseline.raw()[rows.clone()], &actual.raw()[rows]);
            let pixels = out.as_chunks_mut::<4>().0.iter_mut().zip(
                expected
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .zip(found.as_chunks::<4>().0),
            );
            for (i, (pixel, (expected, found))) in pixels.enumerate() {
                let class = self.classes.as_ref().map_or_else(
                    || {
                        if expected == found {
                            Class::Strict
                        } else {
                            Class::StrictDiff
                        }
                    },
                    |classes| classes[start + i],
                );
                *pixel = match class {
                    Class::StrictDiff => MAGENTA,
                    Class::TextDiff if self.text_fails => ORANGE,
                    Class::Strict | Class::Text | Class::TextDiff | Class::Masked => {
                        let [r, g, b, a] = *expected;
                        [r / 2, g / 2, b / 2, a]
                    }
                };
            }
        });
        #[expect(
            clippy::expect_used,
            reason = "`diff` has the length of the baseline's pixels"
        )]
        RgbaImage::from_raw(width, height, diff).expect("the diff has width * height * 4 bytes")
    }
}

/// The size of `baseline` and `actual`.
///
/// # Panics
/// Panics if they differ.
fn same_size(baseline: Pixels<'_>, actual: Pixels<'_>) -> (u32, u32) {
    assert_eq!(
        baseline.dimensions(),
        actual.dimensions(),
        "Image dimensions must match for a pixel comparison"
    );
    baseline.dimensions()
}

/// Compares `baseline` and `actual` (of the same size) pixel by pixel under `regions`.
///
/// # Panics
/// Panics if the images differ in size or a region reaches beyond them.
#[must_use]
pub fn compare<'a>(
    baseline: impl Into<Pixels<'a>>,
    actual: impl Into<Pixels<'a>>,
    regions: &PixelRegions<'_>,
) -> Compared {
    let (baseline, actual) = (baseline.into(), actual.into());
    if regions.is_none() {
        let (width, height) = same_size(baseline, actual);
        let checked_pixels = u64::from(width) * u64::from(height);
        return Compared {
            analysis: PixelAnalysis {
                checked_pixels,
                diff_pixels: count_mismatched_pixels(baseline, actual),
                text: None,
            },
            classes: None,
            text_fails: false,
        };
    }
    let (classes, analysis) = classify(baseline, actual, regions);
    // Every tile of text within the share: a dense cluster (a changed word) fails, noise spread
    // over the text passes.
    let text_fails = analysis
        .text
        .and_then(|text| text.worst_tile)
        .zip(regions.text_tolerance)
        .is_some_and(|(tile, tolerance)| tile.diff_ratio() > tolerance);
    Compared {
        analysis,
        classes: Some(classes),
        text_fails,
    }
}

const MAGENTA: [u8; 4] = [255, 0, 255, 255];
const ORANGE: [u8; 4] = [255, 165, 0, 255];

/// The class of every pixel (row-major) and the analysis.
fn classify(
    baseline: Pixels<'_>,
    actual: Pixels<'_>,
    regions: &PixelRegions<'_>,
) -> (Vec<Class>, PixelAnalysis) {
    let (image_width, image_height) = same_size(baseline, actual);
    let has_text = regions.text_tolerance.is_some() && !regions.text.is_empty();
    let width = image_width as usize;
    let mut classes = region_classes(image_width, image_height, regions, has_text);
    let mut analysis = PixelAnalysis {
        checked_pixels: 0,
        diff_pixels: 0,
        text: None,
    };
    let (mut text_pixels, mut text_diff) = (0, 0);
    let rows = classes
        .chunks_exact_mut(width.max(1))
        .zip(baseline.raw().chunks_exact(width.max(1) * 4))
        .zip(actual.raw().chunks_exact(width.max(1) * 4));
    for ((classes, expected), found) in rows {
        // An equal row (a whole passing frame, most rows of a failing one) is one `memcmp`: its
        // pixels keep their classes and are only counted.
        if expected == found {
            analysis.checked_pixels += count(classes, Class::Strict);
            text_pixels += count(classes, Class::Text);
            continue;
        }
        let pairs = expected
            .as_chunks::<4>()
            .0
            .iter()
            .zip(found.as_chunks::<4>().0);
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
                    if expected != found {
                        text_diff += 1;
                        *class = Class::TextDiff;
                    }
                }
                Class::StrictDiff | Class::TextDiff | Class::Masked => {}
            }
        }
    }
    if has_text {
        analysis.text = Some(TextAnalysis {
            pixels: text_pixels,
            diff_pixels: text_diff,
            // Without a differing text pixel every tile is at 0%: no scan needed.
            worst_tile: if text_diff > 0 {
                worst_tile(&classes, width, regions.text)
            } else {
                None
            },
        });
    }
    (classes, analysis)
}

/// The class of every pixel (row-major) by `regions` alone: masks over text (when `has_text`)
/// over strict.
///
/// # Panics
/// Panics if a region reaches beyond the `image_width` x `image_height` image.
fn region_classes(
    image_width: u32,
    image_height: u32,
    regions: &PixelRegions<'_>,
    has_text: bool,
) -> Vec<Class> {
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
    let width = image_width as usize;
    let mut classes = vec![Class::Strict; width * image_height as usize];
    let mut mark = |region: &Region, class: Class| {
        let rows = region.y as usize * width..(region.y + region.height) as usize * width;
        let columns = region.x as usize..(region.x + region.width) as usize;
        #[expect(
            clippy::expect_used,
            reason = "the regions were checked to lie inside the image"
        )]
        let block = classes
            .get_mut(rows)
            .expect("a region lies inside the image");
        if columns.len() == width {
            // Whole rows are one contiguous block: a single fill.
            block.fill(class);
        } else {
            for row in block.chunks_exact_mut(width) {
                #[expect(
                    clippy::expect_used,
                    reason = "the regions were checked to lie inside the image"
                )]
                row.get_mut(columns.clone())
                    .expect("a region lies inside the image")
                    .fill(class);
            }
        }
    };
    if has_text {
        regions
            .text
            .iter()
            .for_each(|region| mark(region, Class::Text));
    }
    regions
        .masks
        .iter()
        .for_each(|region| mark(region, Class::Masked));

    classes
}

/// The tile of `text` with the largest share of differing text pixels (the first of equal ones):
/// every [`TEXT_TILE`]-pixel square inside a region (the region's own size when it is smaller),
/// so a cluster is judged whole wherever it falls and no thin strip at a region's edge makes a
/// tile of its own. Masked pixels of a tile count as equal, so a mask over most of a tile cannot
/// turn one noisy pixel into a large share. `None` when no text pixel differs.
///
/// The column sums of the tile's rows slide down a region and the tile slides along them, so
/// this takes one pass over each region. `text` lies inside the image ([`classify`]).
fn worst_tile(classes: &[Class], width: usize, text: &[Region]) -> Option<TextTile> {
    let mut worst: Option<TextTile> = None;
    // Per column of a region: its differing text pixels in the tile's rows.
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
        let mut consider = |x: u32, y: u32, diff_pixels: u64| {
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
            let is_worse = diff_pixels > 0
                && worst.is_none_or(|worst| {
                    tile.diff_pixels * worst.pixels > worst.diff_pixels * tile.pixels
                });
            if is_worse {
                worst = Some(tile);
            }
        };
        columns.clear();
        columns.resize(region.width as usize, 0);
        for y in region.y..region.y + tile_height {
            add_row(&mut columns, row(y), true);
        }
        for top in region.y..=region.y + region.height - tile_height {
            if top > region.y {
                add_row(&mut columns, row(top - 1), false);
                add_row(&mut columns, row(top + tile_height - 1), true);
            }
            let mut sum = columns.iter().take(tile_width as usize).sum();
            consider(region.x, top, sum);
            let slides = columns.iter().zip(columns.iter().skip(tile_width as usize));
            for (left, (leaving, entering)) in (region.x + 1..).zip(slides) {
                sum = sum - leaving + entering;
                consider(left, top, sum);
            }
        }
    }
    worst
}

/// The pixels of `classes` in `class`.
fn count(classes: &[Class], class: Class) -> u64 {
    classes.iter().filter(|&&other| other == class).count() as u64
}

/// Adds (or, without `add`, removes) the differing text pixels of `row` to the sums of its
/// `columns` (`row` is as long as `columns`: a row of the region).
fn add_row(columns: &mut [u64], row: &[Class], add: bool) {
    for (column, class) in columns.iter_mut().zip(row) {
        if *class == Class::TextDiff {
            *column = if add { *column + 1 } else { *column - 1 };
        }
    }
}

/// The pixels whose RGBA bytes differ; equal images take one `memcmp`.
///
/// # Panics
/// Panics if `baseline` and `actual` do not have identical dimensions.
#[must_use]
pub fn count_mismatched_pixels<'a>(
    baseline: impl Into<Pixels<'a>>,
    actual: impl Into<Pixels<'a>>,
) -> u64 {
    let (baseline, actual) = (baseline.into(), actual.into());
    same_size(baseline, actual);
    if baseline.raw() == actual.raw() {
        return 0;
    }
    let pairs = baseline
        .raw()
        .as_chunks::<4>()
        .0
        .iter()
        .zip(actual.raw().as_chunks::<4>().0);
    pairs.filter(|(expected, found)| expected != found).count() as u64
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

        let compared = compare(&img1, &img2, &PixelRegions::NONE);
        assert_eq!(compared.analysis.diff_pixels, 0);
        // Matching pixels should be darkened: 255 / 2 = 127
        let diff_img = compared.diff_image(&img1, &img2);
        assert_eq!(*diff_img.get_pixel(0, 0), Rgba([127, 0, 0, 255]));

        assert_eq!(count_mismatched_pixels(&img1, &img2), 0);
    }

    #[test]
    fn test_compare_pixels_mismatch() {
        let img1 = ImageBuffer::from_pixel(10, 10, Rgba([255, 0, 0, 255]));
        let mut img2 = ImageBuffer::from_pixel(10, 10, Rgba([255, 0, 0, 255]));
        img2.put_pixel(5, 5, Rgba([0, 255, 0, 255]));

        let compared = compare(&img1, &img2, &PixelRegions::NONE);
        assert_eq!(compared.analysis.diff_pixels, 1);
        let diff_img = compared.diff_image(&img1, &img2);
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

    /// A text tolerance: at most 10% of a tile may differ.
    const TEXT: f64 = 0.1;

    /// Masks win over text, text under a tolerance is apart from the strict pixels, and both are
    /// compared byte for byte.
    #[test]
    fn test_classes_and_mask_priority() {
        let baseline = ImageBuffer::from_pixel(64, 32, WHITE);
        let mut actual = baseline.clone();
        actual.put_pixel(1, 1, BLACK); // masked
        actual.put_pixel(14, 1, BLACK); // text
        actual.put_pixel(15, 1, GRAY); // text: any byte that differs counts
        actual.put_pixel(40, 20, Rgba([255, 255, 255, 254])); // strict: alpha counts
        let (masks, text) = ([region(0, 0, 12, 4)], [region(8, 0, 24, 16)]);
        let regions = PixelRegions {
            masks: &masks,
            text: &text,
            text_tolerance: Some(TEXT),
        };

        let analysis = compare(&baseline, &actual, &regions).analysis;
        // 64x32 = 2048 pixels; text 24x16 = 384, of which 4x4 masked; masks 12x4 = 48.
        assert_eq!(analysis.checked_pixels, 2048 - 48 - (384 - 16));
        assert_eq!(analysis.diff_pixels, 1);
        let text = analysis.text.unwrap();
        assert_eq!((text.pixels, text.diff_pixels), (384 - 16, 2));
        let worst = text.worst_tile.unwrap();
        // Its 16 masked pixels count as equal.
        assert_eq!(worst.region, region(8, 0, 16, 16));
        assert_eq!((worst.pixels, worst.diff_pixels), (256, 2));

        // Text within its tolerance is no difference to show: darkened, like a mask.
        let compared = compare(&baseline, &actual, &regions);
        assert!(compared.text_passes());
        let diff = compared.diff_image(&baseline, &actual);
        assert_eq!(diff.get_pixel(14, 1).0, [127, 127, 127, 255]);
        assert_eq!(diff.get_pixel(40, 20).0, MAGENTA);
        assert_eq!(
            diff.get_pixel(1, 1).0,
            [127, 127, 127, 255],
            "masked: darkened"
        );
        // Text beyond its tolerance shows its differing pixels.
        let failing = PixelRegions {
            text_tolerance: Some(0.0),
            ..regions
        };
        let compared = compare(&baseline, &actual, &failing);
        assert!(!compared.text_passes());
        let diff = compared.diff_image(&baseline, &actual);
        assert_eq!(diff.get_pixel(14, 1).0, ORANGE);
        assert_eq!(diff.get_pixel(15, 1).0, ORANGE);

        // Without a tolerance text is compared strictly.
        let strict = PixelRegions {
            text_tolerance: None,
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
            text_tolerance: Some(TEXT),
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
        assert!(ratio(clustered) > TEXT, "{clustered:?}");
        assert!(ratio(spread) <= TEXT, "{spread:?}");
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
            text_tolerance: Some(TEXT),
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

        // Text without a differing pixel has no worst tile to point at.
        let same = compare(&baseline, &baseline, &regions)
            .analysis
            .text
            .unwrap();
        assert_eq!((same.diff_pixels, same.worst_tile), (0, None));

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

    /// Regions of whole rows are marked as one block; empty regions (at the image's far edges
    /// too) mark nothing and make no tile.
    #[test]
    fn test_whole_rows_and_empty_regions() {
        let baseline = ImageBuffer::from_pixel(64, 64, WHITE);
        let mut actual = baseline.clone();
        actual.put_pixel(3, 40, BLACK);
        let text = [
            region(64, 64, 0, 0),
            region(64, 0, 0, 64),
            region(0, 64, 64, 0),
            region(0, 32, 64, 16),
        ];
        let masks = [region(0, 0, 64, 8)];
        let regions = PixelRegions {
            masks: &masks,
            text: &text,
            text_tolerance: Some(TEXT),
        };
        let (classes, analysis) = classify((&baseline).into(), (&actual).into(), &regions);
        let class_at = |x: usize, y: usize| classes[y * 64 + x];
        assert_eq!(
            [
                class_at(63, 7),
                class_at(0, 8),
                class_at(0, 47),
                class_at(3, 40)
            ],
            [Class::Masked, Class::Strict, Class::Text, Class::TextDiff]
        );
        assert_eq!(
            (analysis.checked_pixels, analysis.diff_pixels),
            (64 * 40, 0)
        );
        let text = analysis.text.unwrap();
        assert_eq!((text.pixels, text.diff_pixels), (64 * 16, 1));
        let worst = text.worst_tile.unwrap();
        assert_eq!(
            (worst.region, worst.diff_pixels),
            (region(0, 32, 16, 16), 1)
        );
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
            text_tolerance: Some(TEXT),
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
            text_tolerance: Some(TEXT),
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
            text_tolerance: Some(TEXT),
        };
        let text = compare(&baseline, &actual, &regions).analysis.text.unwrap();
        assert_eq!((text.pixels, text.diff_pixels), (16, 2));
        let worst = text.worst_tile.unwrap();
        assert_eq!((worst.pixels, worst.diff_pixels), (256, 2));
        assert!(worst.diff_ratio() <= TEXT);
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
                text_tolerance: Some(TEXT),
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
        let _ = compare(&img1, &img2, &PixelRegions::NONE);
    }

    #[test]
    #[should_panic(expected = "Image dimensions must match")]
    fn test_count_mismatched_pixels_unequal_dimensions_panics() {
        let img1 = ImageBuffer::from_pixel(10, 10, Rgba([255, 0, 0, 255]));
        let img2 = ImageBuffer::from_pixel(10, 20, Rgba([255, 0, 0, 255]));
        let _ = count_mismatched_pixels(&img1, &img2);
    }
}
