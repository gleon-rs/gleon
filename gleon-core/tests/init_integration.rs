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

use std::fs;

use gleon_core::{
    config::GleonConfig,
    context::{ContextOptions, ResolvedContext},
    ops::init_workspace,
};

#[test]
fn test_init_workspace_creates_real_structure_and_valid_config() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    let result = init_workspace(&ctx).expect("init_workspace should succeed");

    assert_eq!(result.gleon_dir, base_path.join(".gleon"));
    let config_path = result.config_created.expect("gleon.yaml should be created");
    assert_eq!(config_path, base_path.join(".gleon").join("gleon.yaml"));

    // Verify directory structure exists on disk
    assert!(base_path.join(".gleon/blobs/sha256").is_dir());
    assert!(base_path.join(".gleon/manifests").is_dir());
    assert!(base_path.join(".gleon/runs/latest").is_dir());
    assert!(config_path.is_file());

    // Verify created config is a valid GleonConfig that can be parsed
    let loaded_config =
        GleonConfig::load_from_file(&config_path).expect("Created config should be valid YAML");
    assert_eq!(loaded_config, GleonConfig::default());
}

#[test]
fn test_init_workspace_idempotent_preserves_custom_config() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let config_path = base_path.join(".gleon").join("gleon.yaml");
    let custom_yaml = r#"
required_version: ">=0.1.0"
screenshots:
  - include: "custom/**/*.png"
"#;
    fs::create_dir_all(base_path.join(".gleon")).unwrap();
    fs::write(&config_path, custom_yaml).unwrap();

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    let result = init_workspace(&ctx).expect("Second init should succeed");

    assert_eq!(result.config_created, None);
    let loaded_config = GleonConfig::load_from_file(&config_path).unwrap();
    assert_eq!(
        loaded_config.screenshots[0].include[0].as_str(),
        "custom/**/*.png"
    );
}

#[test]
fn test_init_workspace_honors_cli_overrides() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();

    let options = ContextOptions {
        branch: Some("feature/login".to_string()),
        os: Some("android".to_string()),
        arch: Some("arm".to_string()),
        labels: vec![("theme".to_string(), "dark".to_string())],
        ..Default::default()
    };
    let ctx = ResolvedContext::from_options(&options, base_path).unwrap();

    init_workspace(&ctx).expect("init_workspace should succeed");

    let platform_key = ctx.platform.to_key().unwrap();
    assert_eq!(platform_key, "android-arm+theme=dark");
    assert!(
        base_path
            .join(".gleon/manifests")
            .join(&platform_key)
            .is_dir()
    );
}

#[test]
fn test_gitignore_append_no_newline() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();
    let gleon_dir = base_path.join(".gleon");
    fs::create_dir_all(&gleon_dir).unwrap();
    let gitignore_path = gleon_dir.join(".gitignore");

    // Write a .gitignore that lacks a trailing newline
    fs::write(&gitignore_path, "node_modules").unwrap();

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    init_workspace(&ctx).unwrap();

    let content = fs::read_to_string(&gitignore_path).unwrap();
    assert!(content.contains("node_modules\nblobs/\n"));
}

#[test]
fn test_env_template_already_exists() {
    let temp_dir = tempfile::tempdir().unwrap();
    let base_path = temp_dir.path();
    let gleon_dir = base_path.join(".gleon");
    fs::create_dir_all(&gleon_dir).unwrap();
    let env_template_path = gleon_dir.join(".env.template");

    fs::write(&env_template_path, "EXISTING_VAR=1\n").unwrap();

    let ctx = ResolvedContext::from_options(&ContextOptions::default(), base_path).unwrap();

    init_workspace(&ctx).unwrap();

    // The .gleon/.env.template shouldn't be overwritten
    let content = fs::read_to_string(&env_template_path).unwrap();
    assert_eq!(content, "EXISTING_VAR=1\n");
}
