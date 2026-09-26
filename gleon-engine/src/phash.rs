//! Perceptual hashing using the `image_hasher` crate.

use std::sync::OnceLock;

use image::RgbaImage;
use image_hasher::{HashAlg, HasherConfig};

/// Errors that can occur during perceptual hashing operations.
#[derive(Debug, thiserror::Error)]
pub enum PhashError {
    /// The phash string format is invalid.
    #[error("Invalid phash format: '{0}'")]
    InvalidFormat(String),

    /// The phash schemes do not match.
    #[error("Phash scheme mismatch: '{0}' vs '{1}'")]
    SchemeMismatch(String, String),

    /// The phash string contains invalid hex characters.
    #[error("Invalid hex in phash")]
    InvalidHex(#[from] hex::FromHexError),

    /// The two phash strings have different lengths.
    #[error("Phash length mismatch: {0} bytes vs {1} bytes")]
    LengthMismatch(usize, usize),
}

/// Computes the perceptual hash of the given image using the dHash (Gradient) algorithm.
/// Returns the hash as a string in the format `dhash:<hex>`.
pub fn compute_phash(img: &RgbaImage) -> String {
    static HASHER: OnceLock<image_hasher::Hasher> = OnceLock::new();
    let hasher = HASHER.get_or_init(|| {
        HasherConfig::new()
            .hash_alg(HashAlg::Gradient)
            .hash_size(8, 8)
            .to_hasher()
    });
    let hash = hasher.hash_image(img);
    let hex_val = hex::encode(hash.as_bytes());
    format!("dhash:{hex_val}")
}

/// Computes the Hamming distance between two phash strings.
/// The phash strings must be in the format `scheme:hex_value`.
///
/// Returns an error if the format is invalid, hex decoding fails, or lengths mismatch.
///
/// # Errors
/// Returns [`PhashError::InvalidFormat`] if either input isn't in `scheme:hex` form,
/// [`PhashError::SchemeMismatch`] if the two schemes differ, [`PhashError::InvalidHex`] if
/// hex decoding fails, or [`PhashError::LengthMismatch`] if the decoded byte lengths differ.
pub fn calculate_hamming_distance(phash1: &str, phash2: &str) -> Result<u32, PhashError> {
    let (scheme1, val1) = phash1
        .split_once(':')
        .ok_or_else(|| PhashError::InvalidFormat(phash1.to_string()))?;
    let (scheme2, val2) = phash2
        .split_once(':')
        .ok_or_else(|| PhashError::InvalidFormat(phash2.to_string()))?;

    if scheme1 != scheme2 {
        return Err(PhashError::SchemeMismatch(
            scheme1.to_string(),
            scheme2.to_string(),
        ));
    }

    let bytes1 = hex::decode(val1).map_err(PhashError::InvalidHex)?;
    let bytes2 = hex::decode(val2).map_err(PhashError::InvalidHex)?;

    if bytes1.len() != bytes2.len() {
        return Err(PhashError::LengthMismatch(bytes1.len(), bytes2.len()));
    }

    let distance = bytes1
        .iter()
        .zip(bytes2.iter())
        .map(|(b1, b2)| (b1 ^ b2).count_ones())
        .sum();

    Ok(distance)
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
    fn test_compute_phash_and_distance() {
        let img1 = ImageBuffer::from_pixel(100, 100, Rgba([255, 0, 0, 255]));
        let img2 = ImageBuffer::from_pixel(100, 100, Rgba([255, 0, 0, 255]));
        let mut img3 = ImageBuffer::from_pixel(100, 100, Rgba([255, 0, 0, 255]));
        // Make img3 slightly different
        img3.put_pixel(50, 50, Rgba([0, 255, 0, 255]));

        let phash1 = compute_phash(&img1);
        let phash2 = compute_phash(&img2);
        let phash3 = compute_phash(&img3);

        assert!(phash1.starts_with("dhash:"));
        assert_eq!(phash1, phash2);

        let dist1 = calculate_hamming_distance(&phash1, &phash2).unwrap();
        assert_eq!(dist1, 0);

        let dist2 = calculate_hamming_distance(&phash1, &phash3).unwrap();
        // Since they are very similar, distance should be very low (usually 0 or 1-2 bits)
        assert!(dist2 < 5);
    }

    #[test]
    fn test_calculate_hamming_distance_errors() {
        assert!(matches!(
            calculate_hamming_distance("dhash:0f", "invalid"),
            Err(PhashError::InvalidFormat(_))
        ));

        assert!(matches!(
            calculate_hamming_distance("dhash:0f", "dhash:zz"),
            Err(PhashError::InvalidHex(_))
        ));

        assert!(matches!(
            calculate_hamming_distance("dhash:0f", "dhash:0f00"),
            Err(PhashError::LengthMismatch(_, _))
        ));
    }
}
