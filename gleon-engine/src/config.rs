//! Comparison configuration types shared by the engine and its callers.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Comparison mode for visual regression testing.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Pixel-by-pixel color comparison.
    Pixel,
    /// Structural Similarity Index comparison.
    Ssim,
}

/// Dimension value that can be specified either in pixels or as a percentage of the image size.
#[derive(Debug, Copy, Clone, PartialEq)]
pub enum Dimension {
    /// Absolute size in pixels.
    Pixels(u32),
    /// Relative size as a percentage [0.0, 100.0].
    Percent(f64),
}

impl<'de> Deserialize<'de> for Dimension {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::Error;

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum RawDimension<'a> {
            Integer(u32),
            Str(std::borrow::Cow<'a, str>),
        }

        RawDimension::deserialize(deserializer).and_then(|raw| match raw {
            RawDimension::Integer(px) => Ok(Self::Pixels(px)),
            RawDimension::Str(s) => {
                let trimmed = s.trim();
                trimmed.strip_suffix('%').map_or_else(
                    || {
                        trimmed
                            .parse::<u32>()
                            .map(Dimension::Pixels)
                            .map_err(D::Error::custom)
                    },
                    |pct| {
                        pct.trim()
                            .parse::<f64>()
                            .map_err(D::Error::custom)
                            .and_then(|val| {
                                if (0.0..=100.0).contains(&val) {
                                    Ok(Self::Percent(val))
                                } else {
                                    Err(D::Error::custom(
                                        "percentage must be between 0.0 and 100.0",
                                    ))
                                }
                            })
                    },
                )
            }
        })
    }
}

impl Serialize for Dimension {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Pixels(px) => serializer.serialize_u32(*px),
            Self::Percent(pct) => serializer.collect_str(&format_args!("{pct}%")),
        }
    }
}

/// Configuration parameters for the diff engine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DiffConfig {
    /// Pixel comparison threshold [0.0, 1.0].
    #[serde(default = "default_threshold", deserialize_with = "deserialize_ratio")]
    pub threshold: f64,
    /// Whether to apply anti-aliasing detection.
    #[serde(default = "default_anti_alias")]
    pub anti_alias: bool,
    /// Minimum local SSIM [0.0, 1.0] every neighborhood must reach (SSIM mode, see
    /// [`crate::ssim`]).
    #[serde(
        default = "default_min_similarity",
        deserialize_with = "deserialize_ratio"
    )]
    pub min_similarity: f64,
    /// Tolerated deviation in 8-bit channel units beyond the local 3x3 envelope (SSIM mode, see
    /// [`crate::ssim`]).
    #[serde(
        default = "default_color_tolerance",
        deserialize_with = "deserialize_color_tolerance"
    )]
    pub color_tolerance: f64,
}

fn deserialize_color_tolerance<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    f64::deserialize(deserializer).and_then(|val| {
        if val.is_finite() && val >= 0.0 {
            Ok(val)
        } else {
            Err(serde::de::Error::custom(
                "color_tolerance must be a finite, non-negative 8-bit channel amount",
            ))
        }
    })
}

fn deserialize_ratio<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    f64::deserialize(deserializer).and_then(|val| {
        if (0.0..=1.0).contains(&val) {
            Ok(val)
        } else {
            Err(serde::de::Error::custom(
                "Value must be between 0.0 and 1.0",
            ))
        }
    })
}

impl Default for DiffConfig {
    fn default() -> Self {
        Self {
            threshold: default_threshold(),
            anti_alias: default_anti_alias(),
            min_similarity: default_min_similarity(),
            color_tolerance: default_color_tolerance(),
        }
    }
}

/// A bounding zone to ignore.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Zone {
    /// The X coordinate of the top-left corner.
    pub x: u32,
    /// The Y coordinate of the top-left corner.
    pub y: u32,
    /// Width of the zone.
    pub width: Dimension,
    /// Height of the zone.
    pub height: Dimension,
}

const fn default_threshold() -> f64 {
    0.1
}

const fn default_anti_alias() -> bool {
    true
}

const fn default_min_similarity() -> f64 {
    0.8
}

const fn default_color_tolerance() -> f64 {
    8.0
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
    use super::*;

    #[test]
    fn test_dimension_deserialization_and_serialization() {
        // Test integer pixels
        let d1: Dimension = serde_yaml::from_str("100").unwrap();
        assert_eq!(d1, Dimension::Pixels(100));
        assert_eq!(serde_yaml::to_string(&d1).unwrap().trim(), "100");

        // Test string pixels
        let d2: Dimension = serde_yaml::from_str("\"150\"").unwrap();
        assert_eq!(d2, Dimension::Pixels(150));
        assert_eq!(serde_yaml::to_string(&d2).unwrap().trim(), "150");

        // Test valid percentage
        let d3: Dimension = serde_yaml::from_str("\"50%\"").unwrap();
        assert_eq!(d3, Dimension::Percent(50.0));
        assert_eq!(serde_yaml::to_string(&d3).unwrap().trim(), "50%");

        // Test invalid negative percentage
        let d_neg_pct: Result<Dimension, _> = serde_yaml::from_str("\"-5%\"");
        assert!(d_neg_pct.is_err());

        // Test invalid excessive percentage
        let d_exc_pct: Result<Dimension, _> = serde_yaml::from_str("\"105%\"");
        assert!(d_exc_pct.is_err());

        // Test invalid format
        let d_invalid: Result<Dimension, _> = serde_yaml::from_str("\"not_a_number\"");
        assert!(d_invalid.is_err());

        // Test invalid float inside percentage
        let d_invalid_pct_float: Result<Dimension, _> = serde_yaml::from_str("\"abc%\"");
        assert!(d_invalid_pct_float.is_err());
    }

    #[test]
    fn test_diff_config_rejects_out_of_range_values() {
        let err = |yaml: &str| {
            serde_yaml::from_str::<DiffConfig>(yaml)
                .unwrap_err()
                .to_string()
        };
        assert!(err("min_similarity: 1.5").contains("between 0.0 and 1.0"));
        assert!(err("threshold: -0.1").contains("between 0.0 and 1.0"));
        assert!(err("color_tolerance: -1").contains("finite, non-negative"));
        let defaults: DiffConfig = serde_yaml::from_str("{}").unwrap();
        assert_eq!(defaults, DiffConfig::default());
    }
}
