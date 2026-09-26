//! Safe ignore-zone masking engine implementation.

use image::RgbaImage;
use tracing::warn;

use crate::config::{Dimension, Zone};

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
/// # Panics
/// Does not panic in practice: internal pixel-slice indices are always clamped to the
/// image's bounds before use, so the internal `.expect()` calls documenting that invariant
/// can never trigger.
pub fn apply_masks(img: &mut RgbaImage, zones: &[Zone]) {
    let img_w = img.width();
    let img_h = img.height();

    if img_w == 0 || img_h == 0 {
        return;
    }

    for zone in zones {
        let w_px = resolve_dimension(zone.width, img_w);
        let h_px = resolve_dimension(zone.height, img_h);

        if w_px == 0 || h_px == 0 {
            continue;
        }

        let x_end = zone.x.saturating_add(w_px);
        let y_end = zone.y.saturating_add(h_px);

        let is_out_of_bounds = zone.x >= img_w || zone.y >= img_h || x_end > img_w || y_end > img_h;

        if is_out_of_bounds {
            warn!(
                "Mask zone extends beyond image bounds: \
                 zone = x:{}, y:{}, w:{:?}, h:{:?}, image_dims = {}x{}",
                zone.x, zone.y, zone.width, zone.height, img_w, img_h
            );
        }

        // Clamp to image bounds. After this, all indices are guaranteed valid.
        let x_min = zone.x.min(img_w);
        let x_max = x_end.min(img_w);
        let y_min = zone.y.min(img_h);
        let y_max = y_end.min(img_h);

        // Guard both axes: if either dimension collapsed to zero after clamping, nothing to paint.
        if x_min == x_max || y_min == y_max {
            continue;
        }

        let img_w_usize = img_w as usize;
        // `.as_mut()` via the public `AsMut<[u8]>` impl — preferred over `&mut **img` (DerefMut hack).
        let raw_pixels: &mut [u8] = img.as_mut();

        // Fast path: if the mask spans the entire width, we can mutate the contiguous memory block at once.
        if x_min == 0 && x_max == img_w {
            let start_idx = (y_min as usize) * img_w_usize * 4;
            let end_idx = (y_max as usize) * img_w_usize * 4;

            #[expect(
                clippy::expect_used,
                reason = "indices are clamped to image bounds above"
            )]
            let block_slice = raw_pixels
                .get_mut(start_idx..end_idx)
                .expect("block indices are clamped to image bounds above");

            fill_black(block_slice);
            continue;
        }

        // O(mask_w * mask_h): compute byte offsets once per row, then fill the exact pixel slice.
        // Indices are guaranteed in-bounds: x_max <= img_w and y_max <= img_h after clamping above.
        for y in y_min..y_max {
            let row_start = (y as usize) * img_w_usize * 4;
            let start_idx = row_start + (x_min as usize) * 4;
            let end_idx = row_start + (x_max as usize) * 4;

            #[expect(
                clippy::expect_used,
                reason = "indices are clamped to image bounds above"
            )]
            let row_slice = raw_pixels
                .get_mut(start_idx..end_idx)
                .expect("pixel indices are clamped to image bounds above");

            fill_black(row_slice);
        }
    }
}

#[inline]
fn fill_black(slice: &mut [u8]) {
    let pixel_val = u32::from_ne_bytes([0, 0, 0, 255]);
    if let Ok(u32_slice) = bytemuck::try_cast_slice_mut::<u8, u32>(slice) {
        u32_slice.fill(pixel_val);
    } else {
        slice.as_chunks_mut::<4>().0.fill([0, 0, 0, 255]);
    }
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
        apply_masks(
            &mut img,
            &[zone(0, 0, Dimension::Pixels(10), Dimension::Pixels(10))],
        );
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
        apply_masks(
            &mut img,
            &[zone(90, 0, Dimension::Pixels(20), Dimension::Pixels(100))],
        );
        assert_eq!(*img.get_pixel(90, 0), BLACK);
        assert_eq!(*img.get_pixel(99, 0), BLACK);
        assert_eq!(*img.get_pixel(89, 0), RED);
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
