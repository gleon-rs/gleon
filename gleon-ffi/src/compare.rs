//! Safe comparison logic behind the C ABI: option parsing, decoding, comparison and reporting.
//!
//! Everything here is plain safe Rust and unit-tested directly; `lib.rs` only converts raw
//! pointers into slices and hands ownership of the [`Outcome`] across the boundary.

use gleon_engine::{
    ComparisonResult, MismatchDetail, compare_images,
    config::{DiffConfig, Mode, Zone},
    decode::{DecodeError, decode_rgba},
    masking::apply_masks,
};
use image::{ImageFormat, RgbaImage};
use serde::{Deserialize, Serialize};

/// Version of the JSON request/response contract. Bumped on any breaking change so the Dart
/// side can refuse a mismatched native library instead of misreading its output.
pub const ABI_VERSION: u32 = 2;

/// Comparison strategy requested by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum RequestedMode {
    /// Every pixel must be identical (the Flutter SDK default).
    Exact,
    /// Tolerates a fraction of differing pixels (`threshold`).
    Pixel,
    /// Tolerates rendering noise under [`gleon_engine::ssim`] policy v2: every local SSIM (half
    /// resolution) must reach `min_similarity`, and no region may exceed its 3x3 envelope by more
    /// than `color_tolerance`.
    Ssim,
}

/// JSON options sent by the Dart side. Mode-specific parameters are required for their mode and
/// rejected for the others, so a typo never silently falls back to a default.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompareOptions {
    mode: RequestedMode,
    #[serde(default)]
    threshold: Option<f64>,
    #[serde(default)]
    min_similarity: Option<f64>,
    #[serde(default)]
    color_tolerance: Option<f64>,
    #[serde(default)]
    masks: Vec<Zone>,
}

/// Top-level verdict. Operational failures are `Error`, never a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Images match within the requested tolerance.
    Match,
    /// Images differ beyond the requested tolerance.
    Mismatch,
    /// Images have different dimensions; no pixel comparison was attempted.
    DimensionMismatch,
    /// Invalid input or options; see `error`.
    Error,
}

/// JSON report returned to the Dart side.
#[derive(Debug, Serialize)]
pub struct Report {
    abi: u32,
    /// Version of the tolerant (SSIM) decision policy that produced the verdict.
    policy_version: u32,
    /// The verdict.
    pub verdict: Verdict,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<MismatchDetail>,
    #[serde(skip_serializing_if = "Option::is_none")]
    baseline_size: Option<(u32, u32)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    candidate_size: Option<(u32, u32)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_pixels: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl Report {
    fn error(message: impl Into<String>) -> Self {
        Self {
            abi: ABI_VERSION,
            policy_version: gleon_engine::ssim::POLICY_VERSION,
            verdict: Verdict::Error,
            detail: None,
            baseline_size: None,
            candidate_size: None,
            total_pixels: None,
            error: Some(message.into()),
        }
    }
}

/// A finished comparison: the JSON report plus an optional PNG-encoded diff visualization.
#[derive(Debug)]
pub struct Outcome {
    /// Serialized [`Report`].
    pub json: Vec<u8>,
    /// PNG diff image, present only on [`Verdict::Mismatch`].
    pub diff_png: Option<Vec<u8>>,
}

impl Outcome {
    fn from_report(report: &Report, diff_png: Option<Vec<u8>>) -> Self {
        // Serializing a struct of plain numbers/strings cannot fail; fall back to a static
        // error document rather than panicking across the FFI boundary if it ever does.
        let json = serde_json::to_vec(report).unwrap_or_else(|_| {
            format!(
                r#"{{"abi":{ABI_VERSION},"verdict":"error","error":"failed to serialize report"}}"#
            )
            .into_bytes()
        });
        Self { json, diff_png }
    }

    /// Builds an error outcome (used for invalid pointers and caught panics).
    #[must_use]
    pub fn error(message: impl Into<String>) -> Self {
        Self::from_report(&Report::error(message), None)
    }
}

fn parse_options(options_json: &[u8]) -> Result<(Mode, DiffConfig, Vec<Zone>), String> {
    let options: CompareOptions = serde_json::from_slice(options_json)
        .map_err(|e| format!("invalid comparison options: {e}"))?;
    let non_negative = |name: &str, value: Option<f64>| -> Result<f64, String> {
        let value = value.ok_or_else(|| format!("`{name}` is required for this mode"))?;
        if value.is_finite() && value >= 0.0 {
            Ok(value)
        } else {
            Err(format!(
                "`{name}` must be a finite, non-negative number (got {value})"
            ))
        }
    };
    let ratio = |name: &str, value: Option<f64>| -> Result<f64, String> {
        let value = value.ok_or_else(|| format!("`{name}` is required for this mode"))?;
        if (0.0..=1.0).contains(&value) {
            Ok(value)
        } else {
            Err(format!(
                "`{name}` must be between 0.0 and 1.0 (got {value})"
            ))
        }
    };
    let reject = |name: &str, value: Option<f64>| -> Result<(), String> {
        value.map_or(Ok(()), |_| {
            Err(format!("`{name}` is not supported for this mode"))
        })
    };
    let base = DiffConfig::default();
    let (mode, config) = match options.mode {
        RequestedMode::Exact => {
            reject("threshold", options.threshold)?;
            reject("min_similarity", options.min_similarity)?;
            reject("color_tolerance", options.color_tolerance)?;
            (
                Mode::Pixel,
                DiffConfig {
                    threshold: 0.0,
                    ..base
                },
            )
        }
        RequestedMode::Pixel => {
            reject("min_similarity", options.min_similarity)?;
            reject("color_tolerance", options.color_tolerance)?;
            (
                Mode::Pixel,
                DiffConfig {
                    threshold: ratio("threshold", options.threshold)?,
                    ..base
                },
            )
        }
        RequestedMode::Ssim => {
            reject("threshold", options.threshold)?;
            (
                Mode::Ssim,
                DiffConfig {
                    min_similarity: ratio("min_similarity", options.min_similarity)?,
                    color_tolerance: non_negative("color_tolerance", options.color_tolerance)?,
                    ..base
                },
            )
        }
    };
    Ok((mode, config, options.masks))
}

fn decode(label: &str, bytes: &[u8]) -> Result<RgbaImage, String> {
    decode_rgba(bytes).map_err(|e: DecodeError| format!("{label} image: {e}"))
}

fn encode_png(image: &RgbaImage) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
        .map(|()| bytes)
        .map_err(|e| format!("failed to encode diff image: {e}"))
}

/// Runs the engine single-threaded inside this library.
///
/// Every `flutter test` worker process loads its own copy of the library, and the test runner
/// already parallelizes across those processes; an all-core rayon pool per process would multiply
/// threads (workers x cores) and thrash. The CLI keeps its parallel global pool. Only the first
/// initialization of the process-wide pool can succeed, so a failure means it is already set up.
fn limit_engine_threads() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let _already_initialized = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build_global();
    });
}

/// Compares two encoded images (PNG) using the JSON `options`.
///
/// Masks are applied to both images before comparing, exactly like `gleon diff`.
#[must_use]
pub fn compare(baseline: &[u8], candidate: &[u8], options_json: &[u8]) -> Outcome {
    limit_engine_threads();
    let run = || -> Result<Outcome, String> {
        let (mode, config, masks) = parse_options(options_json)?;
        let mut baseline_img = decode("baseline", baseline)?;
        let mut candidate_img = decode("candidate", candidate)?;
        let baseline_size = baseline_img.dimensions();
        let candidate_size = candidate_img.dimensions();
        if !masks.is_empty() && baseline_size == candidate_size {
            apply_masks(&mut baseline_img, &masks);
            apply_masks(&mut candidate_img, &masks);
        }
        let mut report = Report {
            abi: ABI_VERSION,
            policy_version: gleon_engine::ssim::POLICY_VERSION,
            verdict: Verdict::Match,
            detail: None,
            baseline_size: Some(baseline_size),
            candidate_size: Some(candidate_size),
            total_pixels: Some(u64::from(baseline_size.0) * u64::from(baseline_size.1)),
            error: None,
        };
        match compare_images(&baseline_img, &candidate_img, mode, &config) {
            ComparisonResult::Match => Ok(Outcome::from_report(&report, None)),
            ComparisonResult::TooLarge {
                size: (width, height),
            } => Err(format!(
                "{width}x{height} exceeds the SSIM analysis budget of {} pixels; use exact or \
                 pixel mode or a smaller capture",
                gleon_engine::ssim::MAX_ANALYSIS_PIXELS
            )),
            ComparisonResult::DimensionMismatch { .. } => {
                report.verdict = Verdict::DimensionMismatch;
                report.total_pixels = None;
                Ok(Outcome::from_report(&report, None))
            }
            ComparisonResult::Mismatch { detail, diff_image } => {
                report.verdict = Verdict::Mismatch;
                report.detail = Some(detail);
                Ok(Outcome::from_report(
                    &report,
                    Some(encode_png(&diff_image)?),
                ))
            }
        }
    };
    run().unwrap_or_else(Outcome::error)
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

    fn png(width: u32, height: u32, paint: impl Fn(u32, u32) -> Rgba<u8>) -> Vec<u8> {
        let img: RgbaImage = ImageBuffer::from_fn(width, height, paint);
        encode_png(&img).unwrap()
    }

    fn report(outcome: &Outcome) -> serde_json::Value {
        serde_json::from_slice(&outcome.json).unwrap()
    }

    const RED: Rgba<u8> = Rgba([255, 0, 0, 255]);
    const BLUE: Rgba<u8> = Rgba([0, 0, 255, 255]);

    #[test]
    fn test_exact_match() {
        let a = png(10, 10, |_, _| RED);
        let out = compare(&a, &a, br#"{"mode":"exact"}"#);
        assert_eq!(report(&out)["verdict"], "match");
        assert!(out.diff_png.is_none());
    }

    #[test]
    fn test_exact_single_pixel_mismatch_has_diff() {
        let a = png(10, 10, |_, _| RED);
        let b = png(10, 10, |x, y| if (x, y) == (3, 3) { BLUE } else { RED });
        let out = compare(&a, &b, br#"{"mode":"exact"}"#);
        let r = report(&out);
        assert_eq!(r["verdict"], "mismatch");
        assert_eq!(r["detail"]["Pixel"]["diff_count"], 1);
        assert_eq!(r["total_pixels"], 100);
        assert!(out.diff_png.is_some());
    }

    #[test]
    fn test_pixel_threshold_tolerates_small_change() {
        let a = png(10, 10, |_, _| RED);
        let b = png(10, 10, |x, y| if (x, y) == (3, 3) { BLUE } else { RED });
        let out = compare(&a, &b, br#"{"mode":"pixel","threshold":0.05}"#);
        assert_eq!(report(&out)["verdict"], "match");
    }

    #[test]
    fn test_ssim_reports_policy_metrics_on_mismatch() {
        let a = png(64, 64, |_, _| RED);
        let b = png(64, 64, |x, _| if x < 32 { BLUE } else { RED });
        let opts = br#"{"mode":"ssim","min_similarity":0.8,"color_tolerance":8.0}"#;
        let r = report(&compare(&a, &b, opts));
        assert_eq!(r["verdict"], "mismatch");
        assert_eq!(r["policy_version"], 2);
        let ssim = &r["detail"]["Ssim"];
        assert!(ssim["max_excess"].as_f64().unwrap() > 100.0, "{r}");
        assert_eq!(ssim["region"]["width"], 32);
    }

    #[test]
    fn test_ssim_tolerates_imperceptible_drift() {
        let a = png(32, 32, |_, _| Rgba([63, 81, 181, 255]));
        let b = png(32, 32, |_, _| Rgba([63, 81, 183, 255]));
        let opts = br#"{"mode":"ssim","min_similarity":0.8,"color_tolerance":8.0}"#;
        assert_eq!(report(&compare(&a, &b, opts))["verdict"], "match");
    }

    #[test]
    fn test_masks_hide_changed_region() {
        let a = png(10, 10, |_, _| RED);
        let b = png(10, 10, |x, y| if x < 2 && y < 2 { BLUE } else { RED });
        let opts = br#"{"mode":"exact","masks":[{"x":0,"y":0,"width":2,"height":2}]}"#;
        assert_eq!(report(&compare(&a, &b, opts))["verdict"], "match");
    }

    #[test]
    fn test_masks_with_dimension_mismatch_still_report_sizes() {
        let a = png(10, 10, |_, _| RED);
        let b = png(12, 10, |_, _| RED);
        let opts = br#"{"mode":"exact","masks":[{"x":0,"y":0,"width":"50%","height":2}]}"#;
        let r = report(&compare(&a, &b, opts));
        assert_eq!(r["verdict"], "dimension_mismatch");
        assert_eq!(r["baseline_size"], serde_json::json!([10, 10]));
        assert_eq!(r["candidate_size"], serde_json::json!([12, 10]));
        assert!(r.get("total_pixels").is_none(), "{r}");
    }

    #[test]
    fn test_ssim_over_analysis_budget_is_an_error() {
        let big = png(4097, 4096, |_, _| RED);
        let opts = br#"{"mode":"ssim","min_similarity":0.8,"color_tolerance":8}"#;
        let r = report(&compare(&big, &big, opts));
        assert_eq!(r["verdict"], "error");
        assert!(
            r["error"]
                .as_str()
                .unwrap()
                .contains("SSIM analysis budget"),
            "{r}"
        );
    }

    #[test]
    fn test_dimension_mismatch() {
        let a = png(10, 10, |_, _| RED);
        let b = png(10, 11, |_, _| RED);
        let r = report(&compare(&a, &b, br#"{"mode":"exact"}"#));
        assert_eq!(r["verdict"], "dimension_mismatch");
        assert_eq!(r["baseline_size"], serde_json::json!([10, 10]));
        assert_eq!(r["candidate_size"], serde_json::json!([10, 11]));
    }

    #[test]
    fn test_invalid_inputs_are_errors_not_passes() {
        let a = png(4, 4, |_, _| RED);
        for (baseline, candidate, opts) in [
            (&b"garbage"[..], &a[..], &br#"{"mode":"exact"}"#[..]),
            (&a[..], &b"garbage"[..], &br#"{"mode":"exact"}"#[..]),
            (&a[..], &a[..], &br#"{"mode":"pixel"}"#[..]),
            (&a[..], &a[..], &br#"{"mode":"exact","threshold":0.1}"#[..]),
            (
                &a[..],
                &a[..],
                &br#"{"mode":"ssim","min_similarity":1.5,"color_tolerance":8}"#[..],
            ),
            (
                &a[..],
                &a[..],
                &br#"{"mode":"ssim","min_similarity":0.8}"#[..],
            ),
            (
                &a[..],
                &a[..],
                &br#"{"mode":"ssim","min_similarity":0.8,"color_tolerance":-1}"#[..],
            ),
            (
                &a[..],
                &a[..],
                &br#"{"mode":"pixel","threshold":0.1,"color_tolerance":8}"#[..],
            ),
            (&a[..], &a[..], &br#"{"mode":"fuzzy"}"#[..]),
            (&a[..], &a[..], &br#"{"mode":"exact","typo":1}"#[..]),
        ] {
            let r = report(&compare(baseline, candidate, opts));
            assert_eq!(r["verdict"], "error", "{r}");
            assert!(r["error"].as_str().is_some_and(|e| !e.is_empty()));
        }
    }
}
