//! Pixel-by-pixel image comparison, with masked and text regions.
//!
//! Every pixel of a frame is in one class: masked (never compared, not counted), text (compared
//! byte for byte, and passing while no [`TEXT_TILE`]-pixel square tile of a text region has more
//! than the text tolerance's share of differing pixels) or strict (compared byte for byte). Masks
//! win over text. Text is what operating systems draw differently (their font engines, hinting);
//! everything else of a test frame renders the same everywhere.
//!
//! [`PixelOptions`] can let some differing pixels pass (counted as equal): small channel deltas,
//! anti-aliased pixels and, outside text, pixels on the baseline's edges.

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

/// Which differing pixels the pixel mode lets pass, counted as equal ([`Self::STRICT`]: none).
///
/// For rendering noise of shapes (GPU drift, anti-aliasing, sub-pixel geometry) in a pixel or
/// exact comparison; not a substitute for SSIM mode, and text keeps its own tolerance (the edge
/// mask never applies to it).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PixelOptions {
    /// A pixel passes when no RGBA byte differs by more than this (0: off).
    pub channel_tolerance: u8,
    /// A pixel passes when it looks anti-aliased in either image (the detection of pixelmatch).
    pub anti_alias: bool,
    /// A pixel outside text passes when the Sobel gradient of the baseline's luma there (8-bit
    /// units, capped at 255) exceeds this (0: off; 255 hides nothing). Every change on edges
    /// passes too: a missing glyph or small icon, a 1px move (see `tests/ssim_corpus.rs`).
    pub edge_threshold: u8,
}

impl PixelOptions {
    /// Every differing pixel is a difference.
    pub const STRICT: Self = Self {
        channel_tolerance: 0,
        anti_alias: false,
        edge_threshold: 0,
    };

    /// Whether every differing pixel is a difference.
    #[must_use]
    pub const fn is_strict(&self) -> bool {
        self.channel_tolerance == 0
            && !self.anti_alias
            && (self.edge_threshold == 0 || self.edge_threshold == u8::MAX)
    }
}

/// A tile of a text region with its pixels and the text pixels that differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextTile {
    /// The tile's part inside its text region ([`TEXT_TILE`] square, or the region's size where
    /// it is smaller).
    pub region: Region,
    /// The pixels of the [`TEXT_TILE`] square, always: masked ones and those outside a region
    /// thinner than a tile count as equal, so a thin or edge-clipped region (a 4x1 strip) never
    /// turns one differing pixel into a large share.
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
    /// Strictly compared pixels whose RGBA bytes differ (beyond the [`PixelOptions`]).
    pub diff_pixels: u64,
    /// Differing pixels (strict or text) the channel tolerance or the anti-aliasing detection
    /// let pass, counted as equal.
    pub tolerated_pixels: u64,
    /// Differing strict pixels on the baseline's edges the edge mask let pass, counted as equal.
    pub edge_pixels: u64,
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
    /// A differing pixel the channel tolerance or the anti-aliasing detection let pass.
    Tolerated,
    /// A differing pixel on an edge of the baseline, hidden by the edge mask.
    Edge,
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
    /// magenta, differing text orange when the text failed its tolerance, differing pixels the
    /// [`PixelOptions`] let pass yellow (tolerated) or cyan (edges), everything else (text within
    /// its tolerance too) the darkened baseline.
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
                    Class::Tolerated => YELLOW,
                    Class::Edge => CYAN,
                    Class::Strict | Class::Text | Class::TextDiff | Class::Masked => {
                        darken(*expected)
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

impl Compared {
    /// Repaints the text regions of an SSIM `diff` (whose analysis had them painted out) from
    /// `baseline`: faded like the rest of it, differing text yellow, or orange when the text
    /// failed its tolerance. Nothing without text.
    pub(crate) fn paint_text(&self, diff: &mut RgbaImage, baseline: Pixels<'_>) {
        let Some(classes) = &self.classes else {
            return;
        };
        let pixels = diff
            .as_mut()
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(classes)
            .zip(baseline.raw().as_chunks::<4>().0);
        for ((pixel, class), expected) in pixels {
            *pixel = match class {
                Class::TextDiff if self.text_fails => ORANGE,
                Class::TextDiff => YELLOW,
                Class::Text => crate::ssim::faded(*expected),
                Class::Strict
                | Class::StrictDiff
                | Class::Masked
                | Class::Tolerated
                | Class::Edge => continue,
            };
        }
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

/// Compares `baseline` and `actual` (of the same size) pixel by pixel under `regions` and
/// `options`.
///
/// # Panics
/// Panics if the images differ in size or a region reaches beyond them.
#[must_use]
pub fn compare<'a>(
    baseline: impl Into<Pixels<'a>>,
    actual: impl Into<Pixels<'a>>,
    regions: &PixelRegions<'_>,
    options: &PixelOptions,
) -> Compared {
    let (baseline, actual) = (baseline.into(), actual.into());
    // Every pixel strict, and no option or equal frames (a pass with options is one `memcmp`).
    if regions.is_none() && (options.is_strict() || baseline.raw() == actual.raw()) {
        let (width, height) = same_size(baseline, actual);
        let checked_pixels = u64::from(width) * u64::from(height);
        return Compared {
            analysis: PixelAnalysis {
                checked_pixels,
                diff_pixels: if options.is_strict() {
                    count_mismatched_pixels(baseline, actual)
                } else {
                    0
                },
                tolerated_pixels: 0,
                edge_pixels: 0,
                text: None,
            },
            classes: None,
            text_fails: false,
        };
    }
    let (classes, analysis) = classify(baseline, actual, regions, *options);
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
/// Differing pixels the [`PixelOptions`] tolerated (the yellow SSIM uses for tolerated changes).
const YELLOW: [u8; 4] = [255, 200, 0, 255];
/// Differing pixels on the baseline's edges, hidden by the edge mask.
const CYAN: [u8; 4] = [0, 200, 255, 255];
/// Stripes over the part of a [`dimension_diff`] only the baseline covers.
const BLUE: [u8; 4] = [0, 112, 255, 255];
/// Stripes over the part of a [`dimension_diff`] only the actual image covers.
const GREEN: [u8; 4] = [0, 200, 83, 255];
/// The corner of a [`dimension_diff`] neither image covers.
const UNCOVERED: [u8; 4] = [64, 64, 64, 255];

/// A pixel of the darkened image the diffs draw on.
const fn darken([r, g, b, a]: [u8; 4]) -> [u8; 4] {
    [r / 2, g / 2, b / 2, a]
}

/// Pixels counted by [`classify`], per block and in total.
#[derive(Debug, Clone, Copy, Default)]
struct Counts {
    /// Strictly compared pixels.
    checked: u64,
    /// Strictly compared pixels that differ beyond the options.
    diff: u64,
    /// Text pixels.
    text: u64,
    /// Text pixels that differ beyond the options.
    text_diff: u64,
    /// Differing pixels the channel tolerance or the anti-aliasing detection let pass.
    tolerated: u64,
    /// Differing pixels on the baseline's edges, hidden by the edge mask.
    edge: u64,
}

impl Counts {
    const fn plus(self, other: Self) -> Self {
        Self {
            checked: self.checked + other.checked,
            diff: self.diff + other.diff,
            text: self.text + other.text,
            text_diff: self.text_diff + other.text_diff,
            tolerated: self.tolerated + other.tolerated,
            edge: self.edge + other.edge,
        }
    }
}

/// The class of every pixel (row-major) and the analysis.
fn classify(
    baseline: Pixels<'_>,
    actual: Pixels<'_>,
    regions: &PixelRegions<'_>,
    options: PixelOptions,
) -> (Vec<Class>, PixelAnalysis) {
    let (image_width, image_height) = same_size(baseline, actual);
    let has_text = regions.text_tolerance.is_some() && !regions.text.is_empty();
    let width = image_width as usize;
    let mut classes = region_classes(image_width, image_height, regions, has_text);
    let width = width.max(1);
    let judge = Judge {
        baseline,
        actual,
        options,
    };
    // Blocks of whole rows of about `BLOCK` bytes: one task each with `parallel`.
    let block = width * (BLOCK / 4 / width).max(1);
    let counts = par::map_chunks_mut(
        &mut classes,
        block,
        |index, classes| {
            let start = index * block * 4;
            let end = start + classes.len() * 4;
            let rows = classes
                .chunks_exact_mut(width)
                .zip(baseline.raw()[start..end].chunks_exact(width * 4))
                .zip(actual.raw()[start..end].chunks_exact(width * 4));
            let mut counts = Counts::default();
            for (row, ((classes, expected), found)) in (index * block / width..).zip(rows) {
                // An equal row (a whole passing frame, most rows of a failing one) is one
                // `memcmp`: its pixels keep their classes and are only counted.
                if expected == found {
                    counts.checked += count(classes, Class::Strict);
                    counts.text += count(classes, Class::Text);
                    continue;
                }
                let pairs = expected
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .zip(found.as_chunks::<4>().0);
                for (column, (class, (expected, found))) in
                    classes.iter_mut().zip(pairs).enumerate()
                {
                    let is_text = match class {
                        Class::Strict => {
                            counts.checked += 1;
                            false
                        }
                        Class::Text => {
                            counts.text += 1;
                            true
                        }
                        Class::StrictDiff
                        | Class::TextDiff
                        | Class::Masked
                        | Class::Tolerated
                        | Class::Edge => continue,
                    };
                    if expected == found {
                        continue;
                    }
                    *class = match judge.tolerance(column, row, *expected, *found, is_text) {
                        Some(Class::Edge) => {
                            counts.edge += 1;
                            Class::Edge
                        }
                        Some(tolerated) => {
                            counts.tolerated += 1;
                            tolerated
                        }
                        None if is_text => {
                            counts.text_diff += 1;
                            Class::TextDiff
                        }
                        None => {
                            counts.diff += 1;
                            Class::StrictDiff
                        }
                    };
                }
            }
            counts
        },
        Counts::default,
        Counts::plus,
    );
    let mut analysis = PixelAnalysis {
        checked_pixels: counts.checked,
        diff_pixels: counts.diff,
        tolerated_pixels: counts.tolerated,
        edge_pixels: counts.edge,
        text: None,
    };
    if has_text {
        analysis.text = Some(TextAnalysis {
            pixels: counts.text,
            diff_pixels: counts.text_diff,
            // Without a differing text pixel every tile is at 0%: no scan needed.
            worst_tile: if counts.text_diff > 0 {
                worst_tile(&classes, width, regions.text)
            } else {
                None
            },
        });
    }
    (classes, analysis)
}

/// Decides whether a differing pixel passes under the [`PixelOptions`], in their order: the
/// channel tolerance, then the anti-aliasing detection, then (outside text) the edge mask.
///
/// Luma is integer arithmetic throughout (Rec. 601 weights 77/150/29 over white, scaled by
/// `255 * 256`), so every platform takes the same decisions bit for bit.
struct Judge<'a> {
    baseline: Pixels<'a>,
    actual: Pixels<'a>,
    options: PixelOptions,
}

impl Judge<'_> {
    /// `Some(Class::Tolerated)`, `Some(Class::Edge)` or `None` (a difference) for the differing
    /// pixel at (`x`, `y`) of `expected` (baseline) and `found` (actual).
    fn tolerance(
        &self,
        x: usize,
        y: usize,
        expected: [u8; 4],
        found: [u8; 4],
        is_text: bool,
    ) -> Option<Class> {
        let PixelOptions {
            channel_tolerance,
            anti_alias,
            edge_threshold,
        } = self.options;
        let within_channels = channel_tolerance > 0
            && expected
                .iter()
                .zip(found)
                .all(|(a, b)| a.abs_diff(b) <= channel_tolerance);
        if within_channels
            || (anti_alias
                && (is_anti_aliased(self.baseline, self.actual, x, y)
                    || is_anti_aliased(self.actual, self.baseline, x, y)))
        {
            return Some(Class::Tolerated);
        }
        (!is_text && is_edge(self.baseline, x, y, edge_threshold)).then_some(Class::Edge)
    }
}

/// The pixel at (`x`, `y`) of `image` (inside it).
fn pixel_at(image: Pixels<'_>, x: usize, y: usize) -> [u8; 4] {
    let start = (y * image.width as usize + x) * 4;
    let mut pixel = [0; 4];
    pixel.copy_from_slice(&image.raw()[start..start + 4]);
    pixel
}

/// The luma of `pixel` composited over white, in units of `1 / (255 * 256)` of 8-bit luma.
fn luma([r, g, b, a]: [u8; 4]) -> i64 {
    let (alpha, rest) = (i64::from(a), 255 - i64::from(a));
    let over_white = |channel: u8| i64::from(channel) * alpha + 255 * rest;
    77 * over_white(r) + 150 * over_white(g) + 29 * over_white(b)
}

/// The 3x3 neighborhood of (`x`, `y`) in a `width` x `height` image, the center excluded and
/// clipped at the image's edges, column by column (the order of pixelmatch), and whether the center
/// lies on an edge of the image.
fn neighbors(
    x: usize,
    y: usize,
    width: usize,
    height: usize,
) -> (impl Iterator<Item = (usize, usize)>, bool) {
    let (x0, x2) = (x.saturating_sub(1), (x + 1).min(width - 1));
    let (y0, y2) = (y.saturating_sub(1), (y + 1).min(height - 1));
    let on_border = x == x0 || x == x2 || y == y0 || y == y2;
    let around = (x0..=x2)
        .flat_map(move |nx| (y0..=y2).map(move |ny| (nx, ny)))
        .filter(move |&point| point != (x, y));
    (around, on_border)
}

/// Whether the pixel at (`x`, `y`) of `image` looks anti-aliased (`antialiased` of pixelmatch):
/// at most two neighbors of its luma, both a darker and a brighter one, and the darkest or the
/// brightest of them sits in a flat area (3+ identical neighbors) in `image` and `other` alike.
fn is_anti_aliased(image: Pixels<'_>, other: Pixels<'_>, x: usize, y: usize) -> bool {
    let (width, height) = (image.width as usize, image.height as usize);
    let center = luma(pixel_at(image, x, y));
    let (around, on_border) = neighbors(x, y, width, height);
    let mut zeroes = usize::from(on_border);
    let (mut darkest, mut brightest) = ((0, None), (0, None));
    for (nx, ny) in around {
        let delta = center - luma(pixel_at(image, nx, ny));
        if delta == 0 {
            zeroes += 1;
            if zeroes > 2 {
                return false;
            }
        } else if delta < darkest.0 {
            darkest = (delta, Some((nx, ny)));
        } else if delta > brightest.0 {
            brightest = (delta, Some((nx, ny)));
        }
    }
    let flat_in_both = |(nx, ny): (usize, usize)| {
        has_many_siblings(image, nx, ny) && has_many_siblings(other, nx, ny)
    };
    match (darkest.1, brightest.1) {
        (Some(darkest), Some(brightest)) => flat_in_both(darkest) || flat_in_both(brightest),
        _ => false,
    }
}

/// Whether the pixel at (`x`, `y`) of `image` has 3+ neighbors of identical bytes (an image edge
/// counts as one).
fn has_many_siblings(image: Pixels<'_>, x: usize, y: usize) -> bool {
    let center = pixel_at(image, x, y);
    let (around, on_border) = neighbors(x, y, image.width as usize, image.height as usize);
    let same = around
        .filter(|&(nx, ny)| pixel_at(image, nx, ny) == center)
        .take(3)
        .count();
    same + usize::from(on_border) > 2
}

/// Whether (`x`, `y`) lies on an edge of `baseline`: its Sobel gradient of luma (8-bit units,
/// neighbors outside the image repeat the border, capped at 255) exceeds `threshold`; never for
/// threshold 0 (off) or 255.
fn is_edge(baseline: Pixels<'_>, x: usize, y: usize, threshold: u8) -> bool {
    if threshold == 0 || threshold == u8::MAX {
        return false;
    }
    let (width, height) = (baseline.width as usize, baseline.height as usize);
    let at = |dx: isize, dy: isize| {
        let nx = x.saturating_add_signed(dx).min(width - 1);
        let ny = y.saturating_add_signed(dy).min(height - 1);
        luma(pixel_at(baseline, nx, ny))
    };
    let gx = at(1, -1) + 2 * at(1, 0) + at(1, 1) - at(-1, -1) - 2 * at(-1, 0) - at(-1, 1);
    let gy = at(-1, 1) + 2 * at(0, 1) + at(1, 1) - at(-1, -1) - 2 * at(0, -1) - at(1, -1);
    // Compared squared, in the units of luma: no square root, no float.
    let limit = i64::from(threshold) * 255 * 256;
    gx * gx + gy * gy > limit * limit
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
/// tile of its own. Masked pixels of a tile count as equal, and so do the pixels of the square
/// outside a region thinner than a tile (a small or edge-clipped region): neither can turn one
/// noisy pixel into a large share. `None` when no text pixel differs.
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
                pixels: u64::from(TEXT_TILE) * u64::from(TEXT_TILE),
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

/// Bytes per task of a pixel pass with `parallel` (whole pixels, or whole rows): enough work to
/// outweigh a task, and an equal block of [`count_mismatched_pixels`] is one `memcmp`.
const BLOCK: usize = 1 << 20;

/// The pixels whose RGBA bytes differ; equal images take one `memcmp`.
///
/// # Panics
/// Panics if `baseline` and `actual` do not have identical dimensions.
#[must_use]
fn count_mismatched_pixels(baseline: Pixels<'_>, actual: Pixels<'_>) -> u64 {
    same_size(baseline, actual);
    if baseline.raw() == actual.raw() {
        return 0;
    }
    let found = actual.raw();
    par::map_chunks(
        baseline.raw(),
        BLOCK,
        |index, expected| {
            let start = index * BLOCK;
            let found = &found[start..start + expected.len()];
            if expected == found {
                return 0;
            }
            let pairs = expected
                .as_chunks::<4>()
                .0
                .iter()
                .zip(found.as_chunks::<4>().0);
            pairs.filter(|(expected, found)| expected != found).count() as u64
        },
        || 0,
        |a, b| a + b,
    )
}

/// The diff visualization of a `baseline` and an `actual` image of different sizes.
///
/// The canvas has the larger width and the larger height: the area both cover is compared like
/// [`compare`] without regions (differences magenta on the darkened baseline), the area only the
/// baseline covers is darkened with blue diagonal stripes, the area only the actual image covers
/// with green ones, and the corner neither covers is gray.
///
/// `None` when the canvas is over the decoding budget ([`crate::decode::fits_budget`]): two
/// images within it can still span a canvas beyond it (16384x1 and 1x16384).
#[must_use]
pub fn dimension_diff<'a>(
    baseline: impl Into<Pixels<'a>>,
    actual: impl Into<Pixels<'a>>,
) -> Option<RgbaImage> {
    let (baseline, actual) = (baseline.into(), actual.into());
    let ((baseline_width, baseline_height), (actual_width, actual_height)) =
        (baseline.dimensions(), actual.dimensions());
    let (width, height) = (
        baseline_width.max(actual_width),
        baseline_height.max(actual_height),
    );
    if !crate::decode::fits_budget(width, height) || width == 0 || height == 0 {
        return None;
    }
    let row_bytes = width as usize * 4;
    // The pixels of row `y` of `image`; empty below its last row.
    let row_of = |image: Pixels<'a>, image_width: u32, image_height: u32, y: usize| {
        let bytes = image_width as usize * 4;
        if y < image_height as usize {
            image.raw()[y * bytes..(y + 1) * bytes].as_chunks::<4>().0
        } else {
            &[]
        }
    };
    let mut diff = vec![0u8; row_bytes * height as usize];
    par::for_each_chunk_mut(&mut diff, row_bytes, |y, out| {
        let expected = row_of(baseline, baseline_width, baseline_height, y);
        let found = row_of(actual, actual_width, actual_height, y);
        for (x, pixel) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let is_stripe = (x + y) % 8 < 4;
            *pixel = match (expected.get(x).copied(), found.get(x).copied()) {
                (Some(expected), Some(found)) if expected == found => darken(expected),
                (Some(_), Some(_)) => MAGENTA,
                (Some(_), None) if is_stripe => BLUE,
                (None, Some(_)) if is_stripe => GREEN,
                (Some(only), None) | (None, Some(only)) => darken(only),
                (None, None) => UNCOVERED,
            };
        }
    });
    RgbaImage::from_raw(width, height, diff)
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
    fn test_identical_images_are_darkened_in_the_diff() {
        let img1 = ImageBuffer::from_pixel(10, 10, Rgba([255, 0, 0, 255]));
        let img2 = ImageBuffer::from_pixel(10, 10, Rgba([255, 0, 0, 255]));

        let compared = compare(&img1, &img2, &PixelRegions::NONE, &PixelOptions::STRICT);
        assert_eq!(compared.analysis.diff_pixels, 0);
        // Matching pixels should be darkened: 255 / 2 = 127
        let diff_img = compared.diff_image(&img1, &img2);
        assert_eq!(*diff_img.get_pixel(0, 0), Rgba([127, 0, 0, 255]));

        assert_eq!(count_mismatched_pixels((&img1).into(), (&img2).into()), 0);
    }

    #[test]
    fn test_strict_differences_are_magenta_in_the_diff() {
        let img1 = ImageBuffer::from_pixel(10, 10, Rgba([255, 0, 0, 255]));
        let mut img2 = ImageBuffer::from_pixel(10, 10, Rgba([255, 0, 0, 255]));
        img2.put_pixel(5, 5, Rgba([0, 255, 0, 255]));

        let compared = compare(&img1, &img2, &PixelRegions::NONE, &PixelOptions::STRICT);
        assert_eq!(compared.analysis.diff_pixels, 1);
        let diff_img = compared.diff_image(&img1, &img2);
        // The mismatched pixel should be magenta
        assert_eq!(*diff_img.get_pixel(5, 5), Rgba([255, 0, 255, 255]));
        // The matching pixel should be darkened
        assert_eq!(*diff_img.get_pixel(0, 0), Rgba([127, 0, 0, 255]));

        assert_eq!(count_mismatched_pixels((&img1).into(), (&img2).into()), 1);

        // Over one block: the equal first block is skipped, the differing last one counted.
        let wide = ImageBuffer::from_pixel(600, 600, Rgba([255, 0, 0, 255]));
        let mut changed = wide.clone();
        changed.put_pixel(599, 599, Rgba([0, 255, 0, 255]));
        assert!(wide.as_raw().len() > BLOCK);
        assert_eq!(
            count_mismatched_pixels((&wide).into(), (&changed).into()),
            1
        );
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

        let analysis = compare(&baseline, &actual, &regions, &PixelOptions::STRICT).analysis;
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
        let compared = compare(&baseline, &actual, &regions, &PixelOptions::STRICT);
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
        let compared = compare(&baseline, &actual, &failing, &PixelOptions::STRICT);
        assert!(!compared.text_passes());
        let diff = compared.diff_image(&baseline, &actual);
        assert_eq!(diff.get_pixel(14, 1).0, ORANGE);
        assert_eq!(diff.get_pixel(15, 1).0, ORANGE);

        // Without a tolerance text is compared strictly.
        let strict = PixelRegions {
            text_tolerance: None,
            ..regions
        };
        let analysis = compare(&baseline, &actual, &strict, &PixelOptions::STRICT).analysis;
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

        let clustered = compare(&baseline, &cluster, &regions, &PixelOptions::STRICT)
            .analysis
            .text
            .unwrap();
        let spread = compare(&baseline, &noise, &regions, &PixelOptions::STRICT)
            .analysis
            .text
            .unwrap();
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
        let worst = compare(&baseline, &actual, &regions, &PixelOptions::STRICT)
            .analysis
            .text
            .unwrap()
            .worst_tile
            .unwrap();
        // The first tile around the pixel that still lies inside the region.
        assert_eq!(worst.region, region(26, 26, 16, 16));
        assert_eq!((worst.pixels, worst.diff_pixels), (256, 1));

        // Text without a differing pixel has no worst tile to point at.
        let same = compare(&baseline, &baseline, &regions, &PixelOptions::STRICT)
            .analysis
            .text
            .unwrap();
        assert_eq!((same.diff_pixels, same.worst_tile), (0, None));

        let all_masked = PixelRegions {
            masks: &text,
            ..regions
        };
        let text = compare(&baseline, &actual, &all_masked, &PixelOptions::STRICT)
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
        let (classes, analysis) = classify(
            (&baseline).into(),
            (&actual).into(),
            &regions,
            PixelOptions::STRICT,
        );
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
            let text = compare(&baseline, &actual, &regions, &PixelOptions::STRICT)
                .analysis
                .text
                .unwrap();
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
        let worst = compare(&baseline, &actual, &regions, &PixelOptions::STRICT)
            .analysis
            .text
            .unwrap()
            .worst_tile
            .unwrap();
        assert_eq!((worst.pixels, worst.diff_pixels), (256, 2), "{worst:?}");

        // A region smaller than a tile is one tile of its own size, judged as a whole tile: the
        // rest of the square counts as equal (one pixel of a 10x6 region is 1 of 256).
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
            &PixelOptions::STRICT,
        )
        .analysis
        .text
        .unwrap()
        .worst_tile
        .unwrap();
        assert_eq!(worst.region, small[0]);
        assert_eq!((worst.pixels, worst.diff_pixels), (256, 1));
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
        let text = compare(&baseline, &actual, &regions, &PixelOptions::STRICT)
            .analysis
            .text
            .unwrap();
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
            let panic = std::panic::catch_unwind(|| {
                compare(&image, &image, &regions, &PixelOptions::STRICT)
            });
            assert!(panic.is_err(), "{text:?}");
        }
    }

    #[test]
    #[should_panic(expected = "Image dimensions must match")]
    fn test_compare_unequal_dimensions_panics() {
        let img1 = ImageBuffer::from_pixel(10, 10, Rgba([255, 0, 0, 255]));
        let img2 = ImageBuffer::from_pixel(20, 10, Rgba([255, 0, 0, 255]));
        let _ = compare(&img1, &img2, &PixelRegions::NONE, &PixelOptions::STRICT);
    }

    #[test]
    #[should_panic(expected = "Image dimensions must match")]
    fn test_count_mismatched_pixels_unequal_dimensions_panics() {
        let img1 = ImageBuffer::from_pixel(10, 10, Rgba([255, 0, 0, 255]));
        let img2 = ImageBuffer::from_pixel(10, 20, Rgba([255, 0, 0, 255]));
        let _ = count_mismatched_pixels((&img1).into(), (&img2).into());
    }

    /// The row fast paths against a per-pixel reference: generated frames (equal rows, changed
    /// rows, widths 0 and 1), masks and text regions anywhere inside, a fixed xorshift sequence.
    #[test]
    fn test_compare_counts_like_a_per_pixel_reference() {
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = |below: u32| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            u32::try_from(state % u64::from(below.max(1))).unwrap()
        };
        for _ in 0..500 {
            let (width, height) = (next(20), 1 + next(12));
            let baseline = ImageBuffer::from_fn(width, height, |x, y| {
                Rgba([u8::try_from((x * 7 + y * 3) % 256).unwrap(), 0, 0, 255])
            });
            let mut actual = baseline.clone();
            for _ in 0..next(4) {
                if width > 0 {
                    actual.put_pixel(next(width), next(height), BLACK);
                }
            }
            let random_regions = |next: &mut dyn FnMut(u32) -> u32| {
                (0..next(3))
                    .map(|_| {
                        let (x, y) = (next(width + 1), next(height + 1));
                        region(x, y, next(width - x + 1), next(height - y + 1))
                    })
                    .collect::<Vec<_>>()
            };
            let masks = random_regions(&mut next);
            let text = random_regions(&mut next);
            let regions = PixelRegions {
                masks: &masks,
                text: &text,
                text_tolerance: Some(TEXT),
            };
            let inside = |regions: &[Region], x: u32, y: u32| {
                regions.iter().any(|r| {
                    (r.x..r.x + r.width).contains(&x) && (r.y..r.y + r.height).contains(&y)
                })
            };
            let (mut checked, mut diff, mut text_pixels, mut text_diff) = (0, 0, 0, 0);
            for (x, y, expected) in baseline.enumerate_pixels() {
                let differs = expected != actual.get_pixel(x, y);
                if inside(&masks, x, y) {
                } else if !text.is_empty() && inside(&text, x, y) {
                    text_pixels += 1;
                    text_diff += u64::from(differs);
                } else {
                    checked += 1;
                    diff += u64::from(differs);
                }
            }
            let analysis = compare(&baseline, &actual, &regions, &PixelOptions::STRICT).analysis;
            let context = format!("{width}x{height} masks {masks:?} text {text:?}");
            assert_eq!(
                (analysis.checked_pixels, analysis.diff_pixels),
                (checked, diff),
                "{context}"
            );
            let measured = analysis.text.map(|text| (text.pixels, text.diff_pixels));
            let expected = (!text.is_empty()).then_some((text_pixels, text_diff));
            assert_eq!(measured, expected, "{context}");
        }
    }

    #[test]
    fn test_dimension_diff_colors_the_overlap_and_each_side() {
        let red = Rgba([200, 0, 0, 255]);
        // 10x10 golden, 12x8 candidate: the golden alone covers rows 8-9 below 10 columns, the
        // candidate alone columns 10-11 of rows 0-7, neither the corner (10..12, 8..10).
        let baseline = ImageBuffer::from_pixel(10, 10, red);
        let mut actual = ImageBuffer::from_pixel(12, 8, red);
        actual.put_pixel(3, 3, Rgba([0, 0, 0, 255]));

        let diff = dimension_diff(&baseline, &actual).unwrap();
        assert_eq!(diff.dimensions(), (12, 10));
        // Overlap: equal pixels darkened, the changed one magenta.
        assert_eq!(*diff.get_pixel(0, 0), Rgba([100, 0, 0, 255]));
        assert_eq!(*diff.get_pixel(3, 3), Rgba(MAGENTA));
        // Golden only (rows 8-9): blue where (x + y) % 8 < 4, else the darkened golden.
        assert_eq!(*diff.get_pixel(0, 8), Rgba(BLUE));
        assert_eq!(*diff.get_pixel(4, 8), Rgba([100, 0, 0, 255]));
        // Candidate only (columns 10-11): green stripes over the darkened candidate.
        assert_eq!(*diff.get_pixel(10, 2), Rgba([100, 0, 0, 255]));
        assert_eq!(*diff.get_pixel(10, 7), Rgba(GREEN));
        // Neither.
        assert_eq!(*diff.get_pixel(11, 9), Rgba(UNCOVERED));
        // The other way round, the stripes swap colors.
        let swapped = dimension_diff(&actual, &baseline).unwrap();
        assert_eq!(*swapped.get_pixel(0, 8), Rgba(GREEN));
        assert_eq!(*swapped.get_pixel(10, 7), Rgba(BLUE));
    }

    #[test]
    fn test_dimension_diff_needs_a_canvas_within_the_budget() {
        let tall = RgbaImage::new(1, 16384);
        let wide = RgbaImage::new(16384, 1);
        assert_eq!(dimension_diff(&tall, &wide), None);
        assert_eq!(
            dimension_diff(&RgbaImage::new(0, 3), &RgbaImage::new(0, 4)),
            None
        );
    }

    /// A 10x10 frame with a vertical edge: black columns 0-4, an anti-aliasing column 5 of gray
    /// `aa`, white columns 6-9.
    fn edge_frame(aa: u8) -> RgbaImage {
        ImageBuffer::from_fn(10, 10, |x, _| match x {
            0..5 => Rgba([0, 0, 0, 255]),
            5 => Rgba([aa, aa, aa, 255]),
            _ => Rgba([255, 255, 255, 255]),
        })
    }

    fn options(channel_tolerance: u8, anti_alias: bool, edge_threshold: u8) -> PixelOptions {
        PixelOptions {
            channel_tolerance,
            anti_alias,
            edge_threshold,
        }
    }

    #[test]
    fn test_channel_tolerance_counts_small_deltas_as_equal() {
        let baseline = ImageBuffer::from_pixel(4, 4, Rgba([100, 100, 100, 200]));
        let mut actual = baseline.clone();
        actual.put_pixel(0, 0, Rgba([104, 96, 100, 200])); // ±4 on RGB
        actual.put_pixel(1, 0, Rgba([100, 100, 100, 204])); // +4 on alpha
        actual.put_pixel(2, 0, Rgba([105, 100, 100, 200])); // +5
        let analysis = |tolerance| {
            compare(
                &baseline,
                &actual,
                &PixelRegions::NONE,
                &options(tolerance, false, 0),
            )
            .analysis
        };
        let four = analysis(4);
        assert_eq!((four.diff_pixels, four.tolerated_pixels), (1, 2));
        assert_eq!(four.checked_pixels, 16);
        let five = analysis(5);
        assert_eq!((five.diff_pixels, five.tolerated_pixels), (0, 3));
        assert_eq!(analysis(0).diff_pixels, 3);
    }

    #[test]
    fn test_anti_aliased_pixels_are_tolerated_and_isolated_dots_are_not() {
        let baseline = edge_frame(128);
        // The anti-aliasing of another rasterizer of the same edge.
        let actual = edge_frame(100);
        let aa = options(0, true, 0);
        let compared = compare(&baseline, &actual, &PixelRegions::NONE, &aa);
        assert_eq!(compared.analysis.diff_pixels, 0);
        assert_eq!(compared.analysis.tolerated_pixels, 10);
        let diff = compared.diff_image(&baseline, &actual);
        assert_eq!(*diff.get_pixel(5, 5), Rgba(YELLOW));
        assert_eq!(*diff.get_pixel(0, 0), Rgba([0, 0, 0, 255]));

        // A dot in a flat area has equal neighbors all around: no anti-aliasing.
        let mut dot = baseline.clone();
        dot.put_pixel(2, 2, Rgba([90, 90, 90, 255]));
        let compared = compare(&baseline, &dot, &PixelRegions::NONE, &aa);
        assert_eq!(compared.analysis.diff_pixels, 1);
        assert_eq!(
            *compared.diff_image(&baseline, &dot).get_pixel(2, 2),
            Rgba(MAGENTA)
        );
        // Detected in either image: the pixel is also anti-aliased the other way round.
        let reversed = compare(&actual, &baseline, &PixelRegions::NONE, &aa).analysis;
        assert_eq!(reversed.diff_pixels, 0);
    }

    #[test]
    fn test_edge_threshold_hides_differences_on_baseline_edges_only() {
        let baseline = edge_frame(128);
        let mut actual = baseline.clone();
        actual.put_pixel(5, 5, Rgba([0, 0, 255, 255])); // on the edge
        actual.put_pixel(1, 1, Rgba([0, 0, 255, 255])); // in the flat black area
        actual.put_pixel(9, 0, Rgba([0, 0, 255, 255])); // a flat corner: border repeats
        let compared = compare(
            &baseline,
            &actual,
            &PixelRegions::NONE,
            &options(0, false, 64),
        );
        assert_eq!(compared.analysis.diff_pixels, 2);
        assert_eq!(compared.analysis.edge_pixels, 1);
        assert_eq!(compared.analysis.tolerated_pixels, 0);
        let diff = compared.diff_image(&baseline, &actual);
        assert_eq!(*diff.get_pixel(5, 5), Rgba(CYAN));
        assert_eq!(*diff.get_pixel(1, 1), Rgba(MAGENTA));

        // 255 hides nothing (gradients are capped at 255): the strict fast path.
        assert!(options(0, false, 255).is_strict());
        let capped = compare(
            &baseline,
            &actual,
            &PixelRegions::NONE,
            &options(0, false, 255),
        );
        assert_eq!(capped.analysis.diff_pixels, 3);

        // Never inside text: glyphs are all edges.
        let text = [Region {
            x: 4,
            y: 4,
            width: 3,
            height: 3,
        }];
        let in_text = compare(
            &baseline,
            &actual,
            &PixelRegions {
                masks: &[],
                text: &text,
                text_tolerance: Some(1.0),
            },
            &options(0, false, 64),
        )
        .analysis;
        assert_eq!(in_text.edge_pixels, 0);
        assert_eq!(in_text.text.unwrap().diff_pixels, 1);
    }

    #[test]
    fn test_options_apply_in_order_channel_then_anti_aliasing_then_edges() {
        let baseline = edge_frame(128);
        let mut actual = baseline.clone();
        // On the edge, within 2 per channel: the channel tolerance takes it first.
        actual.put_pixel(5, 2, Rgba([130, 130, 130, 255]));
        let analysis = compare(
            &baseline,
            &actual,
            &PixelRegions::NONE,
            &options(2, true, 64),
        )
        .analysis;
        assert_eq!((analysis.tolerated_pixels, analysis.edge_pixels), (1, 0));
        // Without it, the anti-aliasing detection (the column is anti-aliased).
        let analysis = compare(
            &baseline,
            &actual,
            &PixelRegions::NONE,
            &options(0, true, 64),
        )
        .analysis;
        assert_eq!((analysis.tolerated_pixels, analysis.edge_pixels), (1, 0));
        // Without both, the edge mask.
        let analysis = compare(
            &baseline,
            &actual,
            &PixelRegions::NONE,
            &options(0, false, 64),
        )
        .analysis;
        assert_eq!((analysis.tolerated_pixels, analysis.edge_pixels), (0, 1));
    }

    #[test]
    fn test_options_count_like_a_per_pixel_reference_for_channel_deltas() {
        let mut seed: u32 = 7;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for _ in 0..50 {
            let (width, height) = (1 + next() % 13, 1 + next() % 9);
            let baseline = ImageBuffer::from_fn(width, height, |_, _| {
                Rgba([next() as u8, next() as u8, next() as u8, 255])
            });
            let mut actual = baseline.clone();
            for pixel in actual.pixels_mut() {
                if next() % 3 == 0 {
                    pixel.0[(next() % 4) as usize] =
                        pixel.0[(next() % 4) as usize].wrapping_add((next() % 9) as u8);
                }
            }
            let tolerance = (next() % 6) as u8;
            let (mut diff, mut tolerated) = (0, 0);
            for (x, y, expected) in baseline.enumerate_pixels() {
                let found = actual.get_pixel(x, y);
                if expected == found {
                } else if tolerance > 0
                    && expected
                        .0
                        .iter()
                        .zip(found.0)
                        .all(|(a, b)| a.abs_diff(b) <= tolerance)
                {
                    tolerated += 1;
                } else {
                    diff += 1;
                }
            }
            let analysis = compare(
                &baseline,
                &actual,
                &PixelRegions::NONE,
                &options(tolerance, false, 0),
            )
            .analysis;
            assert_eq!(
                (analysis.diff_pixels, analysis.tolerated_pixels),
                (diff, tolerated),
                "{width}x{height} tolerance {tolerance}"
            );
        }
    }
}
