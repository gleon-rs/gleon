use std::collections::BTreeMap;

// Platform identity types live in the permissive `gleon-model` crate (shared with the Flutter
// package through `gleon-ffi`); resolution against CLI flags and the environment stays here.
pub use gleon_model::platform::{
    PlatformConfig, PlatformError, PlatformFields, PlatformInfo, PlatformKey, validate_segment,
};

/// Platform-related values read from environment variables (`GLEON_*`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PlatformEnv {
    /// Raw `GLEON_PLATFORM` value (opaque string or key-value pairs).
    pub platform: Option<String>,
    /// Raw `GLEON_FALLBACK_PLATFORM` value.
    pub fallback_platform: Option<String>,
    /// `GLEON_OS` override.
    pub os: Option<String>,
    /// `GLEON_ARCH` override.
    pub arch: Option<String>,
    /// `GLEON_RENDERER` override.
    pub renderer: Option<String>,
}

impl PlatformEnv {
    /// Production constructor — reads from the OS process environment.
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_provider(&crate::env::OsEnv)
    }

    /// Injectable constructor — reads from any `EnvProvider`.
    pub fn from_provider(env: &dyn crate::env::EnvProvider) -> Self {
        Self {
            platform: env.get_var("GLEON_PLATFORM"),
            fallback_platform: env.get_var("GLEON_FALLBACK_PLATFORM"),
            os: env.get_var("GLEON_OS"),
            arch: env.get_var("GLEON_ARCH"),
            renderer: env.get_var("GLEON_RENDERER"),
        }
    }
}

/// CLI-supplied platform overrides, independent of any argument-parsing library.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlatformOverrides<'a> {
    /// OS override.
    pub os: Option<&'a str>,
    /// CPU architecture override.
    pub arch: Option<&'a str>,
    /// Renderer identifier override.
    pub renderer: Option<&'a str>,
    /// Additional isolation labels.
    pub labels: &'a [(String, String)],
    /// Opaque platform override string.
    pub platform: Option<&'a str>,
}

/// Resolves the final platform identity by merging CLI, environment,
/// configuration, and auto-detected sources.
pub struct PlatformResolver;

impl PlatformResolver {
    fn check_opaque_conflict(
        overrides: &PlatformOverrides<'_>,
        env: &PlatformEnv,
        env_fields: Option<&PlatformFields>,
    ) -> Result<(), PlatformError> {
        let mut conflicts = Vec::new();
        if overrides.os.is_some() || env.os.is_some() || env_fields.is_some_and(|f| f.os.is_some())
        {
            conflicts.push("OS");
        }
        if overrides.arch.is_some()
            || env.arch.is_some()
            || env_fields.is_some_and(|f| f.arch.is_some())
        {
            conflicts.push("Architecture");
        }
        if overrides.renderer.is_some()
            || env.renderer.is_some()
            || env_fields.is_some_and(|f| f.renderer.is_some())
        {
            conflicts.push("Renderer");
        }
        if !overrides.labels.is_empty() || env_fields.is_some_and(|f| f.labels.is_some()) {
            conflicts.push("Labels");
        }

        if !conflicts.is_empty() {
            return Err(PlatformError::OpaqueConflict(conflicts.join(", ")));
        }
        Ok(())
    }

    /// Resolves the final platform identity by merging all sources.
    /// Priority per field: env > CLI > config > auto-detect (os/arch only).
    ///
    /// # Errors
    /// Returns `PlatformError::ParseError` if `env.platform` fails to parse,
    /// `PlatformError::OpaqueConflict` if structured overrides are combined with an
    /// opaque platform, `PlatformError::InvalidSegment` if any resolved segment fails
    /// validation, or `PlatformError::ReservedLabelKey` if a label key collides with
    /// a reserved key.
    // Parameter list already consolidated into `PlatformOverrides` (C2); the remaining length
    // is the field-resolution body itself (os/arch/renderer/labels merge logic), not split here
    // to avoid fragmenting a single linear precedence chain across helper functions.
    #[expect(
        clippy::too_many_lines,
        reason = "long by design; see the comment above"
    )]
    pub fn resolve(
        overrides: &PlatformOverrides<'_>,
        env: &PlatformEnv,
        config: Option<&PlatformConfig>,
    ) -> Result<PlatformInfo, PlatformError> {
        const RESERVED_KEYS: &[&str] = &["os", "platform", "arch", "architecture", "renderer"];

        // Parse GLEON_PLATFORM if set
        let env_fields = match env
            .platform
            .as_deref()
            .map(PlatformFields::parse_key_value)
            .transpose()
        {
            Ok(fields) => fields,
            Err(e) => return Err(PlatformError::ParseError(e)),
        };

        // 1. Check if overrides.platform is specified. It acts as a CLI opaque override.
        if let Some(opaque_val) = overrides.platform {
            Self::check_opaque_conflict(overrides, env, env_fields.as_ref())?;

            let validated_opaque = validate_segment(opaque_val)?.into_owned();
            return Ok(PlatformInfo {
                os: validated_opaque,
                arch: None,
                renderer: None,
                labels: BTreeMap::new(),
            });
        }

        // 2. Check GLEON_PLATFORM env var. If config is Opaque, env.platform overrides it entirely.
        // If config is Structured, env.platform only overrides fields specified in it.
        let active_config =
            if env.platform.is_some() && matches!(config, Some(PlatformConfig::Opaque(_))) {
                None
            } else {
                config
            };

        // 2. Check for Opaque config conflict.
        if let Some(PlatformConfig::Opaque(opaque_val)) = active_config {
            Self::check_opaque_conflict(overrides, env, env_fields.as_ref())?;

            let validated_opaque = validate_segment(opaque_val)?.into_owned();
            return Ok(PlatformInfo {
                os: validated_opaque,
                arch: None,
                renderer: None,
                labels: BTreeMap::new(),
            });
        }

        // Resolve fields step-by-step
        let raw_os = env
            .os
            .clone()
            .or_else(|| env_fields.as_ref().and_then(|f| f.os.clone()))
            .or_else(|| overrides.os.map(String::from))
            .or_else(|| {
                if let Some(PlatformConfig::Structured(fields)) = active_config {
                    fields.os.clone()
                } else {
                    None
                }
            })
            .unwrap_or_else(|| gleon_model::platform::HOST_OS.to_owned());
        let resolved_os = validate_segment(&raw_os)?.into_owned();

        let raw_arch = env
            .arch
            .clone()
            .or_else(|| env_fields.as_ref().and_then(|f| f.arch.clone()))
            .or_else(|| overrides.arch.map(String::from))
            .or_else(|| {
                if let Some(PlatformConfig::Structured(fields)) = active_config {
                    fields.arch.clone()
                } else {
                    None
                }
            })
            .unwrap_or_else(|| gleon_model::platform::HOST_ARCH.to_owned());
        let resolved_arch = Some(validate_segment(&raw_arch)?.into_owned());

        let resolved_renderer = env
            .renderer
            .clone()
            .or_else(|| env_fields.as_ref().and_then(|f| f.renderer.clone()))
            .or_else(|| overrides.renderer.map(String::from))
            .or_else(|| {
                if let Some(PlatformConfig::Structured(fields)) = active_config {
                    fields.renderer.clone()
                } else {
                    None
                }
            })
            .map(|r| validate_segment(&r).map(std::borrow::Cow::into_owned))
            .transpose()?;

        // Merge labels
        let mut resolved_labels = BTreeMap::new();

        let mut insert_label = |k: &str, v: &str| -> Result<(), PlatformError> {
            let valid_key = validate_segment(k)?;
            let key_str: &str = &valid_key;
            if RESERVED_KEYS.contains(&key_str) {
                let suggested = match key_str {
                    "architecture" => "arch".to_string(),
                    other => other.to_string(),
                };
                return Err(PlatformError::ReservedLabelKey(
                    valid_key.into_owned(),
                    suggested,
                ));
            }
            let valid_val = validate_segment(v)?;
            resolved_labels.insert(valid_key.into_owned(), valid_val.into_owned());
            Ok(())
        };

        // 1. Config labels
        if let Some(PlatformConfig::Structured(PlatformFields {
            labels: Some(config_labels),
            ..
        })) = active_config
        {
            for (k, v) in config_labels {
                insert_label(k, v)?;
            }
        }

        // 2. CLI labels (override config)
        for (k, v) in overrides.labels {
            insert_label(k, v)?;
        }

        // 3. Env labels (override CLI and config)
        if let Some(PlatformFields {
            labels: Some(env_labels),
            ..
        }) = env_fields.as_ref()
        {
            for (k, v) in env_labels {
                insert_label(k, v)?;
            }
        }

        Ok(PlatformInfo {
            os: resolved_os,
            arch: resolved_arch,
            renderer: resolved_renderer,
            labels: resolved_labels,
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
    use super::*;

    #[test]
    fn test_resolve_opaque_conflict() {
        let config = PlatformConfig::Opaque("custom-opaque".to_string());
        let env = PlatformEnv::default();

        // No overrides: succeeds
        let res = PlatformResolver::resolve(
            &PlatformOverrides {
                os: None,
                arch: None,
                renderer: None,
                labels: &[],
                platform: None,
            },
            &env,
            Some(&config),
        );
        assert!(res.is_ok());
        assert_eq!(res.unwrap().os, "custom-opaque");

        // Override architecture: conflict
        let res_conflict = PlatformResolver::resolve(
            &PlatformOverrides {
                os: None,
                arch: Some("x86_64"),
                renderer: None,
                labels: &[],
                platform: None,
            },
            &env,
            Some(&config),
        );
        assert!(res_conflict.is_err());
        assert!(matches!(
            res_conflict.unwrap_err(),
            PlatformError::OpaqueConflict(_)
        ));
    }

    #[test]
    fn test_resolve_precedence() {
        let config = PlatformConfig::Structured(PlatformFields {
            os: Some("config-os".to_string()),
            arch: Some("config-arch".to_string()),
            renderer: Some("config-renderer".to_string()),
            labels: None,
        });

        // 1. Config only (no CLI, no Env) -> uses config
        let empty_env = PlatformEnv::default();
        let res = PlatformResolver::resolve(
            &PlatformOverrides {
                os: None,
                arch: None,
                renderer: None,
                labels: &[],
                platform: None,
            },
            &empty_env,
            Some(&config),
        )
        .unwrap();
        assert_eq!(res.os, "config-os");
        assert_eq!(res.arch.as_deref(), Some("config-arch"));
        assert_eq!(res.renderer.as_deref(), Some("config-renderer"));

        // 2. CLI overrides Config
        let res = PlatformResolver::resolve(
            &PlatformOverrides {
                os: Some("cli-os"),
                arch: Some("cli-arch"),
                renderer: Some("cli-renderer"),
                labels: &[],
                platform: None,
            },
            &empty_env,
            Some(&config),
        )
        .unwrap();
        assert_eq!(res.os, "cli-os");
        assert_eq!(res.arch.as_deref(), Some("cli-arch"));
        assert_eq!(res.renderer.as_deref(), Some("cli-renderer"));

        // 3. GLEON_PLATFORM (env compound) overrides CLI and Config
        let env_platform = PlatformEnv {
            platform: Some("os=env-plat-os,renderer=env-plat-renderer,theme=dark".to_string()),
            ..Default::default()
        };
        let res = PlatformResolver::resolve(
            &PlatformOverrides {
                os: Some("cli-os"),
                arch: Some("cli-arch"),
                renderer: Some("cli-renderer"),
                labels: &[],
                platform: None,
            },
            &env_platform,
            Some(&config),
        )
        .unwrap();
        assert_eq!(res.os, "env-plat-os");
        assert_eq!(res.arch.as_deref(), Some("cli-arch")); // CLI arch is used since env-platform didn't define arch
        assert_eq!(res.renderer.as_deref(), Some("env-plat-renderer"));
        assert_eq!(res.labels.get("theme").map(String::as_str), Some("dark"));

        // 4. Specific env variables override GLEON_PLATFORM
        let specific_env = PlatformEnv {
            platform: Some("os=env-plat-os,renderer=env-plat-renderer".to_string()),
            os: Some("specific-env-os".to_string()),
            renderer: Some("specific-env-renderer".to_string()),
            ..Default::default()
        };
        let res = PlatformResolver::resolve(
            &PlatformOverrides {
                os: Some("cli-os"),
                arch: Some("cli-arch"),
                renderer: Some("cli-renderer"),
                labels: &[],
                platform: None,
            },
            &specific_env,
            Some(&config),
        )
        .unwrap();
        assert_eq!(res.os, "specific-env-os");
        assert_eq!(res.renderer.as_deref(), Some("specific-env-renderer"));
    }

    #[test]
    fn test_resolve_opaque_conflict_via_env() {
        let config = PlatformConfig::Opaque("custom".into());
        let env = PlatformEnv {
            os: Some("linux".into()),
            ..Default::default()
        };
        let res = PlatformResolver::resolve(
            &PlatformOverrides {
                os: None,
                arch: None,
                renderer: None,
                labels: &[],
                platform: None,
            },
            &env,
            Some(&config),
        );
        assert!(matches!(res.unwrap_err(), PlatformError::OpaqueConflict(_)));
    }

    #[test]
    fn test_resolve_opaque_bypassed_by_gleon_platform() {
        let config = PlatformConfig::Opaque("custom".into());
        let env = PlatformEnv {
            platform: Some("os=override".into()),
            ..Default::default()
        };
        let res = PlatformResolver::resolve(
            &PlatformOverrides {
                os: None,
                arch: None,
                renderer: None,
                labels: &[],
                platform: None,
            },
            &env,
            Some(&config),
        );
        assert!(res.is_ok());
        assert_eq!(res.unwrap().os, "override");
    }

    #[test]
    fn test_reserved_label_key_rejected() {
        let env = PlatformEnv::default();
        let labels = vec![("os".into(), "linux".into())];
        let res = PlatformResolver::resolve(
            &PlatformOverrides {
                os: None,
                arch: None,
                renderer: None,
                labels: &labels,
                platform: None,
            },
            &env,
            None,
        );
        assert_eq!(
            res.unwrap_err(),
            PlatformError::ReservedLabelKey("os".to_string(), "os".to_string())
        );

        // Test synonym mapping
        let labels_syn = vec![("architecture".into(), "x86_64".into())];
        let res_syn = PlatformResolver::resolve(
            &PlatformOverrides {
                os: None,
                arch: None,
                renderer: None,
                labels: &labels_syn,
                platform: None,
            },
            &env,
            None,
        );
        assert_eq!(
            res_syn.unwrap_err(),
            PlatformError::ReservedLabelKey("architecture".to_string(), "arch".to_string())
        );
    }

    #[test]
    fn test_opaque_validation_fails_on_invalid() {
        let config = PlatformConfig::Opaque("mac os".to_string());
        let env = PlatformEnv::default();
        let res = PlatformResolver::resolve(
            &PlatformOverrides {
                os: None,
                arch: None,
                renderer: None,
                labels: &[],
                platform: None,
            },
            &env,
            Some(&config),
        );
        assert!(res.is_err());
        assert!(matches!(res.unwrap_err(), PlatformError::InvalidSegment(_)));
    }

    #[test]
    fn test_reserved_label_case_insensitive_rejected() {
        let env = PlatformEnv::default();
        let labels = vec![("OS".to_string(), "linux".to_string())];
        let res = PlatformResolver::resolve(
            &PlatformOverrides {
                os: None,
                arch: None,
                renderer: None,
                labels: &labels,
                platform: None,
            },
            &env,
            None,
        );
        assert_eq!(
            res.unwrap_err(),
            PlatformError::ReservedLabelKey("os".to_string(), "os".to_string())
        );

        let labels_mixed = vec![("Platform".to_string(), "macos".to_string())];
        let res_mixed = PlatformResolver::resolve(
            &PlatformOverrides {
                os: None,
                arch: None,
                renderer: None,
                labels: &labels_mixed,
                platform: None,
            },
            &env,
            None,
        );
        assert_eq!(
            res_mixed.unwrap_err(),
            PlatformError::ReservedLabelKey("platform".to_string(), "platform".to_string())
        );
    }

    #[test]
    fn test_partial_env_config_merge() {
        let config = PlatformConfig::Structured(PlatformFields {
            os: Some("linux".to_string()),
            arch: Some("x86_64".to_string()),
            renderer: Some("firefox".to_string()),
            labels: None,
        });

        // env.platform specifies only renderer=chrome. Config os/arch should be preserved.
        let env = PlatformEnv {
            platform: Some("renderer=chrome".to_string()),
            ..Default::default()
        };

        let res = PlatformResolver::resolve(
            &PlatformOverrides {
                os: None,
                arch: None,
                renderer: None,
                labels: &[],
                platform: None,
            },
            &env,
            Some(&config),
        )
        .unwrap();
        assert_eq!(res.os, "linux");
        assert_eq!(res.arch.as_deref(), Some("x86_64"));
        assert_eq!(res.renderer.as_deref(), Some("chrome"));
    }

    #[test]
    fn test_resolve_opaque_conflict_all_overrides() {
        let config = PlatformConfig::Opaque("custom-opaque".to_string());
        let env = PlatformEnv {
            os: Some("linux".to_string()),
            arch: Some("x86_64".to_string()),
            renderer: Some("chrome".to_string()),
            ..Default::default()
        };
        let labels = vec![("theme".to_string(), "dark".to_string())];
        let res = PlatformResolver::resolve(
            &PlatformOverrides {
                os: None,
                arch: None,
                renderer: None,
                labels: &labels,
                platform: None,
            },
            &env,
            Some(&config),
        );
        assert!(res.is_err());
        let err = res.unwrap_err().to_string();
        assert!(err.contains("OS"));
        assert!(err.contains("Architecture"));
        assert!(err.contains("Renderer"));
        assert!(err.contains("Labels"));
    }

    #[test]
    fn test_resolve_cli_platform_success() {
        let env = PlatformEnv::default();
        let res = PlatformResolver::resolve(
            &PlatformOverrides {
                os: None,
                arch: None,
                renderer: None,
                labels: &[],
                platform: Some("custom-opaque"),
            },
            &env,
            None,
        )
        .unwrap();
        assert_eq!(res.os, "custom-opaque");
        assert_eq!(res.arch, None);
        assert_eq!(res.renderer, None);
    }

    #[test]
    fn test_resolve_cli_platform_conflict() {
        let env = PlatformEnv::default();
        let res = PlatformResolver::resolve(
            &PlatformOverrides {
                os: None,
                arch: Some("x86_64"),
                renderer: None,
                labels: &[],
                platform: Some("custom-opaque"),
            },
            &env,
            None,
        );
        assert!(res.is_err());
        assert!(matches!(res.unwrap_err(), PlatformError::OpaqueConflict(_)));
    }

    #[test]
    fn test_resolve_cli_platform_conflict_all() {
        let env = PlatformEnv {
            os: Some("linux".to_string()),
            arch: Some("x86_64".to_string()),
            renderer: Some("chrome".to_string()),
            ..Default::default()
        };
        let labels = vec![("theme".to_string(), "dark".to_string())];
        let res = PlatformResolver::resolve(
            &PlatformOverrides {
                os: None,
                arch: None,
                renderer: None,
                labels: &labels,
                platform: Some("custom-opaque"),
            },
            &env,
            None,
        );
        assert!(res.is_err());
        let err = res.unwrap_err().to_string();
        assert!(err.contains("OS"));
        assert!(err.contains("Architecture"));
        assert!(err.contains("Renderer"));
        assert!(err.contains("Labels"));
    }

    #[test]
    fn test_resolve_cli_platform_conflict_with_env_platform() {
        let env = PlatformEnv {
            platform: Some("os=linux,arch=x86_64,renderer=chrome,theme=dark".to_string()),
            ..Default::default()
        };
        let res = PlatformResolver::resolve(
            &PlatformOverrides {
                os: None,
                arch: None,
                renderer: None,
                labels: &[],
                platform: Some("custom-opaque"),
            },
            &env,
            None,
        );
        assert!(res.is_err());
        let err = res.unwrap_err().to_string();
        assert!(err.contains("OS"));
        assert!(err.contains("Architecture"));
        assert!(err.contains("Renderer"));
        assert!(err.contains("Labels"));
    }

    #[test]
    fn test_resolve_cli_platform_success_with_empty_env_platform() {
        let env = PlatformEnv {
            platform: Some(String::new()),
            ..Default::default()
        };
        let res = PlatformResolver::resolve(
            &PlatformOverrides {
                os: None,
                arch: None,
                renderer: None,
                labels: &[],
                platform: Some("custom-opaque"),
            },
            &env,
            None,
        );
        assert!(res.is_ok());
        let info = res.unwrap();
        assert_eq!(info.os, "custom-opaque");
        assert_eq!(info.arch, None);
        assert_eq!(info.renderer, None);
        assert!(info.labels.is_empty());
    }
}
