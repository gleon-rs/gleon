//! Initialization operation for gleon workspace.

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::{
    config::GleonConfig,
    context::ContextError,
    ops::common::{CoreError, append_missing_gitignore_lines, create_new_file_with_content},
    paths::GleonPaths,
    platform::{PlatformConfig, PlatformError, PlatformInfo},
};

/// Which `gleon.yaml` `gleon init` writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Template {
    /// A Flutter package or app ([`GleonConfig::for_flutter`]): the goldens under every `test/`
    /// compared exactly, their text on the platform of the run.
    Flutter,
    /// Any screenshots ([`GleonConfig::default`]): every `.png`, `pixel` mode.
    Generic,
}

impl Template {
    /// The template for the workspace at `base_dir`: [`Self::Flutter`] next to a `pubspec.yaml`
    /// (a Flutter package or app, or the root of a pub workspace), else [`Self::Generic`].
    #[must_use]
    pub fn detect(base_dir: &Path) -> Self {
        if base_dir.join("pubspec.yaml").is_file() {
            Self::Flutter
        } else {
            Self::Generic
        }
    }
}

/// Errors that can occur during workspace initialization.
#[derive(Debug, Error)]
pub enum InitError {
    /// YAML serialization error when writing default config.
    #[error("YAML serialization error: {0}")]
    Yaml(#[from] serde_yaml::Error),

    /// The platform of the run names no OS and architecture a process reports, so the Flutter
    /// template cannot name it as the platform that compares text.
    #[error(
        "the Flutter template names the platform of this run as the one that compares text \
         (`fallback_platform`), but '{platform}' is not an OS and architecture a process reports \
         ({reason}): run it without `--platform`, or with `--os` and `--arch`"
    )]
    FlutterPlatform {
        /// The platform of the run.
        platform: String,
        /// Why integrations would refuse it.
        reason: PlatformError,
    },

    /// Error shared across `ops::*` operations.
    #[error(transparent)]
    Core(#[from] CoreError),
}

/// Result summary of workspace initialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitResult {
    /// Path to the `.gleon` directory.
    pub gleon_dir: PathBuf,
    /// Path to the `gleon.yaml` config file of the workspace.
    pub config_path: PathBuf,
    /// The template the config was created from; `None` when one existed and was kept.
    pub created: Option<Template>,
}

/// Initializes the `.gleon` directory structure and, if missing, a `gleon.yaml` of `template`
/// (`None`: [`Template::detect`]).
///
/// # Errors
///
/// Returns an error if the platform has no key, or names no OS and architecture a process
/// reports for the Flutter template (nothing is created then), if the `.gleon` directory tree
/// cannot be created, if the `gleon.yaml` configuration fails to serialize, or if writing the
/// `.gitignore`, `.env.template`, or `gleon.yaml` scaffold files fails.
pub fn init_workspace(
    context: &crate::context::ResolvedContext,
    template: Option<Template>,
) -> Result<InitResult, InitError> {
    // Checked before anything is created: the manifests of the platform go to its directory.
    let platform_key = context
        .platform
        .key()
        .map_err(|e| CoreError::Context(ContextError::Platform(e)))?;
    let paths = GleonPaths::new(&context.base_dir);
    let gleon_dir = paths.gleon_dir();
    let config_path = paths.config_file();
    // An existing config is kept, whatever the template; a new one is built (and checked) first.
    let new_config = if config_path.exists() {
        None
    } else {
        let template = template.unwrap_or_else(|| Template::detect(&context.base_dir));
        Some((template, new_config(context, template)?))
    };
    let blobs_dir = paths.blob_scheme_dir("sha256");
    let runs_dir = paths.runs_latest();

    std::fs::create_dir_all(&blobs_dir).map_err(CoreError::Io)?;
    std::fs::create_dir_all(&runs_dir).map_err(CoreError::Io)?;
    std::fs::create_dir_all(paths.manifests_dir(&platform_key)).map_err(CoreError::Io)?;

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

    let mut created = None;
    if let Some((template, config)) = new_config {
        let yaml_content = serde_yaml::to_string(&config).map_err(InitError::Yaml)?;
        if create_new_file_with_content(&config_path, yaml_content.as_bytes(), &gleon_dir)? {
            created = Some(template);
        }
    }

    Ok(InitResult {
        gleon_dir,
        config_path,
        created,
    })
}

/// The config `gleon init` writes from `template`: it requires the CLI that wrote it (its
/// release, without a pre-release part), and the Flutter template names the OS and architecture
/// of the run as the platform that compares text.
fn new_config(
    context: &crate::context::ResolvedContext,
    template: Template,
) -> Result<GleonConfig, InitError> {
    let base = match template {
        Template::Flutter => GleonConfig::for_flutter(flutter_platform(&context.platform)?),
        Template::Generic => GleonConfig::default(),
    };
    Ok(GleonConfig {
        required_version: context.cli_version.as_ref().map_or_else(
            || GleonConfig::default().required_version,
            |version| {
                Some(semver::VersionReq {
                    comparators: vec![semver::Comparator {
                        op: semver::Op::GreaterEq,
                        major: version.major,
                        minor: Some(version.minor),
                        patch: Some(version.patch),
                        pre: semver::Prerelease::EMPTY,
                    }],
                })
            },
        ),
        ..base
    })
}

/// The OS and architecture of `platform` (its renderer and labels are no matter for the
/// integrations that read `fallback_platform`), written as integrations read them
/// (`macos-aarch64`).
///
/// # Errors
/// Returns [`InitError::FlutterPlatform`] for a platform no process reports (`--platform
/// my-env`): integrations would refuse the config.
fn flutter_platform(platform: &PlatformInfo) -> Result<PlatformConfig, InitError> {
    let name = platform.arch.as_ref().map_or_else(
        || platform.os.clone(),
        |arch| format!("{}-{arch}", platform.os),
    );
    let config = PlatformConfig::Opaque(name.clone());
    match config.validate_host_names() {
        Ok(()) => Ok(config),
        Err(reason) => Err(InitError::FlutterPlatform {
            platform: name,
            reason,
        }),
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
    use crate::context::{ContextError, ResolvedContext};

    #[test]
    fn test_init_workspace_creates_structure_and_config() {
        let temp_dir = tempfile::tempdir().unwrap();
        let ctx = ResolvedContext {
            base_dir: temp_dir.path().to_path_buf(),
            ..ResolvedContext::default()
        };

        let res = init_workspace(&ctx, None).unwrap();
        assert_eq!(res.created, Some(Template::Generic));
        assert!(res.gleon_dir.exists());
        assert!(res.gleon_dir.join(".gitignore").exists());
        let expected_config = res.gleon_dir.join("gleon.yaml");
        assert_eq!(res.config_path, expected_config);
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

        // An invalid platform fails before anything is created: a workspace without the
        // manifests directory of its platform would only fail later, less clearly.
        let err = init_workspace(&ctx, None).unwrap_err();
        assert!(
            matches!(
                err,
                InitError::Core(CoreError::Context(ContextError::Platform(_)))
            ),
            "{err:?}"
        );
        assert!(err.to_string().contains("invalid/os"), "{err}");
        assert!(!temp.path().join(".gleon").exists());
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
        let res = init_workspace(&ctx, None);
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
        let res = init_workspace(&ctx, None);

        // Restore permissions before assertions
        perms.set_mode(0o755);
        std::fs::set_permissions(&gleon_dir, perms).unwrap();

        if !can_write {
            assert!(matches!(res, Err(InitError::Core(CoreError::Io(_)))));
        }
    }

    /// A `pubspec.yaml` in the workspace root picks the Flutter template; an existing config is
    /// kept whatever the template asked for.
    #[test]
    fn test_template_follows_the_pubspec_and_keeps_a_config() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(Template::detect(temp.path()), Template::Generic);
        std::fs::write(temp.path().join("pubspec.yaml"), "name: app\n").unwrap();
        assert_eq!(Template::detect(temp.path()), Template::Flutter);

        let ctx = ResolvedContext {
            base_dir: temp.path().to_path_buf(),
            platform: PlatformInfo {
                os: "linux".to_owned(),
                arch: Some("x86_64".to_owned()),
                renderer: Some("skia".to_owned()),
                labels: std::collections::BTreeMap::from([("theme".to_owned(), "dark".to_owned())]),
            },
            ..ResolvedContext::default()
        };
        let res = init_workspace(&ctx, None).unwrap();
        assert_eq!(res.created, Some(Template::Flutter));
        let written = std::fs::read_to_string(&res.config_path).unwrap();
        assert!(
            written.contains("fallback_platform: linux-x86_64\n"),
            "{written}"
        );

        let again = init_workspace(&ctx, Some(Template::Generic)).unwrap();
        assert_eq!(again.created, None);
        assert_eq!(std::fs::read_to_string(&res.config_path).unwrap(), written);
    }
}
