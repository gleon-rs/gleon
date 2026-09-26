//! Rendering-noise-tolerant comparison: a shift-tolerant envelope gate plus a coarse-scale SSIM gate.
//!
//! Decision policy ([`POLICY_VERSION`] 2), designed for UI goldens: anti-aliasing, sub-pixel glyph
//! placement and imperceptible color drift must pass; changed, added or removed content, color and
//! alpha changes, and loss of structure (blur) must fail.
//!
//! 1. **Envelope gate (full resolution).** A pixel is *explained* if each premultiplied RGBA
//!    channel of one image lies within the min–max range of the other image's 3x3 neighborhood,
//!    widened by `color_tolerance` plus a fraction of that neighborhood's contrast. Checked in both
//!    directions, so removed content is caught as well as added content. Re-rasterization and
//!    shifts below one pixel only produce values interpolated between neighbors, while new content
//!    or a color change falls outside the envelope. Unexplained pixels are grouped into 8-connected
//!    regions; a region fails if it has at least [`MIN_REGION_PIXELS`] pixels or a strong excess.
//! 2. **Structural gate (half resolution).** Luma (Rec. 601, composited over white) is 2x box
//!    downsampled, where sub-pixel shifts halve, and local SSIM is computed with an 11x11 Gaussian
//!    window (σ = 1.5, Wang et al. 2004). The gate is the *minimum* local SSIM, so a small change
//!    on a large image is not diluted; it catches structural loss such as blur that stays inside
//!    the envelope.
//! 3. **Bounded work.** Both gates are exactly neutral away from differing pixels, so all work is
//!    limited to the bounding box of differing pixels plus the window radius.
//!
//! Calibrated on `tests/ssim_corpus.rs`. Known limits, by construction: moving thin features such
//! as glyphs by half a pixel or more (typical of different font engines across operating systems)
//! is indistinguishable from changed content at pixel level and fails, so keep per-platform
//! goldens; low-contrast color changes of one-pixel lines can pass. Use the exact or pixel modes
//! where every pixel matters.

use image::RgbaImage;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

/// Version of the decision policy; bumped whenever verdicts can change for the same inputs.
pub const POLICY_VERSION: u32 = 2;

/// Largest image (in pixels, 4096x4096) admitted to [`analyze`].
///
/// The analysis workspace grows with the changed area (roughly 16 bytes per pixel on top of the two
/// decoded images in the worst case), so it is bounded separately from, and well below, the decoder
/// budget: realistic goldens (a full `MaterialApp` capture is 2400x1800, a 5K screen 14.7 MP) fit,
/// decoder-limit images cannot exhaust memory.
pub const MAX_ANALYSIS_PIXELS: u64 = 4096 * 4096;

/// Returns whether a `width` x `height` image fits [`MAX_ANALYSIS_PIXELS`].
#[must_use]
pub const fn fits_analysis_budget(width: u32, height: u32) -> bool {
    (width as u64) * (height as u64) <= MAX_ANALYSIS_PIXELS
}

/// Unexplained regions of at least this many pixels fail.
pub const MIN_REGION_PIXELS: usize = 3;
/// Smaller unexplained regions still fail if a pixel exceeds its envelope by this much (8-bit).
const STRONG_EXCESS: f32 = 64.0;
/// Fraction of the neighborhood contrast added to the envelope (edges and text wiggle more).
const CONTRAST_ALLOWANCE: f32 = 0.5;

/// Gaussian window radius in pixels (window side = `2 * RADIUS + 1`).
const RADIUS: usize = 5;
/// Gaussian window standard deviation.
const SIGMA: f32 = 1.5;
/// SSIM stabilizers for an 8-bit dynamic range: `(0.01 * 255)^2` and `(0.03 * 255)^2`.
const C1: f32 = 6.5025;
const C2: f32 = 58.5225;
/// Rows per parallel work band.
const BAND_ROWS: usize = 64;

/// Thresholds of the decision policy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SsimPolicy {
    /// Minimum local SSIM `[0, 1]` (at half resolution) every neighborhood must reach.
    pub min_similarity: f64,
    /// Tolerated deviation (8-bit channel units) beyond the local envelope.
    pub color_tolerance: f64,
}

/// An axis-aligned pixel rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Region {
    /// Left edge.
    pub x: u32,
    /// Top edge.
    pub y: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// Result of [`analyze`].
#[derive(Debug, Clone)]
pub struct SsimAnalysis {
    /// Mean local SSIM over the whole (half-resolution) image; diagnostic only.
    pub mean_ssim: f64,
    /// Lowest local SSIM (the structural gate's value).
    pub min_ssim: f64,
    /// Largest deviation beyond the local envelope among failing regions, in 8-bit channel units
    /// (0 when the envelope gate passed).
    pub max_excess: f64,
    /// Number of full-resolution pixels failing the policy.
    pub failing_pixels: u64,
    /// Bounding box of the changed pixels responsible for the failure.
    pub failing_region: Option<Region>,
    /// Diff visualization, present only when the policy fails: the baseline faded out, tolerated
    /// differences in yellow, failing pixels in red.
    pub diff_image: Option<RgbaImage>,
}

impl SsimAnalysis {
    /// Whether the images are considered equivalent under the policy.
    #[must_use]
    pub const fn passed(&self) -> bool {
        self.failing_pixels == 0
    }
}

/// Half-open pixel rectangle `[x0, x1) x [y0, y1)`.
#[derive(Debug, Clone, Copy)]
struct Rect {
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
}

impl Rect {
    const fn width(self) -> usize {
        self.x1 - self.x0
    }

    const fn height(self) -> usize {
        self.y1 - self.y0
    }

    const fn contains(self, x: usize, y: usize) -> bool {
        x >= self.x0 && x < self.x1 && y >= self.y0 && y < self.y1
    }

    fn expand(self, by: usize, width: usize, height: usize) -> Self {
        Self {
            x0: self.x0.saturating_sub(by),
            y0: self.y0.saturating_sub(by),
            x1: (self.x1 + by).min(width),
            y1: (self.y1 + by).min(height),
        }
    }
}

/// Accumulates a bounding box of points.
#[derive(Debug, Clone, Copy, Default)]
struct BBox(Option<Rect>);

impl BBox {
    fn add(&mut self, x: usize, y: usize) {
        let point = Rect {
            x0: x,
            y0: y,
            x1: x + 1,
            y1: y + 1,
        };
        self.0 = Some(self.0.map_or(point, |r| Rect {
            x0: r.x0.min(x),
            y0: r.y0.min(y),
            x1: r.x1.max(x + 1),
            y1: r.y1.max(y + 1),
        }));
    }

    fn to_region(self) -> Option<Region> {
        let to_u32 = |v: usize| u32::try_from(v).unwrap_or(u32::MAX);
        self.0.map(|r| Region {
            x: to_u32(r.x0),
            y: to_u32(r.y0),
            width: to_u32(r.width()),
            height: to_u32(r.height()),
        })
    }
}

/// Full-resolution image accessor.
#[derive(Clone, Copy)]
struct Img<'a> {
    raw: &'a [u8],
    width: usize,
    height: usize,
}

impl Img<'_> {
    const fn rgba(self, x: usize, y: usize) -> [u8; 4] {
        let i = (y * self.width + x) * 4;
        [
            self.raw[i],
            self.raw[i + 1],
            self.raw[i + 2],
            self.raw[i + 3],
        ]
    }

    /// Premultiplied RGBA in 8-bit units, so alpha changes show in the color channels too.
    fn premultiplied(self, x: usize, y: usize) -> [f32; 4] {
        let rgba = self.rgba(x, y);
        let alpha = f32::from(rgba[3]) / 255.0;
        [
            f32::from(rgba[0]) * alpha,
            f32::from(rgba[1]) * alpha,
            f32::from(rgba[2]) * alpha,
            f32::from(rgba[3]),
        ]
    }
}

/// How far `value` lies outside `other`'s 3x3 envelope at `(x, y)`, minus the allowance.
///
/// Neighbors are premultiplied on the fly: precomputing premultiplied planes was measured slower
/// on full-screen changes (memory traffic outweighs the arithmetic).
fn envelope_excess(value: [f32; 4], other: Img<'_>, x: usize, y: usize, tolerance: f32) -> f32 {
    let mut lo = [f32::MAX; 4];
    let mut hi = [f32::MIN; 4];
    for ny in y.saturating_sub(1)..=(y + 1).min(other.height - 1) {
        for nx in x.saturating_sub(1)..=(x + 1).min(other.width - 1) {
            let p = other.premultiplied(nx, ny);
            for c in 0..4 {
                lo[c] = lo[c].min(p[c]);
                hi[c] = hi[c].max(p[c]);
            }
        }
    }
    let contrast = (0..4).map(|c| hi[c] - lo[c]).fold(0.0f32, f32::max);
    let allowance = CONTRAST_ALLOWANCE.mul_add(contrast, tolerance);
    (0..4)
        .map(|c| (lo[c] - value[c]).max(value[c] - hi[c]) - allowance)
        .fold(f32::MIN, f32::max)
}

/// Symmetric envelope excess at `(x, y)`; positive means unexplained.
fn pixel_excess(base: Img<'_>, cand: Img<'_>, x: usize, y: usize, tolerance: f32) -> f32 {
    envelope_excess(cand.premultiplied(x, y), base, x, y, tolerance).max(envelope_excess(
        base.premultiplied(x, y),
        cand,
        x,
        y,
        tolerance,
    ))
}

/// Unexplained pixels over `changed`, grouped into regions; returns the failing mask (over
/// `changed`) and the largest excess among *failing* regions (tolerated regions don't count, so
/// the reported excess always explains an envelope failure).
fn envelope_gate(base: Img<'_>, cand: Img<'_>, changed: Rect, tolerance: f32) -> (Vec<bool>, f32) {
    let cw = changed.width();
    let mut excess = vec![f32::MIN; cw * changed.height()];
    excess
        .par_chunks_mut(cw)
        .enumerate()
        .for_each(|(row, out)| {
            let y = changed.y0 + row;
            for (col, e) in out.iter_mut().enumerate() {
                let x = changed.x0 + col;
                if base.rgba(x, y) != cand.rgba(x, y) {
                    *e = pixel_excess(base, cand, x, y, tolerance);
                }
            }
        });
    // 8-connected components of unexplained pixels. Indices are `u32` (the analysis budget is far
    // below 2^32 pixels) and no per-component list is kept: a failing component is re-flooded to
    // mark it, so the workspace stays at a few bytes per pixel.
    let (ch, unexplained) = (changed.height(), |i: usize| excess[i] > 0.0);
    let neighbors = |i: u32| {
        let i = i as usize;
        let (cx, cy) = (i % cw, i / cw);
        (cy.saturating_sub(1)..=(cy + 1).min(ch - 1)).flat_map(move |ny| {
            (cx.saturating_sub(1)..=(cx + 1).min(cw - 1)).map(move |nx| ny * cw + nx)
        })
    };
    let to_u32 = |i: usize| u32::try_from(i).unwrap_or(u32::MAX);
    let mut max_excess = 0.0f32;
    let mut failing = vec![false; excess.len()];
    let mut seen = vec![false; excess.len()];
    let mut stack: Vec<u32> = Vec::new();
    for start in 0..excess.len() {
        if seen[start] || !unexplained(start) {
            continue;
        }
        seen[start] = true;
        stack.push(to_u32(start));
        let (mut size, mut strongest) = (0usize, f32::MIN);
        while let Some(i) = stack.pop() {
            size += 1;
            strongest = strongest.max(excess[i as usize]);
            for n in neighbors(i) {
                if !seen[n] && unexplained(n) {
                    seen[n] = true;
                    stack.push(to_u32(n));
                }
            }
        }
        if size >= MIN_REGION_PIXELS || strongest >= STRONG_EXCESS {
            max_excess = max_excess.max(strongest);
            failing[start] = true;
            stack.push(to_u32(start));
            while let Some(i) = stack.pop() {
                for n in neighbors(i) {
                    if !failing[n] && unexplained(n) {
                        failing[n] = true;
                        stack.push(to_u32(n));
                    }
                }
            }
        }
    }
    (failing, max_excess)
}

/// Luma (0–255) of a straight-alpha pixel composited over white.
fn luma_over_white([r, g, b, a]: [u8; 4]) -> f32 {
    let alpha = f32::from(a) / 255.0;
    let over = |c: u8| f32::from(c).mul_add(alpha, 255.0 * (1.0 - alpha));
    0.114f32.mul_add(over(b), 0.299f32.mul_add(over(r), 0.587 * over(g)))
}

/// Half-resolution luma over coarse rect `crop` (coarse pixel = mean of up to 2x2 full pixels).
fn coarse_luma(img: Img<'_>, crop: Rect) -> Vec<f32> {
    let mut out = Vec::with_capacity(crop.width() * crop.height());
    for cy in crop.y0..crop.y1 {
        for cx in crop.x0..crop.x1 {
            let (mut sum, mut n) = (0.0f32, 0.0f32);
            for y in (2 * cy)..(2 * cy + 2).min(img.height) {
                for x in (2 * cx)..(2 * cx + 2).min(img.width) {
                    sum += luma_over_white(img.rgba(x, y));
                    n += 1.0;
                }
            }
            out.push(sum / n);
        }
    }
    out
}

fn gaussian_weights() -> [f32; 2 * RADIUS + 1] {
    let mut weights = [0.0f32; 2 * RADIUS + 1];
    let mut offset = -5.0f32;
    for w in &mut weights {
        *w = (-(offset * offset) / (2.0 * SIGMA * SIGMA)).exp();
        offset += 1.0;
    }
    let sum: f32 = weights.iter().sum();
    for w in &mut weights {
        *w /= sum;
    }
    weights
}

/// Local SSIM for eval rows `band` of the coarse image, flagging `ssim < threshold` in `fails`
/// (one byte per eval-rect pixel of those rows). Returns the band's SSIM sum and minimum.
#[expect(
    clippy::too_many_arguments,
    reason = "internal kernel; grouping the planes into a struct would only add indirection"
)]
fn ssim_band(
    luma_b: &[f32],
    luma_a: &[f32],
    crop: Rect,
    eval: Rect,
    band: (usize, usize),
    weights: &[f32; 2 * RADIUS + 1],
    threshold: f64,
    fails: &mut [u8],
) -> (f64, f32) {
    let cw = crop.width();
    let ew = eval.width();
    let clamp = |v: usize, offset: usize, lo: usize, hi: usize| {
        (v + offset).saturating_sub(RADIUS).clamp(lo, hi - 1)
    };

    // Horizontal pass over the rows the band's windows can touch.
    let rows_lo = band.0.saturating_sub(RADIUS).max(crop.y0);
    let rows_hi = (band.1 + RADIUS).min(crop.y1);
    let plane = (rows_hi - rows_lo) * ew;
    let mut h = vec![0.0f32; 5 * plane];
    for (ri, y) in (rows_lo..rows_hi).enumerate() {
        let row = (y - crop.y0) * cw;
        for (ei, x) in (eval.x0..eval.x1).enumerate() {
            let mut acc = [0.0f32; 5];
            for (k, w) in weights.iter().enumerate() {
                let sx = clamp(x, k, crop.x0, crop.x1) - crop.x0;
                let (vb, va) = (luma_b[row + sx], luma_a[row + sx]);
                acc[0] = w.mul_add(vb, acc[0]);
                acc[1] = w.mul_add(va, acc[1]);
                acc[2] = (w * vb).mul_add(vb, acc[2]);
                acc[3] = (w * va).mul_add(va, acc[3]);
                acc[4] = (w * vb).mul_add(va, acc[4]);
            }
            for (q, value) in acc.into_iter().enumerate() {
                h[q * plane + ri * ew + ei] = value;
            }
        }
    }

    // Vertical pass and SSIM for the band's eval rows.
    let (mut sum, mut min) = (0.0f64, 1.0f32);
    for (bi, y) in (band.0..band.1).enumerate() {
        for ei in 0..ew {
            let mut acc = [0.0f32; 5];
            for (k, w) in weights.iter().enumerate() {
                let base = (clamp(y, k, crop.y0, crop.y1) - rows_lo) * ew + ei;
                for (q, value) in acc.iter_mut().enumerate() {
                    *value = w.mul_add(h[q * plane + base], *value);
                }
            }
            let [mb, ma, bb, aa, ba] = acc;
            let var_b = mb.mul_add(-mb, bb).max(0.0);
            let var_a = ma.mul_add(-ma, aa).max(0.0);
            let cov = mb.mul_add(-ma, ba);
            let ssim = ((2.0 * mb).mul_add(ma, C1) * 2.0f32.mul_add(cov, C2))
                / (mb.mul_add(mb, ma.mul_add(ma, C1)) * (var_b + var_a + C2));
            sum += f64::from(ssim);
            min = min.min(ssim);
            if f64::from(ssim) < threshold {
                fails[bi * ew + ei] = 1;
            }
        }
    }
    (sum, min)
}

/// Coarse SSIM gate around `changed` (full-resolution rect); returns the failing coarse mask over
/// the returned coarse eval rect, the SSIM sum over it, its minimum and the coarse pixel count.
fn structural_gate(
    base: Img<'_>,
    cand: Img<'_>,
    changed: Rect,
    threshold: f64,
) -> (Rect, Vec<u8>, f64, f32) {
    let (cw, ch) = (base.width.div_ceil(2), base.height.div_ceil(2));
    let coarse_changed = Rect {
        x0: changed.x0 / 2,
        y0: changed.y0 / 2,
        x1: changed.x1.div_ceil(2),
        y1: changed.y1.div_ceil(2),
    };
    let eval = coarse_changed.expand(RADIUS, cw, ch);
    let crop = eval.expand(RADIUS, cw, ch);
    let (luma_b, luma_a) = (coarse_luma(base, crop), coarse_luma(cand, crop));
    let weights = gaussian_weights();
    let ew = eval.width();
    let mut fails = vec![0u8; ew * eval.height()];
    let (sum, min) = fails
        .par_chunks_mut(ew * BAND_ROWS)
        .enumerate()
        .map(|(band_index, band_fails)| {
            let y0 = eval.y0 + band_index * BAND_ROWS;
            let y1 = (y0 + BAND_ROWS).min(eval.y1);
            ssim_band(
                &luma_b,
                &luma_a,
                crop,
                eval,
                (y0, y1),
                &weights,
                threshold,
                band_fails,
            )
        })
        .reduce(|| (0.0, 1.0), |a, b| (a.0 + b.0, a.1.min(b.1)));
    (eval, fails, sum, min)
}

/// Bounding box of pixels whose RGBA bytes differ, or `None` if the images are identical.
///
/// A plain scan: the coordinate division runs only for differing pixels, and a row-wise `memcmp`
/// variant measured no faster (the cost of a failing comparison is dominated by the diff image).
fn diff_bbox(base: Img<'_>, cand: Img<'_>) -> Option<Rect> {
    let mut bbox = BBox::default();
    for (i, (b, a)) in base
        .raw
        .as_chunks::<4>()
        .0
        .iter()
        .zip(cand.raw.as_chunks::<4>().0)
        .enumerate()
    {
        if b != a {
            bbox.add(i % base.width, i / base.width);
        }
    }
    bbox.0
}

/// Compares two images of identical dimensions under the decision policy.
///
/// # Panics
/// Panics if `baseline` and `actual` have different dimensions, or if they exceed
/// [`MAX_ANALYSIS_PIXELS`] (check [`fits_analysis_budget`] first; `compare_images` does).
#[must_use]
pub fn analyze(baseline: &RgbaImage, actual: &RgbaImage, policy: &SsimPolicy) -> SsimAnalysis {
    assert_eq!(
        baseline.dimensions(),
        actual.dimensions(),
        "Image dimensions must match for SSIM analysis"
    );
    assert!(
        fits_analysis_budget(baseline.width(), baseline.height()),
        "Image exceeds the SSIM analysis budget"
    );
    let (width, height) = (baseline.width() as usize, baseline.height() as usize);
    let base = Img {
        raw: baseline.as_raw(),
        width,
        height,
    };
    let cand = Img {
        raw: actual.as_raw(),
        width,
        height,
    };

    let Some(changed) = diff_bbox(base, cand) else {
        return SsimAnalysis {
            mean_ssim: 1.0,
            min_ssim: 1.0,
            max_excess: 0.0,
            failing_pixels: 0,
            failing_region: None,
            diff_image: None,
        };
    };

    #[expect(
        clippy::cast_possible_truncation,
        reason = "the tolerance is an 8-bit channel amount; f32 precision is ample"
    )]
    let tolerance = policy.color_tolerance as f32;
    let (envelope_fails, max_excess) = envelope_gate(base, cand, changed, tolerance);
    let (coarse_eval, coarse_fails, ssim_sum, min_ssim) =
        structural_gate(base, cand, changed, policy.min_similarity);

    // Full-resolution failing mask: envelope failures plus pixels under failing coarse windows.
    let fail_rect = Rect {
        x0: coarse_eval.x0 * 2,
        y0: coarse_eval.y0 * 2,
        x1: (coarse_eval.x1 * 2).min(width),
        y1: (coarse_eval.y1 * 2).min(height),
    };
    let fw = fail_rect.width();
    let mut failing = vec![false; fw * fail_rect.height()];
    let mut failing_pixels = 0u64;
    let mut changed_failing = BBox::default();
    let mut any_failing = BBox::default();
    for y in fail_rect.y0..fail_rect.y1 {
        for x in fail_rect.x0..fail_rect.x1 {
            let envelope = changed.contains(x, y)
                && envelope_fails[(y - changed.y0) * changed.width() + (x - changed.x0)];
            let structural = coarse_fails
                [(y / 2 - coarse_eval.y0) * coarse_eval.width() + (x / 2 - coarse_eval.x0)]
                == 1;
            if envelope || structural {
                failing[(y - fail_rect.y0) * fw + (x - fail_rect.x0)] = true;
                failing_pixels += 1;
                any_failing.add(x, y);
                if base.rgba(x, y) != cand.rgba(x, y) {
                    changed_failing.add(x, y);
                }
            }
        }
    }

    let coarse_total = (width.div_ceil(2) * height.div_ceil(2)) as u64;
    let coarse_evaluated = (coarse_eval.width() * coarse_eval.height()) as u64;
    #[expect(
        clippy::cast_precision_loss,
        reason = "pixel counts are far below 2^52, so the f64 conversion is exact"
    )]
    let mean_ssim = (ssim_sum + (coarse_total - coarse_evaluated) as f64) / coarse_total as f64;
    let diff_image =
        (failing_pixels > 0).then(|| render_diff(baseline, actual, fail_rect, &failing));
    SsimAnalysis {
        mean_ssim,
        min_ssim: f64::from(min_ssim),
        max_excess: f64::from(max_excess),
        failing_pixels,
        failing_region: if changed_failing.0.is_some() {
            changed_failing.to_region()
        } else {
            any_failing.to_region()
        },
        diff_image,
    }
}

/// Baseline faded towards white, tolerated differences in yellow, failing pixels in red.
fn render_diff(
    baseline: &RgbaImage,
    actual: &RgbaImage,
    rect: Rect,
    failing: &[bool],
) -> RgbaImage {
    let fw = rect.width();
    RgbaImage::from_fn(baseline.width(), baseline.height(), |x, y| {
        let (xu, yu) = (x as usize, y as usize);
        if rect.contains(xu, yu) && failing[(yu - rect.y0) * fw + (xu - rect.x0)] {
            return image::Rgba([255, 0, 0, 255]);
        }
        let b = baseline.get_pixel(x, y);
        if b != actual.get_pixel(x, y) {
            return image::Rgba([255, 200, 0, 255]);
        }
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "luma is within [0, 255], so the faded value is within [170, 255]"
        )]
        let faded = luma_over_white(b.0).mul_add(1.0 / 3.0, 170.0) as u8;
        image::Rgba([faded, faded, faded, 255])
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
    use image::{ImageBuffer, Rgba};

    use super::*;

    const POLICY: SsimPolicy = SsimPolicy {
        min_similarity: 0.8,
        color_tolerance: 8.0,
    };

    fn solid(w: u32, h: u32, c: [u8; 4]) -> RgbaImage {
        ImageBuffer::from_pixel(w, h, Rgba(c))
    }

    #[test]
    fn test_identical_images_pass_with_perfect_scores() {
        let img = solid(40, 30, [10, 20, 30, 255]);
        let a = analyze(&img, &img, &POLICY);
        assert!(a.passed());
        assert_eq!((a.mean_ssim, a.min_ssim, a.max_excess), (1.0, 1.0, 0.0));
        assert!(a.diff_image.is_none());
    }

    #[test]
    fn test_single_saturated_pixel_in_flat_area_fails_and_is_localized() {
        let base = solid(200, 200, [255, 255, 255, 255]);
        let mut cand = base.clone();
        cand.put_pixel(120, 80, Rgba([0, 0, 0, 255]));
        let a = analyze(&base, &cand, &POLICY);
        assert!(!a.passed(), "{a:?}");
        assert!(a.mean_ssim > 0.99, "mean is diluted: {a:?}");
        assert_eq!(
            a.failing_region,
            Some(Region {
                x: 120,
                y: 80,
                width: 1,
                height: 1
            })
        );
        assert!(a.diff_image.is_some());
    }

    #[test]
    fn test_imperceptible_color_drift_passes() {
        let base = solid(64, 64, [63, 81, 181, 255]);
        let cand = solid(64, 64, [63, 81, 183, 255]);
        let a = analyze(&base, &cand, &POLICY);
        assert!(a.passed(), "{a:?}");
    }

    #[test]
    fn test_hue_shift_with_similar_luma_fails() {
        let base = solid(64, 64, [0x21, 0x96, 0xF3, 255]);
        let cand = solid(64, 64, [0x4C, 0xAF, 0x50, 255]);
        let a = analyze(&base, &cand, &POLICY);
        assert!(!a.passed(), "{a:?}");
    }

    #[test]
    fn test_alpha_change_is_detected() {
        let base = solid(32, 32, [255, 255, 255, 255]);
        let cand = solid(32, 32, [255, 255, 255, 0]);
        assert!(!analyze(&base, &cand, &POLICY).passed());
    }

    #[test]
    fn test_tolerated_envelope_noise_does_not_inflate_max_excess() {
        // A lone pixel with a moderate excess (below STRONG_EXCESS, region < MIN_REGION_PIXELS) is
        // tolerated by the envelope gate; a strict structural gate fails on it instead, and the
        // reported excess must not blame the tolerated envelope noise.
        let strict = SsimPolicy {
            min_similarity: 0.95,
            ..POLICY
        };
        let base = solid(64, 64, [128, 128, 128, 255]);
        let mut cand = base.clone();
        cand.put_pixel(30, 30, Rgba([160, 160, 160, 255]));
        let a = analyze(&base, &cand, &strict);
        assert!(!a.passed(), "{a:?}");
        assert!(a.min_ssim < 0.95, "{a:?}");
        assert_eq!(a.max_excess, 0.0, "{a:?}");
    }

    #[test]
    fn test_analysis_budget_boundaries() {
        assert!(fits_analysis_budget(4096, 4096));
        assert!(fits_analysis_budget(2400, 1800));
        assert!(!fits_analysis_budget(4097, 4096));
    }

    #[test]
    fn test_images_smaller_than_the_window_are_handled() {
        let base = solid(3, 2, [0, 0, 0, 255]);
        let mut cand = base.clone();
        cand.put_pixel(1, 1, Rgba([255, 255, 255, 255]));
        assert!(!analyze(&base, &cand, &POLICY).passed());
        let one = solid(1, 1, [1, 2, 3, 255]);
        assert!(analyze(&one, &one, &POLICY).passed());
    }
}
