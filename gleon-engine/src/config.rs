//! Comparison configuration types shared by the engine and its callers.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Comparison mode for visual regression testing.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
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

/// Configuration parameters for the diff engine.
// Never parsed itself: `gleon_model` reads the `diff` keys of a `gleon.yaml` rule and checks
// their ranges (`gleon_model::tolerance::ToleranceError`). The schema attributes describe those
// keys in the JSON Schema of the config.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(
    feature = "schemars",
    derive(schemars::JsonSchema),
    schemars(deny_unknown_fields)
)]
pub struct DiffConfig {
    /// Largest share [0.0, 1.0] of differing pixels in pixel mode: required by `mode: pixel`
    /// (0: every pixel identical, the pixel options aside).
    #[cfg_attr(
        feature = "schemars",
        schemars(
            default = "default_threshold",
            range(min = 0.0, max = 1.0),
            transform = without_default
        )
    )]
    pub threshold: f64,
    /// Minimum local SSIM [0.0, 1.0] every neighborhood must reach (SSIM mode).
    #[cfg_attr(
        feature = "schemars",
        schemars(default = "default_min_similarity", range(min = 0.0, max = 1.0))
    )]
    pub min_similarity: f64,
    /// Tolerated deviation in 8-bit channel units beyond the local 3x3 envelope (SSIM mode).
    #[cfg_attr(
        feature = "schemars",
        schemars(default = "default_color_tolerance", range(min = 0.0, max = 255.0))
    )]
    pub color_tolerance: f64,
    /// Pixel mode, outside text: a differing pixel counts as equal when none of its RGBA bytes
    /// differs by more than this, `[0, 254]` (default 0: off). For GPU and color-conversion
    /// drift.
    #[cfg_attr(feature = "schemars", schemars(default, range(max = 254)))]
    pub channel_tolerance: u8,
    /// Pixel mode, outside text: a differing pixel counts as equal when it looks anti-aliased in
    /// either image (the detection of pixelmatch: between a darker and a brighter neighbor that
    /// sit in flat areas of both images). Default off.
    #[cfg_attr(feature = "schemars", schemars(default))]
    pub anti_alias: bool,
    /// Pixel mode, outside text: a differing pixel counts as equal when the Sobel gradient of the
    /// baseline's luma there (8-bit units, unnormalized like Skia Gold's: a sharp step of 16
    /// gives 64) exceeds this, `[0, 254]`: the edges a sub-pixel shift or another rasterizer
    /// moves (Skia Gold's edge mask). Default 0: off. Hides every change that lies on edges too:
    /// on the calibration corpus a missing glyph, a card moved by 1px and, at 64, a missing small
    /// icon pass; the lower the value, the more pixels count as edges.
    #[cfg_attr(feature = "schemars", schemars(default, range(max = 254)))]
    pub edge_threshold: u8,
}

/// The largest `channel_tolerance` and `edge_threshold`: 255 would make every pixel equal or
/// hide nothing (gradients are capped at 255), so it is no valid setting.
pub const MAX_PIXEL_OPTION: u8 = 254;

/// The largest `color_tolerance`: a whole 8-bit channel.
pub const MAX_COLOR_TOLERANCE: f64 = 255.0;

impl Default for DiffConfig {
    fn default() -> Self {
        Self {
            threshold: default_threshold(),
            min_similarity: default_min_similarity(),
            color_tolerance: default_color_tolerance(),
            channel_tolerance: 0,
            anti_alias: false,
            edge_threshold: 0,
        }
    }
}

impl From<&DiffConfig> for crate::pixel::PixelOptions {
    fn from(config: &DiffConfig) -> Self {
        Self {
            channel_tolerance: config.channel_tolerance,
            anti_alias: config.anti_alias,
            edge_threshold: config.edge_threshold,
        }
    }
}

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

/// Drops the `default` of a field's schema: a rule names the value, a default would read as one
/// it falls back to.
#[cfg(feature = "schemars")]
fn without_default(schema: &mut schemars::Schema) {
    schema.remove("default");
}

const fn default_threshold() -> f64 {
    0.01
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
