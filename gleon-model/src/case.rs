//! Per-golden case report, the comparison metrics it carries, and the images of a failure.
//!
//! A report is written to `.gleon/runs/latest/cases/<platform>/<name>.json`, the images next to
//! it to `<artifacts dir>/<platform>/<name>/{golden,candidate,diff}.png`: `<platform>` is the key
//! of the report's platform ([`CaseReport::platform_key`], e.g. `macos-aarch64`), `<name>` the
//! canonical name of the golden, the same on every platform.
//!
//! Case reports are written by the integrations (the Flutter package through `gleon-ffi`) and are
//! meant as the one result format the gleon CLI reads too; the schema is committed as
//! `schema/case.v4.json`. Enums are internally tagged (`"kind"`) and every name is `snake_case`, so
//! non-Rust writers never mirror Rust type names. Names and paths are checked when a report is
//! read ([`CaseReport::parse`]), so a report never leads a reader outside its workspace.
//!
//! The directory holds the latest result of every golden on every platform: integrations run
//! tests in many processes, so no single writer can reset it, and each report replaces the
//! previous one of its golden on its platform. Platforms sharing one workspace (a macOS host and a
//! Linux container on the same checkout) never overwrite each other's reports or images. Reports
//! name their run ([`RUN_ID_ENV`]), so readers can take one run whole; reports of goldens that were
//! since removed or renamed stay behind until `gleon clean`.

use std::{
    io::{self, Write as _},
    path::{Path, PathBuf},
    time::Duration,
};

use gleon_engine::{Measurement, Region, config::Zone};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{
    config::ArtifactsDir,
    fs::Durability,
    hash::ImageHash,
    naming::{is_portable_relative_path, validate_canonical_test_name},
    platform::{PlatformConfig, PlatformError, PlatformKey},
    tolerance::{TextTolerance, Tolerance},
};

/// Version of the case report format.
pub const CASE_SCHEMA_VERSION: u32 = 4;

/// Directory of the case reports, relative to `.gleon/`.
pub const CASES_DIR: &str = "runs/latest/cases";

/// Name of the environment variable holding the [`RunId`] of the current run (set by CI or by
/// `gleon test`).
pub const RUN_ID_ENV: &str = "GLEON_RUN_ID";

/// Comparison metrics of one image region.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Metrics {
    /// Exact and pixel tolerances.
    Pixel {
        /// Pixels compared strictly (masked pixels and text under `comparison.text_tolerance` left out).
        total_pixels: u64,
        /// Pixels whose RGBA bytes differ (beyond the tolerance's options).
        diff_pixels: u64,
        /// Differing pixels the channel tolerance or the anti-aliasing detection let pass,
        /// counted as equal; left out when there are none.
        #[serde(default, skip_serializing_if = "crate::serde_skip::is_default")]
        tolerated_pixels: u64,
        /// Differing pixels on the golden's edges the edge mask hid, counted as equal; left out
        /// when there are none. Apart from `tolerated_pixels`: only this option hides changes
        /// that lie on edges (a missing glyph or small icon).
        #[serde(default, skip_serializing_if = "crate::serde_skip::is_default")]
        edge_pixels: u64,
        /// `diff_pixels / total_pixels` (0 for an empty image).
        diff_ratio: f64,
        /// `max_diff_ratio - diff_ratio` (exact: `-diff_ratio`); negative means it failed.
        headroom: f64,
        /// The text regions, compared under `comparison.text_tolerance` instead (left out of the pixels
        /// above); the worst tile is a `text` entry of `regions`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<TextMetrics>,
    },
    /// SSIM tolerance (values as defined by the engine's `SsimAnalysis`).
    Ssim {
        /// Lowest local SSIM, gated by `min_similarity`.
        min_ssim: f64,
        /// Mean local SSIM; diagnostic only.
        mean_ssim: f64,
        /// Largest deviation beyond `color_tolerance` among the failing regions (0 unless the
        /// envelope gate failed).
        max_excess: f64,
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
        /// The text regions, judged by their tiles under `comparison.text_tolerance` and left out
        /// of both gates; the worst tile is a `text` entry of `regions`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<TextMetrics>,
    },
}

/// What the text regions of a comparison measured.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TextMetrics {
    /// Text pixels (masked ones left out).
    pub pixels: u64,
    /// Text pixels that differ.
    pub diff_pixels: u64,
    /// Share of differing pixels of the worst tile.
    pub worst_tile_diff_ratio: f64,
    /// `comparison.text_tolerance` minus `worst_tile_diff_ratio`; negative means it failed.
    pub headroom: f64,
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
    /// Whether any compared pixel differs (text included): a pass without any is the golden
    /// itself.
    #[must_use]
    pub fn differs(&self) -> bool {
        match *self {
            Self::Pixel {
                diff_pixels,
                tolerated_pixels,
                edge_pixels,
                text,
                ..
            } => {
                diff_pixels > 0
                    || tolerated_pixels > 0
                    || edge_pixels > 0
                    || text.is_some_and(|text| text.diff_pixels > 0)
            }
            Self::Ssim {
                changed_pixels,
                text,
                ..
            } => changed_pixels > 0 || text.is_some_and(|text| text.diff_pixels > 0),
        }
    }

    /// The text metrics, of either mode.
    #[must_use]
    pub const fn text(&self) -> Option<TextMetrics> {
        match *self {
            Self::Pixel { text, .. } | Self::Ssim { text, .. } => text,
        }
    }

    /// Combines an engine measurement with the tolerance (and the tolerance of text) it was
    /// taken under.
    ///
    /// Returns `None` if they belong to different modes (a pixel measurement under an SSIM
    /// tolerance or vice versa), which callers treat as an internal error.
    #[must_use]
    pub fn from_measurement(
        measurement: &Measurement,
        tolerance: &Tolerance,
        text: Option<&TextTolerance>,
    ) -> Option<Self> {
        let text_metrics = |analysis: Option<gleon_engine::TextAnalysis>| {
            analysis.zip(text).map(|(analysis, text)| {
                let worst = analysis.worst_tile.map_or(0.0, |tile| tile.diff_ratio());
                TextMetrics {
                    pixels: analysis.pixels,
                    diff_pixels: analysis.diff_pixels,
                    worst_tile_diff_ratio: worst,
                    headroom: text.0 - worst,
                }
            })
        };
        // Every pairing is spelled out, so a new mode cannot silently fall into a wrong arm.
        match (*measurement, *tolerance) {
            (
                Measurement::Pixel {
                    checked_pixels,
                    diff_count,
                    tolerated_count,
                    edge_count,
                    text: analysis,
                },
                Tolerance::Exact {} | Tolerance::Pixel { .. },
            ) => Some(Self::pixel(
                PixelCounts {
                    total: checked_pixels,
                    diff: diff_count,
                    tolerated: tolerated_count,
                    edges: edge_count,
                },
                tolerance.max_diff_ratio(),
                text_metrics(analysis),
            )),
            (
                Measurement::Ssim {
                    mean_ssim,
                    min_ssim,
                    max_excess,
                    peak_excess,
                    changed_pixels,
                    changed_region,
                    failing_pixels,
                    failing_region,
                    text: analysis,
                    ..
                },
                Tolerance::Ssim {
                    min_similarity,
                    color_tolerance,
                },
            ) => Some(Self::Ssim {
                min_ssim,
                mean_ssim,
                max_excess,
                peak_excess,
                changed_pixels,
                changed_region,
                failing_pixels,
                failing_region,
                headroom: SsimHeadroom {
                    similarity: min_ssim - min_similarity,
                    color: color_tolerance - peak_excess,
                },
                text: text_metrics(analysis),
            }),
            (Measurement::Pixel { .. }, Tolerance::Ssim { .. })
            | (Measurement::Ssim { .. }, Tolerance::Exact {} | Tolerance::Pixel { .. }) => None,
        }
    }

    fn pixel(counts: PixelCounts, max_diff_ratio: f64, text: Option<TextMetrics>) -> Self {
        let PixelCounts {
            total: total_pixels,
            diff: diff_pixels,
            tolerated: tolerated_pixels,
            edges: edge_pixels,
        } = counts;
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
            tolerated_pixels,
            edge_pixels,
            diff_ratio,
            headroom: max_diff_ratio - diff_ratio,
            text,
        }
    }
}

/// The pixel counts [`Metrics::Pixel`] is made of.
#[derive(Debug, Clone, Copy, Default)]
struct PixelCounts {
    /// Pixels compared strictly.
    total: u64,
    /// Strictly compared pixels that differ beyond the options.
    diff: u64,
    /// Differing pixels the channel tolerance or the anti-aliasing detection let pass.
    tolerated: u64,
    /// Differing pixels the edge mask hid.
    edges: u64,
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
    fn from_digest(digest: &[u8; 32]) -> Self {
        use std::fmt::Write as _;

        let mut hex = String::with_capacity(64);
        for byte in digest {
            let _infallible = write!(hex, "{byte:02x}");
        }
        Self(hex)
    }

    /// The digest of `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self::from_digest(&Sha256::digest(bytes).into())
    }

    /// The lowercase hex digest.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Sha256Hex {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
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
    /// Pixels compared as an image (the whole golden).
    Image,
    /// Text compared under `comparison.text_tolerance`: the tile of its text regions with the largest
    /// share of differing pixels.
    Text,
    /// SSIM: the bounding box of the changed pixels, with the policy's metrics inside it (its
    /// `mean_ssim` not diluted by the unchanged rest of the golden).
    Changed,
    /// SSIM: the bounding box of the pixels failing the policy, with its metrics inside it.
    Failing,
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

    /// The regions of a comparison: the whole image with `metrics`, then the worst text tile of
    /// `measurement` under `text` (if any), then (SSIM, measured under `tolerance`) its changed
    /// and failing regions.
    #[must_use]
    pub fn of(
        metrics: Metrics,
        measurement: &Measurement,
        tolerance: &Tolerance,
        text: Option<&TextTolerance>,
    ) -> Vec<Self> {
        let analysis = match measurement {
            Measurement::Pixel { text, .. } | Measurement::Ssim { text, .. } => text,
        };
        let tile = text
            .zip(analysis.and_then(|analysis| analysis.worst_tile))
            .map(|(text, tile)| Self {
                kind: RegionKind::Text,
                rect: Some(tile.region),
                metrics: Metrics::pixel(
                    PixelCounts {
                        total: tile.pixels,
                        diff: tile.diff_pixels,
                        ..PixelCounts::default()
                    },
                    text.0,
                    None,
                ),
            });
        let local = match (*measurement, *tolerance) {
            (
                Measurement::Ssim {
                    changed_local,
                    failing_local,
                    ..
                },
                Tolerance::Ssim {
                    min_similarity,
                    color_tolerance,
                },
            ) => {
                let region = |kind, local: gleon_engine::SsimRegion| Self {
                    kind,
                    rect: Some(local.region),
                    metrics: Metrics::Ssim {
                        min_ssim: local.min_ssim,
                        mean_ssim: local.mean_ssim,
                        max_excess: if local.failing_pixels > 0 {
                            (local.peak_excess - color_tolerance).max(0.0)
                        } else {
                            0.0
                        },
                        peak_excess: local.peak_excess,
                        changed_pixels: local.changed_pixels,
                        changed_region: None,
                        failing_pixels: local.failing_pixels,
                        failing_region: None,
                        headroom: SsimHeadroom {
                            similarity: local.min_ssim - min_similarity,
                            color: color_tolerance - local.peak_excess,
                        },
                        text: None,
                    },
                };
                [
                    changed_local.map(|local| region(RegionKind::Changed, local)),
                    failing_local.map(|local| region(RegionKind::Failing, local)),
                ]
            }
            _ => [None, None],
        };
        std::iter::once(Self::whole_image(metrics))
            .chain(tile)
            .chain(local.into_iter().flatten())
            .collect()
    }
}

/// The committed golden.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GoldenImage {
    /// Path relative to the workspace root, `/`-separated, case as on disk.
    #[serde(deserialize_with = "workspace_path")]
    #[schemars(regex(
        pattern = r"^(?!\.\.?(/|$))[A-Za-z0-9._-]+(/(?!\.\.?(/|$))[A-Za-z0-9._-]+)*$"
    ))]
    pub path: String,
    /// SHA-256 of the PNG bytes; absent when the golden does not exist (`missing`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<Sha256Hex>,
    /// The content-addressed baseline (`.gleon/blobs/<scheme>/…`) the golden comes from, as in its
    /// manifest; absent for goldens committed as image files (the Flutter package).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<ImageHash>,
    /// Width in pixels; absent if the PNG header could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Height in pixels; absent if the PNG header could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// The golden compared in place of `path`, which does not exist yet: the shared golden of the
    /// workspace (its `fallback_platform`; integrations with per-platform goldens). `sha256`, `width`
    /// and `height` describe it; `path` is still where `gleon approve` writes, also from a pass,
    /// which keeps its candidate for that.
    #[serde(
        default,
        deserialize_with = "optional_workspace_path",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(regex(
        pattern = r"^(?!\.\.?(/|$))[A-Za-z0-9._-]+(/(?!\.\.?(/|$))[A-Za-z0-9._-]+)*$"
    ))]
    pub fallback: Option<String>,
}

impl GoldenImage {
    /// The golden at `path` with the PNG bytes `png` (`None` when it does not exist).
    #[must_use]
    pub fn of(path: String, png: Option<&[u8]>, blob: Option<ImageHash>) -> Self {
        let size = png.and_then(png_size);
        Self {
            path,
            sha256: png.map(Sha256Hex::of),
            blob,
            width: size.map(|(width, _)| width),
            height: size.map(|(_, height)| height),
            fallback: None,
        }
    }

    /// The golden the case was compared with: [`Self::fallback`], else [`Self::path`].
    #[must_use]
    pub fn compared(&self) -> &str {
        self.fallback.as_deref().unwrap_or(&self.path)
    }
}

/// The image produced by this run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CandidateImage {
    /// SHA-256 of the PNG bytes; absent when the integration compared raw pixels and encoded no
    /// PNG (a pass); a candidate kept in `artifacts` always has it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<Sha256Hex>,
    /// Width in pixels; absent if the PNG header could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Height in pixels; absent if the PNG header could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

impl CandidateImage {
    /// The candidate with the PNG bytes `png`.
    #[must_use]
    pub fn of(png: &[u8]) -> Self {
        let size = png_size(png);
        Self {
            sha256: Some(Sha256Hex::of(png)),
            width: size.map(|(width, _)| width),
            height: size.map(|(_, height)| height),
        }
    }

    /// A candidate of raw pixels, `width` x `height`, encoded as no PNG.
    #[must_use]
    pub const fn raw(width: u32, height: u32) -> Self {
        Self {
            sha256: None,
            width: Some(width),
            height: Some(height),
        }
    }
}

/// Width and height from a PNG header, without decoding (absent when `bytes` do not start with
/// one, or the header claims a zero size, which the PNG specification forbids).
#[must_use]
pub fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    let header = bytes.get(..24)?;
    if !header.starts_with(SIGNATURE) || &header[12..16] != b"IHDR" {
        return None;
    }
    let read = |at: usize| {
        u32::from_be_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]])
    };
    Some((read(16), read(20))).filter(|&(width, height)| width > 0 && height > 0)
}

/// The images of a case, as paths relative to the workspace root (`/`-separated):
/// `<artifacts dir>/<platform key>/<name>/{golden,candidate,diff}.png`.
///
/// `<artifacts dir>` is an artifacts directory (`.gleon/runs/latest/artifacts` or a directory
/// under `.gleon/runs/` outside `latest/`), the same for every image of the case;
/// `<platform key>` is the key of the report's `platform` and `<name>` its `name`, so a report
/// lists only its own images.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Artifacts {
    /// A copy of the golden (mismatches and dimension mismatches):
    /// `<artifacts dir>/<platform key>/<name>/golden.png`.
    #[serde(
        default,
        deserialize_with = "optional_artifact_path",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(regex(
        pattern = r"^\.gleon/runs/(latest/artifacts|(?![Ll][Aa][Tt][Ee][Ss][Tt](/|$))(?!\.\.?(/|$))[A-Za-z0-9._-]+(/(?!\.\.?(/|$))[A-Za-z0-9._-]+)*)/[a-z0-9_.-]+([+=][a-z0-9_.-]+)*/(?!\.\.?(/|$))[a-z0-9_.-]+(/(?!\.\.?(/|$))[a-z0-9_.-]+)*/golden\.png$"
    ))]
    pub golden: Option<String>,
    /// The candidate (mismatches, dimension mismatches, missing goldens and passes against another
    /// platform's golden that differ from it, for `gleon approve`):
    /// `<artifacts dir>/<platform key>/<name>/candidate.png`.
    #[serde(
        default,
        deserialize_with = "optional_artifact_path",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(regex(
        pattern = r"^\.gleon/runs/(latest/artifacts|(?![Ll][Aa][Tt][Ee][Ss][Tt](/|$))(?!\.\.?(/|$))[A-Za-z0-9._-]+(/(?!\.\.?(/|$))[A-Za-z0-9._-]+)*)/[a-z0-9_.-]+([+=][a-z0-9_.-]+)*/(?!\.\.?(/|$))[a-z0-9_.-]+(/(?!\.\.?(/|$))[a-z0-9_.-]+)*/candidate\.png$"
    ))]
    pub candidate: Option<String>,
    /// The diff visualization (mismatches and dimension mismatches):
    /// `<artifacts dir>/<platform key>/<name>/diff.png`.
    #[serde(
        default,
        deserialize_with = "optional_artifact_path",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(regex(
        pattern = r"^\.gleon/runs/(latest/artifacts|(?![Ll][Aa][Tt][Ee][Ss][Tt](/|$))(?!\.\.?(/|$))[A-Za-z0-9._-]+(/(?!\.\.?(/|$))[A-Za-z0-9._-]+)*)/[a-z0-9_.-]+([+=][a-z0-9_.-]+)*/(?!\.\.?(/|$))[a-z0-9_.-]+(/(?!\.\.?(/|$))[a-z0-9_.-]+)*/diff\.png$"
    ))]
    pub diff: Option<String>,
}

impl Artifacts {
    /// Whether no image is kept.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.golden.is_none() && self.candidate.is_none() && self.diff.is_none()
    }
}

/// File names of the images inside `<artifacts dir>/<platform>/<name>/`.
const GOLDEN_ARTIFACT: &str = "golden.png";
/// See [`GOLDEN_ARTIFACT`].
const CANDIDATE_ARTIFACT: &str = "candidate.png";
/// See [`GOLDEN_ARTIFACT`].
const DIFF_ARTIFACT: &str = "diff.png";

/// The PNGs of a case to keep as [`Artifacts`]; `None` for an image the case has none of.
#[derive(Debug, Clone, Copy, Default)]
pub struct ArtifactImages<'a> {
    /// The golden.
    pub golden: Option<&'a [u8]>,
    /// The candidate.
    pub candidate: Option<&'a [u8]>,
    /// The diff visualization.
    pub diff: Option<&'a [u8]>,
}

/// Writes `images` to `<root>/<dir>/<platform>/<name>/{golden,candidate,diff}.png` and returns
/// their paths relative to `root`; `None` without images.
///
/// The files of absent images are removed, so the folder always shows the latest outcome of the
/// golden on this platform: an image left by an earlier failure never passes for this one's. Only
/// this platform's images are removed; another platform's folder of the same golden stays. Empty
/// folders stay (`gleon clean` removes them): removing one could race the parallel writes of a
/// longer name inside it. The images are regenerated by every run, so they are written atomically
/// but not flushed to disk ([`Durability::Atomic`]).
///
/// # Errors
/// Returns [`io::ErrorKind::InvalidInput`] if `name` is not a canonical test name, or the first
/// I/O error of writing or removing a file.
pub fn write_artifacts(
    root: &Path,
    dir: &ArtifactsDir,
    platform: &PlatformKey,
    name: &str,
    images: ArtifactImages<'_>,
) -> io::Result<Option<Artifacts>> {
    validate_canonical_test_name(name)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let folder = crate::naming::join_relative(&dir.to_path(root).join(platform), name);
    let relative = |file: &str| format!("{}/{platform}/{name}/{file}", dir.as_str());
    let write = |file: &str, bytes: Option<&[u8]>| -> io::Result<Option<String>> {
        crate::fs::write_or_remove(&folder.join(file), bytes, Durability::Atomic)
            .map(|()| bytes.map(|_| relative(file)))
    };
    let artifacts = Artifacts {
        golden: write(GOLDEN_ARTIFACT, images.golden)?,
        candidate: write(CANDIDATE_ARTIFACT, images.candidate)?,
        diff: write(DIFF_ARTIFACT, images.diff)?,
    };
    Ok((!artifacts.is_empty()).then_some(artifacts))
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
    /// The tolerance of the text regions the integration reported (under every tolerance);
    /// absent when there were none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_tolerance: Option<TextTolerance>,
    /// Ignored zones, applied to both images.
    pub masks: Vec<Zone>,
    /// Version of the engine's tolerant decisions (SSIM, text tiles): `gleon_engine::POLICY_VERSION`.
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
    /// The golden could not be compared or written: invalid input or config, an I/O, image or
    /// internal error (`error_kind`); see `message`.
    Error,
    /// The golden was rewritten from the candidate (update mode); nothing was compared.
    Updated,
    /// The golden does not exist (a new test without a baseline); nothing was compared.
    Missing,
}

impl CaseOutcome {
    /// Whether this outcome fails its test: a pass is `identical`, `match` or `updated`.
    #[must_use]
    pub const fn is_failure(self) -> bool {
        matches!(
            self,
            Self::Mismatch | Self::DimensionMismatch | Self::Missing | Self::Error
        )
    }

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

/// Class of an `error` outcome, so readers tell a broken image from a broken file system or a
/// bug without parsing messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CaseErrorKind {
    /// The integration passed invalid input.
    InvalidInput,
    /// `.gleon/gleon.yaml`, an environment override or a golden name is invalid.
    Config,
    /// A file could not be read or written.
    Io,
    /// An image could not be decoded, is over the analysis budget or could not be encoded.
    Image,
    /// A bug in gleon.
    Internal,
}

impl CaseErrorKind {
    /// The name of this kind in a case report (`invalid_input`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::Config => "config",
            Self::Io => "io",
            Self::Image => "image",
            Self::Internal => "internal",
        }
    }
}

/// Identifier of a test run, shared by the case reports of that run (see [`RUN_ID_ENV`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct RunId(String);

/// A string that is not a valid [`RunId`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "a run id must be 1 to {max} ASCII letters, digits, `.`, `_` or `-` (got '{0}')",
    max = RunId::MAX_LEN
)]
pub struct InvalidRunId(pub String);

impl RunId {
    /// The longest run id.
    pub const MAX_LEN: usize = 128;

    /// Validates `id`.
    ///
    /// # Errors
    /// Returns [`InvalidRunId`] unless `id` has 1 to [`Self::MAX_LEN`] ASCII letters, digits,
    /// `.`, `_` or `-` (CI ids such as `123-1`; a run id never names a file).
    pub fn new(id: impl Into<String>) -> Result<Self, InvalidRunId> {
        let id = id.into();
        let is_valid = (1..=Self::MAX_LEN).contains(&id.len())
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if is_valid {
            Ok(Self(id))
        } else {
            Err(InvalidRunId(id))
        }
    }

    /// The run id of [`RUN_ID_ENV`] (`env_value` is its raw value, `None` when unset): `None` when
    /// unset or empty, surrounding whitespace ignored.
    ///
    /// # Errors
    /// Returns [`InvalidRunId`] as [`Self::new`] does.
    pub fn from_env(env_value: Option<&str>) -> Result<Option<Self>, InvalidRunId> {
        env_value
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(Self::new)
            .transpose()
    }

    /// The id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for RunId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl schemars::JsonSchema for RunId {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "RunId".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "description": "Identifier of the test run (`GLEON_RUN_ID`), shared by its case reports.",
            "type": "string",
            "pattern": "^[A-Za-z0-9._-]{1,128}$"
        })
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

impl CaseTimings {
    /// The timings of a comparison that took `total`, `native` of it in the engine.
    #[must_use]
    pub fn new(total: Duration, native: Option<Duration>) -> Self {
        Self {
            total: millis(total),
            native: native.map(millis),
        }
    }
}

/// `duration` in milliseconds with microsecond resolution.
#[must_use]
pub fn millis(duration: Duration) -> f64 {
    #[expect(
        clippy::cast_precision_loss,
        reason = "durations are far below 2^52 microseconds"
    )]
    let micros = duration.as_micros() as f64;
    micros / 1000.0
}

/// One golden comparison.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "gleon case report (.gleon/runs/latest/cases/<platform>/<name>.json)")]
pub struct CaseReport {
    /// Version of this format (`CASE_SCHEMA_VERSION`).
    #[serde(deserialize_with = "schema_version")]
    #[schemars(range(min = 4, max = 4))]
    pub schema_version: u32,
    /// Canonical test name: the golden path relative to the workspace root without extension, as
    /// lowercase `[a-z0-9_.-]` segments separated by `/`. It is the file name of the report under
    /// its platform's directory, and the folder of its images; the same on every platform.
    #[serde(deserialize_with = "test_name")]
    #[schemars(regex(pattern = r"^(?!\.\.?(/|$))[a-z0-9_.-]+(/(?!\.\.?(/|$))[a-z0-9_.-]+)*$"))]
    pub name: String,
    /// The committed golden.
    pub golden: GoldenImage,
    /// The image of this run.
    pub candidate: CandidateImage,
    /// What produced the candidate.
    pub source: Source,
    /// Platform the candidate was rendered on: auto-detected `os` and `arch`, named like in the CLI.
    /// Its key names the directory of the report and of its images.
    #[serde(deserialize_with = "keyed_platform")]
    pub platform: PlatformConfig,
    /// The producing test, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test: Option<TestInfo>,
    /// How the images were compared.
    pub comparison: Comparison,
    /// Result of the case.
    pub outcome: CaseOutcome,
    /// Class of an `error` outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<CaseErrorKind>,
    /// Human-readable detail (failure reason or error).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Whole-image metrics; absent when no pixel comparison ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<Metrics>,
    /// Per-region metrics when `metrics` is present: the whole image, then the worst tile of text
    /// (when text was compared under a tolerance).
    pub regions: Vec<RegionMetrics>,
    /// The images kept for the case: of a failure, or the candidate of a recorded pass against
    /// another platform's golden that differs from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<Artifacts>,
    /// Durations.
    pub timings_ms: CaseTimings,
    /// The run the case belongs to, when the run is named (`GLEON_RUN_ID`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    /// When the case was recorded (RFC 3339).
    pub recorded_at: chrono::DateTime<chrono::Utc>,
}

impl CaseReport {
    /// The file of the report named `name` of the platform keyed `platform`
    /// ([`Self::platform_key`]) in the workspace whose `.gleon/` is `gleon_dir`:
    /// `<gleon_dir>/runs/latest/cases/<platform>/<name>.json`.
    #[must_use]
    pub fn path(gleon_dir: &Path, platform: &PlatformKey, name: &str) -> PathBuf {
        gleon_dir
            .join(CASES_DIR)
            .join(platform)
            .join(format!("{name}.json"))
    }

    /// The key of [`Self::platform`], which names the directory of this report and of its images.
    ///
    /// # Errors
    /// Returns [`PlatformError`] for a platform without a key (never for a parsed report).
    pub fn platform_key(&self) -> Result<PlatformKey, PlatformError> {
        self.platform.key()
    }

    /// Parses a report, telling a report of another schema version apart from a broken one (a
    /// reader skips the former with a hint to update the integration that wrote it).
    ///
    /// # Errors
    /// Returns [`CaseParseError::UnsupportedVersion`] for a `schema_version` other than
    /// [`CASE_SCHEMA_VERSION`], [`CaseParseError::Json`] for anything serde rejects (including
    /// names and paths that would lead outside the workspace), or
    /// [`CaseParseError::Inconsistent`] for fields that contradict each other.
    pub fn parse(json: &[u8]) -> Result<Self, CaseParseError> {
        #[derive(Deserialize)]
        struct Version {
            schema_version: u32,
        }
        let report: Self = match serde_json::from_slice(json) {
            Ok(report) => report,
            // Only a report serde rejects (another version among them) is parsed again, for its
            // version.
            Err(error) => {
                return Err(match serde_json::from_slice(json) {
                    Ok(Version { schema_version }) if schema_version != CASE_SCHEMA_VERSION => {
                        CaseParseError::UnsupportedVersion(schema_version)
                    }
                    _ => CaseParseError::Json(error),
                });
            }
        };
        report
            .validate()
            .map(|()| report)
            .map_err(CaseParseError::Inconsistent)
    }

    /// Checks that the fields agree with the outcome: `error_kind` exactly for errors, no golden
    /// hash for a missing golden, equal hashes for identical images, metrics only for `match`
    /// and `mismatch` and of the tolerance's mode, tolerances within their ranges, a text
    /// tolerance only beside a pixel or exact one (and text metrics only with it), images only
    /// for failures that keep them, each at `<artifacts dir>/<platform key>/<name>/<file>` of
    /// this report.
    ///
    /// # Errors
    /// Returns the first [`InconsistentCase`].
    pub fn validate(&self) -> Result<(), InconsistentCase> {
        use CaseOutcome as O;

        if self.error_kind.is_some() != (self.outcome == O::Error) {
            return Err(InconsistentCase::ErrorKind);
        }
        if self.outcome == O::Missing && self.golden.sha256.is_some() {
            return Err(InconsistentCase::MissingGoldenHash);
        }
        if self.golden.fallback.is_some() && matches!(self.outcome, O::Missing | O::Updated) {
            return Err(InconsistentCase::Fallback);
        }
        if self.outcome == O::Identical
            && (self.candidate.sha256.is_none() || self.golden.sha256 != self.candidate.sha256)
        {
            return Err(InconsistentCase::IdenticalHashes);
        }
        let keeps_candidate = self
            .artifacts
            .as_ref()
            .is_some_and(|artifacts| artifacts.candidate.is_some());
        if keeps_candidate && self.candidate.sha256.is_none() {
            return Err(InconsistentCase::CandidateHash);
        }
        if self.metrics.is_some() && !matches!(self.outcome, O::Match | O::Mismatch) {
            return Err(InconsistentCase::Metrics);
        }
        let is_ssim_tolerance = matches!(self.comparison.tolerance, Tolerance::Ssim { .. });
        let has_other_metrics = self
            .metrics
            .is_some_and(|metrics| matches!(metrics, Metrics::Ssim { .. }) != is_ssim_tolerance);
        if has_other_metrics {
            return Err(InconsistentCase::MetricsKind);
        }
        let comparison = &self.comparison;
        if comparison.tolerance.validate().is_err()
            || comparison
                .text_tolerance
                .is_some_and(|text| text.validate().is_err())
        {
            return Err(InconsistentCase::ToleranceRange);
        }
        let has_text_metrics = self.metrics.is_some_and(|metrics| metrics.text().is_some());
        if has_text_metrics && comparison.text_tolerance.is_none() {
            return Err(InconsistentCase::TextTolerance);
        }
        self.validate_regions(is_ssim_tolerance)?;
        if let Some(Metrics::Pixel {
            tolerated_pixels,
            edge_pixels,
            ..
        }) = self.metrics
        {
            let options = comparison.tolerance.pixel_options();
            let can_tolerate = options.channel_tolerance > 0 || options.anti_alias;
            if (tolerated_pixels > 0 && !can_tolerate)
                || (edge_pixels > 0 && options.edge_threshold == 0)
            {
                return Err(InconsistentCase::PixelOptions);
            }
        }
        // A pass against another platform's golden keeps its candidate, for `gleon approve` to
        // record this platform's own golden.
        let keeps_fallback_candidate = self.golden.fallback.is_some()
            && matches!(self.outcome, O::Match | O::Identical)
            && self
                .artifacts
                .as_ref()
                .is_some_and(|a| a.golden.is_none() && a.diff.is_none());
        if self.artifacts.is_some()
            && !keeps_fallback_candidate
            && !matches!(
                self.outcome,
                O::Mismatch | O::DimensionMismatch | O::Missing
            )
        {
            return Err(InconsistentCase::Artifacts);
        }
        self.validate_artifact_paths()
    }

    /// Every region measured in the mode of the comparison (text tiles in pixels), inside the
    /// golden when its size is known.
    fn validate_regions(&self, is_ssim_tolerance: bool) -> Result<(), InconsistentCase> {
        let region_matches = |region: &RegionMetrics| match region.kind {
            RegionKind::Text => matches!(region.metrics, Metrics::Pixel { .. }),
            RegionKind::Image => {
                matches!(region.metrics, Metrics::Ssim { .. }) == is_ssim_tolerance
            }
            RegionKind::Changed | RegionKind::Failing => {
                is_ssim_tolerance && matches!(region.metrics, Metrics::Ssim { .. })
            }
        };
        if !self.regions.iter().all(region_matches) {
            return Err(InconsistentCase::RegionMetrics);
        }
        let within = |start: u32, length: u32, size: Option<u32>| {
            start
                .checked_add(length)
                .is_some_and(|end| size.is_none_or(|size| end <= size))
        };
        let inside_golden = |region: &RegionMetrics| {
            region.rect.is_none_or(|rect| {
                within(rect.x, rect.width, self.golden.width)
                    && within(rect.y, rect.height, self.golden.height)
            })
        };
        if self.regions.iter().all(inside_golden) {
            Ok(())
        } else {
            Err(InconsistentCase::RegionBounds)
        }
    }

    /// Checks that every image lies at `<artifacts dir>/<platform key>/<name>/<file>` of this
    /// report, its file named after its field, all in one artifacts directory.
    fn validate_artifact_paths(&self) -> Result<(), InconsistentCase> {
        let Some(artifacts) = &self.artifacts else {
            return Ok(());
        };
        let key = self.platform_key().ok();
        let mut shared_dir = None;
        for (field, path, file) in [
            ("golden", &artifacts.golden, GOLDEN_ARTIFACT),
            ("candidate", &artifacts.candidate, CANDIDATE_ARTIFACT),
            ("diff", &artifacts.diff, DIFF_ARTIFACT),
        ] {
            let Some(path) = path else {
                continue;
            };
            let dir = key.as_ref().and_then(|key| {
                path.strip_suffix(file)?
                    .strip_suffix('/')?
                    .strip_suffix(self.name.as_str())?
                    .strip_suffix('/')?
                    .strip_suffix(key.as_str())?
                    .strip_suffix('/')
            });
            match dir {
                Some(dir)
                    if ArtifactsDir::is_valid(dir)
                        && shared_dir.is_none_or(|shared| shared == dir) =>
                {
                    shared_dir = Some(dir);
                }
                _ => return Err(InconsistentCase::ArtifactPath(field)),
            }
        }
        Ok(())
    }

    /// Writes this report to [`Self::path`] of its platform (pretty JSON and a final newline),
    /// replacing the previous report of its golden on this platform only.
    ///
    /// Reports are regenerated by every run, so they are written atomically but not flushed to
    /// disk ([`Durability::Atomic`]).
    ///
    /// # Errors
    /// Returns [`io::ErrorKind::InvalidInput`] if the name is not a canonical test name, the
    /// platform has no key or the fields are inconsistent ([`Self::validate`]), or the I/O error of
    /// writing the file.
    pub fn write(&self, gleon_dir: &Path) -> io::Result<()> {
        validate_canonical_test_name(&self.name)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        self.validate()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let platform_key = self
            .platform_key()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let file = Self::path(gleon_dir, &platform_key, &self.name);
        crate::fs::write_atomically_with(&file, Durability::Atomic, |writer| {
            serde_json::to_writer_pretty(&mut *writer, self)?;
            writer.write_all(b"\n")
        })
    }
}

/// Why [`CaseReport::parse`] rejected a report.
#[derive(Debug, thiserror::Error)]
pub enum CaseParseError {
    /// A report of another schema version (an older or newer integration wrote it).
    #[error("case report schema {0} is not supported (this gleon reads {CASE_SCHEMA_VERSION})")]
    UnsupportedVersion(u32),
    /// Not a valid report of this version.
    #[error("invalid case report: {0}")]
    Json(#[source] serde_json::Error),
    /// Fields that contradict each other.
    #[error("inconsistent case report: {0}")]
    Inconsistent(#[source] InconsistentCase),
}

/// Fields of a [`CaseReport`] that contradict its outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InconsistentCase {
    /// `error_kind` without `outcome: error`, or an error without it.
    #[error("`error_kind` is set exactly for `outcome: error`")]
    ErrorKind,
    /// A missing golden with a hash.
    #[error("a missing golden has no `golden.sha256`")]
    MissingGoldenHash,
    /// A fallback golden of an outcome that compared none.
    #[error("`golden.fallback` belongs to outcomes that compared a golden")]
    Fallback,
    /// Identical images with different hashes.
    #[error("`identical` images have the same `sha256`")]
    IdenticalHashes,
    /// A kept candidate image without its hash (`gleon approve` checks the file against it).
    #[error("a candidate kept in `artifacts` has its `candidate.sha256`")]
    CandidateHash,
    /// Metrics of an outcome that compared no pixels or did not finish.
    #[error("`metrics` belong to `match` and `mismatch` only")]
    Metrics,
    /// Metrics of another mode than the tolerance (pixel metrics under an SSIM tolerance or vice
    /// versa).
    #[error("`metrics` are of another mode than `comparison.tolerance`")]
    MetricsKind,
    /// A tolerance of `comparison` outside its range.
    #[error("a tolerance of `comparison` is outside its range")]
    ToleranceRange,
    /// Text metrics without a text tolerance.
    #[error("text metrics need `comparison.text_tolerance`")]
    TextTolerance,
    /// A region measured in another mode than `comparison.tolerance` (text tiles are pixel
    /// metrics), or a `changed`/`failing` region of a comparison that is not SSIM.
    #[error("`regions` are measured in the mode of `comparison.tolerance` (text tiles in pixels)")]
    RegionMetrics,
    /// A region beyond the golden (when its size is known) or past the end of `u32`.
    #[error("`regions` lie inside the golden")]
    RegionBounds,
    /// Tolerated or edge pixels without the pixel options that let them pass.
    #[error("`tolerated_pixels`/`edge_pixels` need the pixel options that let them pass")]
    PixelOptions,
    /// An image outside `<artifacts dir>/<platform key>/<name>/` of this report, under another
    /// file name than its field's, or in another artifacts directory than the other images.
    #[error(
        "`artifacts.{0}` is not `<artifacts dir>/<platform key>/<name>/{0}.png` of this report \
         (one artifacts dir for every image)"
    )]
    ArtifactPath(&'static str),
    /// Images of an outcome that keeps none.
    #[error(
        "`artifacts` belong to `mismatch`, `dimension_mismatch` and `missing`, and the candidate \
         of a pass against `golden.fallback`"
    )]
    Artifacts,
}

fn schema_version<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
    let version = u32::deserialize(deserializer)?;
    if version == CASE_SCHEMA_VERSION {
        Ok(version)
    } else {
        Err(serde::de::Error::custom(
            CaseParseError::UnsupportedVersion(version),
        ))
    }
}

fn test_name<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let name = String::deserialize(deserializer)?;
    validate_canonical_test_name(&name)
        .map(|()| name)
        .map_err(serde::de::Error::custom)
}

/// A platform with a key ([`PlatformConfig::key`], so with an OS): the key names the report's
/// directory.
fn keyed_platform<'de, D: Deserializer<'de>>(deserializer: D) -> Result<PlatformConfig, D::Error> {
    let platform = PlatformConfig::deserialize(deserializer)?;
    platform
        .key()
        .map(|_| platform)
        .map_err(serde::de::Error::custom)
}

fn workspace_path<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    checked_workspace_path(String::deserialize(deserializer)?)
}

/// An optional [`workspace_path`]: `null` (which the schema allows) is `None`, like a missing key.
fn optional_workspace_path<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)?
        .map(checked_workspace_path)
        .transpose()
}

fn checked_workspace_path<E: serde::de::Error>(path: String) -> Result<String, E> {
    if is_portable_relative_path(&path) {
        Ok(path)
    } else {
        Err(E::custom(format!(
            "'{path}' is not a path inside the workspace: `/`-separated names of ASCII letters, \
             digits, `.`, `_` and `-`, without `.` or `..`"
        )))
    }
}

/// An optional image path of [`Artifacts`] (`null` is `None`): only its shape is checked here, a
/// path inside the workspace whose names may hold the `+` and `=` of a platform key;
/// [`CaseReport::validate`] checks where it lies.
fn optional_artifact_path<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)?
        .map(|path| {
            if crate::naming::is_portable_relative_path_with(&path, b"+=") {
                Ok(path)
            } else {
                Err(serde::de::Error::custom(format!(
                    "'{path}' is not an image path inside the workspace: `/`-separated names of \
                     ASCII letters, digits, `.`, `_`, `-`, `+` and `=`, without `.` or `..`"
                )))
            }
        })
        .transpose()
}

/// The texts of metrics, shared by every writer and reader of case reports (the integrations'
/// failure messages, the reports of the CLI), so one comparison reads the same everywhere.
///
/// Numbers are rounded to what a developer acts on: percentages to 2-4 decimals (`6.25%`,
/// `0.0167%`), similarities to 3-4 (`0.931`), colors to 1 (`5.2`).
pub mod text {
    use std::fmt::Write as _;

    use gleon_engine::Region;

    use super::Metrics;
    use crate::tolerance::{TextTolerance, Tolerance};

    /// The most decimals any number is shown with.
    pub const MAX_DECIMALS: usize = 4;

    /// `value` rounded to `max` decimals, trailing zeros dropped down to `min` (`8`, `7.5`,
    /// `0.800`). `-0.0` shows as `0`.
    #[must_use]
    pub fn decimal(value: f64, min: usize, max: usize) -> String {
        let value = if value == 0.0 { 0.0 } else { value };
        let mut text = format!("{value:.max$}");
        if let Some(point) = text.find('.') {
            let kept = text.trim_end_matches('0').len().max(point + 1 + min);
            text.truncate(kept);
            if text.ends_with('.') {
                text.pop();
            }
        }
        text
    }

    /// `ratio` as a percentage with 2 to 4 decimals (`6.25`, `0.0167`); a positive ratio too
    /// small to show is `<0.0001`, never a misleading `0.00`.
    #[must_use]
    pub fn percent(ratio: f64) -> String {
        let text = decimal(ratio * 100.0, 2, MAX_DECIMALS);
        if ratio > 0.0 && text.bytes().all(|b| matches!(b, b'0' | b'.')) {
            format!("<0.{}1", "0".repeat(MAX_DECIMALS - 1))
        } else {
            text
        }
    }

    /// `ratio` as an upper bound in percent: `≤ 6.25%`, or `<0.0001%` for a positive ratio too
    /// small to show (never `≤ <0.0001%`).
    #[must_use]
    pub fn at_most(ratio: f64) -> String {
        bound(ratio, " ")
    }

    /// [`at_most`] without the space, as in console lines: `≤6.25%`, `<0.0001%`.
    #[must_use]
    pub fn at_most_tight(ratio: f64) -> String {
        bound(ratio, "")
    }

    fn bound(ratio: f64, gap: &str) -> String {
        let shown = percent(ratio);
        if shown.starts_with('<') {
            format!("{shown}%")
        } else {
            format!("≤{gap}{shown}%")
        }
    }

    /// A similarity with 3 to 4 decimals (`0.800`, `0.9995`).
    #[must_use]
    pub fn similarity(value: f64) -> String {
        decimal(value, 3, MAX_DECIMALS)
    }

    /// A measured color distance in 8-bit units, one decimal (`5.2`).
    #[must_use]
    pub fn color(value: f64) -> String {
        decimal(value, 1, 1)
    }

    /// `text` of a number with an explicit sign (`+0.131`, `-1.00`).
    #[must_use]
    pub fn signed(text: String) -> String {
        if text.starts_with('-') {
            text
        } else {
            format!("+{text}")
        }
    }

    /// The thresholds of `tolerance` as shown in failure messages (`ssim ≥ 0.800, color ±8`).
    #[must_use]
    pub fn tolerance(tolerance: &Tolerance) -> String {
        match *tolerance {
            Tolerance::Exact {} => "exact".to_owned(),
            Tolerance::Pixel {
                max_diff_ratio,
                channel_tolerance,
                anti_alias,
                edge_threshold,
            } => {
                let mut text = format!("pixel {}", at_most(max_diff_ratio));
                if channel_tolerance > 0 {
                    let _infallible = write!(text, ", ±{channel_tolerance} per channel");
                }
                if anti_alias {
                    text.push_str(", aa ignored");
                }
                if edge_threshold > 0 {
                    let _infallible = write!(text, ", edges >{edge_threshold} ignored");
                }
                text
            }
            Tolerance::Ssim {
                min_similarity,
                color_tolerance,
            } => format!(
                "ssim ≥ {}, color ±{}",
                similarity(min_similarity),
                decimal(color_tolerance, 0, 2)
            ),
        }
    }

    /// A tolerance of text: `text ignored` (it never fails), else e.g. `text ≤ 10.00% per tile`.
    #[must_use]
    pub fn text_tolerance(text: &TextTolerance) -> String {
        if text.0 >= 1.0 {
            "text ignored".to_owned()
        } else {
            format!("text {} per tile", at_most(text.0))
        }
    }

    /// A region, e.g. `(4, 8) 16x32px`.
    #[must_use]
    pub(crate) fn region(region: &Region) -> String {
        format!(
            "({}, {}) {}x{}px",
            region.x, region.y, region.width, region.height
        )
    }

    /// The metrics of a failed comparison, e.g. `0.02% (1 of 6000px) differ`.
    #[must_use]
    pub fn metrics_summary(metrics: &Metrics) -> String {
        match metrics {
            Metrics::Pixel {
                total_pixels,
                diff_pixels,
                diff_ratio,
                text,
                ..
            } => {
                let strict = format!(
                    "{}% ({diff_pixels} of {total_pixels}px) differ",
                    percent(*diff_ratio)
                );
                // Text is part of the reason only when it failed: within its tolerance its
                // differences (another OS's rasterization) are no finding.
                match text.filter(|text| text.headroom < 0.0) {
                    None => strict,
                    Some(text) if *diff_pixels == 0 => format!(
                        "text up to {}% of a tile differs",
                        percent(text.worst_tile_diff_ratio)
                    ),
                    Some(text) => format!(
                        "{strict}, text up to {}% of a tile",
                        percent(text.worst_tile_diff_ratio)
                    ),
                }
            }
            Metrics::Ssim {
                min_ssim,
                peak_excess,
                failing_pixels,
                failing_region,
                headroom,
                text,
                ..
            } => {
                let failed_text = text.filter(|text| text.headroom < 0.0);
                if let (Some(text), 0) = (failed_text, failing_pixels) {
                    return format!(
                        "text up to {}% of a tile differs",
                        percent(text.worst_tile_diff_ratio)
                    );
                }
                let mut gates = format!("min local SSIM {}", similarity(*min_ssim));
                if headroom.color < 0.0 {
                    let _infallible = write!(
                        gates,
                        ", colors deviate by up to {} (8-bit units)",
                        color(*peak_excess)
                    );
                }
                if let Some(text) = failed_text {
                    let _infallible = write!(
                        gates,
                        ", text up to {}% of a tile",
                        percent(text.worst_tile_diff_ratio)
                    );
                }
                match failing_region {
                    Some(bounds) => format!("changed area at {}: {gates}", region(bounds)),
                    None => gates,
                }
            }
        }
    }

    /// A region of a comparison besides the whole image, with its metrics.
    ///
    /// E.g. `changed area (1, 2) 3x4px: min local SSIM 0.500, mean 0.710` or `worst text tile
    /// (0, 0) 16x16px: 18.00% differ`; `None` for the whole image (its metrics are the
    /// comparison's).
    #[must_use]
    pub fn region_summary(region: &super::RegionMetrics) -> Option<String> {
        use super::RegionKind;

        let name = match region.kind {
            RegionKind::Image => return None,
            RegionKind::Text => "worst text tile",
            RegionKind::Changed => "changed area",
            RegionKind::Failing => "failing area",
        };
        let at = region.rect.map(|rect| format!(" {}", self::region(&rect)));
        let metrics = match region.metrics {
            Metrics::Pixel { diff_ratio, .. } => format!("{}% differ", percent(diff_ratio)),
            Metrics::Ssim {
                min_ssim,
                mean_ssim,
                ..
            } => format!(
                "min local SSIM {}, mean {}",
                similarity(min_ssim),
                similarity(mean_ssim)
            ),
        };
        Some(format!("{name}{}: {metrics}", at.unwrap_or_default()))
    }

    /// Both image sizes, e.g. `golden is 100x60px, test image is 100x61px`.
    #[must_use]
    pub fn dimension_summary(golden: (u32, u32), candidate: (u32, u32)) -> String {
        format!(
            "golden is {}x{}px, test image is {}x{}px",
            golden.0, golden.1, candidate.0, candidate.1
        )
    }

    /// The note on a case compared with the shared golden ([`super::GoldenImage::fallback`]).
    ///
    /// Its platform has no own golden yet; written without an intermediate `String`, e.g.
    /// `compared with test/goldens/a.png of the fallback platform`.
    #[derive(Debug, Clone, Copy)]
    pub struct ComparedWithFallback<'a>(pub &'a str);

    impl std::fmt::Display for ComparedWithFallback<'_> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "compared with {} of the fallback platform", self.0)
        }
    }

    /// Why an image cannot be analyzed with SSIM: `width`x`height` exceeds the engine's budget.
    #[must_use]
    pub fn too_large_for_ssim(width: u32, height: u32) -> String {
        format!(
            "{width}x{height} exceeds the SSIM analysis budget of {} pixels; use exact or pixel \
             mode or a smaller capture",
            gleon_engine::ssim::MAX_ANALYSIS_PIXELS
        )
    }

    #[cfg(test)]
    #[allow(
        clippy::unwrap_used,
        clippy::pedantic,
        reason = "test code: panics are assertions"
    )]
    mod tests {
        use super::*;
        use crate::case::SsimHeadroom;

        #[test]
        fn test_numbers_are_rounded_to_what_matters() {
            for (value, min, max, expected) in [
                (8.0, 0, 2, "8"),
                (7.5, 0, 2, "7.5"),
                (0.8, 3, 4, "0.800"),
                (0.9995, 3, 4, "0.9995"),
                (0.999_95, 3, 4, "1.000"),
                (146.0, 1, 1, "146.0"),
                (100.0, 0, 0, "100"),
                (-1.0, 2, 4, "-1.00"),
                (-0.0, 2, 4, "0.00"),
                (7.000_000_000_000_001, 2, 4, "7.00"),
            ] {
                assert_eq!(decimal(value, min, max), expected, "{value}");
            }
            for (ratio, expected) in [
                (0.0, "0.00"),
                (0.0625, "6.25"),
                (1.0 / 6000.0, "0.0167"),
                (0.00001, "0.001"),
                (0.07, "7.00"),
                (1.0, "100.00"),
                (1e-9, "<0.0001"),
            ] {
                assert_eq!(percent(ratio), expected, "{ratio}");
            }
            assert_eq!(signed(similarity(0.131)), "+0.131");
            assert_eq!(signed(color(-138.0)), "-138.0");
        }

        #[test]
        fn test_descriptions_name_the_thresholds() {
            assert_eq!(tolerance(&Tolerance::Exact {}), "exact");
            let pixel = Tolerance::pixel;
            assert_eq!(tolerance(&pixel(0.01)), "pixel ≤ 1.00%");
            assert_eq!(tolerance(&pixel(0.00001)), "pixel ≤ 0.001%");
            assert_eq!(tolerance(&pixel(-0.0)), "pixel ≤ 0.00%");
            assert_eq!(tolerance(&pixel(1e-9)), "pixel <0.0001%");
            assert_eq!(
                tolerance(&Tolerance::Pixel {
                    max_diff_ratio: 0.01,
                    channel_tolerance: 4,
                    anti_alias: true,
                    edge_threshold: 64,
                }),
                "pixel ≤ 1.00%, ±4 per channel, aa ignored, edges >64 ignored"
            );
            assert_eq!(
                text_tolerance(&TextTolerance(0.0625)),
                "text ≤ 6.25% per tile"
            );
            assert_eq!(
                text_tolerance(&TextTolerance(1e-9)),
                "text <0.0001% per tile"
            );
            assert_eq!(text_tolerance(&TextTolerance::DEFAULT), "text ignored");
            assert_eq!(at_most_tight(0.0625), "≤6.25%");
            assert_eq!(at_most_tight(1e-9), "<0.0001%");
            let ssim = |min_similarity, color_tolerance| Tolerance::Ssim {
                min_similarity,
                color_tolerance,
            };
            assert_eq!(tolerance(&ssim(0.8, 8.0)), "ssim ≥ 0.800, color ±8");
            assert_eq!(tolerance(&ssim(0.9995, 7.5)), "ssim ≥ 0.9995, color ±7.5");
        }

        fn ssim_metrics(failing_region: Option<Region>, color: f64) -> Metrics {
            Metrics::Ssim {
                min_ssim: 0.5,
                mean_ssim: 0.9,
                max_excess: 138.0,
                peak_excess: 146.0,
                changed_pixels: 1,
                changed_region: failing_region,
                failing_pixels: 1,
                failing_region,
                headroom: SsimHeadroom {
                    similarity: -0.3,
                    color,
                },
                text: None,
            }
        }

        #[test]
        fn test_summaries() {
            let pixel = Metrics::Pixel {
                total_pixels: 6000,
                diff_pixels: 1,
                tolerated_pixels: 0,
                edge_pixels: 0,
                diff_ratio: 1.0 / 6000.0,
                headroom: -1.0 / 6000.0,
                text: None,
            };
            assert_eq!(metrics_summary(&pixel), "0.0167% (1 of 6000px) differ");
            // Text is named only when it failed its tolerance, not for differences within it
            // (another OS's rasterization under the default, which never fails).
            let with_text = |headroom| Metrics::Pixel {
                total_pixels: 6000,
                diff_pixels: 1,
                tolerated_pixels: 0,
                edge_pixels: 0,
                diff_ratio: 1.0 / 6000.0,
                headroom: -1.0 / 6000.0,
                text: Some(super::super::TextMetrics {
                    pixels: 300,
                    diff_pixels: 120,
                    worst_tile_diff_ratio: 0.4,
                    headroom,
                }),
            };
            assert_eq!(
                metrics_summary(&with_text(0.6)),
                "0.0167% (1 of 6000px) differ"
            );
            assert_eq!(
                metrics_summary(&with_text(-0.3)),
                "0.0167% (1 of 6000px) differ, text up to 40.00% of a tile"
            );
            // Text alone failed (the strict pixels all match, or there are none).
            let text_only = Metrics::Pixel {
                total_pixels: 0,
                diff_pixels: 0,
                tolerated_pixels: 0,
                edge_pixels: 0,
                diff_ratio: 0.0,
                headroom: 0.0,
                text: Some(super::super::TextMetrics {
                    pixels: 300,
                    diff_pixels: 120,
                    worst_tile_diff_ratio: 0.4,
                    headroom: -0.3,
                }),
            };
            assert_eq!(
                metrics_summary(&text_only),
                "text up to 40.00% of a tile differs"
            );
            let area = Region {
                x: 10,
                y: 10,
                width: 1,
                height: 1,
            };
            assert_eq!(
                metrics_summary(&ssim_metrics(Some(area), -138.0)),
                "changed area at (10, 10) 1x1px: min local SSIM 0.500, colors deviate by up to \
                 146.0 (8-bit units)"
            );
            assert_eq!(
                metrics_summary(&ssim_metrics(None, 1.0)),
                "min local SSIM 0.500"
            );
            assert_eq!(
                dimension_summary((100, 60), (100, 61)),
                "golden is 100x60px, test image is 100x61px"
            );
            assert!(too_large_for_ssim(9, 9).starts_with("9x9 exceeds the SSIM analysis budget"));
        }
    }
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

    #[test]
    fn test_outcome_names() {
        for outcome in [
            CaseOutcome::Identical,
            CaseOutcome::Match,
            CaseOutcome::Mismatch,
            CaseOutcome::DimensionMismatch,
            CaseOutcome::Error,
            CaseOutcome::Updated,
            CaseOutcome::Missing,
        ] {
            assert_eq!(
                serde_json::to_value(outcome).unwrap(),
                outcome.as_str(),
                "the case report uses the names of `as_str`"
            );
        }
    }

    fn ssim_measurement() -> Measurement {
        Measurement::Ssim {
            mean_ssim: 0.99,
            min_ssim: 0.931,
            max_excess: 1.5,
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
            changed_local: None,
            failing_local: None,
            text: None,
        }
    }

    #[test]
    fn test_ssim_headroom() {
        let tolerance = Tolerance::Ssim {
            min_similarity: 0.8,
            color_tolerance: 8.0,
        };
        let metrics = Metrics::from_measurement(&ssim_measurement(), &tolerance, None);
        assert!(
            matches!(
                metrics,
                Some(Metrics::Ssim { headroom, max_excess: 1.5, .. })
                    if (headroom.similarity - 0.131).abs() < 1e-9
                        && (headroom.color - 2.8).abs() < 1e-9
            ),
            "{metrics:?}"
        );
    }

    /// An SSIM comparison lists its changed and failing regions after the whole image, with the
    /// policy's metrics inside each.
    #[test]
    fn test_regions_of_an_ssim_comparison() {
        let local = |x, failing_pixels| gleon_engine::SsimRegion {
            region: Region {
                x,
                y: 0,
                width: 4,
                height: 4,
            },
            min_ssim: 0.5,
            mean_ssim: 0.7,
            peak_excess: 20.0,
            changed_pixels: 9,
            failing_pixels,
        };
        let measurement = Measurement::Ssim {
            mean_ssim: 0.99,
            min_ssim: 0.5,
            max_excess: 12.0,
            peak_excess: 20.0,
            changed_pixels: 9,
            changed_region: None,
            failing_pixels: 9,
            failing_region: None,
            changed_local: Some(local(0, 9)),
            failing_local: Some(local(1, 9)),
            text: None,
        };
        let tolerance = Tolerance::Ssim {
            min_similarity: 0.8,
            color_tolerance: 8.0,
        };
        let metrics = Metrics::from_measurement(&measurement, &tolerance, None).unwrap();
        let regions = RegionMetrics::of(metrics, &measurement, &tolerance, None);
        let kinds: Vec<_> = regions.iter().map(|region| region.kind).collect();
        assert_eq!(
            kinds,
            [RegionKind::Image, RegionKind::Changed, RegionKind::Failing]
        );
        assert_eq!(regions[2].rect.unwrap().x, 1);
        assert!(matches!(
            regions[1].metrics,
            Metrics::Ssim {
                mean_ssim,
                max_excess,
                headroom,
                ..
            } if (mean_ssim - 0.7).abs() < 1e-12
                && (max_excess - 12.0).abs() < 1e-12
                && (headroom.similarity + 0.3).abs() < 1e-12
                && (headroom.color + 12.0).abs() < 1e-12
        ));
        // A pixel comparison has no such regions.
        let pixel = Measurement::Pixel {
            checked_pixels: 4,
            diff_count: 0,
            tolerated_count: 0,
            edge_count: 0,
            text: None,
        };
        let metrics = Metrics::from_measurement(&pixel, &Tolerance::Exact {}, None).unwrap();
        assert_eq!(
            RegionMetrics::of(metrics, &pixel, &Tolerance::Exact {}, None).len(),
            1
        );
    }

    #[test]
    fn test_pixel_headroom_for_exact_and_pixel() {
        let measurement = Measurement::Pixel {
            checked_pixels: 100,
            diff_count: 5,
            tolerated_count: 0,
            edge_count: 0,
            text: None,
        };
        assert_eq!(
            Metrics::from_measurement(&measurement, &Tolerance::Exact {}, None),
            Some(Metrics::Pixel {
                total_pixels: 100,
                diff_pixels: 5,
                tolerated_pixels: 0,
                edge_pixels: 0,
                diff_ratio: 0.05,
                headroom: -0.05,
                text: None,
            })
        );
        let pixel = Tolerance::pixel(0.1);
        let metrics = Metrics::from_measurement(&measurement, &pixel, None);
        assert!(
            matches!(metrics, Some(Metrics::Pixel { headroom, .. }) if (headroom - 0.05).abs() < 1e-12),
            "{metrics:?}"
        );
        // Pixels the options let pass are reported apart (those the edge mask hid on their own),
        // and only when there are some.
        let tolerated = Metrics::from_measurement(
            &Measurement::Pixel {
                checked_pixels: 100,
                diff_count: 5,
                tolerated_count: 7,
                edge_count: 3,
                text: None,
            },
            &pixel,
            None,
        )
        .unwrap();
        assert!(matches!(
            tolerated,
            Metrics::Pixel {
                diff_pixels: 5,
                tolerated_pixels: 7,
                edge_pixels: 3,
                ..
            }
        ));
        let json = serde_json::to_string(&tolerated).unwrap();
        assert!(
            json.contains(r#""tolerated_pixels":7,"edge_pixels":3"#),
            "{json}"
        );
        let json = serde_json::to_string(&metrics.unwrap()).unwrap();
        assert!(!json.contains("tolerated_pixels") && !json.contains("edge_pixels"));
        let empty = Metrics::from_measurement(
            &Measurement::Pixel {
                checked_pixels: 0,
                diff_count: 0,
                tolerated_count: 0,
                edge_count: 0,
                text: None,
            },
            &pixel,
            None,
        );
        assert!(matches!(
            empty,
            Some(Metrics::Pixel {
                diff_ratio: 0.0,
                ..
            })
        ));
    }

    #[test]
    fn test_differs_counts_the_pixels_every_option_let_pass() {
        let pixel = |diff_count, tolerated_count, edge_count| {
            Metrics::from_measurement(
                &Measurement::Pixel {
                    checked_pixels: 100,
                    diff_count,
                    tolerated_count,
                    edge_count,
                    text: None,
                },
                &Tolerance::pixel(0.1),
                None,
            )
            .is_some_and(|metrics| metrics.differs())
        };
        assert!(!pixel(0, 0, 0));
        assert!(pixel(1, 0, 0));
        assert!(pixel(0, 1, 0));
        // A pass whose every differing pixel lies on an edge is not the golden itself.
        assert!(pixel(0, 0, 1));
    }

    #[test]
    fn test_mode_mismatch_is_rejected() {
        assert_eq!(
            Metrics::from_measurement(&ssim_measurement(), &Tolerance::Exact {}, None),
            None
        );
        let ssim = Tolerance::Ssim {
            min_similarity: 0.8,
            color_tolerance: 8.0,
        };
        assert_eq!(
            Metrics::from_measurement(
                &Measurement::Pixel {
                    checked_pixels: 1,
                    diff_count: 0,
                    tolerated_count: 0,
                    edge_count: 0,
                    text: None
                },
                &ssim,
                None
            ),
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
        let metrics = Metrics::from_measurement(&ssim_measurement(), &tolerance, None).unwrap();
        let json = serde_json::to_value(RegionMetrics::whole_image(metrics)).unwrap();
        assert_eq!(json["kind"], "image");
        assert!(json.get("rect").is_none());
        assert_eq!(json["metrics"]["kind"], "ssim");
        assert_eq!(json["metrics"]["changed_region"]["width"], 3);
        assert!(json["metrics"].get("failing_region").is_none());
        let back: RegionMetrics = serde_json::from_value(json).unwrap();
        assert_eq!(back.metrics, metrics);
    }

    /// A PNG header (signature and `IHDR`) of `width` x `height`, without image data.
    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut header = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        header.extend_from_slice(&width.to_be_bytes());
        header.extend_from_slice(&height.to_be_bytes());
        header
    }

    #[test]
    fn test_png_size_reads_the_header_only() {
        let header = png_header(100, 60);
        assert_eq!(png_size(&header), Some((100, 60)));
        assert_eq!(png_size(&header[..23]), None);
        let mut other = header.clone();
        other[12..16].copy_from_slice(b"IDAT");
        assert_eq!(png_size(&other), None);
        assert_eq!(png_size(&[7; 64]), None);
        assert_eq!(png_size(&png_header(0, 60)), None);
        assert_eq!(png_size(&png_header(100, 0)), None);
    }

    #[test]
    fn test_images_of_png_bytes() {
        assert_eq!(
            Sha256Hex::of(&[1, 2, 3]).as_str(),
            "039058c6f2c0cb492c533b0a4d14ef77cc0f78abccced5287d84a1a2011cfb81"
        );
        let header = png_header(4, 3);
        let blob: ImageHash = format!("sha256:{}", "a".repeat(64)).parse().unwrap();
        let golden = GoldenImage::of("a.png".to_owned(), Some(&header), Some(blob.clone()));
        assert_eq!(
            golden,
            GoldenImage {
                path: "a.png".to_owned(),
                sha256: Some(Sha256Hex::of(&header)),
                blob: Some(blob),
                width: Some(4),
                height: Some(3),
                fallback: None,
            }
        );
        let missing = GoldenImage::of("a.png".to_owned(), None, None);
        assert_eq!((missing.sha256, missing.width), (None, None));
        let candidate = CandidateImage::of(b"garbage");
        assert_eq!(candidate.sha256, Some(Sha256Hex::of(b"garbage")));
        assert_eq!((candidate.width, candidate.height), (None, None));
    }

    #[test]
    fn test_timings_in_milliseconds() {
        assert_eq!(millis(Duration::from_micros(1_234)), 1.234);
        assert_eq!(
            CaseTimings::new(Duration::from_millis(3), None),
            CaseTimings {
                total: 3.0,
                native: None
            }
        );
        assert_eq!(
            CaseTimings::new(Duration::ZERO, Some(Duration::from_micros(500))).native,
            Some(0.5)
        );
    }

    #[test]
    fn test_run_ids() {
        assert_eq!(
            RunId::new("run-20260930T120000Z-ab12").unwrap().as_str(),
            "run-20260930T120000Z-ab12"
        );
        assert_eq!(
            RunId::new("12345-2.local_1").unwrap().as_str(),
            "12345-2.local_1"
        );
        let too_long = "a".repeat(RunId::MAX_LEN + 1);
        for bad in ["", "with space", "a/b", "12:00", "ü", too_long.as_str()] {
            assert_eq!(RunId::new(bad), Err(InvalidRunId(bad.to_owned())), "{bad}");
        }
        assert_eq!(
            InvalidRunId("a b".to_owned()).to_string(),
            "a run id must be 1 to 128 ASCII letters, digits, `.`, `_` or `-` (got 'a b')"
        );
        assert_eq!(RunId::from_env(None), Ok(None));
        assert_eq!(RunId::from_env(Some("  ")), Ok(None));
        assert_eq!(
            RunId::from_env(Some(" local ")),
            Ok(Some(RunId::new("local").unwrap()))
        );
        assert_eq!(
            RunId::from_env(Some("a b")),
            Err(InvalidRunId("a b".to_owned()))
        );
        let parsed: RunId = serde_json::from_value(serde_json::json!("local")).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), "local");
        assert!(serde_json::from_value::<RunId>(serde_json::json!("a b")).is_err());
    }

    #[test]
    fn test_names_in_case_reports() {
        for (kind, name) in [
            (CaseErrorKind::InvalidInput, "invalid_input"),
            (CaseErrorKind::Config, "config"),
            (CaseErrorKind::Io, "io"),
            (CaseErrorKind::Image, "image"),
            (CaseErrorKind::Internal, "internal"),
        ] {
            assert_eq!(kind.as_str(), name);
            assert_eq!(serde_json::to_value(kind).unwrap(), name);
        }
        assert_eq!(
            CaseOutcome::DimensionMismatch.as_str(),
            "dimension_mismatch"
        );
    }

    #[test]
    fn test_artifacts_are_written_and_stale_ones_removed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let dir = ArtifactsDir::default();
        let key = &PlatformKey::parse("linux-x86_64").unwrap();
        let images = ArtifactImages {
            golden: Some(b"golden"),
            candidate: Some(b"candidate"),
            diff: Some(b"diff"),
        };
        let artifacts = write_artifacts(root, &dir, key, "test/goldens/a", images).unwrap();
        let folder = root.join(".gleon/runs/latest/artifacts/linux-x86_64/test/goldens/a");
        let path = |file: &str| {
            Some(format!(
                ".gleon/runs/latest/artifacts/linux-x86_64/test/goldens/a/{file}"
            ))
        };
        assert_eq!(
            artifacts,
            Some(Artifacts {
                golden: path("golden.png"),
                candidate: path("candidate.png"),
                diff: path("diff.png"),
            })
        );
        assert_eq!(std::fs::read(folder.join("diff.png")).unwrap(), b"diff");

        let candidate_only = ArtifactImages {
            candidate: Some(b"new"),
            ..ArtifactImages::default()
        };
        let artifacts = write_artifacts(root, &dir, key, "test/goldens/a", candidate_only)
            .unwrap()
            .unwrap();
        assert_eq!((artifacts.golden, artifacts.diff), (None, None));
        let mut files: Vec<_> = std::fs::read_dir(&folder)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        files.sort();
        assert_eq!(files, ["candidate.png"], "stale images are removed");

        // A pass keeps nothing: the images go, the (empty) folder stays for `gleon clean`.
        let none =
            write_artifacts(root, &dir, key, "test/goldens/a", ArtifactImages::default()).unwrap();
        assert_eq!(none, None);
        assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 0);
        let absent = write_artifacts(root, &dir, key, "test/goldens/b", ArtifactImages::default());
        assert_eq!(absent.unwrap(), None);

        // A directory where an image goes cannot be removed as a stale file.
        std::fs::create_dir_all(folder.join("diff.png/inside")).unwrap();
        assert!(write_artifacts(root, &dir, key, "test/goldens/a", candidate_only).is_err());

        for name in ["../../outside", "a/../b", "Upper", "a\\b", ""] {
            let err = write_artifacts(root, &dir, key, name, candidate_only).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{name}");
        }
        assert!(!root.join("outside").exists());
    }

    #[test]
    fn test_artifacts_of_platforms_are_kept_apart() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let dir = ArtifactsDir::default();
        let candidate = ArtifactImages {
            candidate: Some(b"candidate"),
            ..ArtifactImages::default()
        };
        let ios = PlatformKey::parse("os=ios-sim+arch=arm").unwrap();
        let kept = write_artifacts(root, &dir, &ios, "test/goldens/a", candidate)
            .unwrap()
            .unwrap();
        assert_eq!(
            kept.candidate.as_deref(),
            Some(".gleon/runs/latest/artifacts/os=ios-sim+arch=arm/test/goldens/a/candidate.png")
        );

        // A pass on another platform removes its own images only.
        let none = write_artifacts(
            root,
            &dir,
            &PlatformKey::parse("linux-x86_64").unwrap(),
            "test/goldens/a",
            ArtifactImages::default(),
        )
        .unwrap();
        assert_eq!(none, None);
        let ios_candidate = root
            .join(".gleon/runs/latest/artifacts/os=ios-sim+arch=arm/test/goldens/a/candidate.png");
        assert_eq!(std::fs::read(ios_candidate).unwrap(), b"candidate");
    }

    #[test]
    fn test_reports_are_written_and_parse_back() {
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        let report = CaseReport {
            schema_version: CASE_SCHEMA_VERSION,
            name: "test/goldens/a".to_owned(),
            golden: GoldenImage::of("test/goldens/a.png".to_owned(), None, None),
            candidate: CandidateImage::of(b"png"),
            source: Source {
                tool: "gleon_cli".to_owned(),
                tool_version: "0.2.2".to_owned(),
                renderer: None,
            },
            platform: PlatformConfig::host(),
            test: None,
            comparison: Comparison {
                tolerance: Tolerance::Exact {},
                masks: vec![],
                policy_version: 2,
                text_tolerance: None,
            },
            outcome: CaseOutcome::Error,
            error_kind: Some(CaseErrorKind::Image),
            message: Some("candidate image: bad".to_owned()),
            metrics: None,
            regions: vec![],
            artifacts: None,
            timings_ms: CaseTimings::new(Duration::from_millis(1), None),
            run_id: Some(RunId::new("local").unwrap()),
            recorded_at: chrono::Utc::now(),
        };
        report.write(&gleon_dir).unwrap();
        let outside = CaseReport {
            name: "../../outside".to_owned(),
            ..report.clone()
        };
        assert_eq!(
            outside.write(&gleon_dir).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let inconsistent = CaseReport {
            error_kind: None,
            ..report.clone()
        };
        assert_eq!(
            inconsistent.write(&gleon_dir).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let key = PlatformKey::host();
        assert_eq!(&report.platform_key().unwrap(), key);
        let file = CaseReport::path(&gleon_dir, key, "test/goldens/a");
        assert_eq!(
            file,
            gleon_dir
                .join("runs/latest/cases")
                .join(key)
                .join("test/goldens/a.json")
        );
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.ends_with("}\n"));
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["error_kind"], "image");
        assert_eq!(json["run_id"], "local");
        assert!(json.get("artifacts").is_none());
        assert_eq!(serde_json::from_str::<CaseReport>(&text).unwrap(), report);
        assert_eq!(CaseReport::parse(text.as_bytes()).unwrap(), report);

        // The same golden on another platform gets its own file; the host's stays.
        let other = CaseReport {
            platform: PlatformConfig::Opaque("freebsd-riscv64".to_owned()),
            ..report.clone()
        };
        other.write(&gleon_dir).unwrap();
        let other_file = CaseReport::path(
            &gleon_dir,
            &PlatformKey::parse("freebsd-riscv64").unwrap(),
            "test/goldens/a",
        );
        assert_ne!(other_file, file);
        assert_eq!(
            CaseReport::parse(&std::fs::read(&other_file).unwrap()).unwrap(),
            other
        );
        assert_eq!(
            CaseReport::parse(&std::fs::read(&file).unwrap()).unwrap(),
            report
        );

        // An OS or architecture with `-` gets the explicit form of the key, labels follow.
        let ios = CaseReport {
            platform: PlatformConfig::Structured(crate::platform::PlatformFields {
                os: Some("ios-sim".to_owned()),
                arch: Some("arm".to_owned()),
                ..crate::platform::PlatformFields::default()
            }),
            ..report.clone()
        };
        ios.write(&gleon_dir).unwrap();
        let ios_file = gleon_dir.join("runs/latest/cases/os=ios-sim+arch=arm/test/goldens/a.json");
        assert_eq!(
            CaseReport::parse(&std::fs::read(&ios_file).unwrap()).unwrap(),
            ios
        );

        // A platform without a key names no directory.
        let keyless = CaseReport {
            platform: PlatformConfig::Opaque("bad key".to_owned()),
            ..report.clone()
        };
        assert_eq!(
            keyless.write(&gleon_dir).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    /// The images of a report lie at `<artifacts dir>/<its platform key>/<its name>/<file>`, all
    /// in one artifacts directory: a report never lists the images of another platform or golden.
    #[test]
    fn test_artifact_paths_follow_the_platform_and_name_of_the_report() {
        let mismatch = |platform: serde_json::Value, dir: &str, key: &str| {
            let path = |file: &str| format!("{dir}/{key}/test/goldens/a/{file}");
            serde_json::json!({
                "schema_version": 4,
                "name": "test/goldens/a",
                "golden": {"path": "test/goldens/a.png", "sha256": "1".repeat(64)},
                "candidate": {"sha256": "0".repeat(64)},
                "source": {"tool": "gleon_flutter", "tool_version": "0.2.0"},
                "platform": platform,
                "comparison": {"tolerance": {"kind": "exact"}, "masks": [], "policy_version": 2},
                "outcome": "mismatch",
                "metrics": {
                    "kind": "pixel", "total_pixels": 4, "diff_pixels": 1, "diff_ratio": 0.25,
                    "headroom": -0.25
                },
                "regions": [],
                "artifacts": {
                    "golden": path("golden.png"),
                    "candidate": path("candidate.png"),
                    "diff": path("diff.png")
                },
                "timings_ms": {"total": 1.0},
                "recorded_at": "2026-09-30T12:00:00Z"
            })
        };
        let parse = |json: &serde_json::Value| CaseReport::parse(json.to_string().as_bytes());
        let macos = serde_json::json!({"os": "macos", "arch": "aarch64"});
        let ios = serde_json::json!({"os": "ios-sim", "arch": "arm"});
        let default_dir = ".gleon/runs/latest/artifacts";
        for (platform, dir, key) in [
            (&macos, default_dir, "macos-aarch64"),
            (&macos, ".gleon/runs/ci", "macos-aarch64"),
            (&ios, default_dir, "os=ios-sim+arch=arm"),
        ] {
            let json = mismatch(platform.clone(), dir, key);
            assert!(parse(&json).is_ok(), "{json}");
        }

        let valid = mismatch(macos.clone(), default_dir, "macos-aarch64");
        let path = |dir: &str, key: &str, name: &str, file: &str| {
            serde_json::Value::from(format!("{dir}/{key}/{name}/{file}"))
        };
        for (field, value) in [
            // Another platform's images.
            (
                "golden",
                path(default_dir, "linux-x86_64", "test/goldens/a", "golden.png"),
            ),
            // Another golden's images.
            (
                "candidate",
                path(
                    default_dir,
                    "macos-aarch64",
                    "test/goldens/b",
                    "candidate.png",
                ),
            ),
            // The image of another field.
            (
                "golden",
                path(default_dir, "macos-aarch64", "test/goldens/a", "diff.png"),
            ),
            // Not an artifacts directory.
            (
                "diff",
                path(
                    ".gleon/runs/latest/cases",
                    "macos-aarch64",
                    "test/goldens/a",
                    "diff.png",
                ),
            ),
            (
                "diff",
                path("test", "macos-aarch64", "test/goldens/a", "diff.png"),
            ),
            // Two artifacts directories in one report.
            (
                "diff",
                path(
                    ".gleon/runs/ci",
                    "macos-aarch64",
                    "test/goldens/a",
                    "diff.png",
                ),
            ),
            // No platform directory.
            (
                "diff",
                serde_json::Value::from(format!("{default_dir}/test/goldens/a/diff.png")),
            ),
        ] {
            let mut broken = valid.clone();
            broken["artifacts"][field] = value.clone();
            let err = parse(&broken).unwrap_err();
            assert!(
                matches!(
                    err,
                    CaseParseError::Inconsistent(InconsistentCase::ArtifactPath(_))
                ),
                "{field}: {value}: {err}"
            );
        }
        assert!(
            InconsistentCase::ArtifactPath("golden")
                .to_string()
                .contains("<artifacts dir>/<platform key>/<name>/golden.png")
        );
    }

    #[test]
    fn test_parsing_rejects_other_versions_and_paths_leaving_the_workspace() {
        let valid = serde_json::json!({
            "schema_version": 4,
            "name": "test/goldens/a",
            "golden": {"path": "test/goldens/A.png"},
            "candidate": {"sha256": "0".repeat(64)},
            "source": {"tool": "gleon_flutter", "tool_version": "0.2.0"},
            "platform": {"os": "macos", "arch": "aarch64"},
            "comparison": {"tolerance": {"kind": "exact"}, "masks": [], "policy_version": 2},
            "outcome": "missing",
            "regions": [],
            "artifacts": {"candidate": ".gleon/runs/latest/artifacts/macos-aarch64/test/goldens/a/candidate.png"},
            "timings_ms": {"total": 1.0},
            "recorded_at": "2026-09-30T12:00:00Z"
        });
        let parse = |json: &serde_json::Value| CaseReport::parse(json.to_string().as_bytes());
        assert!(parse(&valid).is_ok());
        // Text is judged under SSIM tolerances too (policy 4).
        let mut ssim_text = valid.clone();
        ssim_text["comparison"] = serde_json::json!({
            "tolerance": {"kind": "ssim", "min_similarity": 0.8, "color_tolerance": 8.0},
            "text_tolerance": 1.0, "masks": [], "policy_version": 4
        });
        assert!(parse(&ssim_text).is_ok());
        let mut keyed = valid.clone();
        keyed["artifacts"]["candidate"] =
            ".gleon/runs/latest/artifacts/os=ios-sim+arch=arm/test/goldens/a/candidate.png".into();
        keyed["platform"] = serde_json::json!({"os": "ios-sim", "arch": "arm"});
        assert_eq!(
            parse(&keyed).unwrap().platform_key().unwrap(),
            "os=ios-sim+arch=arm"
        );
        let mut outside = valid.clone();
        outside["golden"]["fallback"] = "../a.png".into();
        assert!(matches!(parse(&outside), Err(CaseParseError::Json(_))));
        // A pass against the fallback golden keeps its candidate for `gleon approve`.
        let mut seeding = valid.clone();
        seeding["outcome"] = "match".into();
        seeding["metrics"] = serde_json::json!({
            "kind": "pixel", "total_pixels": 4, "diff_pixels": 0, "diff_ratio": 0.0, "headroom": 0.0
        });
        seeding["golden"] = serde_json::json!({
            "path": "test/goldens/linux-x86_64/A.png", "fallback": "test/goldens/A.png",
            "sha256": "1".repeat(64)
        });
        let report = parse(&seeding).unwrap();
        assert_eq!(report.golden.compared(), "test/goldens/A.png");

        // Other writers may spell an absent image as `null`, which the schema allows.
        let mut nulls = valid.clone();
        nulls["artifacts"]["golden"] = serde_json::Value::Null;
        nulls["artifacts"]["diff"] = serde_json::Value::Null;
        let artifacts = parse(&nulls).unwrap().artifacts.unwrap();
        assert_eq!((artifacts.golden, artifacts.diff), (None, None));
        assert!(artifacts.candidate.is_some());

        // Version 2 had other rules (any `platform`, flat artifact paths): its writers are
        // older integrations, told apart from broken reports.
        for old in [1, 2] {
            let mut older = valid.clone();
            older["schema_version"] = old.into();
            assert!(
                matches!(parse(&older), Err(CaseParseError::UnsupportedVersion(v)) if v == old),
                "{old}"
            );
            assert!(
                serde_json::from_value::<CaseReport>(older).is_err(),
                "serde alone rejects other versions too"
            );
        }
        assert!(matches!(
            CaseReport::parse(b"[]"),
            Err(CaseParseError::Json(_))
        ));

        for (field, value) in [
            ("/name", "../outside"),
            ("/name", "Test/A"),
            ("/name", "test\\goldens\\a"),
            ("/golden/path", "../../.bashrc"),
            ("/golden/path", "/etc/passwd"),
            ("/golden/path", "C:/golden.png"),
            ("/golden/path", "test/goldens/os=ios-sim+arch=arm/a.png"),
            ("/golden/path", "test/goldens/a+b.png"),
            ("/artifacts/candidate", "/etc/passwd"),
            ("/artifacts/candidate", "a/./b.png"),
            ("/platform/os", "Mac OS"),
            ("/platform", "bad key"),
        ] {
            let mut broken = valid.clone();
            *broken.pointer_mut(field).unwrap() = value.into();
            let err = parse(&broken).unwrap_err();
            assert!(matches!(err, CaseParseError::Json(_)), "{field}: {value}");
        }
        // The key of the platform names the report's directory: it needs an OS.
        for platform in [serde_json::json!({}), serde_json::json!({"arch": "arm"})] {
            let mut keyless = valid.clone();
            keyless["platform"] = platform;
            let err = parse(&keyless).unwrap_err();
            assert!(
                matches!(&err, CaseParseError::Json(e) if e.to_string().contains("platform.os is required")),
                "{err}"
            );
        }
        assert_eq!(
            CaseParseError::UnsupportedVersion(2).to_string(),
            "case report schema 2 is not supported (this gleon reads 4)"
        );

        let metrics = serde_json::json!({
            "kind": "pixel", "total_pixels": 4, "diff_pixels": 0, "diff_ratio": 0.0, "headroom": 0.0
        });
        for (patch, expected) in [
            (
                serde_json::json!({"error_kind": "io"}),
                InconsistentCase::ErrorKind,
            ),
            (
                serde_json::json!({"outcome": "error"}),
                InconsistentCase::ErrorKind,
            ),
            (
                serde_json::json!({"golden": {"path": "a.png", "sha256": "0".repeat(64)}}),
                InconsistentCase::MissingGoldenHash,
            ),
            (
                serde_json::json!({"golden": {"path": "linux-x86_64/a.png", "fallback": "a.png"}}),
                InconsistentCase::Fallback,
            ),
            (
                serde_json::json!({
                    "outcome": "match", "metrics": metrics,
                    "golden": {"path": "a.png", "sha256": "1".repeat(64)}
                }),
                InconsistentCase::Artifacts,
            ),
            (
                serde_json::json!({
                    "outcome": "match", "metrics": metrics,
                    "golden": {"path": "linux-x86_64/a.png", "fallback": "a.png", "sha256": "1".repeat(64)},
                    "artifacts": {"golden": ".gleon/runs/latest/artifacts/a/golden.png", "candidate": ".gleon/runs/latest/artifacts/a/candidate.png"}
                }),
                InconsistentCase::Artifacts,
            ),
            (
                serde_json::json!({"outcome": "identical", "golden": {"path": "a.png", "sha256": "1".repeat(64)}}),
                InconsistentCase::IdenticalHashes,
            ),
            (
                serde_json::json!({"metrics": metrics}),
                InconsistentCase::Metrics,
            ),
            (
                serde_json::json!({"candidate": {"width": 4, "height": 4}}),
                InconsistentCase::CandidateHash,
            ),
            (
                serde_json::json!({
                    "outcome": "identical", "artifacts": null, "candidate": {"width": 4},
                    "golden": {"path": "a.png", "sha256": "1".repeat(64)}
                }),
                InconsistentCase::IdenticalHashes,
            ),
            (
                serde_json::json!({"outcome": "updated", "golden": {"path": "a.png", "sha256": "0".repeat(64)}}),
                InconsistentCase::Artifacts,
            ),
            (
                serde_json::json!({
                    "outcome": "match", "artifacts": null, "metrics": metrics,
                    "golden": {"path": "a.png", "sha256": "1".repeat(64)},
                    "comparison": {"tolerance": {"kind": "ssim", "min_similarity": 0.8, "color_tolerance": 8.0}, "masks": [], "policy_version": 2}
                }),
                InconsistentCase::MetricsKind,
            ),
            // Regions measured in another mode than the tolerance, or SSIM regions of a pixel
            // comparison.
            (
                serde_json::json!({
                    "outcome": "match", "artifacts": null, "metrics": metrics,
                    "golden": {"path": "a.png", "sha256": "1".repeat(64)},
                    "regions": [{"kind": "image", "metrics": {
                        "kind": "ssim", "min_ssim": 1.0, "mean_ssim": 1.0, "max_excess": 0.0,
                        "peak_excess": 0.0, "changed_pixels": 0, "failing_pixels": 0,
                        "headroom": {"similarity": 0.2, "color": 8.0}
                    }}]
                }),
                InconsistentCase::RegionMetrics,
            ),
            (
                serde_json::json!({
                    "outcome": "match", "artifacts": null, "metrics": metrics,
                    "golden": {"path": "a.png", "sha256": "1".repeat(64)},
                    "regions": [{"kind": "changed", "rect": {"x": 0, "y": 0, "width": 1, "height": 1}, "metrics": metrics}]
                }),
                InconsistentCase::RegionMetrics,
            ),
            // Regions beyond the golden, or past the end of `u32`.
            (
                serde_json::json!({
                    "outcome": "match", "artifacts": null, "metrics": metrics,
                    "golden": {"path": "a.png", "sha256": "1".repeat(64), "width": 4, "height": 4},
                    "regions": [{"kind": "text", "rect": {"x": 2, "y": 0, "width": 4, "height": 1}, "metrics": metrics}]
                }),
                InconsistentCase::RegionBounds,
            ),
            (
                serde_json::json!({
                    "outcome": "match", "artifacts": null, "metrics": metrics,
                    "golden": {"path": "a.png", "sha256": "1".repeat(64)},
                    "regions": [{"kind": "text", "rect": {"x": u32::MAX, "y": 0, "width": 2, "height": 1}, "metrics": metrics}]
                }),
                InconsistentCase::RegionBounds,
            ),
            // Pixels counted as tolerated or on edges without the options that let them pass.
            (
                serde_json::json!({
                    "outcome": "match", "artifacts": null,
                    "metrics": {
                        "kind": "pixel", "total_pixels": 4, "diff_pixels": 0, "tolerated_pixels": 2,
                        "diff_ratio": 0.0, "headroom": 0.0
                    },
                    "golden": {"path": "a.png", "sha256": "1".repeat(64)}
                }),
                InconsistentCase::PixelOptions,
            ),
            (
                serde_json::json!({
                    "outcome": "match", "artifacts": null,
                    "metrics": {
                        "kind": "pixel", "total_pixels": 4, "diff_pixels": 0, "edge_pixels": 2,
                        "diff_ratio": 0.0, "headroom": 0.0
                    },
                    "golden": {"path": "a.png", "sha256": "1".repeat(64)},
                    "comparison": {"tolerance": {"kind": "pixel", "max_diff_ratio": 0.0, "anti_alias": true}, "masks": [], "policy_version": 4}
                }),
                InconsistentCase::PixelOptions,
            ),
            (
                serde_json::json!({
                    "comparison": {"tolerance": {"kind": "pixel", "max_diff_ratio": 1.5}, "masks": [], "policy_version": 2}
                }),
                InconsistentCase::ToleranceRange,
            ),
            (
                serde_json::json!({
                    "comparison": {"tolerance": {"kind": "exact"}, "text_tolerance": -0.5, "masks": [], "policy_version": 2}
                }),
                InconsistentCase::ToleranceRange,
            ),
            (
                serde_json::json!({
                    "outcome": "match", "artifacts": null,
                    "metrics": {
                        "kind": "pixel", "total_pixels": 4, "diff_pixels": 0, "diff_ratio": 0.0, "headroom": 0.0,
                        "text": {"pixels": 4, "diff_pixels": 0, "worst_tile_diff_ratio": 0.0, "headroom": 1.0}
                    },
                    "golden": {"path": "a.png", "sha256": "1".repeat(64)}
                }),
                InconsistentCase::TextTolerance,
            ),
        ] {
            let mut broken = valid.clone();
            for (key, value) in patch.as_object().unwrap() {
                broken[key] = value.clone();
            }
            assert!(
                matches!(parse(&broken), Err(CaseParseError::Inconsistent(e)) if e == expected),
                "{patch}"
            );
        }
    }
}
