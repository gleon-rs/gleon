//! Per-golden case report written to `.gleon/runs/latest/cases/<name>.json`, the comparison
//! metrics it carries, and the images of a failure next to it
//! (`<artifacts dir>/<name>/{golden,candidate,diff}.png`).
//!
//! Case reports are written by the integrations (the Flutter package through `gleon-ffi`) and are
//! meant as the one result format the gleon CLI reads too; the schema is committed as
//! `schema/case.v2.json`. Enums are internally tagged (`"kind"`) and every name is `snake_case`, so
//! non-Rust writers never mirror Rust type names. Names and paths are checked when a report is
//! read ([`CaseReport::parse`]), so a report never leads a reader outside its workspace.
//!
//! The directory holds the latest result of every golden: integrations run tests in many
//! processes, so no single writer can reset it, and each report replaces the previous one of its
//! golden. Reports name their run ([`RUN_ID_ENV`]), so readers can take one run whole; reports of
//! goldens that were since removed or renamed stay behind until `gleon clean`.

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
    platform::PlatformConfig,
    tolerance::Tolerance,
};

/// Version of the case report format.
pub const CASE_SCHEMA_VERSION: u32 = 2;

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
                    max_excess,
                    peak_excess,
                    changed_pixels,
                    changed_region,
                    failing_pixels,
                    failing_region,
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
        }
    }
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

impl CandidateImage {
    /// The candidate with the PNG bytes `png`.
    #[must_use]
    pub fn of(png: &[u8]) -> Self {
        let size = png_size(png);
        Self {
            sha256: Sha256Hex::of(png),
            width: size.map(|(width, _)| width),
            height: size.map(|(_, height)| height),
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

/// The images of a case, as paths relative to the workspace root (`/`-separated) under the
/// artifacts directory: `<artifacts dir>/<name>/{golden,candidate,diff}.png`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Artifacts {
    /// A copy of the golden (mismatches and dimension mismatches).
    #[serde(
        default,
        deserialize_with = "optional_workspace_path",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(regex(
        pattern = r"^(?!\.\.?(/|$))[A-Za-z0-9._-]+(/(?!\.\.?(/|$))[A-Za-z0-9._-]+)*$"
    ))]
    pub golden: Option<String>,
    /// The candidate (mismatches, dimension mismatches and missing goldens, for `gleon approve`).
    #[serde(
        default,
        deserialize_with = "optional_workspace_path",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(regex(
        pattern = r"^(?!\.\.?(/|$))[A-Za-z0-9._-]+(/(?!\.\.?(/|$))[A-Za-z0-9._-]+)*$"
    ))]
    pub candidate: Option<String>,
    /// The diff visualization (mismatches only).
    #[serde(
        default,
        deserialize_with = "optional_workspace_path",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(regex(
        pattern = r"^(?!\.\.?(/|$))[A-Za-z0-9._-]+(/(?!\.\.?(/|$))[A-Za-z0-9._-]+)*$"
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

/// File names of the images inside `<artifacts dir>/<name>/`.
pub const GOLDEN_ARTIFACT: &str = "golden.png";
/// See [`GOLDEN_ARTIFACT`].
pub const CANDIDATE_ARTIFACT: &str = "candidate.png";
/// See [`GOLDEN_ARTIFACT`].
pub const DIFF_ARTIFACT: &str = "diff.png";

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

impl ArtifactImages<'_> {
    /// Whether there is no image.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.golden.is_none() && self.candidate.is_none() && self.diff.is_none()
    }
}

/// Writes `images` to `<root>/<dir>/<name>/{golden,candidate,diff}.png` and returns their paths
/// relative to `root`.
///
/// The files of absent images are removed (and the folder, once empty), so the folder always
/// shows the latest outcome of the golden: an image left by an earlier failure never passes for
/// this one's. The images are regenerated by every run, so they are written atomically but not
/// flushed to disk ([`Durability::Atomic`]).
///
/// # Errors
/// Returns [`io::ErrorKind::InvalidInput`] if `name` is not a canonical test name, or the first
/// I/O error of writing or removing a file.
pub fn write_artifacts(
    root: &Path,
    dir: &ArtifactsDir,
    name: &str,
    images: ArtifactImages<'_>,
) -> io::Result<Artifacts> {
    validate_canonical_test_name(name)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let folder = name
        .split('/')
        .fold(dir.to_path(root), |folder, segment| folder.join(segment));
    let relative = |file: &str| format!("{}/{name}/{file}", dir.as_str());
    let write = |file: &str, bytes: Option<&[u8]>| -> io::Result<Option<String>> {
        let path = folder.join(file);
        bytes.map_or_else(
            || match std::fs::remove_file(&path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(None),
            },
            |bytes| {
                crate::fs::write_atomically(&path, bytes, Durability::Atomic)
                    .map(|()| Some(relative(file)))
            },
        )
    };
    let artifacts = Artifacts {
        golden: write(GOLDEN_ARTIFACT, images.golden)?,
        candidate: write(CANDIDATE_ARTIFACT, images.candidate)?,
        diff: write(DIFF_ARTIFACT, images.diff)?,
    };
    if artifacts.is_empty() {
        // Absent, or holding the folders of longer names (`a/b` inside `a`): both are fine.
        match std::fs::remove_dir(&folder) {
            Err(e)
                if !matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
                ) =>
            {
                return Err(e);
            }
            _ => {}
        }
    }
    Ok(artifacts)
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
    /// `.`, `_` or `-`, so it can also name a file on every platform.
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
#[schemars(title = "gleon case report (.gleon/runs/latest/cases/<name>.json)")]
pub struct CaseReport {
    /// Version of this format (`CASE_SCHEMA_VERSION`).
    #[serde(deserialize_with = "schema_version")]
    #[schemars(range(min = 2, max = 2))]
    pub schema_version: u32,
    /// Canonical test name (also the case file name): the golden path relative to the workspace
    /// root without extension, as lowercase `[a-z0-9_.-]` segments separated by `/`.
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
    /// Per-region metrics (today a single whole-image region when `metrics` is present).
    pub regions: Vec<RegionMetrics>,
    /// The images kept for the case (failures only).
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
    /// The file of the report named `name` in the workspace whose `.gleon/` is `gleon_dir`.
    #[must_use]
    pub fn path(gleon_dir: &Path, name: &str) -> PathBuf {
        gleon_dir.join(CASES_DIR).join(format!("{name}.json"))
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
        let Version { schema_version } =
            serde_json::from_slice(json).map_err(CaseParseError::Json)?;
        if schema_version != CASE_SCHEMA_VERSION {
            return Err(CaseParseError::UnsupportedVersion(schema_version));
        }
        let report: Self = serde_json::from_slice(json).map_err(CaseParseError::Json)?;
        report
            .validate()
            .map(|()| report)
            .map_err(CaseParseError::Inconsistent)
    }

    /// Checks that the fields agree with the outcome: `error_kind` exactly for errors, no golden
    /// hash for a missing golden, equal hashes for identical images, metrics only for `match`
    /// and `mismatch`, images only for failures that keep them.
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
        if self.outcome == O::Identical
            && self.golden.sha256.as_ref() != Some(&self.candidate.sha256)
        {
            return Err(InconsistentCase::IdenticalHashes);
        }
        if self.metrics.is_some() && !matches!(self.outcome, O::Match | O::Mismatch) {
            return Err(InconsistentCase::Metrics);
        }
        if self.artifacts.is_some()
            && !matches!(
                self.outcome,
                O::Mismatch | O::DimensionMismatch | O::Missing
            )
        {
            return Err(InconsistentCase::Artifacts);
        }
        Ok(())
    }

    /// Writes this report to [`Self::path`] (pretty JSON and a final newline).
    ///
    /// Reports are regenerated by every run, so they are written atomically but not flushed to
    /// disk ([`Durability::Atomic`]).
    ///
    /// # Errors
    /// Returns [`io::ErrorKind::InvalidInput`] if the name is not a canonical test name or the
    /// fields are inconsistent ([`Self::validate`]), or the I/O error of writing the file.
    pub fn write(&self, gleon_dir: &Path) -> io::Result<()> {
        validate_canonical_test_name(&self.name)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        self.validate()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let file = Self::path(gleon_dir, &self.name);
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
    /// Identical images with different hashes.
    #[error("`identical` images have the same `sha256`")]
    IdenticalHashes,
    /// Metrics of an outcome that compared no pixels or did not finish.
    #[error("`metrics` belong to `match` and `mismatch` only")]
    Metrics,
    /// Images of an outcome that keeps none.
    #[error("`artifacts` belong to `mismatch`, `dimension_mismatch` and `missing` only")]
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
                Some(Metrics::Ssim { headroom, max_excess: 1.5, .. })
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
            }
        );
        let missing = GoldenImage::of("a.png".to_owned(), None, None);
        assert_eq!((missing.sha256, missing.width), (None, None));
        let candidate = CandidateImage::of(b"garbage");
        assert_eq!(candidate.sha256, Sha256Hex::of(b"garbage"));
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
        let images = ArtifactImages {
            golden: Some(b"golden"),
            candidate: Some(b"candidate"),
            diff: Some(b"diff"),
        };
        let artifacts = write_artifacts(root, &dir, "test/goldens/a", images).unwrap();
        let folder = root.join(".gleon/runs/latest/artifacts/test/goldens/a");
        assert_eq!(
            artifacts,
            Artifacts {
                golden: Some(".gleon/runs/latest/artifacts/test/goldens/a/golden.png".to_owned()),
                candidate: Some(
                    ".gleon/runs/latest/artifacts/test/goldens/a/candidate.png".to_owned()
                ),
                diff: Some(".gleon/runs/latest/artifacts/test/goldens/a/diff.png".to_owned()),
            }
        );
        assert_eq!(std::fs::read(folder.join("diff.png")).unwrap(), b"diff");

        let candidate_only = ArtifactImages {
            candidate: Some(b"new"),
            ..ArtifactImages::default()
        };
        let artifacts = write_artifacts(root, &dir, "test/goldens/a", candidate_only).unwrap();
        assert_eq!((artifacts.golden, artifacts.diff), (None, None));
        let mut files: Vec<_> = std::fs::read_dir(&folder)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        files.sort();
        assert_eq!(files, ["candidate.png"], "stale images are removed");

        // A pass keeps nothing: the images and the emptied folder go, a nested name's folder stays.
        std::fs::create_dir_all(folder.join("nested")).unwrap();
        let none =
            write_artifacts(root, &dir, "test/goldens/a", ArtifactImages::default()).unwrap();
        assert!(none.is_empty());
        assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 1);
        std::fs::remove_dir(folder.join("nested")).unwrap();
        write_artifacts(root, &dir, "test/goldens/a", ArtifactImages::default()).unwrap();
        assert!(!folder.exists());
        write_artifacts(root, &dir, "test/goldens/a", ArtifactImages::default()).unwrap();

        // A directory where an image goes cannot be removed as a stale file.
        std::fs::create_dir_all(folder.join("diff.png/inside")).unwrap();
        assert!(write_artifacts(root, &dir, "test/goldens/a", candidate_only).is_err());

        for name in ["../../outside", "a/../b", "Upper", "a\\b", ""] {
            let err = write_artifacts(root, &dir, name, candidate_only).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{name}");
        }
        assert!(!root.join("outside").exists());
    }

    /// An emptied folder that cannot be removed is an error, unlike an absent one.
    #[cfg(unix)]
    #[test]
    fn test_artifacts_report_a_folder_that_cannot_be_removed() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let parent = root.join(".gleon/runs/latest/artifacts/test/goldens");
        std::fs::create_dir_all(parent.join("a")).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();
        let probe = std::fs::create_dir(parent.join("probe")); // Always succeeds as root.
        let result = write_artifacts(
            root,
            &ArtifactsDir::default(),
            "test/goldens/a",
            ArtifactImages::default(),
        );
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        if probe.is_err() {
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        }
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
        let file = CaseReport::path(&gleon_dir, "test/goldens/a");
        assert_eq!(
            file,
            gleon_dir.join("runs/latest/cases/test/goldens/a.json")
        );
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.ends_with("}\n"));
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["error_kind"], "image");
        assert_eq!(json["run_id"], "local");
        assert!(json.get("artifacts").is_none());
        assert_eq!(serde_json::from_str::<CaseReport>(&text).unwrap(), report);
        assert_eq!(CaseReport::parse(text.as_bytes()).unwrap(), report);
    }

    #[test]
    fn test_parsing_rejects_other_versions_and_paths_leaving_the_workspace() {
        let valid = serde_json::json!({
            "schema_version": 2,
            "name": "test/goldens/a",
            "golden": {"path": "test/goldens/A.png"},
            "candidate": {"sha256": "0".repeat(64)},
            "source": {"tool": "gleon_flutter", "tool_version": "0.2.0"},
            "platform": {"os": "macos", "arch": "aarch64"},
            "comparison": {"tolerance": {"kind": "exact"}, "masks": [], "policy_version": 2},
            "outcome": "missing",
            "regions": [],
            "artifacts": {"candidate": ".gleon/runs/latest/artifacts/test/goldens/a/candidate.png"},
            "timings_ms": {"total": 1.0},
            "recorded_at": "2026-09-30T12:00:00Z"
        });
        let parse = |json: &serde_json::Value| CaseReport::parse(json.to_string().as_bytes());
        assert!(parse(&valid).is_ok());

        // Other writers may spell an absent image as `null`, which the schema allows.
        let mut nulls = valid.clone();
        nulls["artifacts"]["golden"] = serde_json::Value::Null;
        nulls["artifacts"]["diff"] = serde_json::Value::Null;
        let artifacts = parse(&nulls).unwrap().artifacts.unwrap();
        assert_eq!((artifacts.golden, artifacts.diff), (None, None));
        assert!(artifacts.candidate.is_some());

        let mut v1 = valid.clone();
        v1["schema_version"] = 1.into();
        assert!(matches!(
            parse(&v1),
            Err(CaseParseError::UnsupportedVersion(1))
        ));
        assert!(
            serde_json::from_value::<CaseReport>(v1).is_err(),
            "serde alone rejects other versions too"
        );
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
            ("/artifacts/candidate", "/etc/passwd"),
            ("/artifacts/candidate", "a/./b.png"),
        ] {
            let mut broken = valid.clone();
            *broken.pointer_mut(field).unwrap() = value.into();
            let err = parse(&broken).unwrap_err();
            assert!(matches!(err, CaseParseError::Json(_)), "{field}: {value}");
        }
        assert_eq!(
            CaseParseError::UnsupportedVersion(1).to_string(),
            "case report schema 1 is not supported (this gleon reads 2)"
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
                serde_json::json!({"outcome": "identical", "golden": {"path": "a.png", "sha256": "1".repeat(64)}}),
                InconsistentCase::IdenticalHashes,
            ),
            (
                serde_json::json!({"metrics": metrics}),
                InconsistentCase::Metrics,
            ),
            (
                serde_json::json!({"outcome": "updated", "golden": {"path": "a.png", "sha256": "0".repeat(64)}}),
                InconsistentCase::Artifacts,
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
