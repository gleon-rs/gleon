//! Safe ignore-zone masking engine implementation.

use image::RgbaImage;
use tracing::warn;

use crate::{
    Pixels,
    config::{Dimension, Zone},
    ssim::Region,
};

/// Modifies the provided image buffer, setting masked pixels to absolute black (0, 0, 0, 255).
///
/// If coordinates or sizes are specified in percentages, they are resolved against the runtime
/// dimensions of the image using mathematical rounding (`round()`).
///
/// If a mask zone extends beyond the image boundaries, the coordinates are clamped to the
/// image width/height, a warning is logged via `warn!`, and the operation continues safely.
///
/// A width of `0%`, height of `0`, or an empty `zones` slice is treated as a no-op.
///
/// Returns how many zones extended beyond the image and were clamped, so callers without a
/// `tracing` subscriber (the FFI integrations) can tell their users.
pub fn apply_masks(img: &mut RgbaImage, zones: &[Zone]) -> usize {
    let (regions, clamped) = resolve_zones(zones, img.width(), img.height());
    paint_black(img, &regions);
    clamped
}

/// The pixel rectangles of `zones` in a `width` x `height` image, clamped to it.
///
/// Also returns how many zones reached beyond the image (each logged via `warn!`). Zones of zero
/// width or height, or entirely outside the image, give no rectangle.
#[must_use]
pub fn resolve_zones(zones: &[Zone], width: u32, height: u32) -> (Vec<Region>, usize) {
    if width == 0 || height == 0 {
        return (Vec::new(), 0);
    }
    let mut regions = Vec::with_capacity(zones.len());
    let mut clamped = 0;
    for zone in zones {
        let Some(ResolvedZone {
            x_end,
            y_end,
            is_out_of_bounds,
        }) = resolve_zone(zone, width, height)
        else {
            continue;
        };
        if is_out_of_bounds {
            clamped += 1;
            warn!(
                "Mask zone extends beyond image bounds: \
                 zone = x:{}, y:{}, w:{:?}, h:{:?}, image_dims = {}x{}",
                zone.x, zone.y, zone.width, zone.height, width, height
            );
        }
        let (x, y) = (zone.x.min(width), zone.y.min(height));
        let region = Region {
            x,
            y,
            width: x_end.min(width) - x,
            height: y_end.min(height) - y,
        };
        if region.width > 0 && region.height > 0 {
            regions.push(region);
        }
    }
    (regions, clamped)
}

/// Paints `regions` of `img` opaque black (0, 0, 0, 255); the regions lie inside the image
/// ([`resolve_zones`]).
///
/// # Panics
/// Panics if a region reaches beyond the image.
pub fn paint_black(img: &mut RgbaImage, regions: &[Region]) {
    let row_bytes = img.width() as usize * 4;
    let raw_pixels: &mut [u8] = img.as_mut();
    for region in regions {
        let left = region.x as usize * 4;
        let right = (region.x + region.width) as usize * 4;
        let (top, bottom) = (region.y as usize, (region.y + region.height) as usize);
        let mut fill = |start: usize, end: usize| {
            #[expect(
                clippy::expect_used,
                reason = "the regions are resolved inside the image"
            )]
            fill_black(
                raw_pixels
                    .get_mut(start..end)
                    .expect("a region lies inside the image"),
            );
        };
        // A full-width region is one contiguous block.
        if left == 0 && right == row_bytes {
            fill(top * row_bytes, bottom * row_bytes);
        } else {
            for y in top..bottom {
                fill(y * row_bytes + left, y * row_bytes + right);
            }
        }
    }
}

/// The row spans (byte ranges of the raw buffer) of `regions` in an image of `row_bytes` bytes
/// per row; the regions lie inside the image.
fn row_spans(regions: &[Region], row_bytes: usize) -> impl Iterator<Item = std::ops::Range<usize>> {
    regions.iter().flat_map(move |region| {
        let (left, right) = (
            region.x as usize * 4,
            (region.x + region.width) as usize * 4,
        );
        (region.y as usize..(region.y + region.height) as usize)
            .map(move |y| y * row_bytes + left..y * row_bytes + right)
    })
}

/// Whether `a` and `b` (of the same size) differ anywhere inside `regions` (inside the images).
#[must_use]
pub fn differ_inside(a: Pixels<'_>, b: Pixels<'_>, regions: &[Region]) -> bool {
    let row_bytes = a.dimensions().0 as usize * 4;
    row_spans(regions, row_bytes).any(|span| a.raw()[span.clone()] != b.raw()[span])
}

/// Copies the pixels of `regions` from `source` (of the same size) into `image`.
///
/// The regions then compare as equal while their surroundings keep their real neighbors: an
/// SSIM comparison excludes text and masks this way (painting them over would widen the
/// envelope of the pixels around them).
///
/// # Panics
/// Panics if a region reaches beyond the images or the sizes differ.
pub fn copy_regions(image: &mut RgbaImage, source: Pixels<'_>, regions: &[Region]) {
    assert_eq!(
        image.dimensions(),
        source.dimensions(),
        "same sizes expected"
    );
    let row_bytes = image.width() as usize * 4;
    let raw: &mut [u8] = image.as_mut();
    for span in row_spans(regions, row_bytes) {
        raw[span.clone()].copy_from_slice(&source.raw()[span]);
    }
}

/// How many of `zones` reach beyond a `width` x `height` image: the count [`apply_masks`] returns
/// for such an image, without touching pixels (for callers that skip decoding, such as a
/// byte-identical fast path).
#[must_use]
pub fn clamped_zones(zones: &[Zone], width: u32, height: u32) -> usize {
    if width == 0 || height == 0 {
        return 0;
    }
    zones
        .iter()
        .filter_map(|zone| resolve_zone(zone, width, height))
        .filter(|zone| zone.is_out_of_bounds)
        .count()
}

/// A zone resolved against an image of a non-zero size.
struct ResolvedZone {
    /// Exclusive right edge, not yet clamped.
    x_end: u32,
    /// Exclusive bottom edge, not yet clamped.
    y_end: u32,
    /// Whether the zone reaches beyond the image.
    is_out_of_bounds: bool,
}

/// `zone` resolved against an `img_w` x `img_h` image; `None` for a zone of zero width or height.
fn resolve_zone(zone: &Zone, img_w: u32, img_h: u32) -> Option<ResolvedZone> {
    let w_px = resolve_dimension(zone.width, img_w);
    let h_px = resolve_dimension(zone.height, img_h);
    if w_px == 0 || h_px == 0 {
        return None;
    }
    let x_end = zone.x.saturating_add(w_px);
    let y_end = zone.y.saturating_add(h_px);
    Some(ResolvedZone {
        x_end,
        y_end,
        is_out_of_bounds: zone.x >= img_w || zone.y >= img_h || x_end > img_w || y_end > img_h,
    })
}

#[inline]
fn fill_black(slice: &mut [u8]) {
    slice.as_chunks_mut::<4>().0.fill([0, 0, 0, 255]);
}

/// Resolves a [`Dimension`] to an absolute pixel count relative to `dim_px`.
///
/// Percentage values are rounded to the nearest integer pixel using `round()`.
/// The cast to `u32` is saturating for out-of-range `f64` values, but `Percent` is
/// validated at config load time to be within `[0.0, 100.0]`, so the result is always
/// within `[0, dim_px]`.
fn resolve_dimension(dimension: Dimension, dim_px: u32) -> u32 {
    match dimension {
        Dimension::Pixels(px) => px,
        Dimension::Percent(pct) => {
            // `pct` is validated at config load time to lie within [0.0, 100.0], so the
            // rounded result is always non-negative and within [0, dim_px], fitting `u32`
            // without truncation or sign loss.
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "`pct` is validated to lie in [0.0, 100.0], so the result fits `u32` without truncation or sign loss"
            )]
            let resolved = (pct / 100.0 * f64::from(dim_px)).round() as u32;
            resolved
        }
    }
}

#[cfg(test)]
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
    use image::{ImageBuffer, Rgba, RgbaImage};

    use super::*;
    use crate::config::Dimension;

    fn red_image(w: u32, h: u32) -> RgbaImage {
        ImageBuffer::from_pixel(w, h, Rgba([255, 0, 0, 255]))
    }

    const BLACK: Rgba<u8> = Rgba([0, 0, 0, 255]);
    const RED: Rgba<u8> = Rgba([255, 0, 0, 255]);

    fn zone(x: u32, y: u32, width: Dimension, height: Dimension) -> Zone {
        Zone {
            x,
            y,
            width,
            height,
        }
    }

    // ── resolve_dimension ────────────────────────────────────────────────────

    #[test]
    fn resolve_pixels_is_identity() {
        assert_eq!(resolve_dimension(Dimension::Pixels(42), 100), 42);
    }

    #[test]
    fn resolve_pixels_zero() {
        assert_eq!(resolve_dimension(Dimension::Pixels(0), 100), 0);
    }

    #[test]
    fn resolve_percent_full() {
        assert_eq!(resolve_dimension(Dimension::Percent(100.0), 100), 100);
    }

    #[test]
    fn resolve_percent_zero() {
        assert_eq!(resolve_dimension(Dimension::Percent(0.0), 100), 0);
    }

    #[test]
    fn resolve_percent_rounds_correctly() {
        // 20% of 101 = 20.2 → rounds to 20
        assert_eq!(resolve_dimension(Dimension::Percent(20.0), 101), 20);
        // 50% of 101 = 50.5 → rounds to 51
        assert_eq!(resolve_dimension(Dimension::Percent(50.0), 101), 51);
    }

    // ── apply_masks: trivial no-ops ──────────────────────────────────────────

    #[test]
    fn empty_zones_is_noop() {
        let mut img = red_image(10, 10);
        apply_masks(&mut img, &[]);
        assert_eq!(*img.get_pixel(5, 5), RED);
    }

    #[test]
    fn zero_width_image_is_noop() {
        // ImageBuffer::new(0, 0) is the degenerate image; apply_masks must return early.
        let mut img: RgbaImage = ImageBuffer::new(0, 0);
        apply_masks(
            &mut img,
            &[zone(0, 0, Dimension::Pixels(10), Dimension::Pixels(10))],
        );
        // No panic is the assertion here.
    }

    #[test]
    fn zone_width_zero_pixels_skipped() {
        let mut img = red_image(10, 10);
        apply_masks(
            &mut img,
            &[zone(0, 0, Dimension::Pixels(0), Dimension::Pixels(5))],
        );
        assert_eq!(*img.get_pixel(0, 0), RED);
    }

    #[test]
    fn zone_height_zero_pixels_skipped() {
        let mut img = red_image(10, 10);
        apply_masks(
            &mut img,
            &[zone(0, 0, Dimension::Pixels(5), Dimension::Pixels(0))],
        );
        assert_eq!(*img.get_pixel(0, 0), RED);
    }

    #[test]
    fn zone_width_zero_percent_skipped() {
        let mut img = red_image(10, 10);
        apply_masks(
            &mut img,
            &[zone(0, 0, Dimension::Percent(0.0), Dimension::Pixels(5))],
        );
        assert_eq!(*img.get_pixel(0, 0), RED);
    }

    #[test]
    fn zone_height_zero_percent_skipped() {
        let mut img = red_image(10, 10);
        apply_masks(
            &mut img,
            &[zone(0, 0, Dimension::Pixels(5), Dimension::Percent(0.0))],
        );
        assert_eq!(*img.get_pixel(0, 0), RED);
    }

    // ── apply_masks: in-bounds ───────────────────────────────────────────────

    #[test]
    fn in_bounds_pixel_mask_applied() {
        let mut img = red_image(100, 100);
        apply_masks(
            &mut img,
            &[zone(0, 0, Dimension::Pixels(100), Dimension::Pixels(20))],
        );
        // All pixels in [0..99, 0..19] must be black.
        assert_eq!(*img.get_pixel(0, 0), BLACK);
        assert_eq!(*img.get_pixel(99, 19), BLACK);
        // Row 20 must remain red.
        assert_eq!(*img.get_pixel(0, 20), RED);
    }

    #[test]
    fn in_bounds_percent_mask_applied() {
        let mut img = red_image(100, 100);
        // 100% width, 20% height → covers [0..99, 0..19]
        apply_masks(
            &mut img,
            &[zone(
                0,
                0,
                Dimension::Percent(100.0),
                Dimension::Percent(20.0),
            )],
        );
        assert_eq!(*img.get_pixel(0, 0), BLACK);
        assert_eq!(*img.get_pixel(99, 19), BLACK);
        assert_eq!(*img.get_pixel(0, 20), RED);
    }

    #[test]
    fn mask_fills_exact_boundary() {
        let mut img = red_image(10, 10);
        // A zone that exactly covers the full image — no clamping needed.
        let clamped = apply_masks(
            &mut img,
            &[zone(0, 0, Dimension::Pixels(10), Dimension::Pixels(10))],
        );
        assert_eq!(clamped, 0);
        for y in 0..10 {
            for x in 0..10 {
                assert_eq!(*img.get_pixel(x, y), BLACK);
            }
        }
    }

    #[test]
    fn mask_is_black_rgba_not_transparent() {
        let mut img = red_image(10, 10);
        apply_masks(
            &mut img,
            &[zone(0, 0, Dimension::Pixels(1), Dimension::Pixels(1))],
        );
        assert_eq!(*img.get_pixel(0, 0), Rgba([0, 0, 0, 255]));
    }

    // ── apply_masks: out-of-bounds clamping ──────────────────────────────────

    #[test]
    fn oob_x_end_clamped_to_width() {
        let mut img = red_image(100, 100);
        // x:90, width:20 → x_end=110, clamped to 100 → pixels [90..99] painted black
        let clamped = apply_masks(
            &mut img,
            &[
                zone(90, 0, Dimension::Pixels(20), Dimension::Pixels(100)),
                zone(0, 0, Dimension::Pixels(1), Dimension::Pixels(1)),
            ],
        );
        assert_eq!(clamped, 1, "only the zone reaching beyond the image counts");
        assert_eq!(*img.get_pixel(90, 0), BLACK);
        assert_eq!(*img.get_pixel(99, 0), BLACK);
        assert_eq!(*img.get_pixel(89, 0), RED);
    }

    #[test]
    fn clamped_zones_counts_like_apply_masks() {
        let zones = [
            zone(90, 0, Dimension::Pixels(20), Dimension::Pixels(100)),
            zone(0, 0, Dimension::Pixels(1), Dimension::Pixels(1)),
            zone(0, 99, Dimension::Percent(10.0), Dimension::Pixels(2)),
            zone(500, 500, Dimension::Pixels(1), Dimension::Pixels(1)),
            zone(500, 500, Dimension::Pixels(0), Dimension::Pixels(1)),
        ];
        assert_eq!(
            clamped_zones(&zones, 100, 100),
            3,
            "empty zones never count"
        );
        assert_eq!(apply_masks(&mut red_image(100, 100), &zones), 3);
        assert_eq!(clamped_zones(&zones, 1000, 1000), 0);
        assert_eq!(clamped_zones(&zones, 0, 100), 0);
    }

    #[test]
    fn oob_y_end_clamped_to_height() {
        let mut img = red_image(100, 100);
        // y:90, height:20 → y_end=110, clamped to 100 → rows [90..99] painted black
        apply_masks(
            &mut img,
            &[zone(0, 90, Dimension::Pixels(100), Dimension::Pixels(20))],
        );
        assert_eq!(*img.get_pixel(0, 90), BLACK);
        assert_eq!(*img.get_pixel(0, 99), BLACK);
        assert_eq!(*img.get_pixel(0, 89), RED);
    }

    #[test]
    fn oob_x_start_beyond_image_is_noop() {
        // x_start >= img_w → warn is emitted but no pixels changed
        let mut img = red_image(100, 100);
        apply_masks(
            &mut img,
            &[zone(100, 0, Dimension::Pixels(10), Dimension::Pixels(10))],
        );
        assert_eq!(*img.get_pixel(99, 0), RED);
    }

    #[test]
    fn oob_y_start_beyond_image_is_noop() {
        let mut img = red_image(100, 100);
        apply_masks(
            &mut img,
            &[zone(0, 100, Dimension::Pixels(10), Dimension::Pixels(10))],
        );
        assert_eq!(*img.get_pixel(0, 99), RED);
    }

    #[test]
    fn saturating_add_overflow_safe() {
        // x_start = u32::MAX - 1, width = 10 → saturating_add produces u32::MAX, clamped to img_w
        let mut img = red_image(10, 10);
        apply_masks(
            &mut img,
            &[zone(
                u32::MAX - 1,
                0,
                Dimension::Pixels(10),
                Dimension::Pixels(5),
            )],
        );
        // x_start >= img_w → all red pixels untouched.
        assert_eq!(*img.get_pixel(0, 0), RED);
    }

    // ── apply_masks: multiple zones ──────────────────────────────────────────

    #[test]
    fn multiple_zones_applied_independently() {
        let mut img = red_image(100, 100);
        let zones = vec![
            zone(0, 0, Dimension::Pixels(10), Dimension::Pixels(10)),
            zone(90, 90, Dimension::Pixels(10), Dimension::Pixels(10)),
        ];
        apply_masks(&mut img, &zones);
        assert_eq!(*img.get_pixel(0, 0), BLACK);
        assert_eq!(*img.get_pixel(99, 99), BLACK);
        assert_eq!(*img.get_pixel(50, 50), RED);
    }

    #[test]
    fn fast_path_full_width_applied() {
        let mut img = red_image(100, 100);
        // 0 to 100 width (full-width), 0 to 10 height
        apply_masks(
            &mut img,
            &[zone(0, 0, Dimension::Pixels(100), Dimension::Pixels(10))],
        );
        assert_eq!(*img.get_pixel(0, 0), BLACK);
        assert_eq!(*img.get_pixel(99, 9), BLACK);
        assert_eq!(*img.get_pixel(50, 10), RED);
    }

    #[test]
    fn test_fill_black_aligned_and_unaligned() {
        // Aligned buffer (multiple of 4 bytes)
        let mut buf_aligned = vec![255u8; 16];
        fill_black(&mut buf_aligned);
        assert_eq!(&buf_aligned[0..4], &[0, 0, 0, 255]);
        assert_eq!(&buf_aligned[12..16], &[0, 0, 0, 255]);

        // Unaligned slice (e.g. starting at offset 1)
        let mut buf_unaligned = [255u8; 17];
        fill_black(&mut buf_unaligned[1..17]);
        assert_eq!(&buf_unaligned[1..5], &[0, 0, 0, 255]);
        assert_eq!(&buf_unaligned[13..17], &[0, 0, 0, 255]);
    }
}
