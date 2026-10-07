#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::pedantic,
    clippy::nursery,
    missing_docs,
    reason = "test code: panics are assertions, and pedantic/nursery style lints are not enforced in tests"
)]

//! The committed JSON Schemas under `schema/` must match the Rust types.
//!
//! Regenerate after changing a type: `GLEON_UPDATE_SCHEMAS=1 cargo test -p gleon-model --test schema`.

use std::path::PathBuf;

use gleon_model::{case::CaseReport, config::GleonConfig};

fn check(file: &str, schema: &schemars::Schema) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("schema")
        .join(file);
    let generated = serde_json::to_string_pretty(schema).unwrap() + "\n";
    if std::env::var_os("GLEON_UPDATE_SCHEMAS").is_some() {
        std::fs::write(&path, &generated).unwrap();
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed == generated,
        "{} is stale; run `GLEON_UPDATE_SCHEMAS=1 cargo test -p gleon-model --test schema`",
        path.display()
    );
}

#[test]
fn test_case_schema_is_current() {
    check("case.v3.json", &schemars::schema_for!(CaseReport));
}

#[test]
fn test_config_schema_is_current() {
    check("config.v1.json", &schemars::schema_for!(GleonConfig));
}

/// The fields of a platform are segments of its key (`[A-Za-z0-9_.-]`, lowercased): a schema
/// validator rejects a space or a key separator (`+`, `=`) before gleon does.
#[test]
fn test_platform_fields_are_key_segments_in_the_schema() {
    let schema = serde_json::to_value(schemars::schema_for!(CaseReport)).unwrap();
    let fields = &schema["$defs"]["PlatformFields"]["properties"];
    let pattern = |pointer: &str| {
        let pattern = fields
            .pointer(pointer)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("no pattern at {pointer}: {fields}"));
        regex::Regex::new(pattern).unwrap()
    };
    for pointer in [
        "/os/pattern",
        "/arch/pattern",
        "/renderer/pattern",
        "/labels/propertyNames/pattern",
        "/labels/additionalProperties/pattern",
    ] {
        let pattern = pattern(pointer);
        for good in ["macos", "x86_64", "flutter-3.47.5", "en_US"] {
            assert!(pattern.is_match(good), "{pointer}: {good}");
        }
        for bad in ["mac os", "a+b", "a=b", "a/b", ""] {
            assert!(!pattern.is_match(bad), "{pointer}: {bad:?}");
        }
    }
    let description = schema["$defs"]["PlatformConfig"]["description"]
        .as_str()
        .unwrap();
    assert!(description.contains("os=<os>+arch=<arch>"), "{description}");
}
