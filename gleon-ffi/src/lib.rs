//! C ABI over [`gleon_engine`], loaded by the gleon Flutter package through Dart FFI.
//!
//! Contract:
//! - Input buffers are borrowed only for the duration of a call and never retained.
//! - [`gleon_compare`] never returns null and never unwinds: invalid input and panics become an
//!   `"error"` verdict inside the returned result.
//! - The caller owns the returned result and must release it with [`gleon_result_free`];
//!   pointers obtained from the result getters stay valid until then.

// The whole point of this crate is the unsafe pointer boundary; all logic lives in safe `compare`.
#![expect(
    unsafe_code,
    reason = "C ABI boundary: raw pointers handed over by Dart FFI"
)]

mod compare;

use std::panic::{AssertUnwindSafe, catch_unwind};

use compare::{ABI_VERSION, Outcome};

/// Opaque comparison result owned by the caller until [`gleon_result_free`].
pub struct GleonResult(Outcome);

/// Returns the JSON contract version implemented by this library.
#[unsafe(no_mangle)]
pub const extern "C" fn gleon_ffi_abi_version() -> u32 {
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

/// Compares two PNG-encoded images using JSON `options`.
///
/// Options (unknown keys are rejected):
/// - `"mode"`: `"exact"` (every pixel identical), `"pixel"` or `"ssim"`.
/// - `"threshold"`: required for `"pixel"` only, max fraction of differing pixels in `[0, 1]`.
/// - `"min_similarity"` and `"color_tolerance"`: both required for `"ssim"` only; minimum local
///   SSIM in `[0, 1]` and tolerated envelope deviation in 8-bit units (see [`gleon_engine::ssim`]).
/// - `"masks"`: optional `[{"x":u32,"y":u32,"width":D,"height":D}]`, where `D` is a pixel count or
///   a percentage string such as `"25%"`.
///
/// # Safety
/// Each `(ptr, len)` pair must describe a readable buffer of `len` bytes (or be `(null, 0)`) that
/// stays valid for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gleon_compare(
    baseline_ptr: *const u8,
    baseline_len: usize,
    candidate_ptr: *const u8,
    candidate_len: usize,
    options_ptr: *const u8,
    options_len: usize,
) -> *mut GleonResult {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
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
    }))
    .unwrap_or_else(|_| Outcome::error("internal error: comparison panicked"));
    Box::into_raw(Box::new(GleonResult(outcome)))
}

/// Returns the UTF-8 JSON report of `result` and writes its length to `out_len`
/// (null and 0 if `result` is null).
///
/// # Safety
/// `result` must be null or a live pointer returned by [`gleon_compare`]; `out_len` must be
/// writable.
#[unsafe(no_mangle)]
pub const unsafe extern "C" fn gleon_result_json(
    result: *const GleonResult,
    out_len: *mut usize,
) -> *const u8 {
    // SAFETY: caller guarantees `out_len` is writable and a non-null `result` is live.
    unsafe {
        if result.is_null() {
            out_len.write(0);
            return std::ptr::null();
        }
        let json = &(*result).0.json;
        out_len.write(json.len());
        json.as_ptr()
    }
}

/// Returns the PNG diff image of `result` (null if there is none or `result` is null) and writes
/// its length to `out_len` (0 if there is none).
///
/// # Safety
/// `result` must be null or a live pointer returned by [`gleon_compare`]; `out_len` must be
/// writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gleon_result_diff_png(
    result: *const GleonResult,
    out_len: *mut usize,
) -> *const u8 {
    // SAFETY: caller guarantees `out_len` is writable and a non-null `result` is live.
    unsafe {
        let (ptr, len) = result
            .as_ref()
            .and_then(|r| r.0.diff_png.as_deref())
            .map_or((std::ptr::null(), 0), |png| (png.as_ptr(), png.len()));
        out_len.write(len);
        ptr
    }
}

/// Releases a result returned by [`gleon_compare`]. Passing null is a no-op.
///
/// # Safety
/// `result` must be null or a pointer returned by [`gleon_compare`] that was not freed yet.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gleon_result_free(result: *mut GleonResult) {
    if !result.is_null() {
        // SAFETY: caller guarantees this is an unfreed pointer from `Box::into_raw`.
        drop(unsafe { Box::from_raw(result) });
    }
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

    fn read_json(result: *const GleonResult) -> serde_json::Value {
        let mut len = 0usize;
        let ptr = unsafe { gleon_result_json(result, &raw mut len) };
        serde_json::from_slice(unsafe { std::slice::from_raw_parts(ptr, len) }).unwrap()
    }

    #[test]
    fn test_abi_version_matches_report() {
        assert_eq!(gleon_ffi_abi_version(), ABI_VERSION);
    }

    #[test]
    fn test_null_buffer_with_length_is_an_error() {
        let opts = br#"{"mode":"exact"}"#;
        let result = unsafe {
            gleon_compare(
                std::ptr::null(),
                10,
                std::ptr::null(),
                0,
                opts.as_ptr(),
                opts.len(),
            )
        };
        assert!(!result.is_null());
        let json = read_json(result);
        assert_eq!(json["verdict"], "error");
        assert!(json["error"].as_str().unwrap().contains("baseline"));
        let mut len = 1usize;
        assert!(unsafe { gleon_result_diff_png(result, &raw mut len) }.is_null());
        assert_eq!(len, 0);
        unsafe { gleon_result_free(result) };
    }

    #[test]
    #[cfg_attr(miri, ignore = "reaches the image decoder")]
    fn test_null_with_zero_length_is_an_empty_buffer() {
        let opts = br#"{"mode":"exact"}"#;
        let baseline = png(1, 1, |_, _| image::Rgba([0, 0, 0, 255]));
        let result = unsafe {
            gleon_compare(
                baseline.as_ptr(),
                baseline.len(),
                std::ptr::null(),
                0,
                opts.as_ptr(),
                opts.len(),
            )
        };
        // The empty candidate is accepted by the ABI (not a `borrow` error) and rejected by the
        // decoder once the valid baseline has been decoded.
        let json = read_json(result);
        assert_eq!(json["verdict"], "error");
        assert!(
            json["error"].as_str().unwrap().contains("candidate image"),
            "{json}"
        );
        unsafe { gleon_result_free(result) };
    }

    #[test]
    fn test_getters_tolerate_null() {
        let mut len = 7usize;
        assert!(unsafe { gleon_result_json(std::ptr::null(), &raw mut len) }.is_null());
        assert_eq!(len, 0);
        len = 7;
        assert!(unsafe { gleon_result_diff_png(std::ptr::null(), &raw mut len) }.is_null());
        assert_eq!(len, 0);
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
        let opts = br#"{"mode":"exact"}"#;
        let result = unsafe {
            gleon_compare(
                baseline.as_ptr(),
                baseline.len(),
                candidate.as_ptr(),
                candidate.len(),
                opts.as_ptr(),
                opts.len(),
            )
        };
        assert_eq!(read_json(result)["verdict"], "mismatch");
        let mut len = 0usize;
        let diff = unsafe { gleon_result_diff_png(result, &raw mut len) };
        assert!(!diff.is_null());
        let diff_bytes = unsafe { std::slice::from_raw_parts(diff, len) };
        assert!(image::load_from_memory(diff_bytes).is_ok());
        unsafe { gleon_result_free(result) };
    }

    #[test]
    fn test_free_null_is_noop() {
        unsafe { gleon_result_free(std::ptr::null_mut()) };
    }
}
