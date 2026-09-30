//! C ABI over [`gleon_engine`] for test framework integrations: the gleon Flutter package loads
//! it through Dart FFI today, and nothing here is Flutter-specific (the integration names itself
//! and its failure artifacts when it creates a session).
//!
//! The caller passes facts (paths, the candidate PNG, the call's tolerance and masks) and gets
//! back a verdict plus the texts to show; everything else happens here: finding the workspace,
//! resolving the `.gleon/gleon.yaml` rule, reading and comparing the golden, writing failure
//! artifacts, case reports and (in update mode) the golden itself.
//!
//! Exports go through [`safer_ffi`]: opaque handles, the by-value [`GleonSummary`] and its byte
//! slices use its FFI-safe types, so the only raw pointers left are the input buffers. Those stay
//! raw `(ptr, len)` pairs on purpose: a caller can pass its own buffers without copying them
//! (Dart's `Uint8List.address` and `Uint32List.address` in leaf calls), which is impossible for
//! pointers inside a struct. The strings of a call travel as one UTF-8 buffer plus their byte
//! lengths in a fixed order, so adding a string never adds parameters.
//!
//! Contract:
//! - Input buffers are borrowed only for the duration of a call and never retained. Strings are
//!   UTF-8; an empty string stands for an absent optional one. Flags and codes are integers, never
//!   C `bool`s or enums, so no bit pattern a caller can pass is undefined behavior.
//! - Calls never return null and never unwind: invalid input and panics become an error result.
//! - Sessions may be shared by threads.
//! - The caller owns returned sessions and results and releases them with
//!   [`gleon_session_free`] and [`gleon_result_free`]; slices of a [`GleonSummary`] stay valid
//!   until its result is freed.

// Only this file touches raw pointers; every other module forbids `unsafe`.
#![expect(
    unsafe_code,
    reason = "C ABI boundary: raw input buffers handed over by the caller without copying"
)]

mod case;
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
use gleon_engine::config::{Dimension, Zone};
use gleon_model::{config::METRICS_ENV, tolerance::Tolerance};
use golden::{Finished, Mode, Request};
pub use handles::{GleonResult, GleonSession, GleonSummary};
use safer_ffi::prelude::*;
use session::{ArtifactNames, Integration, Session, SessionOptions};

/// Version of the C contract. Bumped on any breaking change so the caller can refuse a
/// mismatched native library instead of misreading it.
pub const ABI_VERSION: u32 = 6;

/// Session flag: goldens belong to workspaces, each golden to the nearest directory above it with
/// `.gleon/gleon.yaml` (without, every golden compares exactly and nothing is recorded).
pub const SESSION_FIND_WORKSPACE: u32 = 1;

/// Session flag: use the `metrics_env` string as the `GLEON_METRICS` value (empty for unset)
/// instead of reading the process environment. For tests of the integration.
pub const SESSION_METRICS_ENV: u32 = 2;

/// The strings of [`gleon_session_new`], in order.
const SESSION_STRINGS: [&str; 7] = [
    "metrics_env",
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

    /// The verdict of a call and the texts to show, borrowed from its [`GleonResult`].
    #[derive_ReprC]
    #[repr(C)]
    pub struct GleonSummary<'a> {
        /// `0` identical, `1` match, `2` mismatch, `3` dimension mismatch, `4` error,
        /// `5` updated, `6` missing golden.
        pub verdict: u8,
        /// For an error: `1` invalid input, `2` config, `3` I/O, `4` image, `5` internal;
        /// `0` otherwise.
        pub error_kind: u8,
        /// The test failure message (UTF-8); empty for a pass.
        pub message: c_slice::Ref<'a, u8>,
        /// The console line to print; usually empty.
        pub console: c_slice::Ref<'a, u8>,
        /// Warnings to print, one per line; usually empty.
        pub warning: c_slice::Ref<'a, u8>,
    }

    /// Opaque per-process state, owned by the caller until
    /// [`gleon_session_free`](crate::gleon_session_free).
    #[derive_ReprC]
    #[repr(opaque)]
    pub struct GleonSession(pub(crate) Session);

    /// Opaque call result, owned by the caller until [`gleon_result_free`](crate::gleon_result_free).
    #[derive_ReprC]
    #[repr(opaque)]
    pub struct GleonResult(pub(crate) Finished);
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
        Err(format!("{} bytes follow the last string", rest.len()))
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

/// Runs `call` behind the ABI into a result; see [`caught`].
fn guarded(call: impl FnOnce() -> Finished) -> repr_c::Box<GleonResult> {
    Box::new(GleonResult(caught(call, Finished::failed))).into()
}

/// An error result for a broken C contract.
fn invalid_input(message: &str) -> Finished {
    Finished::failed(Failure::invalid_input(format!("gleon: {message}")))
}

/// Creates the session of a test process.
///
/// `flags` combines [`SESSION_FIND_WORKSPACE`] and [`SESSION_METRICS_ENV`]. The strings are
/// `lengths_count` (7) UTF-8 strings packed into `strings`, `lengths` giving their byte
/// lengths, in this order:
/// 1. `metrics_env`: the `GLEON_METRICS` value with [`SESSION_METRICS_ENV`]; otherwise the
///    process environment is read;
/// 2. `tool`, 3. `tool_version`: the integration in case reports (`gleon_flutter`, `0.1.0`);
/// 4. `renderer`: e.g. `flutter-3.47.5`, may be empty;
/// 5. to 7. the file name patterns of the failure artifacts of the golden, the candidate and the
///    diff; each contains `{name}` (the golden's file name without extension), e.g.
///    `{name}_masterImage.png`.
///
/// Invalid input yields a session whose every call fails with the reason.
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
    let unknown = flags & !(SESSION_FIND_WORKSPACE | SESSION_METRICS_ENV);
    if unknown != 0 {
        return Err(format!("unknown session flags {unknown:#x}"));
    }
    let [
        metrics_env,
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
    let metrics_env = if flags & SESSION_METRICS_ENV == 0 {
        std::env::var_os(METRICS_ENV).map(|value| value.to_string_lossy().into_owned())
    } else {
        non_empty(metrics_env).map(str::to_owned)
    };
    Ok(SessionOptions {
        finds_workspaces: flags & SESSION_FIND_WORKSPACE != 0,
        metrics_env,
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

/// The call's tolerance: `kind` `0` uses the `.gleon/gleon.yaml` rule, `1` exact, `2` pixel
/// (`max_diff_ratio`), `3` SSIM (`min_similarity`, `color_tolerance`).
fn call_tolerance(
    kind: u8,
    max_diff_ratio: f64,
    min_similarity: f64,
    color_tolerance: f64,
) -> Result<Option<Tolerance>, String> {
    let tolerance = match kind {
        0 => return Ok(None),
        1 => Tolerance::Exact {},
        2 => Tolerance::Pixel { max_diff_ratio },
        3 => Tolerance::Ssim {
            min_similarity,
            color_tolerance,
        },
        other => return Err(format!("unknown tolerance kind {other}")),
    };
    tolerance
        .validate()
        .map(|()| Some(tolerance))
        .map_err(|e| format!("invalid tolerance: {e}"))
}

/// Pixel masks from `[x, y, width, height]` quadruples.
fn call_masks(flat: &[u32]) -> Vec<Zone> {
    flat.as_chunks::<4>()
        .0
        .iter()
        .map(|&[x, y, width, height]| Zone {
            x,
            y,
            width: Dimension::Pixels(width),
            height: Dimension::Pixels(height),
        })
        .collect()
}

/// Compares the PNG `candidate` against the golden file (`mode` 0), or writes it there (`mode`
/// 1, update mode).
///
/// The strings are `lengths_count` (4) UTF-8 strings packed into `strings`, `lengths` giving
/// their byte lengths, in this order: `golden_path` (the file), `golden_uri` (the key shown in
/// messages), `failures_dir` (the directory for failure artifacts, shown verbatim), `test_name`
/// (the running test, may be empty).
///
/// The call's tolerance is described at [`call_tolerance`]; `mask_count` pixel masks
/// `[x, y, width, height]` are at `masks`.
///
/// # Safety
/// Each `(ptr, len)` pair must describe a readable buffer of `len` elements (bytes for
/// `strings` and `candidate`, aligned `u32`s for `lengths`, `4 * mask_count` aligned `u32`s for
/// `masks`), or be `(null, 0)`, that stays valid for the duration of this call.
#[ffi_export]
#[must_use]
pub unsafe fn gleon_golden(
    session: Option<&GleonSession>,
    mode: u8,
    strings: *const u8,
    strings_len: usize,
    lengths: *const u32,
    lengths_count: usize,
    candidate: *const u8,
    candidate_len: usize,
    tolerance_kind: u8,
    max_diff_ratio: f64,
    min_similarity: f64,
    color_tolerance: f64,
    masks: *const u32,
    mask_count: usize,
) -> repr_c::Box<GleonResult> {
    guarded(|| {
        let Some(GleonSession(session)) = session else {
            return invalid_input("no session");
        };
        let request = || -> Result<Request<'_>, String> {
            let mode = match mode {
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
            let [golden_path, golden_uri, failures_dir, test_name] =
                split(strings, lengths, GOLDEN_STRINGS)?;
            Ok(Request {
                mode,
                golden_path: Path::new(golden_path),
                golden_uri,
                failures_dir,
                test_name: non_empty(test_name),
                candidate,
                tolerance: call_tolerance(
                    tolerance_kind,
                    max_diff_ratio,
                    min_similarity,
                    color_tolerance,
                )?,
                masks: call_masks(masks),
            })
        };
        match request() {
            Ok(request) => golden::run(session, &request),
            Err(message) => invalid_input(&message),
        }
    })
}

/// Returns the verdict and texts of `result` (an error without texts if `result` is null).
#[ffi_export]
#[must_use]
pub fn gleon_result_summary(result: Option<&GleonResult>) -> GleonSummary<'_> {
    result.map_or_else(
        || GleonSummary {
            verdict: golden::Verdict::Error as u8,
            error_kind: ErrorKind::InvalidInput as u8,
            message: slice(""),
            console: slice(""),
            warning: slice(""),
        },
        |GleonResult(finished)| GleonSummary {
            verdict: finished.verdict as u8,
            error_kind: finished.error_kind as u8,
            message: slice(&finished.message),
            console: slice(&finished.console),
            warning: slice(&finished.warning),
        },
    )
}

fn slice(text: &str) -> c_slice::Ref<'_, u8> {
    text.as_bytes().into()
}

/// Releases a result returned by [`gleon_golden`]. Passing null is a no-op.
#[ffi_export]
pub fn gleon_result_free(result: Option<repr_c::Box<GleonResult>>) {
    drop(result);
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

    fn new_session(flags: u32, env: &str) -> repr_c::Box<GleonSession> {
        let [golden, candidate, diff] = FLUTTER_ARTIFACTS;
        session_with(
            flags,
            &[env, "gleon_flutter", "0.1.0", "", golden, candidate, diff],
        )
    }

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

    /// The summary of a call, copied out before the result is freed.
    struct Answer {
        verdict: u8,
        error_kind: u8,
        message: String,
        console: String,
        warning: String,
    }

    fn answer(result: repr_c::Box<GleonResult>) -> Answer {
        let summary = gleon_result_summary(Some(&result));
        let answer = Answer {
            verdict: summary.verdict,
            error_kind: summary.error_kind,
            message: text(summary.message),
            console: text(summary.console),
            warning: text(summary.warning),
        };
        gleon_result_free(Some(result));
        answer
    }

    /// The inputs of one `gleon_golden` call.
    struct Call<'a> {
        mode: u8,
        strings: [&'a str; 4],
        candidate: &'a [u8],
        tolerance: (u8, f64, f64, f64),
        masks: (*const u32, usize),
    }

    impl Default for Call<'_> {
        fn default() -> Self {
            Self {
                mode: 0,
                strings: ["/definitely/missing/golden.png", "a.png", "", ""],
                candidate: &[],
                tolerance: (0, 0.0, 0.0, 0.0),
                masks: (std::ptr::null(), 0),
            }
        }
    }

    fn golden(session: Option<&GleonSession>, call: Call<'_>) -> Answer {
        let (bytes, lengths) = packed(&call.strings);
        let (kind, ratio, similarity, color) = call.tolerance;
        answer(unsafe {
            gleon_golden(
                session,
                call.mode,
                bytes.as_ptr(),
                bytes.len(),
                lengths.as_ptr(),
                lengths.len(),
                call.candidate.as_ptr(),
                call.candidate.len(),
                kind,
                ratio,
                similarity,
                color,
                call.masks.0,
                call.masks.1,
            )
        })
    }

    const INVALID_INPUT: u8 = ErrorKind::InvalidInput as u8;

    #[test]
    fn test_abi_version() {
        assert_eq!(gleon_ffi_abi_version(), ABI_VERSION);
    }

    #[test]
    fn test_invalid_inputs_are_errors() {
        let session = new_session(SESSION_METRICS_ENV, "");
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
        let answer = golden(None, Call::default());
        assert!(answer.message.contains("no session"), "{}", answer.message);
    }

    #[test]
    fn test_packed_strings_are_checked() {
        let names = ["a", "b"];
        assert_eq!(split(b"xyz", &[1, 2], names), Ok(["x", "yz"]));
        assert_eq!(split(b"", &[0, 0], names), Ok(["", ""]));
        for (bytes, lengths, needle) in [
            (&b"xyz"[..], &[1, 2, 0][..], "expected 2 strings, got 3"),
            (b"xy", &[1, 2], "`b` runs past the end"),
            (b"xyzw", &[1, 2], "1 bytes follow the last string"),
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
        assert_eq!(call_tolerance(0, 9.0, 9.0, 9.0), Ok(None));
        assert_eq!(
            call_tolerance(1, 9.0, 9.0, 9.0),
            Ok(Some(Tolerance::Exact {}))
        );
        assert_eq!(
            call_tolerance(2, 0.1, 9.0, 9.0),
            Ok(Some(Tolerance::Pixel {
                max_diff_ratio: 0.1
            }))
        );
        assert_eq!(
            call_tolerance(3, 9.0, 0.8, 8.0),
            Ok(Some(Tolerance::Ssim {
                min_similarity: 0.8,
                color_tolerance: 8.0
            }))
        );
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
            (4, vec![""; 7], "unknown session flags 0x4"),
            (0, vec![""; 6], "expected 7 strings, got 6"),
            (
                0,
                vec!["", "", "", "", "master.png", candidate, diff],
                "must contain `{name}`",
            ),
            (
                SESSION_METRICS_ENV,
                vec!["maybe", "", "", "", golden_artifact, candidate, diff],
                "GLEON_METRICS must be 1, 0, true or false (got 'maybe')",
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
        let (bytes, _) = packed(&[""; 7]);
        let session = unsafe { gleon_session_new(0, bytes.as_ptr(), 0, std::ptr::null(), 7) };
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
    fn test_metrics_follow_the_process_environment_without_the_flag() {
        let session = new_session(0, "ignored");
        assert!(session.0.failure().is_none() || std::env::var_os(METRICS_ENV).is_some());
        gleon_session_free(Some(session));
    }

    #[test]
    fn test_a_panic_becomes_an_internal_error_with_its_message() {
        let result = guarded(|| panic!("boom"));
        let answer = answer(result);
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

    #[test]
    fn test_null_results() {
        let summary = gleon_result_summary(None);
        assert_eq!(summary.verdict, golden::Verdict::Error as u8);
        assert_eq!(summary.error_kind, INVALID_INPUT);
        assert!(summary.message.as_slice().is_empty());
        gleon_result_free(None);
    }

    #[test]
    #[cfg_attr(miri, ignore = "touches the file system")]
    fn test_a_missing_golden_through_the_abi() {
        let session = new_session(SESSION_METRICS_ENV, "");
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
        let root = std::fs::canonicalize(dir.path()).unwrap();
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
        let session = new_session(SESSION_FIND_WORKSPACE | SESSION_METRICS_ENV, "");
        let path = golden_path.display().to_string();
        let failures = root.join("test/failures").display().to_string();
        let strings = [path.as_str(), "goldens/counter.png", failures.as_str(), "t"];
        let case = || -> serde_json::Value {
            let file = root.join(".gleon/runs/latest/cases/test/goldens/counter.json");
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
        gleon_session_free(Some(session));
    }
}
