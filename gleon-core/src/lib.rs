//! Core library for the gleon visual regression testing CLI.

// "Log Output Separation for CLI": only the `gleon` binary owns stdout/stderr;
// the library reports through return values and `tracing`.
#![deny(clippy::print_stdout, clippy::print_stderr)]

// The config and naming modules moved to the permissive `gleon-model` crate (shared with the
// Flutter package); re-exported so `gleon_core::config` / `crate::naming` paths stay valid.
pub use gleon_model::{config, naming};
/// Resolves the effective run context (platform, branch, renderer) from CLI flags, config, and environment.
pub mod context;
/// Historical test results logging and static dashboard compiler.
pub mod dashboard;
pub mod env;
pub mod git;
pub mod io;
/// License validation and enforcement for gated features.
pub mod license;
pub mod manifest;
/// High-level workspace operations (init, stage, approve, diff, push, pull, etc.) invoked by the CLI.
pub mod ops;
/// Canonical layout of the `.gleon` workspace directory.
pub mod paths;
/// Platform key resolution (os/arch/renderer/label) and conflict detection.
pub mod platform;
/// Rendering of run results into HTML, `JUnit` XML, markdown, and PR comment formats.
pub mod report;
/// Results of comparing a captured screenshot against its staged baseline.
pub mod results;
pub mod scanner;
/// Remote storage integration and baseline blob synchronization.
pub mod storage;
pub mod ui;
/// Shared directory-traversal helpers.
pub mod walk;
