//! Initialization operation for gleon workspace.

use std::path::PathBuf;

use thiserror::Error;

use crate::{
    config::GleonConfig,
    ops::common::{CoreError, append_missing_gitignore_lines, create_new_file_with_content},
    paths::GleonPaths,
};

/// Errors that can occur during workspace initialization.
#[derive(Debug, Error)]
pub enum InitError {
    /// YAML serialization error when writing default config.
    #[error("YAML serialization error: {0}")]
    Yaml(#[from] serde_yaml::Error),

    /// Error shared across `ops::*` operations.
    #[error(transparent)]
    Core(#[from] CoreError),
}

/// Result summary of workspace initialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitResult {
    /// Path to the `.gleon` directory.
    pub gleon_dir: PathBuf,
    /// Path to the created `gleon.yaml` config file, if created.
    pub config_created: Option<PathBuf>,
}

/// Initializes the `.gleon` directory structure and default `gleon.yaml` if missing.
///
/// # Errors
///
/// Returns an error if the `.gleon` directory tree cannot be created, if the default
/// `gleon.yaml` configuration fails to serialize, or if writing the `.gitignore`,
/// `.env.template`, or `gleon.yaml` scaffold files fails.
pub fn init_workspace(context: &crate::context::ResolvedContext) -> Result<InitResult, InitError> {
    let paths = GleonPaths::new(&context.base_dir);
    let gleon_dir = paths.gleon_dir();
    let blobs_dir = paths.blob_scheme_dir("sha256");
    let runs_dir = paths.runs_latest();

    std::fs::create_dir_all(&blobs_dir).map_err(CoreError::Io)?;
    std::fs::create_dir_all(&runs_dir).map_err(CoreError::Io)?;

    if let Ok(platform_key) = context.platform.to_key() {
        std::fs::create_dir_all(paths.manifests_dir(&platform_key)).map_err(CoreError::Io)?;
    } else {
        std::fs::create_dir_all(paths.manifests_root()).map_err(CoreError::Io)?;
    }

    // Scaffold .gleon/.gitignore idempotently to prevent committing blobs/ or runs/ artifacts
    append_missing_gitignore_lines(&paths.gitignore(), gleon_model::config::GITIGNORE_LINES)?;

    // Scaffold .gleon/.env.template if it does not exist
    let env_template_content = "# gleon Storage Configuration\n\
        # Copy this file to .env.local and fill in your credentials\n\
        GLEON_STORAGE_URL=\n\
        AWS_ACCESS_KEY_ID=\n\
        AWS_SECRET_ACCESS_KEY=\n\
        # For Cloudflare R2:\n\
        # R2_ACCOUNT_ID=\n";
    create_new_file_with_content(
        &gleon_dir.join(".env.template"),
        env_template_content.as_bytes(),
        &gleon_dir,
    )?;

    let internal_config = paths.config_file();
    let default_config = GleonConfig::default();
    let yaml_content = serde_yaml::to_string(&default_config).map_err(InitError::Yaml)?;
    let config_created =
        create_new_file_with_content(&internal_config, yaml_content.as_bytes(), &gleon_dir)?
            .then_some(internal_config);

    Ok(InitResult {
        gleon_dir,
        config_created,
    })
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
    use crate::context::ResolvedContext;

    #[test]
    fn test_init_workspace_creates_structure_and_config() {
        let temp_dir = tempfile::tempdir().unwrap();
        let ctx = ResolvedContext {
            base_dir: temp_dir.path().to_path_buf(),
            ..ResolvedContext::default()
        };

        let res = init_workspace(&ctx).unwrap();
        assert!(res.gleon_dir.exists());
        assert!(res.gleon_dir.join(".gitignore").exists());
        let expected_config = res.gleon_dir.join("gleon.yaml");
        assert_eq!(res.config_created, Some(expected_config.clone()));
        assert!(expected_config.exists());

        let gitignore = std::fs::read_to_string(res.gleon_dir.join(".gitignore")).unwrap();
        assert!(gitignore.contains("blobs/"));
        assert!(gitignore.contains("runs/"));
        assert!(gitignore.contains(".env.local"));
        assert!(gitignore.contains("credentials"));
        assert!(gitignore.contains("dashboard.html"));
        assert!(gitignore.contains("history.json"));

        let env_template = res.gleon_dir.join(".env.template");
        assert!(env_template.exists());
        let template_str = std::fs::read_to_string(env_template).unwrap();
        assert!(template_str.contains("GLEON_STORAGE_URL="));
    }

    #[test]
    fn test_init_platform_key_error() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            ..ResolvedContext::default()
        };
        ctx.platform.os = "invalid/os".to_string();

        let res = init_workspace(&ctx);
        assert!(res.is_ok());

        // manifests should be created without a platform sub-directory
        let manifests_dir = temp.path().join(".gleon").join("manifests");
        assert!(manifests_dir.exists());
    }

    #[test]
    fn test_init_gitignore_append_newline() {
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();

        // Write .gitignore without trailing newline
        std::fs::write(gleon_dir.join(".gitignore"), "some_ignored_file").unwrap();

        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            ..ResolvedContext::default()
        };
        let res = init_workspace(&ctx);
        assert!(res.is_ok());

        let content = std::fs::read_to_string(gleon_dir.join(".gitignore")).unwrap();
        assert!(content.contains("some_ignored_file\n"));
        assert!(content.contains("runs/"));
    }

    #[test]
    #[cfg(all(unix, not(miri)))]
    fn test_init_read_only_dir_error() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();

        // Make .gleon read-only so OpenOptions::create_new fails
        let mut perms = std::fs::metadata(&gleon_dir).unwrap().permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(&gleon_dir, perms.clone()).unwrap();

        let can_write = std::fs::write(gleon_dir.join("test.txt"), "data").is_ok();
        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            ..ResolvedContext::default()
        };
        let res = init_workspace(&ctx);

        // Restore permissions before assertions
        perms.set_mode(0o755);
        std::fs::set_permissions(&gleon_dir, perms).unwrap();

        if !can_write {
            assert!(matches!(res, Err(InitError::Core(CoreError::Io(_)))));
        }
    }
}
