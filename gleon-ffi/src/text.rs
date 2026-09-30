//! Every text an integration shows: failure messages, metric summaries, tolerance descriptions,
//! warnings and the console line.
//!
//! Numbers are rounded to what a developer acts on: percentages to 2-4 decimals (`6.25%`,
//! `0.0167%`), similarities to 3-4 (`0.931`), colors to 1 (`5.2`). The missing-golden message is
//! Flutter's own; other integrations can show their own from the `missing` verdict.

#![forbid(unsafe_code)]

use std::fmt::Write as _;

use gleon_engine::Region;
use gleon_model::{
    case::{CaseOutcome, Metrics},
    tolerance::Tolerance,
};

/// The most decimals any number is shown with.
const MAX_DECIMALS: usize = 4;

/// `value` rounded to `max` decimals, trailing zeros dropped down to `min` (`8`, `7.5`,
/// `0.800`). `-0.0` shows as `0`.
fn decimal(value: f64, min: usize, max: usize) -> String {
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

/// `ratio` as a percentage with 2 to 4 decimals (`6.25`, `0.0167`); a positive ratio too small to
/// show is `<0.0001`, never a misleading `0.00`.
fn percent(ratio: f64) -> String {
    let text = decimal(ratio * 100.0, 2, MAX_DECIMALS);
    if ratio > 0.0 && text.bytes().all(|b| matches!(b, b'0' | b'.')) {
        format!("<0.{}1", "0".repeat(MAX_DECIMALS - 1))
    } else {
        text
    }
}

/// A similarity with 3 to 4 decimals (`0.800`, `0.9995`).
fn similarity(value: f64) -> String {
    decimal(value, 3, MAX_DECIMALS)
}

/// A measured color distance in 8-bit units, one decimal (`5.2`).
fn color(value: f64) -> String {
    decimal(value, 1, 1)
}

/// `text` of a number with an explicit sign (`+0.131`, `-1.00`).
fn signed(text: String) -> String {
    if text.starts_with('-') {
        text
    } else {
        format!("+{text}")
    }
}

/// The thresholds of `tolerance` as shown in failure messages (`ssim ≥ 0.800, color ±8`).
pub fn tolerance(tolerance: &Tolerance) -> String {
    match *tolerance {
        Tolerance::Exact {} => "exact".to_owned(),
        Tolerance::Pixel { max_diff_ratio } => {
            format!("pixel ≤ {}%", percent(max_diff_ratio))
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

/// A region as `(x, y) WxHpx`.
pub fn region(region: &Region) -> String {
    format!(
        "({}, {}) {}x{}px",
        region.x, region.y, region.width, region.height
    )
}

/// The metrics of a failed comparison, e.g. `0.02% (1 of 6000px) differ`.
pub fn metrics_summary(metrics: &Metrics) -> String {
    match metrics {
        Metrics::Pixel {
            total_pixels,
            diff_pixels,
            diff_ratio,
            ..
        } => format!(
            "{}% ({diff_pixels} of {total_pixels}px) differ",
            percent(*diff_ratio)
        ),
        Metrics::Ssim {
            min_ssim,
            peak_excess,
            failing_region,
            headroom,
            ..
        } => {
            let mut gates = format!("min local SSIM {}", similarity(*min_ssim));
            if headroom.color < 0.0 {
                let _infallible = write!(
                    gates,
                    ", colors deviate by up to {} (8-bit units)",
                    color(*peak_excess)
                );
            }
            match failing_region {
                Some(bounds) => format!("changed area at {}: {gates}", region(bounds)),
                None => gates,
            }
        }
    }
}

/// Both image sizes, e.g. `golden is 100x60px, test image is 100x61px`.
pub fn dimension_summary(golden: (u32, u32), candidate: (u32, u32)) -> String {
    format!(
        "golden is {}x{}px, test image is {}x{}px",
        golden.0, golden.1, candidate.0, candidate.1
    )
}

/// One-character console marker of `outcome`.
const fn symbol(outcome: CaseOutcome) -> &'static str {
    match outcome {
        CaseOutcome::Identical => "=",
        CaseOutcome::Match => "✓",
        CaseOutcome::Mismatch => "✗",
        CaseOutcome::DimensionMismatch => "≠",
        CaseOutcome::Error => "!",
        CaseOutcome::Updated => "↻",
        CaseOutcome::Missing => "?",
    }
}

/// The one-line console summary of a golden, e.g.
/// `gleon ✓ test/goldens/swatch.png  ssim 0.931 (≥0.800, +0.131)  color 5.2 (≤8, +2.8)  12 ms`.
pub fn console_line(
    golden_path: &str,
    outcome: CaseOutcome,
    tolerance: &Tolerance,
    total_ms: f64,
    metrics: Option<&Metrics>,
    message: Option<&str>,
) -> String {
    let detail = match (metrics, *tolerance) {
        (
            Some(Metrics::Ssim {
                min_ssim,
                peak_excess,
                headroom,
                ..
            }),
            Tolerance::Ssim {
                min_similarity,
                color_tolerance,
            },
        ) => format!(
            "ssim {} (≥{}, {})  color {} (≤{}, {})",
            similarity(*min_ssim),
            similarity(min_similarity),
            signed(similarity(headroom.similarity)),
            color(*peak_excess),
            decimal(color_tolerance, 0, 2),
            signed(color(headroom.color))
        ),
        (
            Some(&Metrics::Pixel {
                diff_pixels,
                diff_ratio,
                headroom,
                ..
            }),
            Tolerance::Pixel { .. } | Tolerance::Exact {},
        ) => {
            // Exact is a zero threshold.
            let max_diff_ratio = match *tolerance {
                Tolerance::Pixel { max_diff_ratio } => max_diff_ratio,
                Tolerance::Exact {} | Tolerance::Ssim { .. } => 0.0,
            };
            format!(
                "pixel {}% ({diff_pixels} px, ≤{}%, {}%)",
                percent(diff_ratio),
                percent(max_diff_ratio),
                signed(decimal(headroom * 100.0, 2, MAX_DECIMALS))
            )
        }
        _ => match outcome {
            CaseOutcome::Identical | CaseOutcome::Updated | CaseOutcome::Missing => {
                outcome.as_str().to_owned()
            }
            CaseOutcome::Match
            | CaseOutcome::Mismatch
            | CaseOutcome::DimensionMismatch
            | CaseOutcome::Error => message.unwrap_or_else(|| outcome.as_str()).to_owned(),
        },
    };
    format!(
        "gleon {} {golden_path}  {detail}  {total_ms:.0} ms",
        symbol(outcome)
    )
}

/// Masks of a call or rule that reached beyond the image of `golden_uri`; the engine clipped
/// them, so they may hide less than intended.
pub fn clamped_masks(golden_uri: &str, count: usize) -> String {
    let (masks, reach, were) = if count == 1 {
        ("mask", "reaches", "was")
    } else {
        ("masks", "reach", "were")
    };
    format!(
        "gleon: {count} {masks} of golden \"{golden_uri}\" {reach} beyond the image and {were} \
         clipped to it."
    )
}

/// Flutter's message for a missing golden.
pub fn missing_golden(golden_uri: &str) -> String {
    format!("Could not be compared against non-existent file: \"{golden_uri}\"")
}

/// An invalid `.gleon/gleon.yaml`, `GLEON_METRICS` value or golden name.
pub fn config_error(config_path: &str, message: &str) -> String {
    format!("gleon: {config_path}: {message}")
}

/// The golden could not be read or compared (a corrupt PNG, an I/O error); never a pass.
pub fn could_not_compare(golden_uri: &str, message: &str) -> String {
    format!("Golden \"{golden_uri}\": gleon could not compare: {message}")
}

/// A failed comparison: `reason`, where the artifacts are and, without a workspace, a pointer to
/// `.gleon/gleon.yaml`.
pub fn failure(golden_uri: &str, reason: &str, feedback: &str, has_workspace: bool) -> String {
    let mut message = format!("Golden \"{golden_uri}\": {reason}{feedback}");
    if !has_workspace {
        message.push_str(
            "\nTip: tolerances can be set per golden in .gleon/gleon.yaml, the same file as the \
             gleon CLI.",
        );
    }
    message
}

/// Where the failure artifacts of a failed comparison were written.
pub fn feedback(failures_dir: &str) -> String {
    format!("\nFailure feedback can be found at {failures_dir}")
}

/// Printed once per session when `GLEON_METRICS` asks for metrics without a workspace.
pub const MISSING_WORKSPACE_WARNING: &str = "gleon: GLEON_METRICS is set but there is no \
     .gleon/gleon.yaml: create it (same format as the gleon CLI, see `gleon init`); metrics are \
     written to .gleon/runs/latest/cases/.";

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
    use gleon_model::case::SsimHeadroom;

    use super::*;

    #[test]
    fn test_numbers_are_rounded_to_what_matters() {
        for (value, min, max, expected) in [
            (8.0, 0, 2, "8"),
            (7.5, 0, 2, "7.5"),
            (0.8, 3, 4, "0.800"),
            (0.9995, 3, 4, "0.9995"),
            (0.999_95, 3, 4, "1.000"),
            (146.0, 1, 1, "146.0"),
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
        let pixel = |max_diff_ratio| Tolerance::Pixel { max_diff_ratio };
        assert_eq!(tolerance(&pixel(0.01)), "pixel ≤ 1.00%");
        assert_eq!(tolerance(&pixel(0.00001)), "pixel ≤ 0.001%");
        assert_eq!(tolerance(&pixel(-0.0)), "pixel ≤ 0.00%");
        assert_eq!(tolerance(&pixel(1e-9)), "pixel ≤ <0.0001%");
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
            peak_excess: 146.0,
            changed_pixels: 1,
            changed_region: failing_region,
            failing_pixels: 1,
            failing_region,
            headroom: SsimHeadroom {
                similarity: -0.3,
                color,
            },
        }
    }

    #[test]
    fn test_summaries() {
        let pixel = Metrics::Pixel {
            total_pixels: 6000,
            diff_pixels: 1,
            diff_ratio: 1.0 / 6000.0,
            headroom: -1.0 / 6000.0,
        };
        assert_eq!(metrics_summary(&pixel), "0.0167% (1 of 6000px) differ");
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
    }

    #[test]
    fn test_console_lines() {
        let ssim = Metrics::Ssim {
            min_ssim: 0.931,
            mean_ssim: 0.99,
            peak_excess: 5.2,
            changed_pixels: 10,
            changed_region: None,
            failing_pixels: 0,
            failing_region: None,
            headroom: SsimHeadroom {
                similarity: 0.131,
                color: 2.8,
            },
        };
        let default_ssim = Tolerance::Ssim {
            min_similarity: 0.8,
            color_tolerance: 8.0,
        };
        assert_eq!(
            console_line(
                "test/goldens/swatch.png",
                CaseOutcome::Match,
                &default_ssim,
                12.4,
                Some(&ssim),
                None
            ),
            "gleon ✓ test/goldens/swatch.png  ssim 0.931 (≥0.800, +0.131)  color 5.2 (≤8, +2.8)  12 ms"
        );
        let pixel = Metrics::Pixel {
            total_pixels: 100,
            diff_pixels: 2,
            diff_ratio: 0.02,
            headroom: -0.01,
        };
        assert_eq!(
            console_line(
                "a.png",
                CaseOutcome::Mismatch,
                &Tolerance::Pixel {
                    max_diff_ratio: 0.01
                },
                3.0,
                Some(&pixel),
                None
            ),
            "gleon ✗ a.png  pixel 2.00% (2 px, ≤1.00%, -1.00%)  3 ms"
        );
        let unchanged = Metrics::Pixel {
            total_pixels: 100_000,
            diff_pixels: 0,
            diff_ratio: 0.0,
            headroom: 0.0,
        };
        assert_eq!(
            console_line(
                "a.png",
                CaseOutcome::Match,
                &Tolerance::Exact {},
                0.6,
                Some(&unchanged),
                None
            ),
            "gleon ✓ a.png  pixel 0.00% (0 px, ≤0.00%, +0.00%)  1 ms"
        );
        let line = |outcome, message| {
            console_line("a.png", outcome, &Tolerance::Exact {}, 1.0, None, message)
        };
        assert_eq!(
            line(CaseOutcome::Identical, None),
            "gleon = a.png  identical  1 ms"
        );
        assert_eq!(
            line(CaseOutcome::Updated, None),
            "gleon ↻ a.png  updated  1 ms"
        );
        assert_eq!(
            line(CaseOutcome::Error, Some("bad PNG")),
            "gleon ! a.png  bad PNG  1 ms"
        );
        assert_eq!(
            line(CaseOutcome::DimensionMismatch, None),
            "gleon ≠ a.png  dimension_mismatch  1 ms"
        );
        assert_eq!(
            line(CaseOutcome::Missing, Some("ignored")),
            "gleon ? a.png  missing  1 ms"
        );
    }

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
                "the case report uses the same names"
            );
        }
    }

    #[test]
    fn test_messages() {
        assert_eq!(
            missing_golden("goldens/a.png"),
            "Could not be compared against non-existent file: \"goldens/a.png\""
        );
        assert_eq!(
            config_error("/w/.gleon/gleon.yaml", "bad"),
            "gleon: /w/.gleon/gleon.yaml: bad"
        );
        assert_eq!(
            could_not_compare("a.png", "boom"),
            "Golden \"a.png\": gleon could not compare: boom"
        );
        let feedback = feedback("/t/failures/");
        assert_eq!(
            failure("a.png", "x.", &feedback, true),
            "Golden \"a.png\": x.\nFailure feedback can be found at /t/failures/"
        );
        assert_eq!(
            clamped_masks("a.png", 1),
            "gleon: 1 mask of golden \"a.png\" reaches beyond the image and was clipped to it."
        );
        assert_eq!(
            clamped_masks("a.png", 2),
            "gleon: 2 masks of golden \"a.png\" reach beyond the image and were clipped to it."
        );
        assert!(
            failure("a.png", "x.", &feedback, false)
                .ends_with("\nTip: tolerances can be set per golden in .gleon/gleon.yaml, the same file as the gleon CLI.")
        );
    }
}
