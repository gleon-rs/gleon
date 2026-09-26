//! Single test manifest schema and serialization.

use std::path::Path;

use gleon_engine::decode::{MAX_DIMENSION, MAX_PIXELS};
use serde::{Deserialize, Serialize};

use crate::{
    io::{load_json, save_json_atomically},
    manifest::{ImageHash, ManifestError},
};

/// Supported schema version for individual test manifests.
pub const SUPPORTED_SINGLE_MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Deterministic, noise-free manifest for a single visual regression test case.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SingleTestManifest {
    /// Schema version (always 1).
    pub schema_version: u32,
    /// Primary comparison digest (supported cryptographic schemes: sha256, sha512, sha384, sha224, blake3).
    pub hash: ImageHash,
    /// Perceptual hash digest (must be dhash or valid scheme).
    pub phash: ImageHash,
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
}

impl SingleTestManifest {
    /// Creates a new `SingleTestManifest` with validation.
    ///
    /// # Errors
    /// Returns [`ManifestError::Validation`] if `hash` is not a supported cryptographic
    /// digest scheme/length, `phash` is not a valid `dhash`, or `width`/`height` fail
    /// [`Self::validate_dimensions`].
    pub fn new(
        hash: ImageHash,
        phash: ImageHash,
        width: u32,
        height: u32,
    ) -> Result<Self, ManifestError> {
        let manifest = Self {
            schema_version: SUPPORTED_SINGLE_MANIFEST_SCHEMA_VERSION,
            hash,
            phash,
            width,
            height,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    /// Validates field constraints and digest schemes.
    ///
    /// # Errors
    /// Returns [`ManifestError::Validation`] if `schema_version` is unsupported, `hash`'s
    /// scheme/length/charset is invalid, `phash` is not a valid `dhash`, or the dimensions
    /// fail [`Self::validate_dimensions`].
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.schema_version != SUPPORTED_SINGLE_MANIFEST_SCHEMA_VERSION {
            return Err(ManifestError::Validation(format!(
                "Unsupported single manifest schema version: expected {}, got {}",
                SUPPORTED_SINGLE_MANIFEST_SCHEMA_VERSION, self.schema_version
            )));
        }

        if !matches!(
            self.hash.scheme(),
            "sha256" | "sha512" | "sha384" | "sha224" | "blake3"
        ) {
            return Err(ManifestError::Validation(format!(
                "Unsupported hash scheme '{}'",
                self.hash.scheme()
            )));
        }

        let expected_len = match self.hash.scheme() {
            "sha512" => 128,
            "sha384" => 96,
            "sha256" | "blake3" => 64,
            "sha224" => 56,
            _ => 0,
        };

        if expected_len > 0
            && (self.hash.value().len() != expected_len
                || !self
                    .hash
                    .value()
                    .chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)))
        {
            return Err(ManifestError::Validation(format!(
                "Invalid {} hash value: expected {} lowercase hex characters, got '{}'",
                self.hash.scheme(),
                expected_len,
                self.hash.value()
            )));
        }

        if self.phash.scheme() != "dhash" {
            return Err(ManifestError::Validation(format!(
                "Expected phash scheme 'dhash', got '{}'",
                self.phash.scheme()
            )));
        }

        if self.phash.value().len() != 16
            || !self
                .phash
                .value()
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        {
            return Err(ManifestError::Validation(format!(
                "Invalid dhash value: expected 16 lowercase hex characters, got '{}'",
                self.phash.value()
            )));
        }

        Self::validate_dimensions(self.width, self.height)?;

        Ok(())
    }

    /// Validates width and height constraints.
    ///
    /// # Errors
    /// Returns [`ManifestError::Validation`] if either dimension is zero, exceeds
    /// [`MAX_DIMENSION`], or the total pixel count exceeds [`MAX_PIXELS`].
    pub fn validate_dimensions(width: u32, height: u32) -> Result<(), ManifestError> {
        if width == 0 || height == 0 {
            return Err(ManifestError::Validation(format!(
                "Invalid image dimensions: {width}x{height} (must be > 0)"
            )));
        }

        if width > MAX_DIMENSION || height > MAX_DIMENSION {
            return Err(ManifestError::Validation(format!(
                "Image dimensions {width}x{height} exceed the maximum allowed {MAX_DIMENSION}x{MAX_DIMENSION}"
            )));
        }

        let total_pixels = u64::from(width) * u64::from(height);
        if total_pixels > MAX_PIXELS {
            return Err(ManifestError::Validation(format!(
                "Total pixel count {total_pixels} ({width}x{height}) exceeds maximum allowed budget {MAX_PIXELS}"
            )));
        }

        Ok(())
    }

    fn make_limited_reader(
        bytes: &[u8],
    ) -> Result<image::ImageReader<std::io::Cursor<&[u8]>>, ManifestError> {
        gleon_engine::decode::limited_reader(bytes).map_err(ManifestError::StdIo)
    }

    /// Safely validates image dimensions from raw bytes before fully decoding the image.
    /// This prevents OOM (Out Of Memory) `DoS` attacks from decompression bombs.
    ///
    /// # Errors
    /// Returns [`ManifestError::StdIo`] if the format cannot be guessed, [`ManifestError::Image`]
    /// if the image header cannot be read, or [`ManifestError::Validation`] if the declared
    /// dimensions fail [`Self::validate_dimensions`].
    pub fn validate_image_bytes(bytes: &[u8]) -> Result<(), ManifestError> {
        Self::make_limited_reader(bytes)
            .and_then(|reader| reader.into_dimensions().map_err(ManifestError::Image))
            .and_then(|(width, height)| Self::validate_dimensions(width, height))
    }

    /// Safely decodes an image with enforced resource and dimension limits.
    ///
    /// # Errors
    /// Returns [`ManifestError::Validation`] if [`Self::validate_image_bytes`] rejects the
    /// declared dimensions, or [`ManifestError::Image`] if the image cannot be decoded.
    pub fn load_image_from_bytes(bytes: &[u8]) -> Result<image::DynamicImage, ManifestError> {
        Self::validate_image_bytes(bytes)
            .and_then(|()| Self::make_limited_reader(bytes))
            .and_then(|reader| reader.decode().map_err(ManifestError::Image))
    }

    /// Load a single test manifest from a JSON file.
    ///
    /// # Errors
    /// Returns [`ManifestError::Io`] if the file cannot be read or is not valid JSON, or
    /// [`ManifestError::Validation`] if the deserialized manifest fails [`Self::validate`].
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, ManifestError> {
        let path = path.as_ref();
        tracing::debug!("Loading single test manifest from {:?}", path);
        let manifest: Self = load_json(path)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Save a single test manifest to a JSON file atomically.
    ///
    /// # Errors
    /// Returns [`ManifestError::Validation`] if `self` fails [`Self::validate`], or
    /// [`ManifestError::Io`] if the file cannot be written.
    pub fn save<P: AsRef<Path>>(&self, path: P) -> Result<(), ManifestError> {
        let path = path.as_ref();
        tracing::debug!("Saving single test manifest to {:?}", path);
        self.validate()?;
        save_json_atomically(path, self)?;
        Ok(())
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
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn test_single_manifest_lifecycle() {
        let temp = tempdir().unwrap();
        let file_path = temp.path().join("login.json");

        let hash = ImageHash::new("sha256", "a".repeat(64)).unwrap();
        let phash = ImageHash::new("dhash", "0000000000000000").unwrap();

        let manifest = SingleTestManifest::new(hash, phash, 1080, 1920).unwrap();
        manifest.save(&file_path).unwrap();

        let loaded = SingleTestManifest::load(&file_path).unwrap();
        assert_eq!(manifest, loaded);
    }

    #[test]
    fn test_invalid_single_manifest() {
        assert!(SingleTestManifest::validate_dimensions(16385, 100).is_err());
        assert!(SingleTestManifest::validate_dimensions(100, 16385).is_err());
        assert!(SingleTestManifest::validate_dimensions(10000, 10000).is_err());

        let primary_digest = ImageHash::new("sha256", "a".repeat(64)).unwrap();
        let invalid_phash_scheme = ImageHash::new("md5", "0000000000000000").unwrap();
        assert!(
            SingleTestManifest::new(primary_digest.clone(), invalid_phash_scheme, 100, 100)
                .is_err()
        );

        let invalid_phash_val = ImageHash::new("dhash", "short").unwrap();
        assert!(
            SingleTestManifest::new(primary_digest.clone(), invalid_phash_val, 100, 100).is_err()
        );

        let uppercase_hash = ImageHash::new("sha256", "A".repeat(64)).unwrap();
        let perceptual_digest = ImageHash::new("dhash", "0000000000000000").unwrap();
        let manifest_uppercase =
            SingleTestManifest::new(uppercase_hash, perceptual_digest.clone(), 100, 100).unwrap();
        assert_eq!(manifest_uppercase.hash.value(), "a".repeat(64));

        // Zero dimensions
        assert!(
            SingleTestManifest::new(primary_digest.clone(), perceptual_digest.clone(), 0, 100)
                .is_err()
        );

        // Exceeds max dimensions
        assert!(
            SingleTestManifest::new(primary_digest, perceptual_digest, MAX_DIMENSION + 1, 100)
                .is_err()
        );
    }

    #[test]
    fn test_validate_image_bytes_strict_limits() {
        let invalid_png_bytes = b"not a png image at all";
        assert!(SingleTestManifest::validate_image_bytes(invalid_png_bytes).is_err());

        // Create a 1x1 PNG image and test successful validation
        let mut img_bytes = Vec::new();
        let img = image::RgbaImage::new(1, 1);
        img.write_to(
            &mut std::io::Cursor::new(&mut img_bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
        assert!(SingleTestManifest::validate_image_bytes(&img_bytes).is_ok());

        let decoded = SingleTestManifest::load_image_from_bytes(&img_bytes).unwrap();
        assert_eq!(decoded.width(), 1);
        assert_eq!(decoded.height(), 1);

        assert!(SingleTestManifest::load_image_from_bytes(invalid_png_bytes).is_err());
    }

    #[test]
    fn test_unsupported_schema_version() {
        let hash = ImageHash::new("sha256", "a".repeat(64)).unwrap();
        let phash = ImageHash::new("dhash", "0000000000000000").unwrap();
        let mut manifest = SingleTestManifest::new(hash, phash, 100, 100).unwrap();
        manifest.schema_version = 99;
        assert!(manifest.validate().is_err());
    }
}
