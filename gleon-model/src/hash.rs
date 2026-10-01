//! Content hashes of images as `scheme:value` strings (`sha256:<64 hex>`, `dhash:<16 hex>`), the
//! form of manifests and of the `golden.blob` of case reports.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A string that is not a valid [`ImageHash`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InvalidImageHash {
    /// No `:` between the scheme and the value.
    #[error("Hash must be in 'scheme:value' format")]
    MissingSeparator,
    /// An empty scheme.
    #[error("Hash scheme cannot be empty")]
    EmptyScheme,
    /// A scheme with characters other than ASCII alphanumerics, `_` and `-`.
    #[error("Hash scheme contains invalid characters")]
    InvalidScheme,
    /// An empty value.
    #[error("Hash value cannot be empty")]
    EmptyValue,
    /// A `sha256` value that is not 64 characters long.
    #[error("sha256 hash must be exactly 64 characters long")]
    Sha256Length,
    /// A `sha256` value with characters other than hex digits.
    #[error("sha256 hash must contain only ASCII hexadecimal characters")]
    Sha256Digits,
    /// A value with characters other than ASCII alphanumerics, `_` and `-`.
    #[error("Hash value contains invalid characters")]
    InvalidValue,
}

/// A strongly-typed image comparison hash, serialized as a `scheme:value` string.
///
/// Both parts are lowercased when parsed, so equal hashes compare equal whatever their case.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ImageHash {
    /// The hashing scheme/algorithm (e.g. "sha256", "phash", "dhash", "ssim").
    scheme: String,
    /// The hex or alphanumeric representation of the hash.
    value: String,
}

fn is_token(text: &str) -> bool {
    text.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn validate_hash_parts(scheme: &str, value: &str) -> Result<(), InvalidImageHash> {
    if scheme.is_empty() {
        return Err(InvalidImageHash::EmptyScheme);
    }
    if !is_token(scheme) {
        return Err(InvalidImageHash::InvalidScheme);
    }
    if value.is_empty() {
        return Err(InvalidImageHash::EmptyValue);
    }

    // Strict Cryptographic Digest Checks
    if scheme == "sha256" {
        if value.len() != 64 {
            return Err(InvalidImageHash::Sha256Length);
        }
        if !value.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(InvalidImageHash::Sha256Digits);
        }
    } else if !is_token(value) {
        return Err(InvalidImageHash::InvalidValue);
    }
    Ok(())
}

impl ImageHash {
    /// Constructs a new `ImageHash`, returning a validation error if invalid.
    ///
    /// # Errors
    /// Returns [`InvalidImageHash`] if `scheme` is empty or contains characters other than ASCII
    /// alphanumeric characters, `_`, or `-`; if `value` is empty or contains invalid characters
    /// for the given scheme; or if `scheme` is `sha256` and `value` is not exactly 64 ASCII hex
    /// characters.
    pub fn new(
        scheme: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<Self, InvalidImageHash> {
        let mut scheme_str = scheme.into();
        if scheme_str.bytes().any(|b| b.is_ascii_uppercase()) {
            scheme_str.make_ascii_lowercase();
        }
        let mut value_str = value.into();
        if value_str.bytes().any(|b| b.is_ascii_uppercase()) {
            value_str.make_ascii_lowercase();
        }
        validate_hash_parts(&scheme_str, &value_str).map(|()| Self {
            scheme: scheme_str,
            value: value_str,
        })
    }

    /// Gets the hashing scheme.
    #[must_use]
    pub fn scheme(&self) -> &str {
        &self.scheme
    }

    /// Gets the hash value.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl std::str::FromStr for ImageHash {
    type Err = InvalidImageHash;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (scheme, value) = s
            .split_once(':')
            .ok_or(InvalidImageHash::MissingSeparator)?;

        let scheme_cow = if scheme.bytes().any(|b| b.is_ascii_uppercase()) {
            std::borrow::Cow::Owned(scheme.to_ascii_lowercase())
        } else {
            std::borrow::Cow::Borrowed(scheme)
        };

        let value_cow = if value.bytes().any(|b| b.is_ascii_uppercase()) {
            std::borrow::Cow::Owned(value.to_ascii_lowercase())
        } else {
            std::borrow::Cow::Borrowed(value)
        };

        validate_hash_parts(&scheme_cow, &value_cow).map(|()| Self {
            scheme: scheme_cow.into_owned(),
            value: value_cow.into_owned(),
        })
    }
}

impl<'de> Deserialize<'de> for ImageHash {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = std::borrow::Cow::<'de, str>::deserialize(deserializer)?;
        s.parse::<Self>().map_err(serde::de::Error::custom)
    }
}

impl Serialize for ImageHash {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl std::fmt::Display for ImageHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.scheme, self.value)
    }
}

impl schemars::JsonSchema for ImageHash {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ImageHash".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "description": "Content hash as `scheme:value`, lowercase, e.g. `sha256:<64 hex digits>`.",
            "type": "string",
            "pattern": "^[a-z0-9_-]+:[a-z0-9_-]+$"
        })
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

    const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn test_image_hash_parse_and_display() {
        let hash = format!("sha256:{HEX}").parse::<ImageHash>().unwrap();
        assert_eq!(hash.scheme(), "sha256");
        assert_eq!(hash.value(), HEX);
        assert_eq!(hash.to_string(), format!("sha256:{HEX}"));
    }

    #[test]
    fn test_image_hash_uppercase_normalization() {
        let upper = HEX.to_ascii_uppercase();
        let hash = format!("SHA256:{upper}").parse::<ImageHash>().unwrap();
        assert_eq!(hash.scheme(), "sha256");
        assert_eq!(hash.value(), HEX);

        let hash_new = ImageHash::new("SHA256", upper).unwrap();
        assert_eq!(hash_new, hash);
    }

    #[test]
    fn test_invalid_hashes() {
        for (text, error) in [
            ("sha256", InvalidImageHash::MissingSeparator),
            (":abc", InvalidImageHash::EmptyScheme),
            ("sha 256:abc", InvalidImageHash::InvalidScheme),
            ("dhash:", InvalidImageHash::EmptyValue),
            ("sha256:abc", InvalidImageHash::Sha256Length),
            (
                &format!("sha256:{}", "g".repeat(64)),
                InvalidImageHash::Sha256Digits,
            ),
            ("dhash:ab/cd", InvalidImageHash::InvalidValue),
        ] {
            assert_eq!(text.parse::<ImageHash>(), Err(error), "{text}");
        }
        assert_eq!(
            ImageHash::new("", "abc"),
            Err(InvalidImageHash::EmptyScheme)
        );
        assert_eq!(
            InvalidImageHash::MissingSeparator.to_string(),
            "Hash must be in 'scheme:value' format"
        );
    }

    #[test]
    fn test_serde_round_trip() {
        let hash: ImageHash = serde_json::from_value(serde_json::json!("DHASH:00FF")).unwrap();
        assert_eq!(serde_json::to_value(&hash).unwrap(), "dhash:00ff");
        assert!(serde_json::from_value::<ImageHash>(serde_json::json!("nope")).is_err());
    }
}
