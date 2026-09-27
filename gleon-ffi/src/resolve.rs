//! Safe logic behind `gleon_config_resolve`: validating `.gleon/gleon.yaml` and resolving the rule
//! of one golden, with the same code the CLI scanner uses (`gleon_model::rules`).

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use gleon_model::{
    config::{GleonConfig, MetricsConfig},
    platform::PlatformConfig,
    rules::{RuleMatch, RuleSet},
};
use serde::Serialize;

use crate::compare::{ABI_VERSION, Outcome};

/// JSON response of `gleon_config_resolve`.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Response {
    /// The config is valid and the golden was resolved.
    Resolved {
        abi: u32,
        /// Version of the engine's tolerant (SSIM) decision policy.
        policy_version: u32,
        /// The applicable screenshot rule (with the canonical test name when one matches).
        rule: RuleMatch,
        /// The `metrics:` section with the `GLEON_METRICS` override applied.
        metrics: MetricsConfig,
        /// The auto-detected host platform, named like in the CLI.
        platform: PlatformConfig,
    },
    /// Invalid config, golden path or `GLEON_METRICS` value.
    Error { abi: u32, error: String },
}

/// A parsed, validated config with its globs compiled.
#[derive(Debug)]
struct Compiled {
    yaml: Box<[u8]>,
    metrics: MetricsConfig,
    rules: RuleSet,
}

/// The last successfully compiled config.
///
/// Callers resolve every golden of a test run against the same config text, so parsing the YAML
/// and compiling its globs once instead of per golden keeps resolution at a byte comparison plus
/// glob matching. A different text replaces the entry; invalid configs are not cached.
#[derive(Debug, Default)]
struct ConfigCache(Mutex<Option<Arc<Compiled>>>);

impl ConfigCache {
    fn get(&self, yaml: &[u8]) -> Result<Arc<Compiled>, String> {
        let cached = self
            .slot()
            .as_ref()
            .filter(|entry| *entry.yaml == *yaml)
            .cloned();
        if let Some(hit) = cached {
            return Ok(hit);
        }
        // Compiled outside the lock; a concurrent caller at worst compiles the same text too.
        let text = std::str::from_utf8(yaml).map_err(|e| format!("config is not UTF-8: {e}"))?;
        let config = GleonConfig::from_yaml_str(text).map_err(|e| e.to_string())?;
        let entry = Arc::new(Compiled {
            yaml: yaml.into(),
            metrics: config.metrics,
            rules: RuleSet::new(&config).map_err(|e| format!("invalid glob set: {e}"))?,
        });
        *self.slot() = Some(Arc::clone(&entry));
        Ok(entry)
    }

    /// The cache slot. A poisoned lock only means a panic while replacing the entry; the slot
    /// still holds a valid `Option`.
    fn slot(&self) -> MutexGuard<'_, Option<Arc<Compiled>>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The cache of this process (one library copy per `flutter test` worker).
static CACHE: ConfigCache = ConfigCache(Mutex::new(None));

/// Wraps an error message into a resolve outcome.
#[must_use]
pub fn error(message: impl Into<String>) -> Outcome {
    Outcome::from_serializable(
        &Response::Error {
            abi: ABI_VERSION,
            error: message.into(),
        },
        None,
    )
}

/// Validates the UTF-8 `yaml` config and resolves the golden `path` (relative to the workspace
/// root). `metrics_env` is the caller's raw `GLEON_METRICS` value (`None` when unset).
#[must_use]
pub fn resolve(yaml: &[u8], path: &[u8], metrics_env: Option<&[u8]>) -> Outcome {
    resolve_with(&CACHE, yaml, path, metrics_env)
}

fn resolve_with(
    cache: &ConfigCache,
    yaml: &[u8],
    path: &[u8],
    metrics_env: Option<&[u8]>,
) -> Outcome {
    let run = || -> Result<Response, String> {
        let metrics_env = metrics_env
            .map(std::str::from_utf8)
            .transpose()
            .map_err(|e| format!("GLEON_METRICS is not UTF-8: {e}"))?;
        let path =
            std::str::from_utf8(path).map_err(|e| format!("golden path is not UTF-8: {e}"))?;
        let compiled = cache.get(yaml)?;
        let metrics = compiled
            .metrics
            .effective(metrics_env)
            .map_err(|e| e.to_string())?;
        let rule = compiled.rules.resolve(path).map_err(|e| e.to_string())?;
        Ok(Response::Resolved {
            abi: ABI_VERSION,
            policy_version: gleon_engine::ssim::POLICY_VERSION,
            rule,
            metrics,
            platform: PlatformConfig::host(),
        })
    };
    run().map_or_else(error, |response| {
        Outcome::from_serializable(&response, None)
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

    const YAML: &str = r#"
required_version: ">=0.1.0"
screenshots:
  - include: "test/goldens/**/*.png"
    mode: ssim
    diff: { min_similarity: 0.6, color_tolerance: 64 }
    masks:
      - path: "**/clock.png"
        zones: [{ x: 0, y: 0, width: "25%", height: 10 }]
metrics:
  enabled: true
  console: false
"#;

    fn json(outcome: &Outcome) -> serde_json::Value {
        assert!(outcome.diff_png.is_none());
        serde_json::from_slice(&outcome.json).unwrap()
    }

    #[test]
    fn test_resolves_rule_metrics_and_platform() {
        let r = json(&resolve(YAML.as_bytes(), b"test/goldens/Clock.png", None));
        assert_eq!(r["kind"], "resolved");
        assert_eq!(r["abi"], ABI_VERSION);
        assert_eq!(r["policy_version"], gleon_engine::ssim::POLICY_VERSION);
        assert_eq!(
            r["rule"],
            serde_json::json!({
                "kind": "matched",
                "index": 0,
                "name": "test/goldens/clock",
                "tolerance": {"kind": "ssim", "min_similarity": 0.6, "color_tolerance": 64.0},
                "masks": [{"x": 0, "y": 0, "width": "25%", "height": 10}]
            })
        );
        assert_eq!(
            r["metrics"],
            serde_json::json!({"enabled": true, "console": false})
        );
        assert_eq!(
            r["platform"],
            serde_json::json!({
                "os": gleon_model::platform::HOST_OS,
                "arch": gleon_model::platform::HOST_ARCH
            })
        );
    }

    #[test]
    fn test_env_overrides_metrics() {
        let r = json(&resolve(YAML.as_bytes(), b"test/goldens/a.png", Some(b"0")));
        assert_eq!(r["metrics"]["enabled"], false);
        let r = json(&resolve(
            YAML.as_bytes(),
            b"test/goldens/a.png",
            Some(b"maybe"),
        ));
        assert_eq!(r["kind"], "error");
        assert!(
            r["error"].as_str().unwrap().contains("GLEON_METRICS"),
            "{r}"
        );
    }

    #[test]
    fn test_the_compiled_config_is_cached_per_text() {
        let cache = ConfigCache::default();
        let first = cache.get(YAML.as_bytes()).unwrap();
        assert!(Arc::ptr_eq(&first, &cache.get(YAML.as_bytes()).unwrap()));
        let other = YAML.replace("0.6", "0.7");
        let second = cache.get(other.as_bytes()).unwrap();
        assert!(!Arc::ptr_eq(&first, &second));
        // An invalid text is reported and does not evict the valid entry.
        assert!(cache.get(b"required_version: [").is_err());
        assert!(Arc::ptr_eq(&second, &cache.get(other.as_bytes()).unwrap()));
        // Resolution goes through the cache and reads the rule from it.
        let r = json(&resolve_with(
            &cache,
            other.as_bytes(),
            b"test/goldens/a.png",
            None,
        ));
        assert_eq!(r["rule"]["tolerance"]["min_similarity"], 0.7);
    }

    #[test]
    fn test_unmatched_and_excluded_goldens_skip_name_validation() {
        // The scanner ignores these files, so names it would reject are fine here.
        let r = json(&resolve(YAML.as_bytes(), b"lib/Bad Name.png", None));
        assert_eq!(r["rule"], serde_json::json!({"kind": "unmatched"}));
    }

    #[test]
    fn test_unmatched_golden() {
        let r = json(&resolve(YAML.as_bytes(), b"lib/a.png", None));
        assert_eq!(r["rule"], serde_json::json!({"kind": "unmatched"}));
    }

    #[test]
    fn test_errors_carry_the_parser_message() {
        for (yaml, path, needle) in [
            (
                "required_version: \">=0.1.0\"\nunknown_key: []",
                "a.png",
                "unknown_key",
            ),
            (
                "required_version: \">=0.1.0\"\nscreenshots: []",
                "a.png",
                "at least one rule",
            ),
            (
                YAML,
                "test/goldens/with space.png",
                "not a valid gleon test path",
            ),
        ] {
            let r = json(&resolve(yaml.as_bytes(), path.as_bytes(), None));
            assert_eq!(r["kind"], "error", "{r}");
            assert!(r["error"].as_str().unwrap().contains(needle), "{r}");
        }
        let r = json(&resolve(&[0xff], b"a.png", None));
        assert!(r["error"].as_str().unwrap().contains("UTF-8"), "{r}");
        let r = json(&resolve(YAML.as_bytes(), &[0xff], None));
        assert!(r["error"].as_str().unwrap().contains("UTF-8"), "{r}");
        let r = json(&resolve(YAML.as_bytes(), b"a.png", Some(&[0xff])));
        assert!(
            r["error"].as_str().unwrap().contains("GLEON_METRICS"),
            "{r}"
        );
    }
}
