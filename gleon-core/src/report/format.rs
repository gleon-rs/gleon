//! Path and outcome formatting helpers shared by the report generators.

use std::{
    borrow::Cow,
    path::{Component, Path},
};

use gleon_model::{
    case::{CaseOutcome, CaseReport, text},
    platform::PlatformKey,
};

use crate::cases::Cases;

/// Lexically normalizes a path's components: collapses `foo/../` pairs and drops `.` segments,
/// without touching the filesystem.
fn normalize_components(path: &Path) -> Vec<Component<'_>> {
    let mut normalized = Vec::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir if matches!(normalized.last(), Some(Component::Normal(_))) => {
                normalized.pop();
            }
            Component::CurDir => {}
            _ => normalized.push(comp),
        }
    }
    normalized
}

/// The link from a page in `report_dir` to the image `path` (both resolved against the working
/// directory): relative and `/`-separated, or a `file:///` URL for an image on another drive (a
/// bare `C:/...` would read as the URL scheme `c:`).
pub(super) fn image_link(path: &Path, report_dir: &Path) -> String {
    let absolute = |path: &Path| {
        let path = if path.as_os_str().is_empty() {
            Path::new(".")
        } else {
            path
        };
        std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
    };
    let (path, report_dir) = (absolute(path), absolute(report_dir));
    let (target, base) = (
        normalize_components(&path),
        normalize_components(&report_dir),
    );
    let name = |comp: &Component<'_>| comp.as_os_str().to_string_lossy().into_owned();
    if target.first() != base.first() {
        let parts: Vec<_> = target
            .iter()
            .filter(|comp| !matches!(comp, Component::RootDir))
            .map(name)
            .collect();
        return format!("file:///{}", parts.join("/"));
    }
    let common = target
        .iter()
        .zip(&base)
        .take_while(|(target, base)| target == base)
        .count();
    let parts: Vec<_> = std::iter::repeat_n("..".to_owned(), base.len() - common)
        .chain(target[common..].iter().map(name))
        .collect();
    if parts.is_empty() {
        ".".to_owned()
    } else {
        parts.join("/")
    }
}

/// The name of a case as the reports show it: its test name, followed by the key of its platform
/// (`key`) when the run spans several platforms (`test/goldens/a (linux-x86_64)`), so the cases of
/// one golden on two platforms are told apart. Borrowed (unchanged output) for a run of one
/// platform.
pub(super) fn case_name<'a>(
    cases: &Cases,
    key: &PlatformKey,
    report: &'a CaseReport,
) -> Cow<'a, str> {
    if cases.spans_platforms() {
        Cow::Owned(format!("{} ({key})", report.name))
    } else {
        Cow::Borrowed(&report.name)
    }
}

/// The status of a case as the reports name it.
pub(super) const fn status(outcome: CaseOutcome) -> &'static str {
    match outcome {
        CaseOutcome::Identical | CaseOutcome::Match | CaseOutcome::Updated => "Pass",
        CaseOutcome::Mismatch => "Mismatch",
        CaseOutcome::DimensionMismatch => "Dimension Mismatch",
        CaseOutcome::Missing => "Missing Baseline",
        CaseOutcome::Error => "Error",
    }
}

/// A case in one line, `<status>: <why>`: the message of the case (the texts every writer shares,
/// `gleon_model::case::text`), else what its metrics or sizes say, e.g.
/// `Mismatch: 0.0167% (1 of 6000px) differ` or `Error (image): candidate image: …`.
pub(super) struct CaseSummary<'a>(pub(super) &'a CaseReport);

impl std::fmt::Display for CaseSummary<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let report = self.0;
        let size = |width: Option<u32>, height: Option<u32>| Some((width?, height?));
        let detail = report.message.as_deref().map(Cow::Borrowed).or_else(|| {
            match report.outcome {
                CaseOutcome::DimensionMismatch => Some(text::dimension_summary(
                    size(report.golden.width, report.golden.height)?,
                    size(report.candidate.width, report.candidate.height)?,
                )),
                _ => report.metrics.as_ref().map(text::metrics_summary),
            }
            .map(Cow::Owned)
        });
        f.write_str(status(report.outcome))?;
        if let Some(kind) = report.error_kind {
            write!(f, " ({})", kind.as_str())?;
        }
        let otherwise = match report.outcome {
            CaseOutcome::Missing => "the golden does not exist yet",
            outcome => outcome.as_str(),
        };
        write!(f, ": {}", detail.as_deref().unwrap_or(otherwise))?;
        if let Some(shared) = &report.golden.fallback {
            write!(f, " ({})", text::ComparedWithFallback(shared))?;
        }
        Ok(())
    }
}

#[cfg(test)]
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
    fn test_image_link_is_relative_to_the_report() {
        let cwd = std::env::current_dir().unwrap();
        let link = |path: &str, dir: &str| image_link(Path::new(path), Path::new(dir));
        assert_eq!(
            link(
                ".gleon/runs/latest/artifacts/a/diff.png",
                ".gleon/runs/latest"
            ),
            "artifacts/a/diff.png"
        );
        assert_eq!(
            link("runs/latest/../baseline/a.png", "runs/latest"),
            "../baseline/a.png"
        );
        assert_eq!(
            link("baseline/a.png", "./runs/./latest"),
            "../../baseline/a.png"
        );
        assert_eq!(link("a/b", "a/b"), ".");
        // Mixed frames meet at the working directory (`--out report.html` gives an empty dir).
        let absolute = cwd.join("reports/img.png");
        assert_eq!(image_link(&absolute, Path::new("")), "reports/img.png");
        assert_eq!(image_link(&absolute, &cwd.join("reports")), "img.png");
    }

    #[cfg(windows)]
    #[test]
    fn test_image_link_to_another_drive_is_a_file_url() {
        assert_eq!(
            image_link(Path::new("D:\\runs\\a.png"), Path::new("C:\\reports")),
            "file:///D:/runs/a.png"
        );
    }

    #[test]
    fn test_case_summary_of_every_outcome() {
        use gleon_model::case::Metrics;

        use crate::cases::fixtures::{every_outcome, report};

        let summaries: Vec<_> = every_outcome()
            .iter()
            .map(|report| CaseSummary(report).to_string())
            .collect();
        assert_eq!(
            summaries,
            [
                "Pass: identical",
                "Pass: 0.00% (0 of 100px) differ",
                "Mismatch: 5.00% (5 of 100px) differ",
                "Dimension Mismatch: golden is 10x10px, test image is 20x10px",
                "Error (image): candidate image: corrupt",
                "Pass: updated",
                "Missing Baseline: no golden yet",
            ]
        );

        let mut dimensions = report("a", CaseOutcome::DimensionMismatch);
        dimensions.message = None;
        assert_eq!(
            CaseSummary(&dimensions).to_string(),
            "Dimension Mismatch: golden is 10x10px, test image is 20x10px"
        );
        dimensions.golden.width = None;
        assert_eq!(
            CaseSummary(&dimensions).to_string(),
            "Dimension Mismatch: dimension_mismatch"
        );
        let mut tiny = report("a", CaseOutcome::Mismatch);
        tiny.metrics = Some(Metrics::Pixel {
            total_pixels: 4_000_000,
            diff_pixels: 1,
            diff_ratio: 1.0 / 4_000_000.0,
            headroom: -1.0,
            text: None,
        });
        assert_eq!(
            CaseSummary(&tiny).to_string(),
            "Mismatch: <0.0001% (1 of 4000000px) differ"
        );
        assert_eq!(status(CaseOutcome::Identical), "Pass");

        let mut fallback = report("a", CaseOutcome::Mismatch);
        fallback.golden.fallback = Some("test/goldens/a.png".to_owned());
        assert_eq!(
            CaseSummary(&fallback).to_string(),
            "Mismatch: 5.00% (5 of 100px) differ (compared with test/goldens/a.png of the \
             fallback platform)"
        );
    }
}
