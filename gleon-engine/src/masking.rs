//! Safe ignore-zone masking engine implementation.

use image::RgbaImage;
use tracing::warn;

use crate::{
    Pixels,
    config::{Dimension, Zone},
    ssim::Region,
};

/// The pixel rectangles of `zones` in a `width` x `height` image, clamped to it.
///
/// Percentages resolve against the image's dimensions with mathematical rounding (`round()`).
/// A zone that reaches beyond the image is clamped to it and logged via `warn!`; the count of
/// such zones is returned too, so callers without a `tracing` subscriber (the FFI integrations)
/// can tell their users. Zones of zero width or height, or entirely outside the image, give no
/// rectangle (one outside the image still counts as clamped).
#[must_use]
pub fn resolve_zones(zones: &[Zone], width: u32, height: u32) -> (Vec<Region>, usize) {
    let mut clamped = 0;
    let regions = resolve(zones, width, height, |zone| {
        clamped += 1;
        warn!(
            "Mask zone extends beyond image bounds: \
             zone = x:{}, y:{}, w:{:?}, h:{:?}, image_dims = {}x{}",
            zone.x, zone.y, zone.width, zone.height, width, height
        );
    });
    (regions, clamped)
}

/// [`resolve_zones`] without counting or logging the zones that reach beyond the image: for the
/// diff of images of two sizes, where the sizes are the finding, not the masks.
#[must_use]
pub fn resolve_zones_quietly(zones: &[Zone], width: u32, height: u32) -> Vec<Region> {
    resolve(zones, width, height, |_| {})
}

/// The rectangles of `zones` clamped to a `width` x `height` image, calling `on_clamped` for each
/// zone that reaches beyond it.
fn resolve(
    zones: &[Zone],
    width: u32,
    height: u32,
    mut on_clamped: impl FnMut(&Zone),
) -> Vec<Region> {
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let mut regions = Vec::with_capacity(zones.len());
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
            on_clamped(zone);
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
    regions
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

/// How many of `zones` reach beyond a `width` x `height` image.
///
/// The count [`resolve_zones`] returns for such an image, without building the rectangles (for
/// callers that skip decoding, such as a byte-identical fast path).
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
    use super::*;
    use crate::config::Dimension;

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

    // ── resolve_zones: no rectangles ─────────────────────────────────────────

    fn region(x: u32, y: u32, width: u32, height: u32) -> Region {
        Region {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn empty_zones_give_nothing() {
        assert_eq!(resolve_zones(&[], 10, 10), (vec![], 0));
    }

    #[test]
    fn an_empty_image_gives_nothing() {
        let zones = [zone(0, 0, Dimension::Pixels(10), Dimension::Pixels(10))];
        assert_eq!(resolve_zones(&zones, 0, 0), (vec![], 0));
    }

    #[test]
    fn zones_of_zero_size_are_skipped() {
        for zone in [
            zone(0, 0, Dimension::Pixels(0), Dimension::Pixels(5)),
            zone(0, 0, Dimension::Pixels(5), Dimension::Pixels(0)),
            zone(0, 0, Dimension::Percent(0.0), Dimension::Pixels(5)),
            zone(0, 0, Dimension::Pixels(5), Dimension::Percent(0.0)),
        ] {
            assert_eq!(
                resolve_zones(std::slice::from_ref(&zone), 10, 10),
                (vec![], 0),
                "{zone:?}"
            );
        }
    }

    // ── resolve_zones: in bounds ─────────────────────────────────────────────

    #[test]
    fn in_bounds_pixel_zone() {
        let zones = [zone(0, 0, Dimension::Pixels(100), Dimension::Pixels(20))];
        assert_eq!(
            resolve_zones(&zones, 100, 100),
            (vec![region(0, 0, 100, 20)], 0)
        );
    }

    #[test]
    fn in_bounds_percent_zone() {
        // 100% width, 20% height
        let zones = [zone(
            0,
            0,
            Dimension::Percent(100.0),
            Dimension::Percent(20.0),
        )];
        assert_eq!(
            resolve_zones(&zones, 100, 100),
            (vec![region(0, 0, 100, 20)], 0)
        );
    }

    #[test]
    fn a_zone_of_the_whole_image_is_not_clamped() {
        let zones = [zone(0, 0, Dimension::Pixels(10), Dimension::Pixels(10))];
        assert_eq!(
            resolve_zones(&zones, 10, 10),
            (vec![region(0, 0, 10, 10)], 0)
        );
    }

    #[test]
    fn multiple_zones_resolve_independently() {
        let zones = [
            zone(0, 0, Dimension::Pixels(10), Dimension::Pixels(10)),
            zone(90, 90, Dimension::Pixels(10), Dimension::Pixels(10)),
        ];
        assert_eq!(
            resolve_zones(&zones, 100, 100),
            (vec![region(0, 0, 10, 10), region(90, 90, 10, 10)], 0)
        );
    }

    // ── resolve_zones: out-of-bounds clamping ────────────────────────────────

    #[test]
    fn oob_x_end_clamped_to_width() {
        // x:90, width:20 → x_end=110, clamped to 100
        let zones = [
            zone(90, 0, Dimension::Pixels(20), Dimension::Pixels(100)),
            zone(0, 0, Dimension::Pixels(1), Dimension::Pixels(1)),
        ];
        let (regions, clamped) = resolve_zones(&zones, 100, 100);
        assert_eq!(regions, vec![region(90, 0, 10, 100), region(0, 0, 1, 1)]);
        assert_eq!(clamped, 1, "only the zone reaching beyond the image counts");
    }

    #[test]
    fn oob_y_end_clamped_to_height() {
        // y:90, height:20 → y_end=110, clamped to 100
        let zones = [zone(0, 90, Dimension::Pixels(100), Dimension::Pixels(20))];
        assert_eq!(
            resolve_zones(&zones, 100, 100),
            (vec![region(0, 90, 100, 10)], 1)
        );
    }

    #[test]
    fn oob_percent_height_clamped() {
        // x:90, width:20px, height:100% of a 100x100 image (the former integration fixture)
        let zones = [zone(
            90,
            0,
            Dimension::Pixels(20),
            Dimension::Percent(100.0),
        )];
        assert_eq!(
            resolve_zones(&zones, 100, 100),
            (vec![region(90, 0, 10, 100)], 1)
        );
    }

    #[test]
    fn zones_starting_beyond_the_image_give_nothing_but_count() {
        for zone in [
            zone(100, 0, Dimension::Pixels(10), Dimension::Pixels(10)),
            zone(0, 100, Dimension::Pixels(10), Dimension::Pixels(10)),
            // saturating_add: x_end = u32::MAX
            zone(u32::MAX - 1, 0, Dimension::Pixels(10), Dimension::Pixels(5)),
        ] {
            assert_eq!(
                resolve_zones(std::slice::from_ref(&zone), 100, 100),
                (vec![], 1),
                "{zone:?}"
            );
        }
    }

    /// The log `run` writes on this thread, formatted, and what it returns.
    fn logged<T>(run: impl FnOnce() -> T) -> (T, String) {
        #[derive(Clone, Default)]
        struct Buffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Buffer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buffer = Buffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let value = tracing::subscriber::with_default(subscriber, run);
        let log = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        (value, log)
    }

    /// The zones of a diff of two sizes resolve alike, without a warning: the sizes are the
    /// finding, not the masks.
    #[test]
    fn quiet_resolution_resolves_alike_without_warning() {
        let zones = [
            zone(90, 0, Dimension::Pixels(20), Dimension::Pixels(100)),
            zone(0, 0, Dimension::Pixels(1), Dimension::Pixels(1)),
        ];
        let ((regions, clamped), loud) = logged(|| resolve_zones(&zones, 100, 100));
        assert_eq!(clamped, 1);
        assert!(
            loud.contains("Mask zone extends beyond image bounds"),
            "{loud}"
        );
        let (quiet_regions, quiet) = logged(|| resolve_zones_quietly(&zones, 100, 100));
        assert_eq!(quiet_regions, regions);
        assert_eq!(quiet, "");
    }

    #[test]
    fn clamped_zones_counts_like_resolve_zones() {
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
        assert_eq!(resolve_zones(&zones, 100, 100).1, 3);
        assert_eq!(clamped_zones(&zones, 1000, 1000), 0);
        assert_eq!(clamped_zones(&zones, 0, 100), 0);
    }
}
