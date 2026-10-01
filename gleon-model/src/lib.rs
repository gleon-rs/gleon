//! Shared gleon data model (`MIT OR Apache-2.0`).
//!
//! Everything the gleon CLI and other integrations (the Flutter package and other test frameworks through `gleon-ffi`) must
//! agree on lives here, so both sides read the same `.gleon/gleon.yaml` the same way and write
//! results in one format:
//!
//! - [`config`]: the workspace configuration and its validation;
//! - [`rules`]: which screenshot rule applies to a golden path;
//! - [`naming`]: canonical test names;
//! - [`platform`]: platform identity and storage keys;
//! - [`tolerance`]: how much two images may differ;
//! - [`case`]: the per-golden case report (`.gleon/runs/latest/cases/<name>.json`) and the
//!   images of a failure next to it;
//! - [`hash`]: content hashes of images (`sha256:<hex>`), shared by manifests and case reports;
//! - [`fs`]: atomic file writes.
//!
//! JSON Schemas for the config and the case report are committed under `schema/` and kept in sync
//! by a test.

pub mod case;
pub mod config;
pub mod fs;
pub mod hash;
pub mod naming;
pub mod platform;
pub mod rules;
pub mod tolerance;
