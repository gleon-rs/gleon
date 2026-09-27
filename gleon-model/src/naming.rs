//! Test name normalization and validation shared by the CLI scanner and manifest layers and by
//! other integrations (a golden's case report is named after its test name).

use std::borrow::Cow;

/// Directories unconditionally pruned during workspace traversal to prevent hanging or
/// indexing build artifacts across frontend ecosystems (Flutter, Android, iOS, Web/Node, Rust, Go).
pub const DEFAULT_PRUNED_DIRECTORIES: &[&str] = &[
    ".git",
    ".gleon",
    ".dart_tool",
    "build",
    "target",
    "node_modules",
    "vendor",
    "DerivedData",
    ".gradle",
];

/// Describes why a test name segment failed validation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TestNameError {
    /// A path segment was empty (e.g. from a leading, trailing, or doubled separator).
    #[error("Test name segment cannot be empty")]
    EmptySegment,
    /// A segment was exactly `.` or `..`, which would allow directory traversal.
    #[error("Test name segment cannot be relative path navigation '{segment}'")]
    RelativeNavigation {
        /// The offending segment.
        segment: String,
    },
    /// A segment contained a character outside the allowed `[a-z0-9_.-]` set.
    #[error(
        "Invalid character '{character}' in test name segment '{segment}'. Only lowercase alphanumeric, '_', '-', and '.' are allowed."
    )]
    InvalidCharacter {
        /// The disallowed character.
        character: char,
        /// The segment containing the disallowed character.
        segment: String,
    },
}

/// Validates that all segments of a test name contain only allowed characters `[a-z0-9_.-]`.
/// The name can use either Unix-style forward slashes (`/`) or Windows-style backslashes (`\`) as separators.
///
/// # Errors
/// Returns a [`TestNameError`] describing the offending segment if any segment is empty,
/// is `.`/`..`, or contains characters outside `[a-z0-9_.-]`.
pub fn validate_test_name(name: &str) -> Result<(), TestNameError> {
    for segment in name.split(['/', '\\']) {
        if segment.is_empty() {
            return Err(TestNameError::EmptySegment);
        }
        if segment == "." || segment == ".." {
            return Err(TestNameError::RelativeNavigation {
                segment: segment.to_string(),
            });
        }
        for c in segment.chars() {
            if !c.is_ascii_lowercase() && !c.is_ascii_digit() && c != '_' && c != '-' && c != '.' {
                return Err(TestNameError::InvalidCharacter {
                    character: c,
                    segment: segment.to_string(),
                });
            }
        }
    }
    Ok(())
}

/// Normalizes path separators (`\\` -> `/`) **without touching case**.
///
/// Use this whenever the string is still a real filesystem or repository path (Git index
/// lookups, remote object keys). [`normalize_test_name`] additionally folds case, which is
/// correct for test *identities* but corrupts paths on case-sensitive systems.
#[must_use]
pub fn normalize_path_separators(path: &str) -> Cow<'_, str> {
    if path.contains('\\') {
        Cow::Owned(path.replace('\\', "/"))
    } else {
        Cow::Borrowed(path)
    }
}

/// Normalizes path separators to forward slashes and lowercases ASCII test names without
/// unnecessary allocations.
///
/// Only ASCII case-folding is applied: [`validate_test_name`] rejects any non-ASCII character
/// regardless, so Unicode-aware lowercasing would only spend extra work producing a string
/// that's still invalid.
#[must_use]
pub fn normalize_test_name(test_name: &str) -> Cow<'_, str> {
    if test_name
        .bytes()
        .any(|b| b.is_ascii_uppercase() || b == b'\\')
    {
        let mut s = String::with_capacity(test_name.len());
        for c in test_name.chars() {
            if c == '\\' {
                s.push('/');
            } else {
                s.push(c.to_ascii_lowercase());
            }
        }
        Cow::Owned(s)
    } else {
        Cow::Borrowed(test_name)
    }
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
    use super::*;

    #[test]
    fn test_normalize_path_separators_borrows_when_no_backslash() {
        let path = "billing/Stripe/Form.png";
        match normalize_path_separators(path) {
            Cow::Borrowed(s) => assert_eq!(s, path),
            Cow::Owned(s) => panic!("expected a borrow, got an owned allocation: {s:?}"),
        }
    }

    #[test]
    fn test_normalize_path_separators_only_swaps_slashes_case_is_preserved() {
        // This is the exact shape that broke Git index lookups: folding case here (as
        // `normalize_path_str` does) makes the lookup miss every path containing an uppercase
        // character. Separators must flip; case must survive untouched.
        let path = r"billing\Stripe\Form.png";
        match normalize_path_separators(path) {
            Cow::Owned(s) => assert_eq!(s, "billing/Stripe/Form.png"),
            Cow::Borrowed(s) => panic!("expected an owned allocation, got a borrow: {s:?}"),
        }
    }

    #[test]
    fn test_validate_test_name() {
        assert!(validate_test_name(".").is_err());
        assert!(validate_test_name("billing").is_ok());
        assert!(validate_test_name("billing/stripe").is_ok());
        assert!(validate_test_name("billing/stripe-v2").is_ok());
        assert!(validate_test_name("billing/stripe.v2").is_ok());
        assert!(validate_test_name("billing/stripe_v2").is_ok());

        assert!(validate_test_name("billing/Stripe").is_err());
        assert!(validate_test_name("billing/").is_err());
        assert!(validate_test_name("/billing").is_err());
        assert!(validate_test_name("billing//stripe").is_err());
        assert!(validate_test_name("billing/stripe$").is_err());
        assert!(validate_test_name("billing/..").is_err());
        assert!(validate_test_name("billing/.").is_err());
        assert!(validate_test_name("billing/../stripe").is_err());
    }

    #[test]
    fn test_validate_test_name_windows_separator() {
        assert!(validate_test_name("billing\\stripe").is_ok());
        assert!(validate_test_name("billing\\..").is_err());
    }

    #[test]
    fn test_normalize_test_name_lowercases_ascii_only() {
        assert_eq!(normalize_test_name("Billing/Stripe"), "billing/stripe");
        assert_eq!(normalize_test_name("billing\\stripe"), "billing/stripe");
        assert!(matches!(
            normalize_test_name("already/lower"),
            Cow::Borrowed("already/lower")
        ));
    }

    #[test]
    fn test_normalize_test_name_preserves_non_ascii() {
        // Non-ASCII uppercase is left untouched: it's invalid either way, so we don't
        // pay for Unicode-aware case folding on a string that will be rejected downstream.
        assert_eq!(normalize_test_name("BILLING/É"), "billing/É");
    }

    #[test]
    fn test_default_pruned_directories_contains_common_build_dirs() {
        assert!(DEFAULT_PRUNED_DIRECTORIES.contains(&".git"));
        assert!(DEFAULT_PRUNED_DIRECTORIES.contains(&"node_modules"));
        assert!(DEFAULT_PRUNED_DIRECTORIES.contains(&"target"));
    }
}
