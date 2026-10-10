#![cfg(all(test, not(miri)))]
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

mod common;

use std::fs;

use gleon_core::{
    context::{ContextOptions, ResolvedContext},
    ops::clean::{CleanOptions, clean_workspace},
};
use tempfile::tempdir;

const VALID_PNG_BYTES: &[u8] = include_bytes!("fixtures/baseline_100x100.png");

#[test]
fn test_clean_workspace_full_flow_with_git_untracking() {
    let temp = tempdir().unwrap();
    let base_path = temp.path();

    // 1. Initialize a real git repo using gix
    let repo = gix::init(base_path).unwrap();

    // 2. Setup .gleon structure
    let gleon_dir = base_path.join(".gleon");
    fs::create_dir_all(&gleon_dir).unwrap();
    let runs_dir = gleon_dir.join("runs");
    fs::create_dir_all(&runs_dir).unwrap();
    fs::write(runs_dir.join("run_metadata.json"), b"{}").unwrap();

    common::copy_config(base_path, "monorepo_goldens");

    // 3. Create sample golden files
    let goldens_dir = base_path
        .join("packages")
        .join("app")
        .join("test")
        .join("goldens");
    fs::create_dir_all(&goldens_dir).unwrap();
    let golden_file = goldens_dir.join("login.png");
    fs::write(&golden_file, VALID_PNG_BYTES).unwrap();

    // 4. Create and populate git index
    let index_path = base_path.join(".git").join("index");
    let state = gix::index::State::new(gix::hash::Kind::Sha1);
    let mut index = gix::index::File::from_state(state, index_path);
    let rel_path_bstr = "packages/app/test/goldens/login.png";
    index.dangerously_push_entry(
        gix::index::entry::Stat::default(),
        gix::hash::ObjectId::empty_tree(gix::hash::Kind::Sha1),
        gix::index::entry::Flags::empty(),
        gix::index::entry::Mode::FILE,
        rel_path_bstr.as_bytes().into(),
    );
    index.write(gix::index::write::Options::default()).unwrap();

    // Verify entry is currently in index
    let index_before = repo.open_index().unwrap();
    assert!(
        index_before
            .entry_index_by_path(rel_path_bstr.into())
            .is_ok()
    );

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    // 5. Run clean
    let opts = CleanOptions {
        screenshots: true,
        ..CleanOptions::default()
    };
    let res = clean_workspace(&ctx, &opts).unwrap();

    assert_eq!(res.deleted_files.len(), 1);
    assert_eq!(res.untracked_files.len(), 1);
    assert!(!golden_file.exists());
    assert!(!runs_dir.exists());

    // 6. Verify entry was removed from Git index
    let index_after = repo.open_index().unwrap();
    assert!(
        index_after
            .entry_index_by_path(rel_path_bstr.into())
            .is_err()
    );

    // 7. Verify .gitignore wildcard entry
    let gitignore = fs::read_to_string(base_path.join(".gitignore")).unwrap();
    assert!(gitignore.contains("packages/app/test/goldens/**/*.png"));
}

#[test]
fn test_clean_workspace_dry_run_leaves_state_intact() {
    let temp = tempdir().unwrap();
    let base_path = temp.path();

    let gleon_dir = base_path.join(".gleon");
    fs::create_dir_all(&gleon_dir).unwrap();
    let runs_dir = gleon_dir.join("runs");
    fs::create_dir_all(&runs_dir).unwrap();
    fs::write(runs_dir.join("data.txt"), b"temp").unwrap();

    common::copy_config(base_path, "goldens");

    let goldens_dir = base_path.join("test").join("goldens");
    fs::create_dir_all(&goldens_dir).unwrap();
    let golden_file = goldens_dir.join("button.png");
    fs::write(&golden_file, VALID_PNG_BYTES).unwrap();

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    let opts = CleanOptions {
        screenshots: true,
        dry_run: true,
        skip_gitignore: false,
        keep_runs: false,
    };
    let res = clean_workspace(&ctx, &opts).unwrap();

    assert_eq!(res.deleted_files.len(), 1);
    assert!(golden_file.exists());
    assert!(runs_dir.exists());
    assert!(!base_path.join(".gitignore").exists());
}

#[test]
fn test_clean_workspace_outside_git_repository() {
    let temp = tempdir().unwrap();
    let base_path = temp.path();

    let gleon_dir = base_path.join(".gleon");
    fs::create_dir_all(&gleon_dir).unwrap();

    common::copy_config(base_path, "goldens");

    let goldens_dir = base_path.join("test").join("goldens");
    fs::create_dir_all(&goldens_dir).unwrap();
    let golden_file = goldens_dir.join("header.png");
    fs::write(&golden_file, VALID_PNG_BYTES).unwrap();

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    let opts = CleanOptions {
        screenshots: true,
        ..CleanOptions::default()
    };
    let res = clean_workspace(&ctx, &opts).unwrap();

    assert_eq!(res.deleted_files.len(), 1);
    assert!(!golden_file.exists());
    assert!(base_path.join(".gitignore").exists());
}

/// `clean` must untrack files whose repository path contains uppercase characters.
///
/// The index lookup is byte-exact, so case-folding the path before `entry_index_by_path`
/// silently misses the entry: the file is deleted from disk but left staged, leaving the
/// repository with a phantom unstaged deletion.
#[test]
fn test_clean_workspace_untracks_mixed_case_paths() {
    let temp = tempdir().unwrap();
    let base_path = temp.path();

    let repo = gix::init(base_path).unwrap();

    let gleon_dir = base_path.join(".gleon");
    fs::create_dir_all(&gleon_dir).unwrap();
    common::copy_config(base_path, "goldens_mixed_case");

    let goldens_dir = base_path.join("test").join("Goldens");
    fs::create_dir_all(&goldens_dir).unwrap();
    let golden_file = goldens_dir.join("LoginScreen.png");
    fs::write(&golden_file, VALID_PNG_BYTES).unwrap();

    let index_path = base_path.join(".git").join("index");
    let state = gix::index::State::new(gix::hash::Kind::Sha1);
    let mut index = gix::index::File::from_state(state, index_path);
    let rel_path_bstr = "test/Goldens/LoginScreen.png";
    index.dangerously_push_entry(
        gix::index::entry::Stat::default(),
        gix::hash::ObjectId::empty_tree(gix::hash::Kind::Sha1),
        gix::index::entry::Flags::empty(),
        gix::index::entry::Mode::FILE,
        rel_path_bstr.as_bytes().into(),
    );
    index.write(gix::index::write::Options::default()).unwrap();

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();
    let res = clean_workspace(
        &ctx,
        &CleanOptions {
            screenshots: true,
            ..CleanOptions::default()
        },
    )
    .unwrap();

    assert_eq!(res.deleted_files.len(), 1, "file removed from disk");
    assert_eq!(
        res.untracked_files.len(),
        1,
        "mixed-case path must also be untracked from the Git index"
    );

    let index_after = repo.open_index().unwrap();
    assert!(
        index_after
            .entry_index_by_path(rel_path_bstr.into())
            .is_err(),
        "entry must be gone from the index"
    );
}
