//! Resource-limited image decoding.
//!
//! Every decode of untrusted image bytes must go through these limits to prevent OOM
//! allocations and decompression bombs.

use image::{ImageReader, Limits, RgbaImage};
use thiserror::Error;

/// Maximum allowed width or height in pixels to prevent OOM allocations.
pub const MAX_DIMENSION: u32 = 16384;

/// Maximum allowed total decoded pixels (67,108,864 = 8192x8192) to prevent decompression bombs.
pub const MAX_PIXELS: u64 = 67_108_864;

/// Errors produced while decoding image bytes.
#[derive(Debug, Error)]
pub enum DecodeError {
    /// The image format could not be determined from the bytes.
    #[error("failed to detect image format: {0}")]
    Format(#[source] std::io::Error),
    /// The image header or pixel data could not be decoded.
    #[error("failed to decode image: {0}")]
    Image(#[source] image::ImageError),
    /// The declared dimensions exceed [`MAX_DIMENSION`] or [`MAX_PIXELS`].
    #[error(
        "image dimensions {width}x{height} exceed the allowed budget ({MAX_DIMENSION}px per side, {MAX_PIXELS} pixels total)"
    )]
    TooLarge {
        /// Declared width in pixels.
        width: u32,
        /// Declared height in pixels.
        height: u32,
    },
}

/// Bytes per pixel of the widest buffer the decoder allocates (16-bit RGBA PNGs decode to
/// RGBA16 before conversion to RGBA8).
const MAX_BYTES_PER_PIXEL: u64 = 8;

/// Decoder limits derived from [`MAX_DIMENSION`] and [`MAX_PIXELS`].
#[must_use]
pub fn limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_PIXELS * MAX_BYTES_PER_PIXEL);
    limits
}

/// Returns whether `width` x `height` fits within [`MAX_DIMENSION`] and [`MAX_PIXELS`].
#[must_use]
pub const fn fits_budget(width: u32, height: u32) -> bool {
    width <= MAX_DIMENSION
        && height <= MAX_DIMENSION
        && (width as u64) * (height as u64) <= MAX_PIXELS
}

/// An image reader over `bytes` with the format guessed and [`limits`] applied.
///
/// # Errors
/// Returns the I/O error of format detection.
pub fn limited_reader(bytes: &[u8]) -> std::io::Result<ImageReader<std::io::Cursor<&[u8]>>> {
    ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map(|mut reader| {
            reader.limits(limits());
            reader
        })
}

/// Decodes `bytes` into straight (non-premultiplied) RGBA8, validating the declared dimensions
/// against the budget before any pixel buffer is allocated.
///
/// # Errors
/// Returns [`DecodeError::TooLarge`] if the header declares dimensions over budget,
/// [`DecodeError::Format`] if the format cannot be detected, or [`DecodeError::Image`] if the
/// header or pixel data is invalid.
pub fn decode_rgba(bytes: &[u8]) -> Result<RgbaImage, DecodeError> {
    let (width, height) = limited_reader(bytes)
        .map_err(DecodeError::Format)?
        .into_dimensions()
        .map_err(DecodeError::Image)?;
    if !fits_budget(width, height) {
        return Err(DecodeError::TooLarge { width, height });
    }
    limited_reader(bytes)
        .map_err(DecodeError::Format)?
        .decode()
        .map(|img| img.to_rgba8())
        .map_err(DecodeError::Image)
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
    use image::{ImageBuffer, ImageFormat, Rgba};

    use super::*;

    fn encode_png(width: u32, height: u32) -> Vec<u8> {
        let img: RgbaImage = ImageBuffer::from_pixel(width, height, Rgba([10, 20, 30, 40]));
        let mut bytes = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        bytes
    }

    #[test]
    fn test_decode_rgba_roundtrip_keeps_straight_alpha() {
        let decoded = decode_rgba(&encode_png(3, 2)).unwrap();
        assert_eq!(decoded.dimensions(), (3, 2));
        assert_eq!(decoded.get_pixel(0, 0), &Rgba([10, 20, 30, 40]));
    }

    #[test]
    fn test_decode_rgba_rejects_garbage() {
        assert!(matches!(
            decode_rgba(b"definitely not an image"),
            Err(DecodeError::Format(_) | DecodeError::Image(_))
        ));
    }

    #[test]
    fn test_decode_rgba_accepts_16_bit_png() {
        let img: ImageBuffer<Rgba<u16>, Vec<u16>> =
            ImageBuffer::from_pixel(4, 3, Rgba([65535, 0, 32768, 65535]));
        let mut bytes = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        let decoded = decode_rgba(&bytes).unwrap();
        assert_eq!(decoded.get_pixel(0, 0), &Rgba([255, 0, 128, 255]));
    }

    #[test]
    fn test_decode_rgba_rejects_header_over_pixel_budget_before_decoding() {
        // A PNG declaring 16384x4097 (per-side limits pass, total pixels do not) with an empty
        // IDAT, so reaching the decoder would fail with a different error.
        let mut ihdr = b"IHDR".to_vec();
        ihdr.extend_from_slice(&16384u32.to_be_bytes());
        ihdr.extend_from_slice(&4097u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(&ihdr);
        bytes.extend_from_slice(&crc32(&ihdr).to_be_bytes());
        for chunk in [&b"IDAT"[..], &b"IEND"[..]] {
            bytes.extend_from_slice(&0u32.to_be_bytes());
            bytes.extend_from_slice(chunk);
            bytes.extend_from_slice(&crc32(chunk).to_be_bytes());
        }
        assert!(matches!(
            decode_rgba(&bytes),
            Err(DecodeError::TooLarge {
                width: 16384,
                height: 4097
            })
        ));
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &byte in data {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    #[test]
    fn test_fits_budget_boundaries() {
        assert!(fits_budget(MAX_DIMENSION, 4096));
        assert!(!fits_budget(MAX_DIMENSION + 1, 1));
        assert!(!fits_budget(MAX_DIMENSION, MAX_DIMENSION));
    }
}
