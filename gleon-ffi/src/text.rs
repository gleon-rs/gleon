//! Every text an integration shows: failure messages, warnings and the console line. Metric
//! summaries and tolerance descriptions are the model's (`gleon_model::case::text`), shared with
//! the reports of the CLI.
//!
//! The missing-golden message is Flutter's own; other integrations can show their own from the
//! `missing` verdict.

#![forbid(unsafe_code)]

pub use gleon_model::case::text::{dimension_summary, metrics_summary, tolerance};
use gleon_model::{
    case::{
        CaseOutcome, Metrics,
        text::{MAX_DECIMALS, color, decimal, percent, signed, similarity},
    },
    tolerance::Tolerance,
};

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
    fn test_console_lines() {
        let ssim = Metrics::Ssim {
            min_ssim: 0.931,
            mean_ssim: 0.99,
            max_excess: 0.0,
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
