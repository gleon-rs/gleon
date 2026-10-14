//! Environment variable access, injection, and `.gleon/.env` file loading.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use tracing::debug;

/// Abstraction over environment variable access.
///
/// Letting call sites depend on this trait (instead of `std::env` directly) allows tests to
/// inject a fake environment rather than mutating the real process environment, which is
/// unsound under Cargo's default parallel test execution.
pub trait EnvProvider: Sync {
    /// Gets the environment variable value.
    fn get_var(&self, key: &str) -> Option<String>;

    /// Returns `true` if the environment variable is set.
    ///
    /// The default implementation defers to [`EnvProvider::get_var`]. Implementations backed
    /// by the real process environment should override this to answer a presence check
    /// without decoding and allocating a value the caller only wants to check for.
    fn has_var(&self, key: &str) -> bool {
        self.get_var(key).is_some()
    }
}

/// Standard OS environment variable provider.
pub struct OsEnv;

impl EnvProvider for OsEnv {
    fn get_var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }

    fn has_var(&self, key: &str) -> bool {
        std::env::var_os(key).is_some()
    }
}

/// Gets an environment variable, trims surrounding whitespace, and treats a blank result the
/// same as unset.
#[must_use]
pub fn get_trimmed_var(env: &dyn EnvProvider, key: &str) -> Option<String> {
    env.get_var(key)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The home directory of the user ([`gleon_model::workspace::HOME_VAR`] of `env`), resolved like
/// the paths the CLI walks; never the root of a workspace.
#[must_use]
pub fn home(env: &dyn EnvProvider) -> Option<PathBuf> {
    env.get_var(gleon_model::workspace::HOME_VAR)
        .and_then(|named| gleon_model::workspace::home(named.as_ref()))
}

/// The `.gleon/` directory of the workspace of `base_dir` ([`crate::paths::find_workspace_root`]
/// with [`crate::paths::has_gleon_dir`]), else of `base_dir` itself.
fn find_gleon_dir(base_dir: &Path, home: Option<&Path>) -> PathBuf {
    crate::paths::find_workspace_root(base_dir, home, crate::paths::has_gleon_dir)
        .unwrap_or_else(|| crate::paths::GleonPaths::new(base_dir))
        .gleon_dir()
}

/// Parses `.gleon/.env` and `.gleon/.env.local` of the workspace of `base_dir` into a key-value
/// map; `home` (see [`home`]) is never a workspace, so its `.gleon/.env` applies nowhere else.
///
/// Precedence order: `.env.local` keys override `.env` keys.
/// Does NOT mutate the process-global environment.
#[must_use]
pub fn load_dotenv(base_dir: &Path, home: Option<&Path>) -> HashMap<String, String> {
    let gleon_dir = find_gleon_dir(base_dir, home);
    let mut map = HashMap::new();
    merge_env_file(&mut map, &gleon_dir.join(".env"));
    merge_env_file(&mut map, &gleon_dir.join(".env.local"));
    map
}

/// Parses the dotenv-format file at `path` and merges its keys into `map`, overwriting any
/// existing keys. Missing or unparsable files are logged and otherwise ignored.
fn merge_env_file(map: &mut HashMap<String, String>, path: &Path) {
    match dotenvy::from_path_iter(path) {
        Ok(iter) => {
            debug!("Parsed environment file: {}", path.display());
            for result in iter {
                match result {
                    Ok((k, v)) => {
                        map.insert(k, v);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to parse line in {}: {}", path.display(), e);
                    }
                }
            }
        }
        Err(e) => {
            debug!("Skipping environment file {}: {}", path.display(), e);
        }
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
    use std::collections::HashMap as Map;

    use tempfile::tempdir;

    use super::*;

    struct MapEnv(Map<String, String>);

    impl EnvProvider for MapEnv {
        fn get_var(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    #[test]
    fn test_get_trimmed_var_rejects_blank_and_whitespace_only() {
        let env = MapEnv(Map::from([
            ("SET".to_string(), "value".to_string()),
            ("PADDED".to_string(), "  padded  ".to_string()),
            ("BLANK".to_string(), "   ".to_string()),
        ]));
        assert_eq!(get_trimmed_var(&env, "SET").as_deref(), Some("value"));
        assert_eq!(get_trimmed_var(&env, "PADDED").as_deref(), Some("padded"));
        assert_eq!(get_trimmed_var(&env, "BLANK"), None);
        assert_eq!(get_trimmed_var(&env, "MISSING"), None);
    }

    #[test]
    fn test_has_var_default_impl_matches_get_var() {
        let env = MapEnv(Map::from([("SET".to_string(), String::new())]));
        assert!(env.has_var("SET"));
        assert!(!env.has_var("MISSING"));
    }

    #[test]
    fn test_load_dotenv_missing_dir() {
        let temp = tempdir().unwrap();
        assert!(load_dotenv(temp.path(), None).is_empty());
    }

    #[test]
    fn test_load_dotenv_valid_and_corrupt_files() {
        let temp = tempdir().unwrap();
        let gleon_dir = temp.path().join(".gleon");
        std::fs::create_dir_all(&gleon_dir).unwrap();

        // 1. Valid .env and .env.local with shared variable to test precedence
        std::fs::write(
            gleon_dir.join(".env"),
            "TEST_VAR_ENV=1\nTEST_SHARED=from_env\n",
        )
        .unwrap();
        std::fs::write(
            gleon_dir.join(".env.local"),
            "TEST_VAR_LOCAL=1\nTEST_SHARED=from_local\n",
        )
        .unwrap();

        let env_map = load_dotenv(temp.path(), None);
        assert_eq!(env_map.get("TEST_VAR_ENV").map(String::as_str), Some("1"));
        assert_eq!(env_map.get("TEST_VAR_LOCAL").map(String::as_str), Some("1"));
        assert_eq!(
            env_map.get("TEST_SHARED").map(String::as_str),
            Some("from_local")
        );

        // 2. Mixed valid and corrupt .env and .env.local (invalid syntax)
        std::fs::write(
            gleon_dir.join(".env"),
            "INVALID_LINE_WITHOUT_EQUALS\nVALID_MIXED=1\n",
        )
        .unwrap();
        std::fs::write(
            gleon_dir.join(".env.local"),
            "INVALID_LINE_WITHOUT_EQUALS\nVALID_LOCAL=1\n",
        )
        .unwrap();

        let env_map = load_dotenv(temp.path(), None);
        assert_eq!(env_map.get("VALID_MIXED").map(String::as_str), Some("1"));
        assert_eq!(env_map.get("VALID_LOCAL").map(String::as_str), Some("1"));
    }
}
