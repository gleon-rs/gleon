//! C ABI over [`gleon_engine`], loaded by the gleon Flutter package through Dart FFI.
//!
//! Exports go through [`safer_ffi`]: the opaque result, nullable handles and returned byte slices
//! use its FFI-safe types, so the only raw pointers left are the input buffers. Those stay raw
//! `(ptr, len)` pairs on purpose: Dart can pass its own byte lists to a leaf call without copying
//! them (`Uint8List.address`), which is impossible for a slice struct passed by value.
//!
//! Contract:
//! - Input buffers are borrowed only for the duration of a call and never retained.
//! - [`gleon_compare`] and [`gleon_config_resolve`] never return null and never unwind: invalid
//!   input and panics become an error inside the returned result's JSON.
//! - The caller owns the returned result and must release it with [`gleon_result_free`];
//!   slices obtained from the result getters stay valid until then.

// Only `borrow` and the raw input pointers of the exports need it; all logic is in the safe
// `compare` and `resolve` modules.
#![expect(
    unsafe_code,
    reason = "C ABI boundary: raw input buffers handed over by Dart FFI without copying"
)]

mod compare;
mod resolve;

use std::panic::{AssertUnwindSafe, catch_unwind};

use compare::{ABI_VERSION, Outcome};
pub use result::GleonResult;
use safer_ffi::prelude::*;

#[expect(
    clippy::expl_impl_clone_on_copy,
    reason = "safer-ffi generates these implementations for its opaque type marker"
)]
mod result {
    use safer_ffi::prelude::*;

    use crate::compare::Outcome;

    /// Opaque comparison result owned by the caller until [`gleon_result_free`](crate::gleon_result_free).
    #[derive_ReprC]
    #[repr(opaque)]
    pub struct GleonResult(pub(crate) Outcome);
}

/// Returns the JSON contract version implemented by this library.
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

/// Borrows `len` bytes at `ptr` as a slice. A null pointer is only valid together with `len == 0`.
///
/// # Safety
/// If `ptr` is non-null it must point to `len` initialized bytes that stay valid and unmodified
/// for the returned lifetime.
unsafe fn borrow<'a>(ptr: *const u8, len: usize, name: &str) -> Result<&'a [u8], String> {
    if ptr.is_null() {
        return if len == 0 {
            Ok(&[])
        } else {
            Err(format!("`{name}` is null but its length is {len}"))
        };
    }
    // SAFETY: non-null and, per this function's contract, valid for `len` bytes.
    Ok(unsafe { std::slice::from_raw_parts(ptr, len) })
}

/// Runs `call` behind the ABI: a caught panic becomes the `on_panic` outcome.
fn guarded(
    call: impl FnOnce() -> Outcome,
    on_panic: fn(&'static str) -> Outcome,
) -> repr_c::Box<GleonResult> {
    let outcome = catch_unwind(AssertUnwindSafe(call))
        .unwrap_or_else(|_| on_panic("internal error: the native call panicked"));
    Box::new(GleonResult(outcome)).into()
}

/// Compares two PNG-encoded images using JSON `options`.
///
/// Options (unknown keys are rejected):
/// - `"tolerance"` (required): `{"kind":"exact"}` (every pixel identical),
///   `{"kind":"pixel","max_diff_ratio":R}` (max fraction of differing pixels in `[0, 1]`) or
///   `{"kind":"ssim","min_similarity":S,"color_tolerance":C}` (minimum local SSIM in `[0, 1]`
///   and tolerated envelope deviation in 8-bit units, see [`gleon_engine::ssim`]). Parameters of
///   another kind are rejected.
/// - `"masks"`: optional `[{"x":u32,"y":u32,"width":D,"height":D}]`, where `D` is a pixel count or
///   a percentage string such as `"25%"`.
///
/// The JSON report carries `abi`, `policy_version`, `verdict` (`match`, `mismatch`,
/// `dimension_mismatch`, `error`), `baseline`/`candidate` sizes, whole-image `metrics` and
/// `regions` (for matches too, see `gleon_model::case::Metrics`), `timings_us` and `error`.
///
/// # Safety
/// Each `(ptr, len)` pair must describe a readable buffer of `len` bytes (or be `(null, 0)`) that
/// stays valid for the duration of this call.
#[ffi_export]
#[must_use]
pub unsafe fn gleon_compare(
    baseline_ptr: *const u8,
    baseline_len: usize,
    candidate_ptr: *const u8,
    candidate_len: usize,
    options_ptr: *const u8,
    options_len: usize,
) -> repr_c::Box<GleonResult> {
    guarded(
        || {
            // SAFETY: forwarded caller contract; the slices do not outlive this closure.
            let inputs = unsafe {
                borrow(baseline_ptr, baseline_len, "baseline").and_then(|baseline| {
                    borrow(candidate_ptr, candidate_len, "candidate").and_then(|candidate| {
                        borrow(options_ptr, options_len, "options")
                            .map(|options| (baseline, candidate, options))
                    })
                })
            };
            inputs.map_or_else(Outcome::error, |(baseline, candidate, options)| {
                compare::compare(baseline, candidate, options)
            })
        },
        Outcome::error,
    )
}

/// Validates the UTF-8 `.gleon/gleon.yaml` text `yaml` and resolves the screenshot rule of the
/// golden at `path` (UTF-8, relative to the workspace root, either separator) exactly like the
/// gleon CLI scanner.
///
/// `metrics_env` is the caller's raw `GLEON_METRICS` value (UTF-8), or null when unset; the
/// caller passes it so its environment stays the single source (and injectable in tests).
///
/// The JSON result is either
/// `{"kind":"resolved","abi":4,"policy_version":2,"rule":..,"metrics":{"enabled":..,"console":..},"platform":{"os":..,"arch":..}}`
/// (`rule.kind` is `matched` with `index`, the canonical test `name`, `tolerance` and `masks`,
/// or `excluded`, or `unmatched`; `metrics` has the `GLEON_METRICS` override applied;
/// `platform` is the host, named like in the CLI) or `{"kind":"error","abi":4,"error":".."}` with
/// the message of the parser.
///
/// # Safety
/// Each `(ptr, len)` pair must describe a readable buffer of `len` bytes (or be `(null, 0)`) that
/// stays valid for the duration of this call.
#[ffi_export]
#[must_use]
pub unsafe fn gleon_config_resolve(
    yaml_ptr: *const u8,
    yaml_len: usize,
    path_ptr: *const u8,
    path_len: usize,
    metrics_env_ptr: *const u8,
    metrics_env_len: usize,
) -> repr_c::Box<GleonResult> {
    guarded(
        || {
            // SAFETY: forwarded caller contract; the slices do not outlive this closure.
            let inputs = unsafe {
                borrow(yaml_ptr, yaml_len, "yaml").and_then(|yaml| {
                    borrow(path_ptr, path_len, "path").and_then(|path| {
                        borrow(metrics_env_ptr, metrics_env_len, "metrics_env")
                            .map(|env| (yaml, path, (!metrics_env_ptr.is_null()).then_some(env)))
                    })
                })
            };
            inputs.map_or_else(resolve::error, |(yaml, path, env)| {
                resolve::resolve(yaml, path, env)
            })
        },
        resolve::error,
    )
}

/// Returns the UTF-8 JSON report of `result` (null slice if `result` is null).
#[ffi_export]
#[must_use]
pub fn gleon_result_json(result: Option<&GleonResult>) -> Option<c_slice::Ref<'_, u8>> {
    result.map(|r| r.0.json.as_slice().into())
}

/// Returns the PNG diff image of `result` (null slice if there is none or `result` is null).
#[ffi_export]
#[must_use]
pub fn gleon_result_diff_png(result: Option<&GleonResult>) -> Option<c_slice::Ref<'_, u8>> {
    result.and_then(|r| r.0.diff_png.as_deref()).map(Into::into)
}

/// Releases a result returned by [`gleon_compare`] or [`gleon_config_resolve`]. Passing null is a
/// no-op.
#[ffi_export]
pub fn gleon_result_free(result: Option<repr_c::Box<GleonResult>>) {
    drop(result);
}

// Runs under Miri too: these tests exercise the unsafe pointer boundary without the engine.
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

    fn read_json(result: &GleonResult) -> serde_json::Value {
        let json = gleon_result_json(Some(result)).unwrap();
        serde_json::from_slice(json.as_slice()).unwrap()
    }

    fn compare(
        baseline: (*const u8, usize),
        candidate: (*const u8, usize),
        opts: &[u8],
    ) -> repr_c::Box<GleonResult> {
        unsafe {
            gleon_compare(
                baseline.0,
                baseline.1,
                candidate.0,
                candidate.1,
                opts.as_ptr(),
                opts.len(),
            )
        }
    }

    #[test]
    fn test_abi_version_matches_report() {
        assert_eq!(gleon_ffi_abi_version(), ABI_VERSION);
    }

    #[test]
    fn test_null_buffer_with_length_is_an_error() {
        let result = compare(
            (std::ptr::null(), 10),
            (std::ptr::null(), 0),
            br#"{"tolerance":{"kind":"exact"}}"#,
        );
        let json = read_json(&result);
        assert_eq!(json["verdict"], "error");
        assert!(json["error"].as_str().unwrap().contains("baseline"));
        assert!(gleon_result_diff_png(Some(&result)).is_none());
        gleon_result_free(Some(result));
    }

    #[test]
    #[cfg_attr(miri, ignore = "reaches the image decoder")]
    fn test_null_with_zero_length_is_an_empty_buffer() {
        let baseline = png(1, 1, |_, _| image::Rgba([0, 0, 0, 255]));
        let result = compare(
            (baseline.as_ptr(), baseline.len()),
            (std::ptr::null(), 0),
            br#"{"tolerance":{"kind":"exact"}}"#,
        );
        // The empty candidate is accepted by the ABI (not a `borrow` error) and rejected by the
        // decoder once the valid baseline has been decoded.
        let json = read_json(&result);
        assert_eq!(json["verdict"], "error");
        assert!(
            json["error"].as_str().unwrap().contains("candidate image"),
            "{json}"
        );
        gleon_result_free(Some(result));
    }

    #[test]
    fn test_getters_tolerate_null() {
        assert!(gleon_result_json(None).is_none());
        assert!(gleon_result_diff_png(None).is_none());
    }

    fn png(width: u32, height: u32, paint: impl Fn(u32, u32) -> image::Rgba<u8>) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(width, height, paint);
        let mut bytes = Vec::new();
        img.write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
        bytes
    }

    #[test]
    #[cfg_attr(miri, ignore = "runs the image engine, far too slow under Miri")]
    fn test_mismatch_round_trip_through_the_abi() {
        let red = image::Rgba([255, 0, 0, 255]);
        let baseline = png(8, 8, |_, _| red);
        let candidate = png(8, 8, |x, y| {
            if (x, y) == (2, 2) {
                image::Rgba([0, 0, 255, 255])
            } else {
                red
            }
        });
        let result = compare(
            (baseline.as_ptr(), baseline.len()),
            (candidate.as_ptr(), candidate.len()),
            br#"{"tolerance":{"kind":"exact"}}"#,
        );
        assert_eq!(read_json(&result)["verdict"], "mismatch");
        let diff = gleon_result_diff_png(Some(&result)).unwrap();
        assert!(image::load_from_memory(diff.as_slice()).is_ok());
        gleon_result_free(Some(result));
    }

    #[test]
    fn test_resolve_null_buffer_with_length_is_an_error() {
        let result = unsafe {
            gleon_config_resolve(
                std::ptr::null(),
                3,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
            )
        };
        let json = read_json(&result);
        assert_eq!(json["kind"], "error");
        assert!(json["error"].as_str().unwrap().contains("yaml"), "{json}");
        assert!(gleon_result_diff_png(Some(&result)).is_none());
        gleon_result_free(Some(result));
    }

    #[test]
    #[cfg_attr(miri, ignore = "compiles globs, far too slow under Miri")]
    fn test_resolve_round_trip_through_the_abi() {
        let yaml = b"required_version: '>=0.1.0'\nscreenshots:\n  - include: '**/*.png'\n";
        let path = b"test/goldens/a.png";
        let resolve = |env: Option<&[u8]>| {
            let (env_ptr, env_len) = env.map_or((std::ptr::null(), 0), |e| (e.as_ptr(), e.len()));
            let result = unsafe {
                gleon_config_resolve(
                    yaml.as_ptr(),
                    yaml.len(),
                    path.as_ptr(),
                    path.len(),
                    env_ptr,
                    env_len,
                )
            };
            let json = read_json(&result);
            gleon_result_free(Some(result));
            json
        };
        let json = resolve(None);
        assert_eq!(json["kind"], "resolved", "{json}");
        assert_eq!(json["rule"]["name"], "test/goldens/a");
        assert_eq!(json["metrics"]["enabled"], false);
        assert_eq!(resolve(Some(b"1"))["metrics"]["enabled"], true);
    }

    #[test]
    fn test_a_panic_becomes_an_error_result() {
        let panicking = || -> Outcome { panic!("boom") };
        let result = guarded(panicking, Outcome::error);
        let json = read_json(&result);
        assert_eq!(json["verdict"], "error");
        assert_eq!(json["error"], "internal error: the native call panicked");
        gleon_result_free(Some(result));

        let result = guarded(panicking, resolve::error);
        assert_eq!(read_json(&result)["kind"], "error");
        gleon_result_free(Some(result));
    }

    #[test]
    fn test_free_null_is_noop() {
        gleon_result_free(None);
    }
}
