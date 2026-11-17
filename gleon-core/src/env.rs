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
}

/// Standard OS environment variable provider.
pub struct OsEnv;

impl EnvProvider for OsEnv {
    fn get_var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

/// The OS process environment with the values of `.gleon/.env` files ([`load_dotenv`]) as
/// fallback defaults: the process environment always wins.
pub struct MergedEnv {
    dotenv: HashMap<String, String>,
}

impl MergedEnv {
    /// Falls back to `dotenv` for the keys the process environment lacks.
    #[must_use]
    pub const fn new(dotenv: HashMap<String, String>) -> Self {
        Self { dotenv }
    }
}

impl EnvProvider for MergedEnv {
    fn get_var(&self, key: &str) -> Option<String> {
        OsEnv.get_var(key).or_else(|| self.dotenv.get(key).cloned())
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

/// Whether the flag variable `key` of `env` is on: `1` or `true` turn it on, `0`, `false`, blank
/// or unset leave it off (the rule of every `GLEON_*` flag, [`gleon_model::config::env_flag`]).
///
/// Read through `env`, so a [`MergedEnv`] takes it from the process environment, else from
/// `.gleon/.env` and `.gleon/.env.local`.
///
/// # Errors
/// Returns [`gleon_model::config::ConfigError::InvalidEnvFlag`] for any other value, naming the
/// variable.
pub fn flag(
    env: &dyn EnvProvider,
    key: &'static str,
) -> Result<bool, gleon_model::config::ConfigError> {
    gleon_model::config::env_flag(key, env.get_var(key).as_deref()).map(|on| on == Some(true))
}

/// Whether the variable `key` a CI provider sets (`GITHUB_ACTIONS`, `CIRCLECI`, `TF_BUILD`, ...)
/// marks a run there: `true` in any case or `1`, surrounding whitespace aside.
///
/// Lenient, unlike [`flag`]: gleon does not own these values, so any other value is simply off.
#[must_use]
pub fn is_ci_marker(env: &dyn EnvProvider, key: &str) -> bool {
    get_trimmed_var(env, key)
        .is_some_and(|value| value.eq_ignore_ascii_case("true") || value == "1")
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

/// The directory to read `.gleon/.env` from ([`load_dotenv`]).
///
/// With an explicit `config` (relative to `current_dir`), the workspace of its directory, as the
/// context resolves it ([`crate::context`]); else `current_dir`.
#[must_use]
pub fn dotenv_dir(current_dir: &Path, config: Option<&Path>, home: Option<&Path>) -> PathBuf {
    config
        .and_then(|config| current_dir.join(config).parent().map(Path::to_path_buf))
        .and_then(|dir| crate::paths::find_workspace_root(&dir, home, crate::paths::has_gleon_dir))
        .map_or_else(
            || current_dir.to_path_buf(),
            |workspace| workspace.base_dir().to_path_buf(),
        )
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
    fn test_flags_are_one_true_zero_or_false() {
        let env = MapEnv(
            [
                ("ONE", "1"),
                ("TRUE", " TRUE "),
                ("ZERO", "0"),
                ("FALSE", "False"),
                ("BLANK", "  "),
                ("YES", "yes"),
            ]
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect(),
        );
        for on in ["ONE", "TRUE"] {
            assert!(flag(&env, on).unwrap(), "{on}");
        }
        for off in ["ZERO", "FALSE", "BLANK", "MISSING"] {
            assert!(!flag(&env, off).unwrap(), "{off}");
        }
        assert_eq!(
            flag(&env, "YES").unwrap_err().to_string(),
            "YES must be 1, 0, true or false (got 'yes')"
        );
    }

    /// A CI marker is read leniently: on as `true` (any case) or `1`, off for anything else.
    #[test]
    fn test_ci_markers() {
        let env = MapEnv(
            [
                ("ON", " TRUE "),
                ("ONE", "1"),
                ("OFF", "false"),
                ("OTHER", "yes"),
                ("BLANK", ""),
            ]
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect(),
        );
        for on in ["ON", "ONE"] {
            assert!(is_ci_marker(&env, on), "{on}");
        }
        for off in ["OFF", "OTHER", "BLANK", "MISSING"] {
            assert!(!is_ci_marker(&env, off), "{off}");
        }
    }

    /// The process environment wins over `.env` values, which fill the keys it lacks.
    #[test]
    fn test_merged_env_prefers_the_process() {
        // Set in every process of the test runner, on every OS.
        let process = std::env::var("PATH").unwrap();
        let only_dotenv = "GLEON_TEST_MERGED_ENV_DOTENV_ONLY";
        let env = MergedEnv::new(HashMap::from([
            ("PATH".to_owned(), "from .env".to_owned()),
            (only_dotenv.to_owned(), "from .env".to_owned()),
        ]));

        assert_eq!(env.get_var("PATH"), Some(process));
        assert_eq!(env.get_var(only_dotenv).as_deref(), Some("from .env"));
        assert_eq!(env.get_var("GLEON_TEST_MERGED_ENV_UNSET"), None);
    }

    /// `--config` of a nested workspace reads the `.env` of that workspace, not the one of the
    /// working directory; a config outside every workspace keeps the working directory's.
    #[test]
    fn test_dotenv_follows_an_explicit_config() {
        let temp = tempdir().unwrap();
        let root = temp.path();
        let app = root.join("packages/app");
        for (dir, value) in [(root, "root"), (app.as_path(), "app")] {
            std::fs::create_dir_all(dir.join(".gleon")).unwrap();
            std::fs::write(dir.join(".gleon/gleon.yaml"), "screenshots: []\n").unwrap();
            std::fs::write(dir.join(".gleon/.env"), format!("KEY={value}\n")).unwrap();
        }
        let key = |config: Option<&Path>| {
            load_dotenv(&dotenv_dir(root, config, None), None)
                .get("KEY")
                .cloned()
        };

        assert_eq!(key(None).as_deref(), Some("root"));
        let nested = Path::new("packages/app/.gleon/gleon.yaml");
        assert_eq!(key(Some(nested)).as_deref(), Some("app"));
        assert_eq!(
            key(Some(&app.join(".gleon/gleon.yaml"))).as_deref(),
            Some("app")
        );
        let outside = tempdir().unwrap();
        assert_eq!(
            key(Some(&outside.path().join("custom.yaml"))).as_deref(),
            Some("root")
        );
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
