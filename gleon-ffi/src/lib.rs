//! C ABI over [`gleon_engine`] for test framework integrations: the gleon Flutter package loads
//! it through Dart FFI today, and nothing here is Flutter-specific (the integration names itself
//! and its failure artifacts when it creates a session).
//!
//! The caller passes facts (paths, the candidate as PNG or raw pixels, the call's tolerance, masks
//! and text regions) and gets back a verdict plus the texts to show; everything else happens
//! here: finding the workspace,
//! resolving the `.gleon/gleon.yaml` rule, reading and comparing the golden, writing failure
//! artifacts, case reports and (in update mode) the golden itself.
//!
//! Exports go through [`safer_ffi`]: opaque handles, the [`GleonSummary`] written to the caller's
//! buffer and its byte slices use its FFI-safe types, so the only raw pointers left are the inputs. Buffers stay raw
//! `(ptr, len)` pairs on purpose: a caller can pass its own buffers without copying them (Dart's
//! `Uint8List.address` and `Uint32List.address` in leaf calls), which is impossible for pointers
//! inside a struct. The scalars of a call travel in one [`GleonCall`] struct and its strings as
//! one UTF-8 buffer plus their byte lengths in a fixed order, so adding either never adds
//! parameters.
//!
//! Contract:
//! - Input buffers are borrowed only for the duration of a call and never retained. Strings are
//!   UTF-8; an empty string stands for an absent optional one. Flags and codes are integers, never
//!   C `bool`s or enums, so no bit pattern a caller can pass is undefined behavior.
//! - Calls never return null and never unwind: invalid input and panics become an error result.
//! - Sessions may be shared by threads.
//! - The caller owns returned sessions and the `texts` of a [`GleonSummary`] and releases them
//!   with [`gleon_session_free`] and [`gleon_texts_free`]; the summary's slices stay valid until
//!   its `texts` are freed. A summary without texts (a pass, as a rule) has null `texts`, so a
//!   passing comparison is one call.
//! - [`gleon_golden`] writes its summary to a buffer of the caller instead of returning it: in
//!   Dart, leaf calls returning a struct by value with `.address` arguments return null without
//!   calling ([dart-lang/sdk#64368](https://github.com/dart-lang/sdk/issues/64368)).

// Only this file touches raw pointers; every other module forbids `unsafe`.
#![expect(
    unsafe_code,
    reason = "C ABI boundary: raw input buffers handed over by the caller without copying"
)]

mod compare;
mod error;
mod golden;
mod session;
mod text;

use std::{
    any::Any,
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
};

use error::{ErrorKind, Failure};
use gleon_engine::{
    Region,
    config::{Dimension, Zone},
};
use gleon_model::{
    case::RUN_ID_ENV,
    compare::{Candidate, is_rgba_len},
    config::{ARTIFACTS_ENV, METRICS_ENV},
    tolerance::{TextTolerance, Tolerance},
};
use golden::{Finished, Mode, Request};
pub use handles::{GleonCall, GleonSession, GleonSummary, GleonTexts};
use safer_ffi::prelude::*;
use session::{ArtifactNames, Integration, Session, SessionOptions};

/// Version of the C contract. Bumped on any breaking change so the caller can refuse a
/// mismatched native library instead of misreading it.
pub const ABI_VERSION: u32 = 12;

/// Session flag: goldens belong to workspaces.
///
/// Each golden belongs to the nearest directory above it with `.gleon/gleon.yaml`. Without the
/// flag every golden compares under the call's tolerance, else exactly, and nothing is recorded.
pub const SESSION_FIND_WORKSPACE: u32 = 1;

/// Session flag: take the environment from the session strings, not from the process.
///
/// `metrics_env`, `artifacts_env` and `run_id_env` become the values of `GLEON_METRICS`,
/// `GLEON_ARTIFACTS_DIR` and `GLEON_RUN_ID` (empty for unset), so tests of an integration never
/// depend on the shell they run in.
pub const SESSION_ENV: u32 = 2;

/// The strings of [`gleon_session_new`], in order.
const SESSION_STRINGS: [&str; 9] = [
    "metrics_env",
    "artifacts_env",
    "run_id_env",
    "tool",
    "tool_version",
    "renderer",
    "golden_artifact",
    "candidate_artifact",
    "diff_artifact",
];

/// The strings of [`gleon_golden`], in order.
const GOLDEN_STRINGS: [&str; 4] = ["golden_path", "golden_uri", "failures_dir", "test_name"];

#[expect(
    clippy::expl_impl_clone_on_copy,
    clippy::useless_let_if_seq,
    reason = "safer-ffi generates these implementations for its types"
)]
mod handles {
    use safer_ffi::prelude::*;

    use crate::{golden::Finished, session::Session};

    /// The scalars of one [`gleon_golden`](crate::gleon_golden) call. Read unaligned, so the
    /// caller may pass any buffer of its 48 bytes (Dart: `Struct.create` and `.address`).
    #[derive_ReprC]
    #[repr(C)]
    pub struct GleonCall {
        /// Pixel tolerance (`tolerance_kind` 2): the largest share of differing pixels.
        pub max_diff_ratio: f64,
        /// SSIM tolerance (`tolerance_kind` 3): the lowest local similarity.
        pub min_similarity: f64,
        /// SSIM tolerance (`tolerance_kind` 3): the color deviation beyond the envelope.
        pub color_tolerance: f64,
        /// The largest share of differing pixels in any tile of text, `[0, 1]`; NaN: the rule's.
        pub text_tolerance: f64,
        /// Width of a raw candidate (`candidate_format` 1).
        pub candidate_width: u32,
        /// Height of a raw candidate (`candidate_format` 1).
        pub candidate_height: u32,
        /// `0` compare, `1` update (write the golden; PNG only).
        pub mode: u8,
        /// `0` PNG bytes, `1` raw straight RGBA8 pixels.
        pub candidate_format: u8,
        /// `0` the `.gleon/gleon.yaml` rule, `1` exact, `2` pixel, `3` SSIM.
        pub tolerance_kind: u8,
        /// Pixel tolerance (`tolerance_kind` 2), outside text: a differing pixel counts as equal
        /// when no RGBA byte differs by more than this, `[0, 254]` (0: off).
        pub channel_tolerance: u8,
        /// Pixel tolerance (`tolerance_kind` 2), outside text: `1` lets anti-aliased pixels
        /// pass, `0` not.
        pub anti_alias: u8,
        /// Pixel tolerance (`tolerance_kind` 2), outside text: differing pixels on the golden's
        /// edges (Sobel gradient above this) pass, `[0, 254]` (0: off).
        pub edge_threshold: u8,
    }

    /// The verdict of a call and the texts to show, owned by `texts`; written unaligned to the
    /// caller's buffer of its 64 bytes.
    #[derive_ReprC]
    #[repr(C)]
    pub struct GleonSummary {
        /// `0` error, `1` identical, `2` match, `3` mismatch, `4` dimension mismatch,
        /// `5` updated, `6` missing golden. Zero is an error, so a summary never written (a call
        /// that did not happen) is never a pass.
        pub verdict: u8,
        /// For an error: `1` invalid input, `2` config, `3` I/O, `4` image, `5` internal;
        /// `0` otherwise.
        pub error_kind: u8,
        /// The test failure message (UTF-8); empty for a pass.
        pub message: c_slice::Ref<'static, u8>,
        /// The console line to print; usually empty.
        pub console: c_slice::Ref<'static, u8>,
        /// Warnings to print, one per line; usually empty.
        pub warning: c_slice::Ref<'static, u8>,
        /// Owns the three texts until [`gleon_texts_free`](crate::gleon_texts_free); null when
        /// they are all empty.
        pub texts: Option<repr_c::Box<GleonTexts>>,
    }

    // The layout the integrations declare (Dart `Struct`s in the Flutter package): a change here
    // is a change of the C contract and needs an `ABI_VERSION` bump.
    const _: () = {
        use std::mem::{offset_of, size_of};
        assert!(size_of::<GleonCall>() == 48);
        assert!(offset_of!(GleonCall, max_diff_ratio) == 0);
        assert!(offset_of!(GleonCall, min_similarity) == 8);
        assert!(offset_of!(GleonCall, color_tolerance) == 16);
        assert!(offset_of!(GleonCall, text_tolerance) == 24);
        assert!(offset_of!(GleonCall, candidate_width) == 32);
        assert!(offset_of!(GleonCall, candidate_height) == 36);
        assert!(offset_of!(GleonCall, mode) == 40);
        assert!(offset_of!(GleonCall, candidate_format) == 41);
        assert!(offset_of!(GleonCall, tolerance_kind) == 42);
        assert!(offset_of!(GleonCall, channel_tolerance) == 43);
        assert!(offset_of!(GleonCall, anti_alias) == 44);
        assert!(offset_of!(GleonCall, edge_threshold) == 45);
        assert!(size_of::<GleonSummary>() == 64);
        assert!(offset_of!(GleonSummary, verdict) == 0);
        assert!(offset_of!(GleonSummary, error_kind) == 1);
        assert!(offset_of!(GleonSummary, message) == 8);
        assert!(offset_of!(GleonSummary, console) == 24);
        assert!(offset_of!(GleonSummary, warning) == 40);
        assert!(offset_of!(GleonSummary, texts) == 56);
    };

    /// Opaque per-process state, owned by the caller until
    /// [`gleon_session_free`](crate::gleon_session_free).
    #[derive_ReprC]
    #[repr(opaque)]
    pub struct GleonSession(pub(crate) Session);

    /// Opaque texts of a call, owned by the caller until
    /// [`gleon_texts_free`](crate::gleon_texts_free).
    #[derive_ReprC]
    #[repr(opaque)]
    pub struct GleonTexts(pub(crate) Finished);
}

/// Returns the contract version implemented by this library.
#[ffi_export]
#[must_use]
// `allow`, not `expect`: safer-ffi copies attributes onto its non-const shim, where the lint
// never fires and an expectation would be unfulfilled.
#[allow(
    clippy::missing_const_for_fn,
    reason = "the safer-ffi shim calling this function cannot be const"
)]
pub fn gleon_ffi_abi_version() -> u32 {
    ABI_VERSION
}

/// Borrows `len` elements at `ptr` as a slice. With `len == 0` the pointer is never read, so it
/// may be anything (null, or an unaligned address of an empty buffer); otherwise it must be
/// non-null and aligned.
///
/// # Safety
/// If `len > 0`, `ptr` must point to `len` initialized elements that stay valid and unmodified
/// for the returned lifetime.
unsafe fn borrow<'a, T>(ptr: *const T, len: usize, name: &str) -> Result<&'a [T], String> {
    if len == 0 {
        return Ok(&[]);
    }
    if ptr.is_null() {
        return Err(format!("`{name}` is null but its length is {len}"));
    }
    // `from_raw_parts` requires the slice to span at most `isize::MAX` bytes.
    let fits = len
        .checked_mul(size_of::<T>())
        .is_some_and(|bytes| bytes <= isize::MAX.unsigned_abs());
    if !fits {
        return Err(format!("`{name}` is too long"));
    }
    if !ptr.is_aligned() {
        return Err(format!("`{name}` is not aligned"));
    }
    // SAFETY: non-null, aligned, at most `isize::MAX` bytes and, per this function's contract,
    // valid for `len` elements.
    Ok(unsafe { std::slice::from_raw_parts(ptr, len) })
}

/// Splits `bytes` into `N` UTF-8 strings of the byte `lengths`, named `names` in errors.
fn split<'a, const N: usize>(
    bytes: &'a [u8],
    lengths: &[u32],
    names: [&str; N],
) -> Result<[&'a str; N], String> {
    if lengths.len() != N {
        return Err(format!("expected {N} strings, got {}", lengths.len()));
    }
    let mut strings = [""; N];
    let mut rest = bytes;
    for ((string, &len), name) in strings.iter_mut().zip(lengths).zip(names) {
        let (head, tail) = usize::try_from(len)
            .ok()
            .and_then(|len| rest.split_at_checked(len))
            .ok_or_else(|| format!("`{name}` runs past the end of the strings"))?;
        *string = std::str::from_utf8(head).map_err(|e| format!("`{name}` is not UTF-8: {e}"))?;
        rest = tail;
    }
    if rest.is_empty() {
        Ok(strings)
    } else {
        Err(format!(
            "the lengths cover {} of {} bytes",
            bytes.len() - rest.len(),
            bytes.len()
        ))
    }
}

/// `None` for an empty string.
fn non_empty(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}

/// Runs `call` behind the ABI: a caught panic becomes `on_panic` of an internal failure.
fn caught<T>(call: impl FnOnce() -> T, on_panic: impl FnOnce(Failure) -> T) -> T {
    catch_unwind(AssertUnwindSafe(call)).unwrap_or_else(|payload| {
        on_panic(Failure::new(ErrorKind::Internal, panic_message(&*payload)))
    })
}

/// The message of a caught panic, with its text when it has one.
fn panic_message(payload: &(dyn Any + Send)) -> String {
    let detail = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("no message");
    format!("gleon: internal error, the native call panicked: {detail}")
}

/// Runs `call` behind the ABI into a summary; see [`caught`].
fn guarded(call: impl FnOnce() -> Finished) -> GleonSummary {
    summary(caught(call, Finished::failed))
}

/// The summary of `finished`: its texts move to the heap, owned by the summary's `texts`, unless
/// they are all empty (no allocation, nothing for the caller to free).
fn summary(finished: Finished) -> GleonSummary {
    let (verdict, error_kind) = (finished.verdict as u8, finished.error_kind as u8);
    if finished.message.is_empty() && finished.console.is_empty() && finished.warning.is_empty() {
        return GleonSummary {
            verdict,
            error_kind,
            message: c_slice::Ref::default(),
            console: c_slice::Ref::default(),
            warning: c_slice::Ref::default(),
            texts: None,
        };
    }
    let texts: repr_c::Box<GleonTexts> = Box::new(GleonTexts(finished)).into();
    let GleonTexts(finished) = &*texts;
    // SAFETY: the strings live on the heap of the boxed result, which the summary carries as
    // `texts`: moving the box moves no byte of them, and the caller reads the slices only before
    // it frees `texts` (the contract of `GleonSummary`).
    let text = |text: &str| -> c_slice::Ref<'static, u8> {
        unsafe { std::slice::from_raw_parts(text.as_ptr(), text.len()) }.into()
    };
    GleonSummary {
        verdict,
        error_kind,
        message: text(&finished.message),
        console: text(&finished.console),
        warning: text(&finished.warning),
        texts: Some(texts),
    }
}

/// An error result for a broken C contract.
fn invalid_input(message: &str) -> Finished {
    Finished::failed(Failure::invalid_input(format!("gleon: {message}")))
}

/// Creates the session of a test process.
///
/// `flags` combines [`SESSION_FIND_WORKSPACE`] and [`SESSION_ENV`]. The strings are
/// `lengths_count` (9) UTF-8 strings packed into `strings`, `lengths` giving their byte
/// lengths, in this order:
/// 1. to 3. `metrics_env`, `artifacts_env`, `run_id_env`: with [`SESSION_ENV`] the values of
///    `GLEON_METRICS` (metrics on or off), `GLEON_ARTIFACTS_DIR` (the artifacts directory of
///    every workspace) and `GLEON_RUN_ID` (the run in case reports), empty for unset; without
///    it they are ignored and the process environment is read;
/// 4. `tool`, 5. `tool_version`: the integration in case reports (`gleon_flutter`, `0.1.0`);
/// 6. `renderer`: e.g. `flutter-3.47.5`, may be empty;
/// 7. to 9. the file name patterns of the failure artifacts of the golden, the candidate and the
///    diff; each contains `{name}` (the golden's file name without extension), e.g.
///    `{name}_masterImage.png`.
///
/// Invalid input, or an invalid value of one of the variables, yields a session whose every call
/// fails with the reason.
///
/// # Safety
/// `(strings, strings_len)` must describe `strings_len` readable bytes and `(lengths,
/// lengths_count)` `lengths_count` aligned `u32`s (or be `(null, 0)`), valid for the duration of
/// this call.
#[ffi_export]
#[must_use]
pub unsafe fn gleon_session_new(
    flags: u32,
    strings: *const u8,
    strings_len: usize,
    lengths: *const u32,
    lengths_count: usize,
) -> repr_c::Box<GleonSession> {
    let session = caught(
        || {
            // SAFETY: forwarded caller contract; the strings are copied before returning.
            let strings = unsafe { borrow(strings, strings_len, "strings") };
            // SAFETY: as above.
            let lengths = unsafe { borrow(lengths, lengths_count, "lengths") };
            strings
                .and_then(|strings| session_options(flags, strings, lengths?))
                .map_or_else(
                    |message| Session::failed(Failure::invalid_input(format!("gleon: {message}"))),
                    Session::new,
                )
        },
        Session::failed,
    );
    Box::new(GleonSession(session)).into()
}

/// The options of [`gleon_session_new`].
fn session_options(flags: u32, strings: &[u8], lengths: &[u32]) -> Result<SessionOptions, String> {
    let unknown = flags & !(SESSION_FIND_WORKSPACE | SESSION_ENV);
    if unknown != 0 {
        return Err(format!("unknown session flags {unknown:#x}"));
    }
    let [
        metrics_env,
        artifacts_env,
        run_id_env,
        tool,
        tool_version,
        renderer,
        golden,
        candidate,
        diff,
    ] = split(strings, lengths, SESSION_STRINGS)?;
    let artifacts = ArtifactNames {
        golden: golden.to_owned(),
        candidate: candidate.to_owned(),
        diff: diff.to_owned(),
    };
    artifacts.validate()?;
    let env = |name: &str, given: &str| {
        if flags & SESSION_ENV == 0 {
            std::env::var_os(name).map(|value| value.to_string_lossy().into_owned())
        } else {
            non_empty(given).map(str::to_owned)
        }
    };
    Ok(SessionOptions {
        finds_workspaces: flags & SESSION_FIND_WORKSPACE != 0,
        metrics_env: env(METRICS_ENV, metrics_env),
        artifacts_env: env(ARTIFACTS_ENV, artifacts_env),
        run_id_env: env(RUN_ID_ENV, run_id_env),
        integration: Integration {
            tool: tool.to_owned(),
            tool_version: tool_version.to_owned(),
            renderer: non_empty(renderer).map(str::to_owned),
            artifacts,
        },
    })
}

/// Releases a session. Passing null is a no-op.
#[ffi_export]
pub fn gleon_session_free(session: Option<repr_c::Box<GleonSession>>) {
    drop(session);
}

/// The call's tolerance: `tolerance_kind` `0` uses the `.gleon/gleon.yaml` rule, `1` exact, `2`
/// pixel (`max_diff_ratio`), `3` SSIM (`min_similarity`, `color_tolerance`).
fn call_tolerance(call: &GleonCall) -> Result<Option<Tolerance>, String> {
    let tolerance = match call.tolerance_kind {
        0 => return Ok(None),
        1 => Tolerance::Exact {},
        2 => Tolerance::Pixel {
            max_diff_ratio: call.max_diff_ratio,
            channel_tolerance: call.channel_tolerance,
            anti_alias: match call.anti_alias {
                0 => false,
                1 => true,
                other => return Err(format!("`anti_alias` must be 0 or 1 (got {other})")),
            },
            edge_threshold: call.edge_threshold,
        },
        3 => Tolerance::Ssim {
            min_similarity: call.min_similarity,
            color_tolerance: call.color_tolerance,
        },
        other => return Err(format!("unknown tolerance kind {other}")),
    }
    .without_negative_zero();
    tolerance
        .validate()
        .map(|()| Some(tolerance))
        .map_err(|e| format!("invalid tolerance: {e}"))
}

/// The call's tolerance of text; NaN uses the `.gleon/gleon.yaml` rule's.
fn call_text_tolerance(share: f64) -> Result<Option<TextTolerance>, String> {
    if share.is_nan() {
        return Ok(None);
    }
    let text = TextTolerance(share).without_negative_zero();
    text.validate()
        .map(|()| Some(text))
        .map_err(|e| format!("invalid text tolerance: {e}"))
}

/// The candidate: `candidate_format` `0` PNG bytes, `1` raw straight RGBA8 of
/// `candidate_width` x `candidate_height`.
fn call_candidate<'a>(call: &GleonCall, bytes: &'a [u8]) -> Result<Candidate<'a>, String> {
    let (width, height) = (call.candidate_width, call.candidate_height);
    match call.candidate_format {
        0 => Ok(Candidate::Png(bytes)),
        1 => {
            if !is_rgba_len(width, height, bytes.len()) {
                return Err(format!(
                    "`candidate` has {} bytes, {width}x{height} RGBA needs {}",
                    bytes.len(),
                    u64::from(width) * u64::from(height) * 4
                ));
            }
            Ok(Candidate::Rgba {
                width,
                height,
                pixels: bytes,
            })
        }
        other => Err(format!("unknown candidate format {other}")),
    }
}

/// Text regions from `[x, y, width, height]` quadruples.
fn call_regions(flat: &[u32]) -> Vec<Region> {
    quadruples(flat)
        .map(|[x, y, width, height]| Region {
            x,
            y,
            width,
            height,
        })
        .collect()
}

/// Pixel masks from `[x, y, width, height]` quadruples.
fn call_masks(flat: &[u32]) -> Vec<Zone> {
    quadruples(flat)
        .map(|[x, y, width, height]| Zone {
            x,
            y,
            width: Dimension::Pixels(width),
            height: Dimension::Pixels(height),
        })
        .collect()
}

/// The `[x, y, width, height]` quadruples of `flat` (a trailing partial one is ignored).
fn quadruples(flat: &[u32]) -> impl Iterator<Item = [u32; 4]> {
    flat.as_chunks::<4>().0.iter().copied()
}

/// Compares `candidate` against the golden file (`call.mode` 0), or writes it there (`mode` 1,
/// update mode, PNG only), and writes the verdict with the texts to show to `summary`.
///
/// `golden_path` is the shared golden. When the `.gleon/gleon.yaml` of its workspace names the
/// `fallback_platform` the shared goldens were recorded on and this process runs on another,
/// its own golden `<dir>/<os>-<arch>/<file>` is written and compared instead, and the shared one
/// only while the own one does not exist.
///
/// The strings are `lengths_count` (4) UTF-8 strings packed into `strings`, `lengths` giving
/// their byte lengths, in this order: `golden_path` (the file), `golden_uri` (the key shown in
/// messages), `failures_dir` (the directory for failure artifacts, shown verbatim), `test_name`
/// (the running test, may be empty).
///
/// `call` holds the scalars ([`GleonCall`]): `candidate_format` `0` passes PNG bytes, `1` the raw
/// straight (not premultiplied) RGBA8 pixels of a `candidate_width` x `candidate_height` capture
/// (exactly `4 * width * height` bytes), which spares the integration encoding a PNG on every
/// passing comparison; `tolerance_kind` `0` uses the `.gleon/gleon.yaml` rule, `1` exact, `2`
/// pixel (`max_diff_ratio` and the options `channel_tolerance`, `anti_alias`, `edge_threshold`),
/// `3` SSIM (`min_similarity`, `color_tolerance`).
///
/// `mask_count` pixel masks `[x, y, width, height]` are at `masks`. `text_region_count` text
/// regions `[x, y, width, height]` (candidate pixels) are at `text_regions`, compared under
/// every tolerance by `call.text_tolerance`: the largest share of differing pixels in any tile
/// of text, `[0, 1]` (NaN: the rule's `text_tolerance`, else 0.05 against a golden of this
/// platform and 1, so text never fails, against any other); in SSIM mode the text is left out of
/// both gates, and no pixel option applies to it.
///
/// # Safety
/// `call` must point to a readable [`GleonCall`] (any alignment) or be null, `summary` to a
/// writable buffer of a [`GleonSummary`] (any alignment; null drops the summary). The buffer is
/// overwritten without being read: free the `texts` of a summary it held before. Each `(ptr, len)`
/// pair must describe a readable buffer of `len` elements (bytes for `strings` and `candidate`,
/// aligned `u32`s for `lengths`, `4 * mask_count` aligned `u32`s for `masks`,
/// `4 * text_region_count` for `text_regions`), or be `(null, 0)`. All stay valid for the
/// duration of this call.
#[ffi_export]
pub unsafe fn gleon_golden(
    session: Option<&GleonSession>,
    call: *const GleonCall,
    summary: *mut GleonSummary,
    strings: *const u8,
    strings_len: usize,
    lengths: *const u32,
    lengths_count: usize,
    candidate: *const u8,
    candidate_len: usize,
    masks: *const u32,
    mask_count: usize,
    text_regions: *const u32,
    text_region_count: usize,
) {
    let result = guarded(|| {
        let Some(GleonSession(session)) = session else {
            return invalid_input("no session");
        };
        if call.is_null() {
            return invalid_input("`call` is null");
        }
        // SAFETY: non-null and, per the caller contract, a readable `GleonCall`; read unaligned,
        // so its alignment does not matter.
        let call = unsafe { call.read_unaligned() };
        let request = || -> Result<Request<'_>, String> {
            let mode = match call.mode {
                0 => Mode::Compare,
                1 => Mode::Update,
                other => return Err(format!("unknown mode {other}")),
            };
            // SAFETY: forwarded caller contract; nothing borrowed outlives this call.
            let strings = unsafe { borrow(strings, strings_len, "strings") }?;
            // SAFETY: as above.
            let lengths = unsafe { borrow(lengths, lengths_count, "lengths") }?;
            // SAFETY: as above.
            let candidate = unsafe { borrow(candidate, candidate_len, "candidate") }?;
            let flat_len = mask_count.checked_mul(4).ok_or("`masks` is too long")?;
            // SAFETY: as above.
            let masks = unsafe { borrow(masks, flat_len, "masks") }?;
            let text_len = text_region_count
                .checked_mul(4)
                .ok_or("`text_regions` is too long")?;
            // SAFETY: as above.
            let text_regions = unsafe { borrow(text_regions, text_len, "text_regions") }?;
            let candidate = call_candidate(&call, candidate)?;
            let [golden_path, golden_uri, failures_dir, test_name] =
                split(strings, lengths, GOLDEN_STRINGS)?;
            Ok(Request {
                mode,
                golden_path: Path::new(golden_path),
                golden_uri,
                failures_dir,
                test_name: non_empty(test_name),
                candidate,
                tolerance: call_tolerance(&call)?,
                masks: call_masks(masks),
                text_regions: call_regions(text_regions),
                text: call_text_tolerance(call.text_tolerance)?,
            })
        };
        match request() {
            Ok(request) => golden::run(session, &request),
            Err(message) => invalid_input(&message),
        }
    });
    if !summary.is_null() {
        // SAFETY: non-null and, per the caller contract, a writable buffer of a `GleonSummary`;
        // written unaligned, so its alignment does not matter. A null `summary` drops `result`,
        // which frees its texts.
        unsafe { summary.write_unaligned(result) };
    }
}

/// Releases the `texts` of a [`GleonSummary`]. Passing null is a no-op.
///
/// # Safety
/// `texts` must come from a summary of [`gleon_golden`] and be freed once; no slice of that
/// summary may be read afterwards (they borrow from `texts`).
#[ffi_export]
pub unsafe fn gleon_texts_free(texts: Option<repr_c::Box<GleonTexts>>) {
    drop(texts);
}

// Runs under Miri too: these tests exercise the unsafe pointer boundary; the ones touching the
// file system or the engine are ignored there.
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
    use gleon_model::platform::PlatformKey;

    use super::*;

    const FLUTTER_ARTIFACTS: [&str; 3] = [
        "{name}_masterImage.png",
        "{name}_testImage.png",
        "{name}_gleonDiff.png",
    ];

    fn text(slice: c_slice::Ref<'_, u8>) -> String {
        String::from_utf8(slice.as_slice().to_vec()).unwrap()
    }

    /// `strings` packed as the ABI expects: the bytes and their lengths.
    fn packed(strings: &[&str]) -> (Vec<u8>, Vec<u32>) {
        let lengths = strings
            .iter()
            .map(|s| u32::try_from(s.len()).unwrap())
            .collect();
        (strings.concat().into_bytes(), lengths)
    }

    /// A session of the Flutter integration; with [`SESSION_ENV`], `env` gives the values of
    /// `GLEON_METRICS`, `GLEON_ARTIFACTS_DIR` and `GLEON_RUN_ID`.
    fn new_session(flags: u32, env: [&str; 3]) -> repr_c::Box<GleonSession> {
        let [metrics, artifacts, run_id] = env;
        let [golden, candidate, diff] = FLUTTER_ARTIFACTS;
        session_with(
            flags,
            &[
                metrics,
                artifacts,
                run_id,
                "gleon_flutter",
                "0.1.0",
                "",
                golden,
                candidate,
                diff,
            ],
        )
    }

    const UNSET: [&str; 3] = ["", "", ""];

    fn session_with(flags: u32, strings: &[&str]) -> repr_c::Box<GleonSession> {
        let (bytes, lengths) = packed(strings);
        unsafe {
            gleon_session_new(
                flags,
                bytes.as_ptr(),
                bytes.len(),
                lengths.as_ptr(),
                lengths.len(),
            )
        }
    }

    /// The summary of a call, copied out before its texts are freed.
    struct Answer {
        verdict: u8,
        error_kind: u8,
        message: String,
        console: String,
        warning: String,
        /// Whether the summary owned texts to free.
        had_texts: bool,
    }

    fn answer(summary: GleonSummary) -> Answer {
        let answer = Answer {
            verdict: summary.verdict,
            error_kind: summary.error_kind,
            message: text(summary.message),
            console: text(summary.console),
            warning: text(summary.warning),
            had_texts: summary.texts.is_some(),
        };
        unsafe { gleon_texts_free(summary.texts) };
        answer
    }

    /// The inputs of one `gleon_golden` call.
    struct Call<'a> {
        mode: u8,
        strings: [&'a str; 4],
        candidate: &'a [u8],
        /// Format, width and height of the candidate.
        format: (u8, u32, u32),
        tolerance: (u8, f64, f64, f64),
        masks: (*const u32, usize),
        text_regions: (*const u32, usize),
        /// The text tolerance (NaN: the rule's).
        text: f64,
    }

    impl Call<'_> {
        /// The scalars, in an unaligned buffer like any caller may pass.
        fn scalars(&self) -> Vec<u8> {
            let (tolerance_kind, max_diff_ratio, min_similarity, color_tolerance) = self.tolerance;
            let call = GleonCall {
                max_diff_ratio,
                min_similarity,
                color_tolerance,
                text_tolerance: self.text,
                candidate_width: self.format.1,
                candidate_height: self.format.2,
                mode: self.mode,
                candidate_format: self.format.0,
                tolerance_kind,
                channel_tolerance: 0,
                anti_alias: 0,
                edge_threshold: 0,
            };
            let mut buffer = vec![0u8; size_of::<GleonCall>() + 1];
            unsafe {
                buffer
                    .as_mut_ptr()
                    .add(1)
                    .cast::<GleonCall>()
                    .write_unaligned(call);
            }
            buffer
        }
    }

    impl Default for Call<'_> {
        fn default() -> Self {
            Self {
                mode: 0,
                strings: ["/definitely/missing/golden.png", "a.png", "", ""],
                candidate: &[],
                format: (0, 0, 0),
                tolerance: (0, 0.0, 0.0, 0.0),
                masks: (std::ptr::null(), 0),
                text_regions: (std::ptr::null(), 0),
                text: f64::NAN,
            }
        }
    }

    /// Runs `gleon_golden` into an unaligned buffer, like any caller may pass, and reads the
    /// summary back. The buffer starts as a valid summary without texts, so a call that wrote nothing
    /// reads as the error of verdict 0, never as undefined memory.
    unsafe fn summarized(golden: impl FnOnce(*mut GleonSummary)) -> GleonSummary {
        let mut buffer = vec![0u8; size_of::<GleonSummary>() + 1];
        let summary = unsafe { buffer.as_mut_ptr().add(1) }.cast::<GleonSummary>();
        unsafe { summary.write_unaligned(blank_summary()) };
        golden(summary);
        unsafe { summary.read_unaligned() }
    }

    /// A summary before any call: zeros, empty slices.
    fn blank_summary() -> GleonSummary {
        GleonSummary {
            verdict: 0,
            error_kind: 0,
            message: c_slice::Ref::default(),
            console: c_slice::Ref::default(),
            warning: c_slice::Ref::default(),
            texts: None,
        }
    }

    /// Zero is no pass: a summary the library never wrote is an error.
    #[test]
    fn test_an_unwritten_summary_is_an_error() {
        assert_eq!(golden::Verdict::Error as u8, blank_summary().verdict);
    }

    fn golden(session: Option<&GleonSession>, call: Call<'_>) -> Answer {
        let (bytes, lengths) = packed(&call.strings);
        let scalars = call.scalars();
        answer(unsafe {
            summarized(|summary| {
                gleon_golden(
                    session,
                    scalars.as_ptr().add(1).cast::<GleonCall>(),
                    summary,
                    bytes.as_ptr(),
                    bytes.len(),
                    lengths.as_ptr(),
                    lengths.len(),
                    call.candidate.as_ptr(),
                    call.candidate.len(),
                    call.masks.0,
                    call.masks.1,
                    call.text_regions.0,
                    call.text_regions.1,
                )
            })
        })
    }

    const INVALID_INPUT: u8 = ErrorKind::InvalidInput as u8;

    #[test]
    fn test_abi_version() {
        assert_eq!(gleon_ffi_abi_version(), ABI_VERSION);
    }

    /// Raw candidates must have the length of their size; a text tolerance is a share or NaN.
    #[test]
    fn test_raw_candidates_and_text_tolerances_are_checked() {
        let session = new_session(SESSION_ENV, UNSET);
        let session = Some(&*session);
        let pixels = [0u8; 4 * 4 * 3];
        for (call, needle) in [
            (
                Call {
                    candidate: &pixels,
                    format: (1, 4, 4),
                    ..Call::default()
                },
                "`candidate` has 48 bytes, 4x4 RGBA needs 64",
            ),
            (
                Call {
                    format: (2, 0, 0),
                    ..Call::default()
                },
                "unknown candidate format 2",
            ),
            (
                Call {
                    candidate: &pixels,
                    format: (1, 4, 3),
                    mode: 1,
                    ..Call::default()
                },
                "update mode takes the candidate as PNG",
            ),
            (
                Call {
                    text: 2.0,
                    ..Call::default()
                },
                "invalid text tolerance",
            ),
        ] {
            let answer = golden(session, call);
            assert_eq!(answer.error_kind, INVALID_INPUT, "{needle}");
            assert!(answer.message.contains(needle), "{}", answer.message);
        }
    }

    /// Text regions and a text tolerance cross the C contract; a valid call goes on to the
    /// golden (missing here).
    #[test]
    #[cfg_attr(miri, ignore = "touches the file system")]
    fn test_text_regions_cross_the_contract() {
        let session = new_session(SESSION_ENV, UNSET);
        let pixels = [0u8; 4 * 4 * 4];
        let regions = [0u32, 0, 4, 2, 1, 1, 2, 2];
        let answer = golden(
            Some(&*session),
            Call {
                candidate: &pixels,
                format: (1, 4, 4),
                text_regions: (regions.as_ptr(), 2),
                text: 0.1,
                ..Call::default()
            },
        );
        assert_eq!(
            answer.verdict,
            golden::Verdict::Missing as u8,
            "{}",
            answer.message
        );
        assert_eq!(answer.error_kind, ErrorKind::None as u8);
    }

    #[test]
    fn test_invalid_inputs_are_errors() {
        let session = new_session(SESSION_ENV, UNSET);
        let session = Some(&*session);
        for (call, needle) in [
            (
                Call {
                    masks: (std::ptr::null(), 1),
                    ..Call::default()
                },
                "`masks` is null but its length is 4",
            ),
            (
                Call {
                    masks: (std::ptr::null(), usize::MAX),
                    ..Call::default()
                },
                "`masks` is too long",
            ),
            (
                Call {
                    tolerance: (9, 0.0, 0.0, 0.0),
                    ..Call::default()
                },
                "unknown tolerance kind 9",
            ),
            (
                Call {
                    tolerance: (2, 1.5, 0.0, 0.0),
                    ..Call::default()
                },
                "invalid tolerance",
            ),
            (
                Call {
                    mode: 7,
                    ..Call::default()
                },
                "unknown mode 7",
            ),
        ] {
            let answer = golden(session, call);
            assert_eq!(answer.verdict, golden::Verdict::Error as u8);
            assert_eq!(answer.error_kind, INVALID_INPUT);
            assert!(answer.message.starts_with("gleon: "), "{}", answer.message);
            assert!(answer.message.contains(needle), "{}", answer.message);
        }
        let no_session = golden(None, Call::default());
        assert!(
            no_session.message.contains("no session"),
            "{}",
            no_session.message
        );
        let null_call = answer(unsafe {
            summarized(|summary| {
                gleon_golden(
                    session,
                    std::ptr::null(),
                    summary,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                )
            })
        });
        assert_eq!(null_call.error_kind, INVALID_INPUT);
        // Without a summary buffer the result is dropped (its texts freed), nothing written.
        unsafe {
            gleon_golden(
                session,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
            );
        }
        assert!(
            null_call.message.contains("`call` is null"),
            "{}",
            null_call.message
        );
    }

    #[test]
    fn test_packed_strings_are_checked() {
        let names = ["a", "b"];
        assert_eq!(split(b"xyz", &[1, 2], names), Ok(["x", "yz"]));
        assert_eq!(split(b"", &[0, 0], names), Ok(["", ""]));
        for (bytes, lengths, needle) in [
            (&b"xyz"[..], &[1, 2, 0][..], "expected 2 strings, got 3"),
            (b"xy", &[1, 2], "`b` runs past the end"),
            (b"xyz.", &[1, 2], "the lengths cover 3 of 4 bytes"),
            (b"x\xff", &[1, 1], "`b` is not UTF-8"),
        ] {
            assert!(
                split(bytes, lengths, names).unwrap_err().contains(needle),
                "{needle}"
            );
        }
    }

    #[test]
    fn test_call_tolerances() {
        let call = |tolerance_kind, max_diff_ratio, min_similarity, color_tolerance| GleonCall {
            max_diff_ratio,
            min_similarity,
            color_tolerance,
            text_tolerance: f64::NAN,
            candidate_width: 0,
            candidate_height: 0,
            mode: 0,
            candidate_format: 0,
            tolerance_kind,
            channel_tolerance: 0,
            anti_alias: 0,
            edge_threshold: 0,
        };
        let tolerance = |tolerance_kind, max_diff_ratio, min_similarity, color_tolerance| {
            call_tolerance(&call(
                tolerance_kind,
                max_diff_ratio,
                min_similarity,
                color_tolerance,
            ))
        };
        // The pixel options cross the contract; an anti-aliasing flag is 0 or 1.
        let options = GleonCall {
            channel_tolerance: 4,
            anti_alias: 1,
            edge_threshold: 64,
            ..call(2, 0.01, 9.0, 9.0)
        };
        assert_eq!(
            call_tolerance(&options),
            Ok(Some(Tolerance::Pixel {
                max_diff_ratio: 0.01,
                channel_tolerance: 4,
                anti_alias: true,
                edge_threshold: 64,
            }))
        );
        assert_eq!(
            call_tolerance(&GleonCall {
                anti_alias: 2,
                ..options
            }),
            Err("`anti_alias` must be 0 or 1 (got 2)".to_owned())
        );
        assert_eq!(tolerance(0, 9.0, 9.0, 9.0), Ok(None));
        assert_eq!(tolerance(1, 9.0, 9.0, 9.0), Ok(Some(Tolerance::Exact {})));
        assert_eq!(tolerance(2, 0.1, 9.0, 9.0), Ok(Some(Tolerance::pixel(0.1))));
        assert_eq!(
            tolerance(3, 9.0, 0.8, 8.0),
            Ok(Some(Tolerance::Ssim {
                min_similarity: 0.8,
                color_tolerance: 8.0
            }))
        );
        assert!(matches!(
            tolerance(2, -0.0, 0.0, 0.0),
            Ok(Some(Tolerance::Pixel { max_diff_ratio, .. })) if max_diff_ratio.is_sign_positive()
        ));
    }

    #[test]
    fn test_call_masks() {
        assert_eq!(
            call_masks(&[1, 2, 3, 4, 9]),
            [Zone {
                x: 1,
                y: 2,
                width: Dimension::Pixels(3),
                height: Dimension::Pixels(4)
            }]
        );
    }

    #[test]
    fn test_borrow_rules() {
        // One byte into a `u32` buffer is never aligned for `u32`.
        let buffer = [0u32; 2];
        let ptr = buffer.as_ptr().cast::<u8>().wrapping_add(1).cast::<u32>();
        assert!(
            unsafe { borrow(ptr, 1, "masks") }
                .unwrap_err()
                .contains("not aligned")
        );
        assert!(
            unsafe { borrow(buffer.as_ptr(), usize::MAX / 2, "masks") }
                .unwrap_err()
                .contains("too long"),
            "checked before the pointer is used"
        );
        assert_eq!(
            unsafe { borrow::<u8>(std::ptr::null(), 0, "empty") },
            Ok(&[][..])
        );
        assert_eq!(
            unsafe { borrow(ptr, 0, "empty") },
            Ok(&[][..]),
            "an empty buffer may be unaligned"
        );
    }

    #[test]
    fn test_invalid_session_inputs_fail_every_call() {
        let [golden_artifact, candidate, diff] = FLUTTER_ARTIFACTS;
        for (flags, strings, needle) in [
            (4, vec![""; 9], "unknown session flags 0x4"),
            (0, vec![""; 8], "expected 9 strings, got 8"),
            (
                0,
                vec!["", "", "", "", "", "", "master.png", candidate, diff],
                "must contain `{name}`",
            ),
            (
                SESSION_ENV,
                vec![
                    "maybe",
                    "",
                    "",
                    "",
                    "",
                    "",
                    golden_artifact,
                    candidate,
                    diff,
                ],
                "GLEON_METRICS must be 1, 0, true or false (got 'maybe')",
            ),
            (
                SESSION_ENV,
                vec!["", "out", "", "", "", "", golden_artifact, candidate, diff],
                "GLEON_ARTIFACTS_DIR: 'out' must be",
            ),
            (
                SESSION_ENV,
                vec![
                    "",
                    "",
                    "run 1",
                    "",
                    "",
                    "",
                    golden_artifact,
                    candidate,
                    diff,
                ],
                "GLEON_RUN_ID: a run id must be",
            ),
        ] {
            let session = session_with(flags, &strings);
            let answer = golden(
                Some(&session),
                Call {
                    mode: 1,
                    ..Call::default()
                },
            );
            assert_eq!(answer.verdict, golden::Verdict::Error as u8);
            assert!(answer.message.starts_with("gleon: "), "{}", answer.message);
            assert!(answer.message.contains(needle), "{}", answer.message);
            gleon_session_free(Some(session));
        }
        let (bytes, _) = packed(&[""; 9]);
        let session = unsafe { gleon_session_new(0, bytes.as_ptr(), 0, std::ptr::null(), 9) };
        let answer = golden(Some(&session), Call::default());
        assert_eq!(answer.error_kind, INVALID_INPUT);
        assert!(
            answer.message.contains("`lengths` is null"),
            "{}",
            answer.message
        );
        gleon_session_free(Some(session));
        gleon_session_free(None);
    }

    #[test]
    fn test_the_environment_is_read_without_the_flag() {
        let session = new_session(0, ["maybe", "out", "run 1"]);
        let is_set = [METRICS_ENV, ARTIFACTS_ENV, RUN_ID_ENV]
            .into_iter()
            .any(|name| std::env::var_os(name).is_some());
        assert!(
            session.0.failure().is_none() || is_set,
            "the given values are ignored"
        );
        gleon_session_free(Some(session));
    }

    #[test]
    fn test_a_panic_becomes_an_internal_error_with_its_message() {
        let answer = answer(guarded(|| panic!("boom")));
        assert_eq!(answer.verdict, golden::Verdict::Error as u8);
        assert_eq!(answer.error_kind, ErrorKind::Internal as u8);
        assert_eq!(
            answer.message,
            "gleon: internal error, the native call panicked: boom"
        );
        let session = caught(|| panic!("{}", 7), Session::failed);
        assert!(session.failure().unwrap().message.ends_with("panicked: 7"));
        assert!(panic_message(&42).ends_with("panicked: no message"));
    }

    /// A pass has no texts: nothing to free, one call per golden.
    #[test]
    fn test_summaries_own_their_texts_only_when_there_are_some() {
        let pass = answer(summary(Finished {
            verdict: golden::Verdict::Match,
            error_kind: ErrorKind::None,
            message: String::new(),
            console: String::new(),
            warning: String::new(),
        }));
        assert_eq!(pass.verdict, golden::Verdict::Match as u8);
        assert!(!pass.had_texts);
        let warned = answer(summary(Finished {
            verdict: golden::Verdict::Match,
            error_kind: ErrorKind::None,
            message: String::new(),
            console: String::new(),
            warning: "careful".to_owned(),
        }));
        assert!(warned.had_texts);
        assert_eq!(warned.warning, "careful");
        unsafe { gleon_texts_free(None) };
    }

    #[test]
    #[cfg_attr(miri, ignore = "touches the file system")]
    fn test_a_missing_golden_through_the_abi() {
        let session = new_session(SESSION_ENV, UNSET);
        let masks = [0u32; 4];
        let answer = golden(
            Some(&session),
            Call {
                strings: ["/definitely/missing/golden.png", "a.png", "", "t"],
                candidate: b"png",
                tolerance: (3, 0.0, 0.8, 8.0),
                masks: (masks.as_ptr(), 1),
                ..Call::default()
            },
        );
        assert_eq!(answer.verdict, golden::Verdict::Missing as u8);
        assert_eq!(answer.error_kind, 0);
        assert_eq!(
            answer.message,
            "Could not be compared against non-existent file: \"a.png\""
        );
        assert!(answer.console.is_empty() && answer.warning.is_empty());
        gleon_session_free(Some(session));
    }

    /// A whole workspace driven through the C ABI only: compare with the rule, a call tolerance
    /// and masks, update, and the case report of each.
    #[test]
    #[cfg_attr(miri, ignore = "touches the file system and runs the engine")]
    fn test_a_workspace_through_the_abi() {
        const COUNTER_0: &[u8] = include_bytes!("../tests/fixtures/counter_initial.png");
        const COUNTER_3: &[u8] = include_bytes!("../tests/fixtures/counter_three_taps.png");
        let dir = tempfile::tempdir().unwrap();
        // Without the `\\?\` prefix of Windows, like the session's own paths.
        let root = session::canonical(dir.path()).unwrap();
        std::fs::create_dir_all(root.join(".gleon")).unwrap();
        std::fs::write(
            root.join(".gleon/gleon.yaml"),
            "required_version: \">=0.1.0\"\nscreenshots:\n  - include: \"test/**/*.png\"\n    \
             mode: ssim\nmetrics:\n  enabled: true\n  console: false\n",
        )
        .unwrap();
        let golden_path = root.join("test/goldens/counter.png");
        std::fs::create_dir_all(golden_path.parent().unwrap()).unwrap();
        std::fs::write(&golden_path, COUNTER_0).unwrap();
        let session = new_session(SESSION_FIND_WORKSPACE | SESSION_ENV, ["", "", "ci-7"]);
        let path = golden_path.display().to_string();
        let failures = root.join("test/failures").display().to_string();
        let strings = [path.as_str(), "goldens/counter.png", failures.as_str(), "t"];
        let case = || -> serde_json::Value {
            let file = root
                .join(".gleon/runs/latest/cases")
                .join(PlatformKey::host())
                .join("test/goldens/counter.json");
            serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap()
        };

        let identical = golden(
            Some(&session),
            Call {
                strings,
                candidate: COUNTER_0,
                ..Call::default()
            },
        );
        assert_eq!(identical.verdict, golden::Verdict::Identical as u8);
        assert!(identical.console.is_empty(), "console: false");
        assert!(!identical.had_texts, "a pass is one call");
        assert_eq!(case()["outcome"], "identical");

        // The rule is SSIM with the default thresholds: the counter text changing fails it.
        let changed = golden(
            Some(&session),
            Call {
                strings,
                candidate: COUNTER_3,
                ..Call::default()
            },
        );
        assert_eq!(changed.verdict, golden::Verdict::Mismatch as u8);
        assert!(
            changed.message.contains("(gleon ssim ≥ "),
            "{}",
            changed.message
        );
        assert_eq!(case()["comparison"]["tolerance"]["kind"], "ssim");
        assert_eq!(std::fs::read_dir(&failures).unwrap().count(), 3);
        let diff = format!(
            ".gleon/runs/latest/artifacts/{}/test/goldens/counter/diff.png",
            PlatformKey::host()
        );
        assert_eq!(case()["artifacts"]["diff"], diff.as_str());
        assert!(root.join(&diff).is_file());

        // A whole-image mask through the ABI hides everything; the call's pixel tolerance wins.
        let masks = [0, 0, 360, 640, 350, 630, 20, 20];
        let masked = golden(
            Some(&session),
            Call {
                strings,
                candidate: COUNTER_3,
                tolerance: (2, 0.0, 0.0, 0.0),
                masks: (masks.as_ptr(), 2),
                ..Call::default()
            },
        );
        assert_eq!(
            masked.verdict,
            golden::Verdict::Match as u8,
            "{}",
            masked.message
        );
        assert!(
            masked.warning.contains("1 mask of golden"),
            "{}",
            masked.warning
        );
        assert_eq!(case()["comparison"]["masks"].as_array().unwrap().len(), 2);
        assert!(
            !root.join(diff).exists(),
            "a pass removes the images of the earlier failure"
        );

        let updated = golden(
            Some(&session),
            Call {
                mode: 1,
                strings,
                candidate: COUNTER_3,
                ..Call::default()
            },
        );
        assert_eq!(updated.verdict, golden::Verdict::Updated as u8);
        assert_eq!(std::fs::read(&golden_path).unwrap(), COUNTER_3);
        assert_eq!(case()["outcome"], "updated");
        assert_eq!(
            case()["schema_version"],
            gleon_model::case::CASE_SCHEMA_VERSION
        );
        assert_eq!(case()["run_id"], "ci-7");
        assert!(case().get("artifacts").is_none());
        gleon_session_free(Some(session));
    }
}
