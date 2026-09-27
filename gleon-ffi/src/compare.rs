//! Safe comparison logic behind the C ABI: option parsing, decoding, comparison and reporting.
//!
//! Everything here is plain safe Rust and unit-tested directly; `lib.rs` only converts raw
//! pointers into slices and hands ownership of the [`Outcome`] across the boundary.

use std::time::{Duration, Instant};

use gleon_engine::{
    ComparisonResult, compare_images,
    config::Zone,
    decode::{DecodeError, decode_rgba},
    masking::apply_masks,
};
use gleon_model::{
    case::{Metrics, RegionMetrics},
    tolerance::Tolerance,
};
use image::{ImageFormat, RgbaImage};
use serde::{Deserialize, Serialize};

/// Version of the JSON request/response contract. Bumped on any breaking change so the Dart
/// side can refuse a mismatched native library instead of misreading its output.
pub const ABI_VERSION: u32 = 4;

/// JSON options sent by the Dart side. The tolerance carries only the parameters of its own mode
/// (`gleon_model::tolerance::Tolerance`), so a parameter of another mode is rejected, never
/// silently ignored.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompareOptions {
    tolerance: Tolerance,
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

/// Image dimensions in pixels.
#[derive(Debug, Clone, Copy, Serialize)]
struct Size {
    width: u32,
    height: u32,
}

impl From<(u32, u32)> for Size {
    fn from((width, height): (u32, u32)) -> Self {
        Self { width, height }
    }
}

/// Time spent in each native stage, in microseconds.
#[derive(Debug, Clone, Copy, Default, Serialize)]
struct Timings {
    /// Decoding both PNGs.
    decode: u64,
    /// Masking and comparing.
    compare: u64,
    /// Encoding the diff PNG (0 unless the verdict is a mismatch).
    encode: u64,
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
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
    baseline: Option<Size>,
    #[serde(skip_serializing_if = "Option::is_none")]
    candidate: Option<Size>,
    /// Whole-image metrics, present for `match` and `mismatch`.
    #[serde(skip_serializing_if = "Option::is_none")]
    metrics: Option<Metrics>,
    /// Per-region metrics: one whole-image region alongside `metrics`.
    regions: Vec<RegionMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timings_us: Option<Timings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl Report {
    fn error(message: impl Into<String>) -> Self {
        Self {
            abi: ABI_VERSION,
            policy_version: gleon_engine::ssim::POLICY_VERSION,
            verdict: Verdict::Error,
            baseline: None,
            candidate: None,
            metrics: None,
            regions: Vec::new(),
            timings_us: None,
            error: Some(message.into()),
        }
    }
}

/// A finished call: the JSON report plus an optional PNG-encoded diff visualization.
#[derive(Debug)]
pub struct Outcome {
    /// Serialized report.
    pub json: Vec<u8>,
    /// PNG diff image, present only on [`Verdict::Mismatch`].
    pub diff_png: Option<Vec<u8>>,
}

impl Outcome {
    /// Serializes `report` (a compare report or a resolve response).
    ///
    /// Serializing plain numbers and strings cannot fail; should it ever, the result is a static
    /// error document rather than a panic across the FFI boundary. It carries both `"verdict"`
    /// and `"kind"` so either contract's parser reads it as an error.
    pub(crate) fn from_serializable(report: &impl Serialize, diff_png: Option<Vec<u8>>) -> Self {
        let json = serde_json::to_vec(report).unwrap_or_else(|_| {
            format!(
                r#"{{"abi":{ABI_VERSION},"verdict":"error","kind":"error","error":"failed to serialize report"}}"#
            )
            .into_bytes()
        });
        Self { json, diff_png }
    }

    /// Builds a comparison error outcome (used for invalid pointers and caught panics).
    #[must_use]
    pub fn error(message: impl Into<String>) -> Self {
        Self::from_serializable(&Report::error(message), None)
    }
}

fn parse_options(options_json: &[u8]) -> Result<CompareOptions, String> {
    let options: CompareOptions = serde_json::from_slice(options_json)
        .map_err(|e| format!("invalid comparison options: {e}"))?;
    options
        .tolerance
        .validate()
        .map_err(|e| format!("invalid comparison options: {e}"))?;
    Ok(options)
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
/// Masks are applied to both images before comparing, exactly like `gleon diff`. Matches report
/// their metrics too, so callers can see the headroom to the tolerance.
#[must_use]
pub fn compare(baseline: &[u8], candidate: &[u8], options_json: &[u8]) -> Outcome {
    limit_engine_threads();
    let run = || -> Result<Outcome, String> {
        let CompareOptions { tolerance, masks } = parse_options(options_json)?;
        let (mode, config) = tolerance.engine_config();

        let started = Instant::now();
        let mut baseline_img = decode("baseline", baseline)?;
        let mut candidate_img = decode("candidate", candidate)?;
        let decoded = Instant::now();
        let baseline_size = baseline_img.dimensions();
        let candidate_size = candidate_img.dimensions();
        if !masks.is_empty() && baseline_size == candidate_size {
            apply_masks(&mut baseline_img, &masks);
            apply_masks(&mut candidate_img, &masks);
        }
        let result = compare_images(&baseline_img, &candidate_img, mode, &config);
        let compared = Instant::now();
        let mut timings = Timings {
            decode: micros(decoded - started),
            compare: micros(compared - decoded),
            encode: 0,
        };

        let mut report = Report {
            abi: ABI_VERSION,
            policy_version: gleon_engine::ssim::POLICY_VERSION,
            verdict: Verdict::Match,
            baseline: Some(baseline_size.into()),
            candidate: Some(candidate_size.into()),
            metrics: None,
            regions: Vec::new(),
            timings_us: Some(timings),
            error: None,
        };
        let total_pixels = u64::from(baseline_size.0) * u64::from(baseline_size.1);
        let with_metrics = |report: &mut Report, measurement| {
            let metrics = Metrics::from_measurement(&measurement, &tolerance, total_pixels)
                .ok_or("internal error: the engine measurement does not match the tolerance")?;
            report.metrics = Some(metrics);
            report.regions = vec![RegionMetrics::whole_image(metrics)];
            Ok::<_, String>(())
        };
        match result {
            ComparisonResult::Match { measurement } => {
                with_metrics(&mut report, measurement)?;
                Ok(Outcome::from_serializable(&report, None))
            }
            ComparisonResult::TooLarge {
                size: (width, height),
            } => Err(format!(
                "{width}x{height} exceeds the SSIM analysis budget of {} pixels; use exact or \
                 pixel mode or a smaller capture",
                gleon_engine::ssim::MAX_ANALYSIS_PIXELS
            )),
            ComparisonResult::DimensionMismatch { .. } => {
                report.verdict = Verdict::DimensionMismatch;
                Ok(Outcome::from_serializable(&report, None))
            }
            ComparisonResult::Mismatch {
                measurement,
                diff_image,
            } => {
                report.verdict = Verdict::Mismatch;
                with_metrics(&mut report, measurement)?;
                let encoding = Instant::now();
                let diff_png = encode_png(&diff_image)?;
                timings.encode = micros(encoding.elapsed());
                report.timings_us = Some(timings);
                Ok(Outcome::from_serializable(&report, Some(diff_png)))
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
    const EXACT: &[u8] = br#"{"tolerance":{"kind":"exact"}}"#;
    const SSIM: &[u8] =
        br#"{"tolerance":{"kind":"ssim","min_similarity":0.8,"color_tolerance":8.0}}"#;

    /// A report whose serialization fails, to reach the static fallback document.
    struct Unserializable;

    impl Serialize for Unserializable {
        fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("unserializable"))
        }
    }

    #[test]
    fn test_serialization_failure_is_an_error_for_both_contracts() {
        let out = Outcome::from_serializable(&Unserializable, None);
        let r = report(&out);
        assert_eq!(r["abi"], ABI_VERSION);
        // The compare parser keys on `verdict`, the resolve parser on `kind`.
        assert_eq!(
            (&r["verdict"], &r["kind"]),
            (&"error".into(), &"error".into())
        );
        assert_eq!(r["error"], "failed to serialize report");
    }

    #[test]
    fn test_exact_match_reports_metrics_and_timings() {
        let a = png(10, 10, |_, _| RED);
        let out = compare(&a, &a, EXACT);
        let r = report(&out);
        assert_eq!(r["abi"], 4);
        assert_eq!(r["verdict"], "match");
        assert_eq!(
            r["metrics"],
            serde_json::json!({
                "kind": "pixel", "total_pixels": 100, "diff_pixels": 0,
                "diff_ratio": 0.0, "headroom": 0.0
            })
        );
        assert_eq!(r["regions"][0]["kind"], "image");
        assert_eq!(r["regions"][0]["metrics"], r["metrics"]);
        assert_eq!(
            r["baseline"],
            serde_json::json!({"width": 10, "height": 10})
        );
        assert_eq!(r["timings_us"]["encode"], 0);
        assert!(r["timings_us"]["decode"].is_u64());
        assert!(out.diff_png.is_none());
    }

    #[test]
    fn test_exact_single_pixel_mismatch_has_diff() {
        let a = png(10, 10, |_, _| RED);
        let b = png(10, 10, |x, y| if (x, y) == (3, 3) { BLUE } else { RED });
        let out = compare(&a, &b, EXACT);
        let r = report(&out);
        assert_eq!(r["verdict"], "mismatch");
        assert_eq!(r["metrics"]["diff_pixels"], 1);
        assert_eq!(r["metrics"]["total_pixels"], 100);
        assert_eq!(r["metrics"]["headroom"], -0.01);
        assert!(out.diff_png.is_some());
    }

    #[test]
    fn test_pixel_threshold_tolerates_small_change_with_headroom() {
        let a = png(10, 10, |_, _| RED);
        let b = png(10, 10, |x, y| if (x, y) == (3, 3) { BLUE } else { RED });
        let opts = br#"{"tolerance":{"kind":"pixel","max_diff_ratio":0.05}}"#;
        let r = report(&compare(&a, &b, opts));
        assert_eq!(r["verdict"], "match");
        assert_eq!(r["metrics"]["diff_pixels"], 1);
        assert!((r["metrics"]["headroom"].as_f64().unwrap() - 0.04).abs() < 1e-12);
    }

    #[test]
    fn test_ssim_reports_policy_metrics_on_mismatch() {
        let a = png(64, 64, |_, _| RED);
        let b = png(64, 64, |x, _| if x < 32 { BLUE } else { RED });
        let r = report(&compare(&a, &b, SSIM));
        assert_eq!(r["verdict"], "mismatch");
        assert_eq!(r["policy_version"], 2);
        let m = &r["metrics"];
        assert_eq!(m["kind"], "ssim");
        assert!(m["peak_excess"].as_f64().unwrap() > 100.0, "{r}");
        assert!(m["headroom"]["color"].as_f64().unwrap() < 0.0, "{r}");
        assert_eq!(m["failing_region"]["width"], 32);
        assert_eq!(m["changed_pixels"], 32 * 64);
    }

    #[test]
    fn test_ssim_match_reports_headroom() {
        let a = png(32, 32, |_, _| Rgba([63, 81, 181, 255]));
        let b = png(32, 32, |_, _| Rgba([63, 81, 183, 255]));
        let r = report(&compare(&a, &b, SSIM));
        assert_eq!(r["verdict"], "match");
        let m = &r["metrics"];
        assert_eq!(m["peak_excess"], 2.0);
        assert_eq!(m["headroom"]["color"], 6.0);
        assert!(m["headroom"]["similarity"].as_f64().unwrap() > 0.0, "{r}");
        assert_eq!(m["failing_pixels"], 0);
        assert!(m.get("failing_region").is_none(), "{r}");
    }

    #[test]
    fn test_masks_hide_changed_region() {
        let a = png(10, 10, |_, _| RED);
        let b = png(10, 10, |x, y| if x < 2 && y < 2 { BLUE } else { RED });
        let opts =
            br#"{"tolerance":{"kind":"exact"},"masks":[{"x":0,"y":0,"width":2,"height":2}]}"#;
        assert_eq!(report(&compare(&a, &b, opts))["verdict"], "match");
    }

    #[test]
    fn test_masks_with_dimension_mismatch_still_report_sizes() {
        let a = png(10, 10, |_, _| RED);
        let b = png(12, 10, |_, _| RED);
        let opts =
            br#"{"tolerance":{"kind":"exact"},"masks":[{"x":0,"y":0,"width":"50%","height":2}]}"#;
        let r = report(&compare(&a, &b, opts));
        assert_eq!(r["verdict"], "dimension_mismatch");
        assert_eq!(
            r["baseline"],
            serde_json::json!({"width": 10, "height": 10})
        );
        assert_eq!(
            r["candidate"],
            serde_json::json!({"width": 12, "height": 10})
        );
        assert!(r.get("metrics").is_none(), "{r}");
        assert_eq!(r["regions"], serde_json::json!([]));
    }

    #[test]
    fn test_ssim_over_analysis_budget_is_an_error() {
        let big = png(4097, 4096, |_, _| RED);
        let r = report(&compare(&big, &big, SSIM));
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
    fn test_invalid_inputs_are_errors_not_passes() {
        let a = png(4, 4, |_, _| RED);
        for (baseline, candidate, opts) in [
            (&b"garbage"[..], &a[..], EXACT),
            (&a[..], &b"garbage"[..], EXACT),
            (&a[..], &a[..], &br#"{"tolerance":{"kind":"pixel"}}"#[..]),
            (
                &a[..],
                &a[..],
                &br#"{"tolerance":{"kind":"exact","max_diff_ratio":0.1}}"#[..],
            ),
            (
                &a[..],
                &a[..],
                &br#"{"tolerance":{"kind":"ssim","min_similarity":1.5,"color_tolerance":8}}"#[..],
            ),
            (
                &a[..],
                &a[..],
                &br#"{"tolerance":{"kind":"ssim","min_similarity":0.8}}"#[..],
            ),
            (
                &a[..],
                &a[..],
                &br#"{"tolerance":{"kind":"ssim","min_similarity":0.8,"color_tolerance":-1}}"#[..],
            ),
            (
                &a[..],
                &a[..],
                &br#"{"tolerance":{"kind":"pixel","max_diff_ratio":0.1,"color_tolerance":8}}"#[..],
            ),
            (&a[..], &a[..], &br#"{"tolerance":{"kind":"fuzzy"}}"#[..]),
            (
                &a[..],
                &a[..],
                &br#"{"tolerance":{"kind":"exact"},"typo":1}"#[..],
            ),
            (&a[..], &a[..], &br#"{"mode":"exact"}"#[..]),
        ] {
            let r = report(&compare(baseline, candidate, opts));
            assert_eq!(r["verdict"], "error", "{r}");
            assert!(r["error"].as_str().is_some_and(|e| !e.is_empty()));
            assert!(r.get("timings_us").is_none());
        }
    }
}
