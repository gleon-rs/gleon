use crate::{
    config::{ConfigError, GleonConfig},
    platform::{PlatformEnv, PlatformError, PlatformInfo, PlatformOverrides, PlatformResolver},
};

/// Errors that can occur while resolving a `ResolvedContext` from CLI arguments.
#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    /// Loading or parsing the `gleon.yaml` configuration file failed.
    #[error("Configuration error: {0}")]
    Config(#[from] ConfigError),
    /// Resolving the platform identity failed.
    #[error("Platform error: {0}")]
    Platform(#[from] PlatformError),
    /// Resolving the current Git branch failed with a non-recoverable error.
    #[error("Git error: {0}")]
    Git(#[from] crate::git::GitError),
}

/// Traverses parent directories starting from `start_dir` to find `.gleon/gleon.yaml`.
///
/// Returns `Some((config_path, root_dir))` if found, or `None` if not found.
#[must_use]
pub fn find_config_and_root(
    start_dir: &std::path::Path,
) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let paths = crate::paths::find_workspace_root(start_dir, |p| p.config_file().is_file())?;
    Some((paths.config_file(), paths.base_dir().to_path_buf()))
}

/// Platform/branch/config overrides used to resolve a [`ResolvedContext`], independent of any
/// CLI argument-parsing library.
#[derive(Debug, Clone, Default)]
pub struct ContextOptions {
    /// Explicit path to a `gleon.yaml` configuration file, bypassing directory discovery.
    pub config_path: Option<std::path::PathBuf>,
    /// OS platform override.
    pub os: Option<String>,
    /// CPU architecture platform override.
    pub arch: Option<String>,
    /// Renderer platform override.
    pub renderer: Option<String>,
    /// Additional platform isolation labels.
    pub labels: Vec<(String, String)>,
    /// Opaque platform override string.
    pub platform: Option<String>,
    /// Branch name override.
    pub branch: Option<String>,
    /// Target branch to compare against; empty/whitespace-only is treated as `"main"`.
    pub target_branch: String,
}

/// Fully resolved runtime context for a `gleon` command invocation,
/// combining CLI arguments, discovered configuration, platform identity, and Git state.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ResolvedContext {
    /// The loaded `gleon.yaml` configuration, or `None` if none was found.
    pub config: Option<GleonConfig>,
    /// The resolved platform identity used for baseline isolation.
    pub platform: PlatformInfo,
    /// The resolved fallback platform key, if a fallback platform was configured.
    pub fallback_platform_key: Option<String>,
    /// The resolved current branch name.
    pub branch: String,
    /// The resolved target branch name to compare against.
    pub target_branch: String,
    /// The resolved repository/configuration root directory.
    pub base_dir: std::path::PathBuf,
    /// The resolved Git HEAD commit SHA, or `None` if not inside a Git repository or offline.
    pub commit_sha: Option<String>,
}

impl Default for ResolvedContext {
    fn default() -> Self {
        Self {
            config: None,
            platform: PlatformInfo {
                os: "unknown".to_string(),
                arch: None,
                renderer: None,
                labels: std::collections::BTreeMap::new(),
            },
            fallback_platform_key: None,
            branch: "main".to_string(),
            target_branch: "main".to_string(),
            base_dir: std::path::PathBuf::from("."),
            commit_sha: None,
        }
    }
}

impl ResolvedContext {
    /// Builds a `ResolvedContext` from resolved options, reading configuration from disk
    /// and platform/environment variables from the real OS process environment.
    ///
    /// # Errors
    /// Returns `ContextError::Config` if the `gleon.yaml` configuration fails to load
    /// or parse, `ContextError::Platform` if the platform identity cannot be resolved,
    /// or `ContextError::Git` if the current branch name is invalid.
    pub fn from_options(
        options: &ContextOptions,
        base_dir: &std::path::Path,
    ) -> Result<Self, ContextError> {
        Self::resolve(options, base_dir, &crate::env::OsEnv)
    }

    /// Builds a `ResolvedContext` from resolved options using an injectable `EnvProvider`,
    /// allowing environment variables to be mocked in tests.
    ///
    /// # Errors
    /// Returns `ContextError::Config` if the `gleon.yaml` configuration fails to load
    /// or parse, `ContextError::Platform` if the platform identity or fallback platform
    /// cannot be resolved, or `ContextError::Git` if the current branch name is invalid.
    pub fn resolve(
        options: &ContextOptions,
        base_dir: &std::path::Path,
        env_provider: &dyn crate::env::EnvProvider,
    ) -> Result<Self, ContextError> {
        let platform_env = PlatformEnv::from_provider(env_provider);

        let (config, resolved_base_dir) = if let Some(ref path) = options.config_path {
            tracing::debug!(
                "Loading configuration from explicitly provided path: {:?}",
                path
            );
            let resolved_path = if path.is_absolute() {
                path.clone()
            } else {
                base_dir.join(path)
            };
            let cfg = GleonConfig::load_from_file(&resolved_path)?;
            let config_dir = resolved_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."));
            let root = crate::paths::find_workspace_root(config_dir, |p| p.gleon_dir().is_dir())
                .or_else(|| crate::paths::find_workspace_root(base_dir, |p| p.gleon_dir().is_dir()))
                .map_or_else(|| base_dir.to_path_buf(), |p| p.base_dir().to_path_buf());
            (Some(cfg), root)
        } else if let Some((config_path, root_dir)) = find_config_and_root(base_dir) {
            tracing::debug!(
                "Discovered gleon.yaml at {:?} (root: {:?})",
                config_path,
                root_dir
            );
            let cfg = GleonConfig::load_from_file(&config_path)?;
            (Some(cfg), root_dir)
        } else {
            (None, base_dir.to_path_buf())
        };

        let overrides = PlatformOverrides {
            os: options.os.as_deref(),
            arch: options.arch.as_deref(),
            renderer: options.renderer.as_deref(),
            labels: &options.labels,
            platform: options.platform.as_deref(),
        };
        let platform = PlatformResolver::resolve(
            &overrides,
            &platform_env,
            config.as_ref().and_then(|c| c.platform.as_ref()),
        )?;

        let fallback_platform_key = if let Some(ref fb_env) = platform_env.fallback_platform {
            let plat_cfg = crate::platform::PlatformFields::parse_key_value(fb_env).map_or_else(
                |_| crate::platform::PlatformConfig::Opaque(fb_env.clone()),
                crate::platform::PlatformConfig::Structured,
            );
            Some(plat_cfg.to_key().map_err(ContextError::Platform)?)
        } else if let Some(ref cfg) = config {
            cfg.fallback_platform
                .as_ref()
                .map(|fb_cfg| fb_cfg.to_key().map_err(ContextError::Platform))
                .transpose()?
        } else {
            None
        };

        let branch = match crate::git::GitResolver::resolve_branch_impl(
            options.branch.as_deref(),
            &resolved_base_dir,
            env_provider,
        ) {
            Ok(b) => b,
            Err(e @ crate::git::GitError::InvalidBranchName(_)) => {
                return Err(ContextError::Git(e));
            }
            Err(e) => {
                tracing::debug!(
                    "Git branch resolution failed: {}. Falling back to 'main' for offline mode.",
                    e
                );
                "main".to_string()
            }
        };

        let target_branch = if options.target_branch.trim().is_empty() {
            "main".to_string()
        } else {
            options.target_branch.clone()
        };

        let commit_sha = match crate::git::GitResolver::get_head_commit_sha(&resolved_base_dir) {
            Ok(sha) => Some(sha),
            Err(e) => {
                tracing::debug!("Git commit SHA resolution skipped or failed: {e}");
                None
            }
        };

        Ok(Self {
            config,
            platform,
            fallback_platform_key,
            branch,
            target_branch,
            base_dir: resolved_base_dir,
            commit_sha,
        })
    }
}

#[cfg(test)]
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
    use std::{fs::File, io::Write, path::PathBuf};

    use tempfile::tempdir;

    use super::*;

    struct EmptyEnv;
    impl crate::env::EnvProvider for EmptyEnv {
        fn get_var(&self, _key: &str) -> Option<String> {
            None
        }
    }

    struct MapEnv(std::collections::HashMap<&'static str, &'static str>);
    impl crate::env::EnvProvider for MapEnv {
        fn get_var(&self, key: &str) -> Option<String> {
            self.0.get(key).map(|v| (*v).to_string())
        }
    }

    fn create_mock_git_repo(path: &std::path::Path, head_content: &str) {
        let git_dir = path.join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::create_dir_all(git_dir.join("objects")).unwrap();
        std::fs::create_dir_all(git_dir.join("refs")).unwrap();
        std::fs::write(git_dir.join("HEAD"), head_content).unwrap();
    }

    #[test]
    fn test_from_cli_with_config_path() {
        let dir = tempdir().unwrap();
        create_mock_git_repo(dir.path(), "ref: refs/heads/main\n");
        let config_path = dir.path().join("my_config.yaml");
        let mut file = File::create(&config_path).unwrap();
        writeln!(
            file,
            "required_version: \">=0.1.0\"\nscreenshots:\n  - include: \"*.png\""
        )
        .unwrap();

        let options = ContextOptions {
            target_branch: "develop".to_string(),
            config_path: Some(config_path),
            ..Default::default()
        };

        let context = ResolvedContext::resolve(&options, dir.path(), &EmptyEnv).unwrap();
        assert!(context.config.is_some());
        assert_eq!(context.branch, "main");
        assert_eq!(context.target_branch, "develop");
    }

    #[test]
    fn test_from_cli_with_relative_config_path() {
        let dir = tempdir().unwrap();
        create_mock_git_repo(dir.path(), "ref: refs/heads/main\n");
        let sub_dir = dir.path().join("configs");
        std::fs::create_dir_all(&sub_dir).unwrap();
        let config_file = sub_dir.join("custom.yaml");
        let mut file = File::create(&config_file).unwrap();
        writeln!(
            file,
            "required_version: \">=0.1.0\"\nscreenshots:\n  - include: \"*.png\""
        )
        .unwrap();

        let relative_path = PathBuf::from("configs/custom.yaml");
        let options = ContextOptions {
            config_path: Some(relative_path),
            ..Default::default()
        };

        let context = ResolvedContext::resolve(&options, dir.path(), &EmptyEnv).unwrap();
        assert!(context.config.is_some());
    }

    #[test]
    fn test_resolve_derives_base_dir_from_explicit_config_path() {
        let external_dir = tempdir().unwrap();
        let unrelated_dir = tempdir().unwrap();
        create_mock_git_repo(external_dir.path(), "ref: refs/heads/feature\n");

        let gleon_dir = external_dir.path().join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();
        let config_path = gleon_dir.join("gleon.yaml");
        let mut file = File::create(&config_path).unwrap();
        writeln!(
            file,
            "required_version: \">=0.1.0\"\nscreenshots:\n  - include: \"*.png\""
        )
        .unwrap();

        let options = ContextOptions {
            config_path: Some(config_path),
            ..Default::default()
        };

        // Pass unrelated_dir as the base_dir argument; resolve must derive base_dir from config_path's root.
        let context = ResolvedContext::resolve(&options, unrelated_dir.path(), &EmptyEnv).unwrap();
        assert_eq!(context.base_dir, external_dir.path());
        assert_eq!(context.branch, "feature");
    }

    #[test]
    fn test_from_cli_no_config_no_default_file() {
        let dir = tempdir().unwrap();
        create_mock_git_repo(dir.path(), "ref: refs/heads/main\n");
        let options = ContextOptions {
            target_branch: "develop".to_string(),
            ..Default::default()
        };

        let context = ResolvedContext::resolve(&options, dir.path(), &EmptyEnv).unwrap();
        assert!(context.config.is_none());
        assert_eq!(context.branch, "main");
        assert_eq!(context.target_branch, "develop");
    }

    #[test]
    fn test_from_cli_no_config_with_default_file() {
        let dir = tempdir().unwrap();
        create_mock_git_repo(dir.path(), "ref: refs/heads/main\n");
        let gleon_dir = dir.path().join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();
        let default_path = gleon_dir.join("gleon.yaml");
        let mut file = File::create(&default_path).unwrap();
        writeln!(
            file,
            "required_version: \">=0.1.0\"\nscreenshots:\n  - include: \"*.png\""
        )
        .unwrap();

        let options = ContextOptions {
            target_branch: "develop".to_string(),
            ..Default::default()
        };

        let context = ResolvedContext::resolve(&options, dir.path(), &EmptyEnv).unwrap();
        assert!(context.config.is_some());
        assert_eq!(context.branch, "main");
        assert_eq!(context.target_branch, "develop");
    }

    #[test]
    fn test_from_cli_production_wrapper() {
        let dir = tempdir().unwrap();
        create_mock_git_repo(dir.path(), "ref: refs/heads/main\n");
        let options = ContextOptions {
            branch: Some("main".to_string()),
            target_branch: "develop".to_string(),
            os: Some("linux".to_string()),
            arch: Some("x86_64".to_string()),
            ..Default::default()
        };
        let context = ResolvedContext::from_options(&options, dir.path()).unwrap();
        assert_eq!(context.branch, "main");
    }

    #[test]
    fn test_from_cli_errors() {
        let dir = tempdir().unwrap();

        // 1. Platform resolver error
        let options_platform_err = ContextOptions {
            branch: Some("main".to_string()),
            target_branch: "develop".to_string(),
            platform: Some("custom-opaque".to_string()),
            ..Default::default()
        };
        let env_conflict = MapEnv(std::collections::HashMap::from([("GLEON_OS", "linux")]));
        let result = ResolvedContext::resolve(&options_platform_err, dir.path(), &env_conflict);
        assert!(result.is_err());

        // 2. Git resolver error propagation (invalid branch name is returned as Err)
        let options_git_err = ContextOptions {
            branch: Some("invalid branch name space".to_string()),
            target_branch: "develop".to_string(),
            ..Default::default()
        };
        let result = ResolvedContext::resolve(&options_git_err, dir.path(), &EmptyEnv);
        assert!(matches!(
            result,
            Err(ContextError::Git(crate::git::GitError::InvalidBranchName(
                _
            )))
        ));
    }

    #[test]
    fn test_from_cli_root_discovery() {
        let dir = tempdir().unwrap();
        let root_dir = dir.path();
        let nested_dir = root_dir.join("src/features/billing");
        std::fs::create_dir_all(&nested_dir).unwrap();
        let gleon_dir = root_dir.join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();

        let config_path = gleon_dir.join("gleon.yaml");
        let yaml_content = "required_version: \">=0.1.0\"\nscreenshots:\n  - include: \"*.png\"";
        std::fs::write(&config_path, yaml_content).unwrap();

        let options = ContextOptions::default();

        // Call from nested_dir
        let ctx = ResolvedContext::resolve(&options, &nested_dir, &EmptyEnv).unwrap();

        assert!(ctx.config.is_some());
        assert_eq!(ctx.base_dir, root_dir);
    }

    #[test]
    fn test_from_cli_corrupted_discovered_config() {
        let dir = tempdir().unwrap();
        let root_dir = dir.path();
        let gleon_dir = root_dir.join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();
        let config_path = gleon_dir.join("gleon.yaml");
        std::fs::write(&config_path, "invalid_yaml: : : [bad syntax]").unwrap();

        let options = ContextOptions::default();

        let result = ResolvedContext::resolve(&options, root_dir, &EmptyEnv);

        assert!(result.is_err());
        assert!(matches!(result, Err(ContextError::Config(_))));
    }

    #[test]
    fn test_fallback_platform_key_resolution() {
        let dir = tempdir().unwrap();
        let root_dir = dir.path();
        let gleon_dir = root_dir.join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();
        let config_path = gleon_dir.join("gleon.yaml");
        let yaml_content = "required_version: \">=0.1.0\"\nfallback_platform:\n  os: linux\n  arch: x86_64\nscreenshots:\n  - include: \"*.png\"";
        std::fs::write(&config_path, yaml_content).unwrap();

        let options = ContextOptions::default();

        // 1. Resolve from config
        let ctx = ResolvedContext::resolve(&options, root_dir, &EmptyEnv).unwrap();
        assert_eq!(ctx.fallback_platform_key.as_deref(), Some("linux-x86_64"));

        // 2. Resolve from env (overrides config)
        let env = MapEnv(std::collections::HashMap::from([(
            "GLEON_FALLBACK_PLATFORM",
            "macos-aarch64",
        )]));
        let ctx_env = ResolvedContext::resolve(&options, root_dir, &env).unwrap();
        assert_eq!(
            ctx_env.fallback_platform_key.as_deref(),
            Some("macos-aarch64")
        );

        // 3. An opaque value in the config names the manifests of the resolved platform.
        std::fs::write(
            &config_path,
            yaml_content.replace(
                "fallback_platform:\n  os: linux\n  arch: x86_64",
                "fallback_platform: linux-x86_64",
            ),
        )
        .unwrap();
        let linux = ContextOptions {
            os: Some("linux".to_owned()),
            arch: Some("x86_64".to_owned()),
            ..ContextOptions::default()
        };
        let ctx = ResolvedContext::resolve(&linux, root_dir, &EmptyEnv).unwrap();
        assert_eq!(
            ctx.fallback_platform_key,
            Some(ctx.platform.to_key().unwrap())
        );
    }

    #[test]
    fn test_resolved_context_default() {
        let ctx = ResolvedContext::default();
        assert_eq!(ctx.platform.os, "unknown");
        assert_eq!(ctx.platform.arch, None);
        assert_eq!(ctx.platform.renderer, None);
        assert_eq!(ctx.branch, "main");
    }

    #[test]
    fn test_invalid_fallback_platform_error() {
        let temp = tempdir().unwrap();
        let root_dir = temp.path();

        let options = ContextOptions::default();

        // Invalid fallback in env
        let env_invalid = MapEnv(std::collections::HashMap::from([(
            "GLEON_FALLBACK_PLATFORM",
            "INVALID/OS",
        )]));
        let err = ResolvedContext::resolve(&options, root_dir, &env_invalid).unwrap_err();
        assert!(matches!(err, ContextError::Platform(_)));

        // Invalid fallback in config
        let gleon_dir = root_dir.join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();
        let config_path = gleon_dir.join("invalid_gleon.yaml");
        let invalid_cfg = GleonConfig {
            fallback_platform: Some(crate::platform::PlatformConfig::Opaque(
                "INVALID/OS".to_string(),
            )),
            ..Default::default()
        };
        let yaml_str = serde_yaml::to_string(&invalid_cfg).unwrap();
        std::fs::write(&config_path, yaml_str).unwrap();

        let options_cfg = ContextOptions {
            config_path: Some(config_path),
            ..options
        };
        let err_cfg = ResolvedContext::resolve(&options_cfg, root_dir, &EmptyEnv).unwrap_err();
        assert!(matches!(
            err_cfg,
            ContextError::Config(_) | ContextError::Platform(_)
        ));
    }

    #[test]
    fn test_target_branch_defaulting_and_retention() {
        let temp = tempdir().unwrap();
        let root_dir = temp.path();

        let make_options = |tb: &str| ContextOptions {
            target_branch: tb.to_string(),
            ..Default::default()
        };

        // Empty string defaults to "main"
        let ctx_empty = ResolvedContext::resolve(&make_options(""), root_dir, &EmptyEnv).unwrap();
        assert_eq!(ctx_empty.target_branch, "main");

        // Whitespace-only defaults to "main"
        let ctx_spaces =
            ResolvedContext::resolve(&make_options("   "), root_dir, &EmptyEnv).unwrap();
        assert_eq!(ctx_spaces.target_branch, "main");

        // Explicit non-empty target branch retained
        let ctx_develop =
            ResolvedContext::resolve(&make_options("develop"), root_dir, &EmptyEnv).unwrap();
        assert_eq!(ctx_develop.target_branch, "develop");
    }

    #[test]
    #[cfg(not(miri))]
    fn test_context_resolve_head_commit_sha() {
        fn run_check(predicate: impl Fn(&crate::paths::GleonPaths) -> bool) {
            let cur = std::env::current_dir().unwrap();
            let Some(paths) = crate::paths::find_workspace_root(&cur, predicate) else {
                return;
            };
            let ctx =
                ResolvedContext::resolve(&ContextOptions::default(), paths.base_dir(), &EmptyEnv)
                    .unwrap();
            assert!(ctx.commit_sha.is_some());
        }

        run_check(|p| p.base_dir().join(".git").exists());
        run_check(|_| false);
    }
}
