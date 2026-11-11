//! The decision of `build.rs` whether a build is official (`build/official.rs`).

#[path = "../build/official.rs"]
mod official;

use official::{decide, hashes_to};

#[test]
fn test_secret_is_compared_by_its_sha256() {
    // SHA-256 of "abc", the first example of the SHA-2 standard.
    let abc = [
        0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae, 0x22,
        0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61, 0xf2, 0x00,
        0x15, 0xad,
    ];
    assert!(hashes_to("abc", &abc));
    assert!(!hashes_to("abd", &abc));
    assert!(!hashes_to("", &abc));
}

#[test]
fn test_without_the_secret_a_build_is_unofficial() {
    assert_eq!(decide(None, false), Ok(false));
    assert_eq!(decide(Some("not the secret"), false), Ok(false));
}

#[test]
fn test_a_required_official_build_fails_without_the_secret() {
    for secret in [None, Some(""), Some("not the secret")] {
        assert!(
            matches!(decide(secret, true), Err(reason) if reason.contains("GLEON_REQUIRE_OFFICIAL")),
            "{secret:?}"
        );
    }
}
