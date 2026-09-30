//! Per-golden case report written to `.gleon/runs/latest/cases/<name>.json`, plus the comparison
//! metrics shared with the `gleon-ffi` report.
//!
//! Case reports are the input of future `gleon report` / dashboard aggregation; the schema is
//! committed as `schema/case.v1.json`. Enums are internally tagged (`"kind"`) and every name is
//! `snake_case`, so non-Rust writers never mirror Rust type names.
//!
//! The directory holds the latest result of every golden: integrations run tests in many
//! processes, so no single writer can reset it, and each report replaces the previous one of its
//! golden. Reports of goldens that were since removed or renamed stay behind until `gleon clean`;
//! readers skip reports whose `golden.path` no longer exists and group a run by `recorded_at`. The
//! gleon CLI never deletes this directory outside `gleon clean`.

use gleon_engine::{Measurement, Region, config::Zone};
use serde::{Deserialize, Serialize};

use crate::{platform::PlatformConfig, tolerance::Tolerance};

/// Version of the case report format.
pub const CASE_SCHEMA_VERSION: u32 = 1;

/// Directory of the case reports, relative to `.gleon/`.
pub const CASES_DIR: &str = "runs/latest/cases";

/// Comparison metrics of one image region.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Metrics {
    /// Exact and pixel tolerances.
    Pixel {
        /// Pixels compared.
        total_pixels: u64,
        /// Pixels whose RGBA bytes differ.
        diff_pixels: u64,
        /// `diff_pixels / total_pixels` (0 for an empty image).
        diff_ratio: f64,
        /// `max_diff_ratio - diff_ratio` (exact: `-diff_ratio`); negative means it failed.
        headroom: f64,
    },
    /// SSIM tolerance (values as defined by the engine's `SsimAnalysis`).
    Ssim {
        /// Lowest local SSIM, gated by `min_similarity`.
        min_ssim: f64,
        /// Mean local SSIM; diagnostic only.
        mean_ssim: f64,
        /// Largest envelope deviation over the changed pixels, `color_tolerance` not subtracted.
        peak_excess: f64,
        /// Pixels whose RGBA bytes differ.
        changed_pixels: u64,
        /// Bounding box of the changed pixels.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        changed_region: Option<Region>,
        /// Pixels failing the policy.
        failing_pixels: u64,
        /// Bounding box of the changed pixels that failed the policy.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failing_region: Option<Region>,
        /// Distance to the thresholds; negative means that gate is exceeded.
        headroom: SsimHeadroom,
    },
}

/// Distance of SSIM metrics to their thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SsimHeadroom {
    /// `min_ssim - min_similarity`.
    pub similarity: f64,
    /// `color_tolerance - peak_excess`. Negative does not always fail: small isolated regions
    /// with a weak excess are tolerated by the policy.
    pub color: f64,
}

impl Metrics {
    /// Combines an engine measurement with the tolerance it was taken under.
    ///
    /// Returns `None` if they belong to different modes (a pixel measurement under an SSIM
    /// tolerance or vice versa), which callers treat as an internal error.
    #[must_use]
    pub fn from_measurement(
        measurement: &Measurement,
        tolerance: &Tolerance,
        total_pixels: u64,
    ) -> Option<Self> {
        // Every pairing is spelled out, so a new mode cannot silently fall into a wrong arm.
        match (*measurement, *tolerance) {
            (Measurement::Pixel { diff_count }, Tolerance::Exact {}) => {
                Some(Self::pixel(diff_count, total_pixels, 0.0))
            }
            (Measurement::Pixel { diff_count }, Tolerance::Pixel { max_diff_ratio }) => {
                Some(Self::pixel(diff_count, total_pixels, max_diff_ratio))
            }
            (
                Measurement::Ssim {
                    mean_ssim,
                    min_ssim,
                    peak_excess,
                    changed_pixels,
                    changed_region,
                    failing_pixels,
                    failing_region,
                    ..
                },
                Tolerance::Ssim {
                    min_similarity,
                    color_tolerance,
                },
            ) => Some(Self::Ssim {
                min_ssim,
                mean_ssim,
                peak_excess,
                changed_pixels,
                changed_region,
                failing_pixels,
                failing_region,
                headroom: SsimHeadroom {
                    similarity: min_ssim - min_similarity,
                    color: color_tolerance - peak_excess,
                },
            }),
            (Measurement::Pixel { .. }, Tolerance::Ssim { .. })
            | (Measurement::Ssim { .. }, Tolerance::Exact {} | Tolerance::Pixel { .. }) => None,
        }
    }

    fn pixel(diff_pixels: u64, total_pixels: u64, max_diff_ratio: f64) -> Self {
        #[expect(
            clippy::cast_precision_loss,
            reason = "pixel counts are far below 2^52, so the f64 conversion is exact"
        )]
        let diff_ratio = if total_pixels == 0 {
            0.0
        } else {
            diff_pixels as f64 / total_pixels as f64
        };
        Self::Pixel {
            total_pixels,
            diff_pixels,
            diff_ratio,
            headroom: max_diff_ratio - diff_ratio,
        }
    }
}

/// A SHA-256 digest as 64 lowercase hex characters.
///
/// Validated (and lowercased) when parsed, so a case report never carries a malformed digest.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct Sha256Hex(String);

/// A string that is not a SHA-256 hex digest.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("expected a SHA-256 digest of 64 hex characters, got '{0}'")]
pub struct InvalidSha256(pub String);

impl Sha256Hex {
    /// Validates `hex` (either case) and stores it lowercased.
    ///
    /// # Errors
    /// Returns [`InvalidSha256`] unless `hex` is exactly 64 ASCII hex digits.
    pub fn new(hex: impl Into<String>) -> Result<Self, InvalidSha256> {
        let hex = hex.into();
        if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            Ok(Self(hex.to_ascii_lowercase()))
        } else {
            Err(InvalidSha256(hex))
        }
    }

    /// The hex form of a raw 32-byte digest (always valid).
    #[must_use]
    pub fn from_digest(digest: &[u8; 32]) -> Self {
        use std::fmt::Write as _;

        let mut hex = String::with_capacity(64);
        for byte in digest {
            let _infallible = write!(hex, "{byte:02x}");
        }
        Self(hex)
    }

    /// The lowercase hex digest.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Sha256Hex {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl schemars::JsonSchema for Sha256Hex {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Sha256Hex".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "description": "Lowercase hex SHA-256 digest of the PNG bytes.",
            "type": "string",
            "pattern": "^[0-9a-f]{64}$"
        })
    }
}

/// Kind of a compared region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RegionKind {
    /// Pixels compared as an image (today: the whole golden).
    Image,
}

/// Metrics of one region of the golden.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegionMetrics {
    /// How the region was compared.
    pub kind: RegionKind,
    /// Region bounds in golden pixels; absent for the whole image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rect: Option<Region>,
    /// The region's metrics.
    pub metrics: Metrics,
}

impl RegionMetrics {
    /// The whole-image region.
    #[must_use]
    pub const fn whole_image(metrics: Metrics) -> Self {
        Self {
            kind: RegionKind::Image,
            rect: None,
            metrics,
        }
    }
}

/// The committed golden.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GoldenImage {
    /// Path relative to the workspace root, `/`-separated, case as on disk.
    pub path: String,
    /// SHA-256 of the PNG bytes; absent when the golden does not exist (`missing`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<Sha256Hex>,
    /// Width in pixels; absent if the PNG header could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Height in pixels; absent if the PNG header could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

/// The image produced by this run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CandidateImage {
    /// SHA-256 of the PNG bytes.
    pub sha256: Sha256Hex,
    /// Width in pixels; absent if the PNG header could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Height in pixels; absent if the PNG header could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

/// What produced the candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Source {
    /// Integration that ran the comparison, e.g. `gleon_flutter`.
    pub tool: String,
    /// Version of that integration.
    pub tool_version: String,
    /// Renderer identifier, e.g. `flutter-3.47.5`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renderer: Option<String>,
}

/// The test that produced the candidate, when the integration knows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TestInfo {
    /// Full test name (group names included).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// How the images were compared.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Comparison {
    /// The effective tolerance.
    pub tolerance: Tolerance,
    /// Ignored zones, applied to both images.
    pub masks: Vec<Zone>,
    /// Version of the engine's tolerant (SSIM) decision policy.
    pub policy_version: u32,
}

/// Result of the case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CaseOutcome {
    /// Byte-identical PNGs; decided without decoding, so there are no metrics.
    Identical,
    /// Within the tolerance.
    Match,
    /// Beyond the tolerance.
    Mismatch,
    /// Different image sizes; no pixel comparison was attempted.
    DimensionMismatch,
    /// Invalid input or options; see `message`.
    Error,
    /// The golden was rewritten from the candidate (update mode); nothing was compared.
    Updated,
    /// The golden does not exist (a new test without a baseline); nothing was compared.
    Missing,
}

impl CaseOutcome {
    /// The name of this outcome in a case report (`dimension_mismatch`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Identical => "identical",
            Self::Match => "match",
            Self::Mismatch => "mismatch",
            Self::DimensionMismatch => "dimension_mismatch",
            Self::Error => "error",
            Self::Updated => "updated",
            Self::Missing => "missing",
        }
    }
}

/// Wall-clock durations in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CaseTimings {
    /// The whole comparison as seen by the integration.
    pub total: f64,
    /// Time spent in the native engine (decode, compare, encode), when it ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<f64>,
}

/// One golden comparison.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "gleon case report (.gleon/runs/latest/cases/<name>.json)")]
pub struct CaseReport {
    /// Version of this format (`CASE_SCHEMA_VERSION`).
    #[schemars(range(min = 1, max = 1))]
    pub schema_version: u32,
    /// Canonical test name (also the case file name): the golden path relative to the workspace
    /// root without extension, as lowercase `[a-z0-9_.-]` segments separated by `/`.
    #[schemars(regex(pattern = r"^[a-z0-9_.-]+(/[a-z0-9_.-]+)*$"))]
    pub name: String,
    /// The committed golden.
    pub golden: GoldenImage,
    /// The image of this run.
    pub candidate: CandidateImage,
    /// What produced the candidate.
    pub source: Source,
    /// Platform the candidate was rendered on: auto-detected `os` and `arch`, named like in the CLI.
    pub platform: PlatformConfig,
    /// The producing test, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test: Option<TestInfo>,
    /// How the images were compared.
    pub comparison: Comparison,
    /// Result of the case.
    pub outcome: CaseOutcome,
    /// Human-readable detail (failure reason or error).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Whole-image metrics; absent when no pixel comparison ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<Metrics>,
    /// Per-region metrics (today a single whole-image region when `metrics` is present).
    pub regions: Vec<RegionMetrics>,
    /// Durations.
    pub timings_ms: CaseTimings,
    /// When the case was recorded (RFC 3339).
    pub recorded_at: chrono::DateTime<chrono::Utc>,
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::float_cmp,
    clippy::pedantic,
    clippy::nursery,
    reason = "test code: panics are assertions, and pedantic/nursery style lints are not enforced in tests"
)]
mod tests {
    use super::*;

    fn ssim_measurement() -> Measurement {
        Measurement::Ssim {
            mean_ssim: 0.99,
            min_ssim: 0.931,
            max_excess: 0.0,
            peak_excess: 5.2,
            changed_pixels: 12,
            changed_region: Some(Region {
                x: 1,
                y: 2,
                width: 3,
                height: 4,
            }),
            failing_pixels: 0,
            failing_region: None,
        }
    }

    #[test]
    fn test_ssim_headroom() {
        let tolerance = Tolerance::Ssim {
            min_similarity: 0.8,
            color_tolerance: 8.0,
        };
        let metrics = Metrics::from_measurement(&ssim_measurement(), &tolerance, 100);
        assert!(
            matches!(
                metrics,
                Some(Metrics::Ssim { headroom, .. })
                    if (headroom.similarity - 0.131).abs() < 1e-9
                        && (headroom.color - 2.8).abs() < 1e-9
            ),
            "{metrics:?}"
        );
    }

    #[test]
    fn test_pixel_headroom_for_exact_and_pixel() {
        let measurement = Measurement::Pixel { diff_count: 5 };
        assert_eq!(
            Metrics::from_measurement(&measurement, &Tolerance::Exact {}, 100),
            Some(Metrics::Pixel {
                total_pixels: 100,
                diff_pixels: 5,
                diff_ratio: 0.05,
                headroom: -0.05
            })
        );
        let pixel = Tolerance::Pixel {
            max_diff_ratio: 0.1,
        };
        let metrics = Metrics::from_measurement(&measurement, &pixel, 100);
        assert!(
            matches!(metrics, Some(Metrics::Pixel { headroom, .. }) if (headroom - 0.05).abs() < 1e-12),
            "{metrics:?}"
        );
        let empty = Metrics::from_measurement(&Measurement::Pixel { diff_count: 0 }, &pixel, 0);
        assert!(matches!(
            empty,
            Some(Metrics::Pixel {
                diff_ratio: 0.0,
                ..
            })
        ));
    }

    #[test]
    fn test_mode_mismatch_is_rejected() {
        assert_eq!(
            Metrics::from_measurement(&ssim_measurement(), &Tolerance::Exact {}, 1),
            None
        );
        let ssim = Tolerance::Ssim {
            min_similarity: 0.8,
            color_tolerance: 8.0,
        };
        assert_eq!(
            Metrics::from_measurement(&Measurement::Pixel { diff_count: 0 }, &ssim, 1),
            None
        );
    }

    #[test]
    fn test_sha256_is_validated_and_lowercased() {
        let upper = "AB".repeat(32);
        assert_eq!(Sha256Hex::new(&upper).unwrap().as_str(), "ab".repeat(32));
        assert_eq!(
            Sha256Hex::from_digest(&[0xab; 32]).as_str(),
            "ab".repeat(32)
        );
        for bad in ["", "abc", &"g".repeat(64), &"a".repeat(65)] {
            assert_eq!(Sha256Hex::new(bad), Err(InvalidSha256(bad.to_owned())));
        }
        let parsed: Sha256Hex = serde_json::from_value(serde_json::json!(upper)).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), "ab".repeat(32));
        assert!(serde_json::from_value::<Sha256Hex>(serde_json::json!("nope")).is_err());
    }

    #[test]
    fn test_metrics_json_shape() {
        let tolerance = Tolerance::Ssim {
            min_similarity: 0.8,
            color_tolerance: 8.0,
        };
        let metrics = Metrics::from_measurement(&ssim_measurement(), &tolerance, 100).unwrap();
        let json = serde_json::to_value(RegionMetrics::whole_image(metrics)).unwrap();
        assert_eq!(json["kind"], "image");
        assert!(json.get("rect").is_none());
        assert_eq!(json["metrics"]["kind"], "ssim");
        assert_eq!(json["metrics"]["changed_region"]["width"], 3);
        assert!(json["metrics"].get("failing_region").is_none());
        let back: RegionMetrics = serde_json::from_value(json).unwrap();
        assert_eq!(back.metrics, metrics);
    }
}
