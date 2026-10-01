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
    check("case.v2.json", &schemars::schema_for!(CaseReport));
}

#[test]
fn test_config_schema_is_current() {
    check("config.v1.json", &schemars::schema_for!(GleonConfig));
}
