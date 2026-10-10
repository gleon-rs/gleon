//! Perceptual hashing using the `image_hasher` crate.

use std::sync::OnceLock;

use image::RgbaImage;
use image_hasher::{HashAlg, HasherConfig};

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
    fn test_compute_phash() {
        let img1 = ImageBuffer::from_pixel(100, 100, Rgba([255, 0, 0, 255]));
        let img2 = ImageBuffer::from_pixel(100, 100, Rgba([255, 0, 0, 255]));

        let phash1 = compute_phash(&img1);
        assert!(phash1.starts_with("dhash:"));
        assert_eq!(phash1, compute_phash(&img2));
    }
}
