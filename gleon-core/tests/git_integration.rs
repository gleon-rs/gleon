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

use std::{env, path::PathBuf};

use gleon_core::git::GitResolver;

fn find_repo_root() -> Option<PathBuf> {
    let mut repo_root = env::current_dir().ok()?;
    loop {
        if repo_root.join(".git").exists() {
            return Some(repo_root);
        }
        if !repo_root.pop() {
            return None;
        }
    }
}

#[test]
#[cfg(not(miri))]
fn test_resolve_branch_real_repo() {
    let repo_root = if let Some(root) = find_repo_root() {
        root
    } else {
        eprintln!("Skipping test: not running inside a git repository");
        return;
    };

    let result = GitResolver::resolve_branch(None, &repo_root, &gleon_core::env::OsEnv);
    assert!(
        result.is_ok(),
        "Expected branch resolution to succeed on real repo, got {result:?}"
    );
    let branch = result.unwrap();
    assert!(!branch.is_empty(), "Branch name should not be empty");
}

#[test]
#[cfg(not(miri))]
fn test_resolve_context_real_repo_commit_sha() {
    let repo_root = if let Some(root) = find_repo_root() {
        root
    } else {
        eprintln!("Skipping test: not running inside a git repository");
        return;
    };

    let options = gleon_core::context::ContextOptions::default();
    let ctx = gleon_core::context::ResolvedContext::resolve(
        &options,
        &repo_root,
        &gleon_core::env::OsEnv,
    )
    .unwrap();

    assert!(ctx.commit_sha.is_some());
    assert!(!ctx.commit_sha.unwrap().is_empty());
}
