//! Comparison configuration types shared by the engine and its callers.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{pixel::PixelOptions, ssim::SsimPolicy};

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
                // Exactly the grammar of the JSON Schema below: `u32::from_str` and
                // `f64::from_str` alone would also accept signs, exponents and `.5`.
                trimmed.strip_suffix('%').map_or_else(
                    || {
                        if is_decimal(trimmed, false) {
                            trimmed
                                .parse::<u32>()
                                .map(Dimension::Pixels)
                                .map_err(D::Error::custom)
                        } else {
                            Err(D::Error::custom(format!(
                                "expected a pixel count or a percentage such as \"25%\", got \"{trimmed}\""
                            )))
                        }
                    },
                    |pct| {
                        let pct = pct.trim_end();
                        if !is_decimal(pct, true) {
                            return Err(D::Error::custom(format!(
                                "expected a percentage such as \"25%\", got \"{trimmed}\""
                            )));
                        }
                        pct.parse::<f64>()
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

/// Whether `s` is ASCII digits, optionally (`fraction`) followed by `.` and more digits.
fn is_decimal(s: &str, fraction: bool) -> bool {
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    match s.split_once('.') {
        Some((int, frac)) => fraction && digits(int) && digits(frac),
        None => digits(s),
    }
}

#[cfg(feature = "schemars")]
impl schemars::JsonSchema for Dimension {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Dimension".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "description": "A pixel count, or a percentage of the image size such as \"25%\".",
            "oneOf": [
                { "type": "integer", "minimum": 0, "maximum": u32::MAX },
                { "type": "string", "pattern": "^\\s*(\\d+|\\d+(\\.\\d+)?\\s*%)\\s*$" }
            ]
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

/// What a comparison tolerates: the input of [`crate::compare_images`].
///
/// Never parsed: `gleon_model` reads the `diff` keys of a `gleon.yaml` rule into its tolerance
/// and builds this from it for each comparison.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Policy {
    /// Pixel by pixel (exact with a ratio of 0 and strict options).
    Pixel {
        /// Largest share `[0, 1]` of the strictly compared pixels that may differ (0: every
        /// pixel identical, the options aside).
        max_diff_ratio: f64,
        /// The differing pixels outside text that count as equal ([`PixelOptions`]).
        options: PixelOptions,
    },
    /// Structural similarity with the color envelope ([`crate::ssim`]).
    Ssim(SsimPolicy),
}

/// The default `min_similarity` of SSIM mode.
pub const DEFAULT_MIN_SIMILARITY: f64 = 0.8;

/// The default `color_tolerance` of SSIM mode.
pub const DEFAULT_COLOR_TOLERANCE: f64 = 8.0;

/// The largest `channel_tolerance` and `edge_threshold`: 255 would make every pixel equal or
/// hide nothing (gradients are capped at 255), so it is no valid setting.
pub const MAX_PIXEL_OPTION: u8 = 254;

/// The largest `color_tolerance`: a whole 8-bit channel.
pub const MAX_COLOR_TOLERANCE: f64 = 255.0;

/// A bounding zone to ignore.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
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
    fn test_dimension_strings_follow_the_schema_grammar() {
        let parse = |yaml: &str| serde_yaml::from_str::<Dimension>(yaml);
        assert_eq!(parse("\" 12.5 % \"").unwrap(), Dimension::Percent(12.5));
        assert_eq!(parse("\"0%\"").unwrap(), Dimension::Percent(0.0));
        assert_eq!(parse("\" 7 \"").unwrap(), Dimension::Pixels(7));
        // `f64`/`u32` parsing alone would accept all of these.
        for bad in [
            "+5%", "1e1%", ".5%", "5.%", "inf%", "NaN%", "+150", "1.5", "12 5",
        ] {
            let err = parse(&format!("\"{bad}\"")).unwrap_err().to_string();
            assert!(err.contains("expected a"), "{bad}: {err}");
        }
    }
}
