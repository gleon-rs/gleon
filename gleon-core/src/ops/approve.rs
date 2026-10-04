//! Baseline approval: promotes the candidates of failed cases to baselines.
//!
//! The cases come from the case reports of the latest run (or of downloaded copies of
//! `.gleon/runs/latest`, e.g. the CI artifacts of other machines), each failed one with its
//! candidate image in the artifacts directory. A case of `gleon diff` ([`CLI_TOOL`]) becomes a
//! manifest plus blob on the platform the case ran on; a case of an integration whose goldens are
//! PNG files in the repository (no `golden.blob`) overwrites that file.
//!
//! Every candidate is checked before anything is written, so a bad one approves nothing.

use std::{
    collections::{HashMap, hash_map::Entry},
    io,
    path::{Component, Path, PathBuf},
};

use gleon_model::{
    case::{self, CaseOutcome, CaseReport, RunId, Sha256Hex},
    fs::Durability,
};
use rayon::prelude::*;
use thiserror::Error;

use crate::{
    cases::{Cases, CasesError, check_run_dir},
    context::{ContextError, ResolvedContext},
    manifest::{ManifestError, SingleTestManifest, WorkspaceIndex},
    ops::{
        common::{CoreError, build_manifest, ensure_initialized, load_index_with_fallback},
        diff::CLI_TOOL,
    },
};

/// Where an approved candidate goes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Target {
    /// The manifest and blob of the test `name` on the platform `platform_key` (`gleon diff`).
    Manifest { platform_key: String, name: String },
    /// A golden file of an integration, inside the workspace.
    File(PathBuf),
}

/// A candidate to approve.
struct Candidate<'a> {
    report: &'a CaseReport,
    /// Its hash: a kept candidate always has one (`CaseReport::validate`).
    sha256: &'a Sha256Hex,
    file: PathBuf,
    target: Target,
}

/// A checked candidate and what approving it writes.
enum Approval<'a> {
    /// The candidate becomes the golden file `golden`.
    File {
        candidate: &'a Candidate<'a>,
        golden: &'a Path,
    },
    /// The candidate becomes a blob and the manifest of its test on `platform_key`.
    Manifest {
        candidate: &'a Candidate<'a>,
        platform_key: &'a str,
        phash: String,
        width: u32,
        height: u32,
    },
}

const MAX_IMAGE_FILE_SIZE: u64 = 64 * 1024 * 1024; // 64 MB

/// The bytes of the candidate, size-checked and checked to be the candidate its case report
/// describes.
fn read_candidate(candidate: &Candidate<'_>) -> Result<Vec<u8>, ApproveError> {
    use std::io::Read as _;

    let path = &candidate.file;
    let io_error = |source| ApproveError::Io {
        path: path.clone(),
        source,
    };
    let file = std::fs::File::open(path).map_err(io_error)?;
    let size = file.metadata().map_err(io_error)?.len();
    let too_large = |size| ApproveError::ImageTooLarge {
        path: path.clone(),
        size,
        limit: MAX_IMAGE_FILE_SIZE,
    };
    if size > MAX_IMAGE_FILE_SIZE {
        return Err(too_large(size));
    }
    let mut bytes = Vec::new();
    // Bounded again: the file may grow after its size was read.
    let read = file
        .take(MAX_IMAGE_FILE_SIZE + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)? as u64;
    if read > MAX_IMAGE_FILE_SIZE {
        return Err(too_large(read));
    }
    if Sha256Hex::of(&bytes) != *candidate.sha256 {
        return Err(ApproveError::CandidateChanged {
            name: candidate.report.name.clone(),
            path: path.clone(),
        });
    }
    Ok(bytes)
}

/// Reads and checks the candidate: a valid PNG for a golden file, decoded and measured for a
/// manifest.
fn check<'a>(candidate: &'a Candidate<'a>) -> Result<Approval<'a>, ApproveError> {
    let bytes = read_candidate(candidate)?;
    let decode_error = |e| match e {
        ManifestError::Image(source) => ApproveError::ImageDecode {
            path: candidate.file.clone(),
            source,
        },
        other => CoreError::Manifest(other).into(),
    };
    Ok(match &candidate.target {
        Target::File(golden) => {
            SingleTestManifest::validate_image_bytes(&bytes).map_err(decode_error)?;
            Approval::File { candidate, golden }
        }
        Target::Manifest { platform_key, .. } => {
            let image = SingleTestManifest::load_image_from_bytes(&bytes).map_err(decode_error)?;
            Approval::Manifest {
                candidate,
                platform_key,
                phash: gleon_engine::phash::compute_phash(&image.to_rgba8()),
                width: image.width(),
                height: image.height(),
            }
        }
    })
}

/// Errors that can occur during baseline approval.
#[derive(Debug, Error)]
pub enum ApproveError {
    /// No failed case with a candidate image to approve.
    #[error(
        "No failed cases with candidate images{filters} to approve in {paths}: run the tests (or \
         `gleon diff`) first, or pass the downloaded `.gleon/runs/latest` of a CI run with --from"
    )]
    NothingToApprove {
        /// The run directories that were read, listed for the message.
        paths: String,
        /// The filters, listed for the message (` matching 'a', 'b'`), or empty.
        filters: String,
    },

    /// The candidate image of a case is not the one its report describes.
    #[error(
        "The candidate of '{name}' at '{path}' does not match its case report: the run changed \
         it, run the tests again"
    )]
    CandidateChanged {
        /// The test name.
        name: String,
        /// The candidate image.
        path: PathBuf,
    },

    /// A case lists its candidate outside the run directory.
    #[error("The candidate of '{name}' ('{path}') is not inside .gleon/runs")]
    CandidateOutsideRuns {
        /// The test name.
        name: String,
        /// The path in the case report.
        path: String,
    },

    /// An integration's golden path that approve does not write: no `.png` file, inside a hidden
    /// directory such as `.git/`, `.github/` or `.gleon/`, a symlink or a path leading outside the
    /// workspace through one, or an existing file that is no PNG.
    #[error(
        "Refusing to write the golden '{path}' of '{name}': approve only replaces PNG goldens \
         inside the workspace, outside hidden directories and not through symlinks"
    )]
    UnsafeGoldenPath {
        /// The test name.
        name: String,
        /// The golden path of the case report.
        path: String,
    },

    /// A case of an integration whose baseline is a blob: no integration keeps manifests yet.
    #[error(
        "Cannot approve '{name}' of '{tool}': its baseline is a blob, which only `gleon diff` \
         cases are approved into"
    )]
    UnsupportedBaseline {
        /// The test name.
        name: String,
        /// The integration that wrote the case.
        tool: String,
    },

    /// Two runs offer different candidates for the same baseline.
    #[error(
        "Different candidates for '{name}' in '{first}' and '{second}': approve one run at a time"
    )]
    ConflictingCandidates {
        /// The test name.
        name: String,
        /// The run directory of the first candidate.
        first: PathBuf,
        /// The run directory of the second candidate.
        second: PathBuf,
    },

    /// Image file exceeds maximum allowed size.
    #[error("Image file '{path}' exceeds maximum size of {limit} bytes (actual: {size} bytes)")]
    ImageTooLarge {
        /// The path to the oversized image.
        path: PathBuf,
        /// The image's actual size in bytes.
        size: u64,
        /// The maximum allowed size in bytes.
        limit: u64,
    },

    /// Error decoding image file.
    #[error("Image decode error for '{path}'")]
    ImageDecode {
        /// The path to the image that failed to decode.
        path: PathBuf,
        /// The underlying decode error.
        #[source]
        source: image::ImageError,
    },

    /// A candidate cannot be read, or a golden or blob cannot be written.
    #[error("cannot access '{path}': {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },

    /// The case reports cannot be read.
    #[error(transparent)]
    Cases(#[from] CasesError),

    /// Error shared across `ops::*` operations.
    #[error(transparent)]
    Core(#[from] CoreError),
}

/// Result summary of approving screenshots.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ApproveResult {
    /// The test names approved.
    pub approved_test_cases: Vec<String>,
    /// What reading the runs skipped (see [`Cases::warnings`]).
    pub warnings: Vec<String>,
}

/// Whether `report` names `filter`: its test name, golden path or compared golden (the shared
/// golden another platform's case fell back to, which its test prints) starts with it (whole
/// names).
fn matches_filter(report: &CaseReport, filter: &Path) -> bool {
    let matches_golden = |golden: &str| {
        let golden = Path::new(golden);
        golden.starts_with(filter) || golden.with_extension("").starts_with(filter)
    };
    Path::new(&report.name).starts_with(filter)
        || matches_golden(&report.golden.path)
        || matches_golden(report.golden.compared())
}

/// Whether `file` resolves inside `base_dir`: its nearest existing directory, symlinks followed,
/// lies there (the missing ones are created inside it).
fn resolves_inside(base_dir: &Path, file: &Path) -> bool {
    let Ok(base) = base_dir.canonicalize() else {
        return false;
    };
    file.ancestors()
        .skip(1)
        .find_map(|dir| dir.canonicalize().ok())
        .is_some_and(|dir| dir.starts_with(base))
}

/// The golden file of an integration case inside the workspace at `base_dir`, if approve may
/// write it: a `.png` path outside hidden directories, not a symlink nor through one out of the
/// workspace, and no file there that is not a PNG. The case reports may come from a CI artifact
/// of untrusted code, so they never pick another file.
fn golden_file(base_dir: &Path, report: &CaseReport) -> Result<PathBuf, ApproveError> {
    use std::io::Read as _;

    let path = Path::new(&report.golden.path);
    let is_png = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("png"));
    let is_hidden = path.components().any(|component| {
        matches!(component, Component::Normal(name) if name.to_string_lossy().starts_with('.'))
    });
    let file = base_dir.join(path);
    let is_link = std::fs::symlink_metadata(&file).is_ok_and(|meta| meta.file_type().is_symlink());
    // The PNG header (signature and size) is enough to tell.
    let mut head = Vec::new();
    let replaces_no_png = std::fs::File::open(&file)
        .and_then(|file| file.take(24).read_to_end(&mut head))
        .is_ok_and(|_| case::png_size(&head).is_none());
    if !is_png || is_hidden || is_link || replaces_no_png || !resolves_inside(base_dir, &file) {
        return Err(ApproveError::UnsafeGoldenPath {
            name: report.name.clone(),
            path: report.golden.path.clone(),
        });
    }
    Ok(file)
}

/// The golden `report` was compared with, inside the workspace at `base_dir`: the source of a
/// per-platform golden approved from a pass without differences.
fn compared_golden(base_dir: &Path, report: &CaseReport) -> Result<PathBuf, ApproveError> {
    let file = base_dir.join(report.golden.compared());
    let is_link = std::fs::symlink_metadata(&file).is_ok_and(|meta| meta.file_type().is_symlink());
    if is_link || !resolves_inside(base_dir, &file) {
        return Err(ApproveError::UnsafeGoldenPath {
            name: report.name.clone(),
            path: report.golden.compared().to_owned(),
        });
    }
    Ok(file)
}

/// Where the candidate of `report` goes.
fn target(base_dir: &Path, report: &CaseReport) -> Result<Target, ApproveError> {
    if report.source.tool == CLI_TOOL {
        let platform_key = report
            .platform
            .to_key()
            .map_err(|e| CoreError::Context(ContextError::Platform(e)))?;
        return Ok(Target::Manifest {
            platform_key,
            name: report.name.clone(),
        });
    }
    if report.golden.blob.is_some() {
        return Err(ApproveError::UnsupportedBaseline {
            name: report.name.clone(),
            tool: report.source.tool.clone(),
        });
    }
    golden_file(base_dir, report).map(Target::File)
}

/// The candidates of the failed cases of the `loaded` runs that match `filters`, one per
/// baseline.
fn candidates_of<'a>(
    base_dir: &Path,
    filters: &[PathBuf],
    loaded: &'a [(&'a Path, Cases)],
) -> Result<Vec<Candidate<'a>>, ApproveError> {
    let mut candidates = Vec::new();
    let mut by_target = HashMap::<Target, (&Sha256Hex, &Path)>::new();
    for (run, cases) in loaded {
        for report in cases.reports() {
            // A pass against another platform's golden kept its candidate to become this
            // platform's own golden.
            let is_seed = report.golden.fallback.is_some()
                && matches!(report.outcome, CaseOutcome::Match | CaseOutcome::Identical);
            let can_approve = (is_seed
                || matches!(
                    report.outcome,
                    CaseOutcome::Mismatch | CaseOutcome::DimensionMismatch | CaseOutcome::Missing
                ))
                && (filters.is_empty() || filters.iter().any(|p| matches_filter(report, p)));
            if !can_approve {
                continue;
            }
            let kept = report
                .artifacts
                .as_ref()
                .and_then(|artifacts| artifacts.candidate.as_ref());
            let (file, sha256) = match (kept, is_seed) {
                (Some(candidate), _) => {
                    let Some(sha256) = report.candidate.sha256.as_ref() else {
                        continue;
                    };
                    let file = cases.artifact_path(candidate).ok_or_else(|| {
                        ApproveError::CandidateOutsideRuns {
                            name: report.name.clone(),
                            path: candidate.clone(),
                        }
                    })?;
                    (file, sha256)
                }
                // A pass without differences kept no candidate: the compared (shared) golden is
                // this platform's rendering, checked against the hash the report recorded.
                (None, true) => {
                    let Some(sha256) = report.golden.sha256.as_ref() else {
                        continue;
                    };
                    (compared_golden(base_dir, report)?, sha256)
                }
                (None, false) => continue,
            };
            let target = target(base_dir, report)?;
            match by_target.entry(target.clone()) {
                Entry::Occupied(first) if first.get().0 != sha256 => {
                    return Err(ApproveError::ConflictingCandidates {
                        name: report.name.clone(),
                        first: first.get().1.to_path_buf(),
                        second: run.to_path_buf(),
                    });
                }
                // The same candidate from another run (e.g. two jobs of one platform).
                Entry::Occupied(_) => continue,
                Entry::Vacant(slot) => {
                    let _ = slot.insert((sha256, run));
                }
            }
            candidates.push(Candidate {
                report,
                sha256,
                file,
                target,
            });
        }
    }
    Ok(candidates)
}

/// [`ApproveError::NothingToApprove`] in `runs` with `filters`.
fn nothing_to_approve(runs: &[PathBuf], filters: &[PathBuf]) -> ApproveError {
    let quoted = |paths: &[PathBuf]| {
        paths
            .iter()
            .map(|path| format!("'{}'", path.display()))
            .collect::<Vec<_>>()
            .join(", ")
    };
    ApproveError::NothingToApprove {
        paths: quoted(runs),
        filters: if filters.is_empty() {
            String::new()
        } else {
            format!(" matching {}", quoted(filters))
        },
    }
}

/// Promotes the candidates of the failed cases of a run to baselines.
///
/// Failed cases are `mismatch`, `dimension_mismatch` and `missing`; an integration's pass against
/// another platform's golden (`golden.fallback`) becomes this platform's own golden too: its kept
/// candidate, or the compared golden itself when no pixel differed. Cases of `gleon diff` become
/// manifests and blobs on the platform of the case, cases of integrations without manifests
/// overwrite their golden PNG file. Every candidate is checked before the first write.
///
/// `runs` are directories standing in for `.gleon/runs/latest` (the downloaded runs of CI jobs,
/// with `cases/` and `artifacts/`), each read as it is; without them the latest run of the
/// workspace is read, picked with `run_id` (`GLEON_RUN_ID`). `paths` keeps only the cases whose test name or
/// golden path starts with one of them (`./` ignored).
///
/// # Errors
///
/// Returns an error if the workspace is not initialized, if one of `runs` is no run directory,
/// if the runs have no failed case with a candidate image (after filtering), if the case reports
/// or a candidate cannot be read, if a candidate is not the one its report describes, exceeds the
/// maximum size or fails to decode, if a golden path is not one approve writes, if two runs offer
/// different candidates for one baseline, or if writing manifests, blobs or goldens fails.
pub fn approve_workspace(
    context: &ResolvedContext,
    paths: &[PathBuf],
    runs: &[PathBuf],
    run_id: Option<&RunId>,
) -> Result<ApproveResult, ApproveError> {
    let base_dir = context.base_dir.as_path();
    let gleon_paths = ensure_initialized(base_dir)?;
    let own_run = [gleon_paths.runs_latest()];
    let (runs, run_id) = if runs.is_empty() {
        (&own_run[..], run_id)
    } else {
        for run in runs {
            check_run_dir(run)?;
        }
        (runs, None)
    };
    let loaded = runs
        .iter()
        .map(|run| Cases::load(run, run_id).map(|cases| (run.as_path(), cases)))
        .collect::<Result<Vec<_>, _>>()?;

    // Paths from the shell (`./shots/a`) filter like their plain form.
    let filters: Vec<PathBuf> = paths
        .iter()
        .map(|path| {
            path.components()
                .filter(|component| *component != Component::CurDir)
                .collect()
        })
        .collect();
    let candidates = candidates_of(base_dir, &filters, &loaded)?;
    if candidates.is_empty() {
        return Err(nothing_to_approve(runs, &filters));
    }

    // Checked (and decoded) in parallel, one image in memory per thread; nothing is written
    // unless every candidate is fine.
    let approvals = candidates
        .par_iter()
        .map(check)
        .collect::<Result<Vec<_>, _>>()?;

    let blobs_dir = gleon_paths.blob_scheme_dir("sha256");
    // The manifests of each platform, with those of the fallback platform, loaded once.
    let mut indexes = HashMap::<&str, (WorkspaceIndex, Option<WorkspaceIndex>)>::new();
    let mut approved_test_cases = Vec::with_capacity(approvals.len());
    for approval in approvals {
        let write = |path: &Path, bytes: &[u8]| {
            gleon_model::fs::write_atomically(path, bytes, Durability::Durable).map_err(|source| {
                ApproveError::Io {
                    path: path.to_path_buf(),
                    source,
                }
            })
        };
        let (candidate, platform_key, phash, width, height) = match approval {
            Approval::File { candidate, golden } => {
                // Read again: it must still be the checked candidate.
                write(golden, &read_candidate(candidate)?)?;
                approved_test_cases.push(candidate.report.name.clone());
                continue;
            }
            Approval::Manifest {
                candidate,
                platform_key,
                phash,
                width,
                height,
            } => (candidate, platform_key, phash, width, height),
        };
        let report = candidate.report;
        let sha256_hex = candidate.sha256.as_str();
        let blob = blobs_dir.join(sha256_hex);
        // Blobs are named by their content and written atomically: one that exists is this one
        // (cases often share a candidate), and a durable write costs an fsync.
        if !blob.is_file() {
            write(&blob, &read_candidate(candidate)?)?;
        }

        let new_manifest = build_manifest(sha256_hex, &phash, width, height)?;
        let (workspace_index, fallback_index) = match indexes.entry(platform_key) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(load_index_with_fallback(
                &gleon_paths,
                platform_key,
                context.fallback_platform_key.as_deref(),
            )?),
        };
        let manifests_dir = gleon_paths.manifests_dir(platform_key);

        let matches_fallback = fallback_index
            .as_ref()
            .and_then(|fb| fb.get(&report.name))
            .is_some_and(|fb_manifest| fb_manifest.hash == new_manifest.hash);

        if matches_fallback {
            // If a local override existed on disk, remove it to keep repository sparse
            let _ = workspace_index
                .remove_test(&manifests_dir, &report.name)
                .map_err(CoreError::Manifest)?;
            tracing::debug!(
                "Approved test '{}' matches fallback platform manifest — pruned/skipped local override.",
                report.name
            );
        } else {
            workspace_index
                .save_test(&manifests_dir, &report.name, &new_manifest)
                .map_err(CoreError::Manifest)?;
        }

        approved_test_cases.push(report.name.clone());
    }

    Ok(ApproveResult {
        approved_test_cases,
        warnings: loaded
            .iter()
            .flat_map(|(_, cases)| cases.warnings().iter().cloned())
            .collect(),
    })
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
    use gleon_model::platform::{PlatformConfig, PlatformFields};
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::{cases::fixtures::report, config::ConfigError, manifest::ImageHash};

    /// A workspace with `.gleon/` and the context of its host platform.
    fn workspace() -> (tempfile::TempDir, ResolvedContext) {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".gleon")).unwrap();
        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            ..ResolvedContext::default()
        };
        (temp, ctx)
    }

    /// A 10x10 PNG of `shade`.
    fn png(shade: u8) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::RgbaImage::from_pixel(10, 10, image::Rgba([shade, 0, 0, 255]))
            .write_to(&mut io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        bytes
    }

    fn linux() -> PlatformConfig {
        PlatformConfig::Structured(PlatformFields {
            os: Some("linux".to_owned()),
            arch: Some("x86_64".to_owned()),
            ..PlatformFields::default()
        })
    }

    /// Records a failed case `name` of `tool` with `candidate` in the run directory
    /// `runs_latest` (run `run-1`, platform `platform`); its golden is `goldens/<name>.png`.
    fn failed_case_on(
        runs_latest: &Path,
        name: &str,
        tool: &str,
        outcome: CaseOutcome,
        candidate: &[u8],
        platform: PlatformConfig,
    ) -> CaseReport {
        let mut case = report(name, outcome);
        // One run: the goldens of this run need not exist on disk.
        case.run_id = Some(RunId::new("run-1").unwrap());
        case.source.tool = tool.to_owned();
        case.platform = platform;
        case.golden.path = format!("goldens/{name}.png");
        case.candidate.sha256 = Some(Sha256Hex::of(candidate));
        let file = runs_latest.join(format!("artifacts/{name}/candidate.png"));
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, candidate).unwrap();
        write_case(runs_latest, &case);
        case
    }

    fn failed_case(
        runs_latest: &Path,
        name: &str,
        tool: &str,
        outcome: CaseOutcome,
        candidate: &[u8],
    ) -> CaseReport {
        failed_case_on(runs_latest, name, tool, outcome, candidate, linux())
    }

    fn write_case(runs_latest: &Path, case: &CaseReport) {
        let json = runs_latest.join(format!("cases/{}.json", case.name));
        std::fs::create_dir_all(json.parent().unwrap()).unwrap();
        std::fs::write(json, serde_json::to_vec(case).unwrap()).unwrap();
    }

    fn runs_latest(ctx: &ResolvedContext) -> PathBuf {
        ctx.base_dir.join(".gleon/runs/latest")
    }

    fn manifest_of(ctx: &ResolvedContext, platform_key: &str, name: &str) -> SingleTestManifest {
        let file = ctx
            .base_dir
            .join(".gleon/manifests")
            .join(platform_key)
            .join(format!("{name}.json"));
        SingleTestManifest::load(file).unwrap()
    }

    const LINUX: &str = "linux-x86_64";

    #[test]
    fn test_approve_error_display() {
        let err: ApproveError = CoreError::NotInitialized.into();
        assert!(err.to_string().contains("not initialized"));

        let nothing = ApproveError::NothingToApprove {
            paths: "'foo'".to_owned(),
            filters: " matching 'a/b'".to_owned(),
        };
        assert!(
            nothing.to_string().starts_with(
                "No failed cases with candidate images matching 'a/b' to approve in 'foo'"
            ),
            "{nothing}"
        );

        let config: ApproveError =
            CoreError::Config(ConfigError::Validation("bad".to_string())).into();
        assert!(config.to_string().contains("Config error"));

        let decode = ApproveError::ImageDecode {
            path: PathBuf::from("bar.png"),
            source: image::ImageError::Limits(image::error::LimitError::from_kind(
                image::error::LimitErrorKind::DimensionError,
            )),
        };
        assert!(
            decode
                .to_string()
                .contains("Image decode error for 'bar.png'")
        );
    }

    #[test]
    fn test_approve_not_initialized() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            ..ResolvedContext::default()
        };
        assert!(matches!(
            approve_workspace(&ctx, &[], &[], None),
            Err(ApproveError::Core(CoreError::NotInitialized))
        ));
    }

    #[test]
    fn test_approve_without_failed_cases() {
        let (_temp, ctx) = workspace();
        assert!(matches!(
            approve_workspace(&ctx, &[], &[], None),
            Err(ApproveError::NothingToApprove { .. })
        ));

        // Passing cases and errors have nothing to approve.
        let latest = runs_latest(&ctx);
        for (name, outcome) in [("a", CaseOutcome::Match), ("b", CaseOutcome::Error)] {
            write_case(&latest, &report(name, outcome));
        }
        assert!(matches!(
            approve_workspace(&ctx, &[], &[], None),
            Err(ApproveError::NothingToApprove { .. })
        ));
        let err = approve_workspace(&ctx, &[], &[PathBuf::from("relative")], None).unwrap_err();
        assert!(
            matches!(err, ApproveError::Cases(CasesError::NotARun { ref path }) if path == Path::new("relative")),
            "{err}"
        );
        std::fs::create_dir_all(latest.join("cases")).unwrap();
        let err = approve_workspace(&ctx, &[PathBuf::from("b")], &[latest], None).unwrap_err();
        assert!(
            matches!(err, ApproveError::NothingToApprove { ref filters, .. } if filters == " matching 'b'"),
            "{err}"
        );
    }

    /// Paths from the shell (`./shots/a`) filter like their plain form.
    #[test]
    fn test_approve_filters_ignore_a_leading_dot() {
        let (_temp, ctx) = workspace();
        let latest = runs_latest(&ctx);
        failed_case(&latest, "shots/a", CLI_TOOL, CaseOutcome::Mismatch, &png(1));
        failed_case(&latest, "shots/b", CLI_TOOL, CaseOutcome::Mismatch, &png(2));
        let res = approve_workspace(&ctx, &[PathBuf::from("./shots/a")], &[], None).unwrap();
        assert_eq!(res.approved_test_cases, ["shots/a"]);
    }

    /// One bad candidate approves nothing: every candidate is checked before the first write.
    #[test]
    fn test_approve_writes_nothing_when_a_candidate_is_bad() {
        let (temp, ctx) = workspace();
        let latest = runs_latest(&ctx);
        failed_case(&latest, "a", CLI_TOOL, CaseOutcome::Mismatch, &png(1));
        failed_case(&latest, "b", "gleon_flutter", CaseOutcome::Missing, &png(2));
        failed_case(&latest, "z", CLI_TOOL, CaseOutcome::Mismatch, &png(3));
        std::fs::write(latest.join("artifacts/z/candidate.png"), png(9)).unwrap();
        assert!(matches!(
            approve_workspace(&ctx, &[], &[], None),
            Err(ApproveError::CandidateChanged { ref name, .. }) if name == "z"
        ));
        assert!(!temp.path().join(".gleon/manifests").exists());
        assert!(!temp.path().join(".gleon/blobs").exists());
        assert!(!temp.path().join("goldens/b.png").exists());

        std::fs::remove_file(latest.join("artifacts/z/candidate.png")).unwrap();
        let err = approve_workspace(&ctx, &[], &[], None).unwrap_err();
        assert!(
            matches!(err, ApproveError::Io { ref path, .. } if path.ends_with("z/candidate.png")),
            "{err}"
        );
        assert!(err.to_string().contains("candidate.png"), "{err}");
    }

    /// A golden that cannot be written fails with its path, after the checks passed.
    #[test]
    fn test_approve_names_the_golden_it_cannot_write() {
        let (temp, ctx) = workspace();
        let latest = runs_latest(&ctx);
        failed_case(&latest, "g", "gleon_flutter", CaseOutcome::Missing, &png(1));
        std::fs::create_dir_all(temp.path().join("goldens/g.png/inside")).unwrap();
        let err = approve_workspace(&ctx, &[], &[], None).unwrap_err();
        assert!(
            matches!(err, ApproveError::Io { ref path, .. } if path.ends_with("goldens/g.png")),
            "{err}"
        );
    }

    /// A golden that is a symlink, or lies behind one leading out of the workspace, is never
    /// written: approve would write wherever it points.
    #[cfg(unix)]
    #[test]
    fn test_approve_does_not_write_through_symlinks() {
        let (temp, ctx) = workspace();
        let outside = tempfile::tempdir().unwrap();
        let latest = runs_latest(&ctx);
        std::fs::create_dir_all(temp.path().join("goldens")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("target.txt"),
            temp.path().join("goldens/a.png"),
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.path(), temp.path().join("linked")).unwrap();
        for golden in ["goldens/a.png", "linked/b.png", "linked/new/c.png"] {
            let mut case =
                failed_case(&latest, "g", "gleon_flutter", CaseOutcome::Missing, &png(1));
            case.golden.path = golden.to_owned();
            write_case(&latest, &case);
            let err = approve_workspace(&ctx, &[], &[], None).unwrap_err();
            assert!(
                matches!(err, ApproveError::UnsafeGoldenPath { .. }),
                "{golden}: {err}"
            );
        }
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn test_approve_picks_the_run_of_the_caller() {
        let (_temp, ctx) = workspace();
        let latest = runs_latest(&ctx);
        failed_case(&latest, "a", CLI_TOOL, CaseOutcome::Mismatch, &png(1));
        let other = RunId::new("other").unwrap();
        assert!(matches!(
            approve_workspace(&ctx, &[], &[], Some(&other)),
            Err(ApproveError::NothingToApprove { .. })
        ));
        let run = RunId::new("run-1").unwrap();
        let res = approve_workspace(&ctx, &[], &[], Some(&run)).unwrap();
        assert_eq!(res.approved_test_cases, ["a"]);
    }

    #[test]
    fn test_approve_cli_cases_as_manifests_of_their_platform() {
        let (temp, ctx) = workspace();
        let latest = runs_latest(&ctx);
        failed_case(
            &latest,
            "auth/login",
            CLI_TOOL,
            CaseOutcome::Mismatch,
            &png(1),
        );
        failed_case(
            &latest,
            "author/profile",
            CLI_TOOL,
            CaseOutcome::Missing,
            &png(2),
        );

        // "auth" matches `auth/login`, not `author/profile` (whole names).
        let filtered = approve_workspace(&ctx, &[PathBuf::from("auth")], &[], None).unwrap();
        assert_eq!(filtered.approved_test_cases, ["auth/login"]);
        // The manifests of the platform the case ran on, not of this machine.
        assert_ne!(ctx.platform.to_key().unwrap(), LINUX);
        let manifest = manifest_of(&ctx, LINUX, "auth/login");
        let sha = hex::encode(Sha256::digest(png(1)));
        assert_eq!(manifest.hash.value(), sha);
        assert!(temp.path().join(".gleon/blobs/sha256").join(&sha).is_file());
        assert!(
            !temp.path().join("goldens").exists(),
            "no PNG golden for CLI cases"
        );

        // A golden path filter works too.
        let by_path =
            approve_workspace(&ctx, &[PathBuf::from("goldens/author")], &[], None).unwrap();
        assert_eq!(by_path.approved_test_cases, ["author/profile"]);
    }

    #[test]
    fn test_approve_several_runs_of_several_platforms() {
        let (temp, ctx) = workspace();
        let macos = PlatformConfig::Structured(PlatformFields {
            os: Some("macos".to_owned()),
            arch: Some("aarch64".to_owned()),
            ..PlatformFields::default()
        });
        let linux_run = temp.path().join("download/linux/latest");
        let macos_run = temp.path().join("download/macos/latest");
        failed_case(&linux_run, "a", CLI_TOOL, CaseOutcome::Mismatch, &png(1));
        failed_case_on(
            &macos_run,
            "a",
            CLI_TOOL,
            CaseOutcome::Mismatch,
            &png(2),
            macos,
        );

        let res = approve_workspace(&ctx, &[], &[linux_run, macos_run], None).unwrap();
        assert_eq!(res.approved_test_cases.len(), 2);
        let sha = |shade| hex::encode(Sha256::digest(png(shade)));
        assert_eq!(manifest_of(&ctx, LINUX, "a").hash.value(), sha(1));
        assert_eq!(manifest_of(&ctx, "macos-aarch64", "a").hash.value(), sha(2));
    }

    #[test]
    fn test_approve_integration_cases_write_their_golden_file() {
        let (temp, ctx) = workspace();
        let downloaded = temp.path().join("download/runs-latest");
        failed_case(
            &downloaded,
            "test/goldens/a",
            "gleon_flutter",
            CaseOutcome::Mismatch,
            &png(3),
        );
        failed_case(
            &downloaded,
            "test/goldens/b",
            "gleon_flutter",
            CaseOutcome::DimensionMismatch,
            &png(4),
        );

        let res = approve_workspace(&ctx, &[], &[downloaded], None).unwrap();
        assert_eq!(res.approved_test_cases.len(), 2);
        assert_eq!(
            std::fs::read(temp.path().join("goldens/test/goldens/a.png")).unwrap(),
            png(3)
        );
        assert_eq!(
            std::fs::read(temp.path().join("goldens/test/goldens/b.png")).unwrap(),
            png(4)
        );
        assert!(!temp.path().join(".gleon/manifests").exists());
    }

    /// One golden file, two candidates (e.g. the same golden rendered on two hosts): ambiguous.
    #[test]
    fn test_approve_refuses_conflicting_candidates() {
        let (temp, ctx) = workspace();
        let first = temp.path().join("download/a/latest");
        let second = temp.path().join("download/b/latest");
        let third = temp.path().join("download/c/latest");
        failed_case(&first, "g", "gleon_flutter", CaseOutcome::Mismatch, &png(1));
        failed_case(
            &second,
            "g",
            "gleon_flutter",
            CaseOutcome::Mismatch,
            &png(2),
        );
        failed_case(&third, "g", "gleon_flutter", CaseOutcome::Mismatch, &png(1));

        let err = approve_workspace(&ctx, &[], &[first.clone(), second.clone()], None).unwrap_err();
        assert!(
            matches!(err, ApproveError::ConflictingCandidates { ref name, .. } if name == "g"),
            "{err}"
        );
        assert!(!temp.path().join("goldens/g.png").exists());
        // The same candidate twice is one approval.
        let res = approve_workspace(&ctx, &[], &[first, third], None).unwrap();
        assert_eq!(res.approved_test_cases, ["g"]);
    }

    /// Integrations with per-platform goldens (`fallback_platform`): each platform's case names
    /// its own golden file, so the runs of two platforms approve side by side.
    #[test]
    fn test_approve_per_platform_goldens_of_integrations() {
        let (temp, ctx) = workspace();
        let macos = PlatformConfig::Structured(PlatformFields {
            os: Some("macos".to_owned()),
            arch: Some("aarch64".to_owned()),
            ..PlatformFields::default()
        });
        let macos_run = temp.path().join("download/metrics-macos-arm64");
        let linux_run = temp.path().join("download/metrics-linux-x64");
        let name = "test/goldens/a";
        failed_case_on(
            &macos_run,
            name,
            "gleon_flutter",
            CaseOutcome::Mismatch,
            &png(1),
            macos,
        );
        let mut linux = failed_case(
            &linux_run,
            name,
            "gleon_flutter",
            CaseOutcome::Mismatch,
            &png(2),
        );
        // Compared with the shared (macOS) golden, since Linux had none of its own yet.
        linux.golden.path = "test/goldens/linux-x86_64/a.png".to_owned();
        linux.golden.fallback = Some("test/goldens/a.png".to_owned());
        write_case(&linux_run, &linux);

        // The path the Linux test printed (the compared shared golden) selects its case too.
        let res = approve_workspace(
            &ctx,
            &[PathBuf::from("test/goldens/a.png")],
            &[macos_run.clone(), linux_run.clone()],
            None,
        )
        .unwrap();
        assert_eq!(res.approved_test_cases, [name]);
        let golden = |path: &str| std::fs::read(temp.path().join(path)).unwrap();
        assert_eq!(golden("test/goldens/linux-x86_64/a.png"), png(2));

        let res = approve_workspace(&ctx, &[], &[macos_run, linux_run], None).unwrap();
        assert_eq!(res.approved_test_cases, [name, name]);
        assert_eq!(golden("goldens/test/goldens/a.png"), png(1));
    }

    /// A pass against another platform's golden kept its candidate: approving it records this
    /// platform's own golden; a pass without `golden.fallback` has nothing to approve.
    #[test]
    fn test_approve_seeds_own_goldens_from_passes_against_the_fallback() {
        let (temp, ctx) = workspace();
        let run = temp.path().join("download/metrics-windows-x64");
        let mut seed = failed_case(
            &run,
            "test/goldens/a",
            "gleon_flutter",
            CaseOutcome::Match,
            &png(3),
        );
        seed.golden.path = "test/goldens/windows-x86_64/a.png".to_owned();
        seed.golden.fallback = Some("test/goldens/a.png".to_owned());
        seed.artifacts = Some(case::Artifacts {
            candidate: Some(".gleon/runs/latest/artifacts/test/goldens/a/candidate.png".to_owned()),
            ..case::Artifacts::default()
        });
        seed.validate().unwrap();
        write_case(&run, &seed);
        failed_case(
            &run,
            "test/goldens/b",
            "gleon_flutter",
            CaseOutcome::Match,
            &png(4),
        );

        let res = approve_workspace(&ctx, &[], &[run], None).unwrap();
        assert_eq!(res.approved_test_cases, ["test/goldens/a"]);
        assert_eq!(
            std::fs::read(temp.path().join("test/goldens/windows-x86_64/a.png")).unwrap(),
            png(3)
        );
    }

    /// A pass against the fallback without a differing pixel keeps no candidate: approving it
    /// copies the compared golden, which must still be the one the report hashed.
    #[test]
    fn test_approve_seeds_passes_without_differences_from_the_compared_golden() {
        let (temp, ctx) = workspace();
        let shared = temp.path().join("test/goldens/a.png");
        std::fs::create_dir_all(shared.parent().unwrap()).unwrap();
        std::fs::write(&shared, png(5)).unwrap();
        let run = temp.path().join("download/metrics-windows-x64");
        let mut seed = report("test/goldens/a", CaseOutcome::Match);
        seed.run_id = Some(RunId::new("run-1").unwrap());
        seed.source.tool = "gleon_flutter".to_owned();
        seed.golden.path = "test/goldens/windows-x86_64/a.png".to_owned();
        seed.golden.fallback = Some("test/goldens/a.png".to_owned());
        seed.golden.sha256 = Some(Sha256Hex::of(&png(5)));
        seed.validate().unwrap();
        write_case(&run, &seed);

        let res = approve_workspace(&ctx, &[], std::slice::from_ref(&run), None).unwrap();
        assert_eq!(res.approved_test_cases, ["test/goldens/a"]);
        let own = temp.path().join("test/goldens/windows-x86_64/a.png");
        assert_eq!(std::fs::read(&own).unwrap(), png(5));

        // The shared golden changed since the run: nothing is approved from it.
        std::fs::remove_file(&own).unwrap();
        std::fs::write(&shared, png(6)).unwrap();
        assert!(matches!(
            approve_workspace(&ctx, &[], &[run], None),
            Err(ApproveError::CandidateChanged { .. })
        ));
        assert!(!own.exists());
    }

    /// Case reports may come from CI artifacts of untrusted code: they never pick another file.
    #[test]
    fn test_approve_writes_only_png_goldens_outside_hidden_directories() {
        let (temp, ctx) = workspace();
        let latest = runs_latest(&ctx);
        let candidate = png(1);
        std::fs::write(temp.path().join("notes.png"), "not a png").unwrap();
        for path in [
            ".github/workflows/ci.yml",
            ".git/hooks/pre-commit.png",
            "src/.hidden/a.png",
            "README.md",
            "notes.png",
        ] {
            let mut case = failed_case(
                &latest,
                "g",
                "gleon_flutter",
                CaseOutcome::Mismatch,
                &candidate,
            );
            case.golden.path = path.to_owned();
            write_case(&latest, &case);
            let err = approve_workspace(&ctx, &[], &[], None).unwrap_err();
            assert!(
                matches!(err, ApproveError::UnsafeGoldenPath { .. }),
                "{path}: {err}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(temp.path().join("notes.png")).unwrap(),
            "not a png"
        );

        let mut blob = failed_case(&latest, "g", "gleon_web", CaseOutcome::Mismatch, &candidate);
        blob.golden.blob = Some(ImageHash::new("sha256", "1".repeat(64)).unwrap());
        write_case(&latest, &blob);
        assert!(matches!(
            approve_workspace(&ctx, &[], &[], None),
            Err(ApproveError::UnsupportedBaseline { .. })
        ));
    }

    #[test]
    fn test_approve_rejects_candidates_that_do_not_match_their_report() {
        let (_temp, ctx) = workspace();
        let latest = runs_latest(&ctx);
        failed_case(&latest, "a", CLI_TOOL, CaseOutcome::Mismatch, &png(1));
        std::fs::write(latest.join("artifacts/a/candidate.png"), png(9)).unwrap();
        assert!(matches!(
            approve_workspace(&ctx, &[], &[], None),
            Err(ApproveError::CandidateChanged { ref name, .. }) if name == "a"
        ));

        failed_case(&latest, "a", CLI_TOOL, CaseOutcome::Mismatch, b"not a png");
        assert!(matches!(
            approve_workspace(&ctx, &[], &[], None),
            Err(ApproveError::ImageDecode { .. })
        ));
        failed_case(
            &latest,
            "a",
            "gleon_flutter",
            CaseOutcome::Mismatch,
            b"not a png",
        );
        assert!(matches!(
            approve_workspace(&ctx, &[], &[], None),
            Err(ApproveError::ImageDecode { .. })
        ));

        let mut case = report("b", CaseOutcome::Missing);
        case.run_id = Some(RunId::new("run-1").unwrap());
        case.artifacts.as_mut().unwrap().candidate = Some("goldens/b.png".to_owned());
        write_case(&latest, &case);
        assert!(matches!(
            approve_workspace(&ctx, &[PathBuf::from("b")], &[], None),
            Err(ApproveError::CandidateOutsideRuns { .. })
        ));
    }

    #[test]
    fn test_approve_rejects_file_exceeding_max_size() {
        let (_temp, ctx) = workspace();
        let latest = runs_latest(&ctx);
        failed_case(&latest, "big", CLI_TOOL, CaseOutcome::Mismatch, &png(1));
        let file = std::fs::File::create(latest.join("artifacts/big/candidate.png")).unwrap();
        file.set_len(65 * 1024 * 1024).unwrap();
        assert!(matches!(
            approve_workspace(&ctx, &[], &[], None),
            Err(ApproveError::ImageTooLarge { .. })
        ));
    }

    #[test]
    fn test_approve_removes_redundant_override_when_matching_fallback() {
        let (temp, mut ctx) = workspace();
        let gleon_dir = temp.path().join(".gleon");
        let macos_key = "macos-aarch64";
        ctx.fallback_platform_key = Some(macos_key.to_string());

        let candidate = png(5);
        let sha = hex::encode(Sha256::digest(&candidate));
        failed_case(
            &runs_latest(&ctx),
            "test1",
            CLI_TOOL,
            CaseOutcome::Mismatch,
            &candidate,
        );
        let dhash = ImageHash::new("dhash", "0000000000000000").unwrap();
        SingleTestManifest::new(
            ImageHash::new("sha256", &sha).unwrap(),
            dhash.clone(),
            10,
            10,
        )
        .unwrap()
        .save(
            gleon_dir
                .join("manifests")
                .join(macos_key)
                .join("test1.json"),
        )
        .unwrap();
        let linux_override = gleon_dir.join("manifests").join(LINUX).join("test1.json");
        SingleTestManifest::new(
            ImageHash::new("sha256", "9".repeat(64)).unwrap(),
            dhash,
            10,
            10,
        )
        .unwrap()
        .save(&linux_override)
        .unwrap();

        let res = approve_workspace(&ctx, &[], &[], None).unwrap();
        assert_eq!(res.approved_test_cases, ["test1"]);
        assert!(
            !linux_override.exists(),
            "the fallback already has this baseline"
        );
        assert!(gleon_dir.join("blobs/sha256").join(&sha).exists());
    }
}
