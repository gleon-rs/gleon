//! Untrusted PNG bytes (goldens, candidates) never panic or allocate past the decoding budget.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    if let Ok(image) = gleon_engine::decode::decode_rgba(bytes) {
        assert!(gleon_engine::decode::fits_budget(image.width(), image.height()));
    }
});
