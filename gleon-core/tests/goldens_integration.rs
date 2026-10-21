#![cfg(not(miri))]
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

//! The operations that work with manifests and blobs refuse a workspace whose baselines are golden
//! files (`baselines: goldens`), whoever calls them, before they touch storage or files.

mod common;

use gleon_core::{
    context::{ContextOptions, ResolvedContext},
    ops::{
        common::CoreError,
        gc::{GcError, GcOptions, garbage_collect},
        lint::{LintError, lint_workspace_manifests},
        pull::PullError,
        pull_blobs,
        push::PushError,
        push_blobs,
    },
    storage::StorageConfig,
};

fn goldens() -> (tempfile::TempDir, ResolvedContext) {
    let temp = tempfile::tempdir().unwrap();
    common::goldens_workspace(temp.path());
    let ctx = ResolvedContext::from_options(&ContextOptions::default(), temp.path()).unwrap();
    (temp, ctx)
}

/// With storage configured, gc would find no manifest and take every remote blob for an orphan.
#[tokio::test]
async fn test_gc_refuses_a_goldens_workspace() {
    let (_temp, ctx) = goldens();
    let storage = StorageConfig::new("memory://");
    let err = garbage_collect(&ctx, Some(&storage), &GcOptions::new(false, 24, true))
        .await
        .unwrap_err();
    assert!(
        matches!(err, GcError::Core(CoreError::BaselinesAreGoldens)),
        "{err}"
    );
}

#[tokio::test]
async fn test_pull_and_push_refuse_a_goldens_workspace() {
    let (temp, ctx) = goldens();
    let storage = StorageConfig::new("memory://");
    let err = pull_blobs(&ctx, Some(&storage), false, None, true)
        .await
        .unwrap_err();
    assert!(
        matches!(err, PullError::Core(CoreError::BaselinesAreGoldens)),
        "{err}"
    );
    let err = push_blobs(&ctx, Some(&storage), false, None, true)
        .await
        .unwrap_err();
    assert!(
        matches!(err, PushError::Core(CoreError::BaselinesAreGoldens)),
        "{err}"
    );
    assert!(!temp.path().join(".gleon/blobs").exists());
}

#[test]
fn test_lint_refuses_a_goldens_workspace() {
    let (_temp, ctx) = goldens();
    let err = lint_workspace_manifests(&ctx, None).unwrap_err();
    assert!(
        matches!(err, LintError::Core(CoreError::BaselinesAreGoldens)),
        "{err}"
    );
}
