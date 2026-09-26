//! Pixel-by-pixel image comparison.

use image::RgbaImage;
use rayon::prelude::*;

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
