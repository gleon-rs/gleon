//! The `.gleon/gleon.yaml` workspace configuration, shared by the gleon CLI and the gleon
//! Flutter package (through `gleon-ffi`).

use std::path::{Path, PathBuf};

use gleon_engine::config::{DiffConfig, Dimension, Mode, Zone};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::platform::{PlatformConfig, PlatformError};

/// Errors that can occur during configuration loading or manifest operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// Configuration file not found.
    #[error("Configuration file not found: {0}")]
    NotFound(PathBuf),

    /// I/O error during file read or write.
    #[error("Failed to read/write file: {0}")]
    Io(#[from] std::io::Error),

    /// Deserialization error for YAML configuration files.
    #[error("Failed to parse YAML configuration: {0}")]
    YamlParse(#[from] serde_yaml::Error),

    /// gleon CLI version does not satisfy the `required_version`.
    #[error("Incompatible version. Required: {0}, Current: {1}")]
    IncompatibleVersion(String, String),

    /// gleon CLI version string is not a valid semver format.
    #[error("Invalid version format: {0}")]
    InvalidVersionFormat(String),

    /// Configuration is semantically invalid (e.g. empty screenshots list).
    #[error("Invalid configuration: {0}")]
    Validation(String),

    /// `platform` or `fallback_platform` contains an invalid segment.
    #[error("Invalid configuration: {field}: {source}")]
    InvalidPlatform {
        /// The offending field (`platform` or `fallback_platform`).
        field: &'static str,
        /// Why the platform is invalid.
        #[source]
        source: PlatformError,
    },

    /// A boolean environment override (e.g. [`METRICS_ENV`]) has an unsupported value.
    #[error("{name} must be 1, 0, true or false (got '{value}')")]
    InvalidEnvFlag {
        /// Name of the environment variable.
        name: &'static str,
        /// Its trimmed value.
        value: String,
    },

    /// [`ARTIFACTS_ENV`] is not a valid [`ArtifactsDir`].
    #[error("{ARTIFACTS_ENV}: {0}")]
    InvalidArtifactsEnv(#[source] InvalidArtifactsDir),
}

/// A compiled glob pattern for fast file matching, serialized as a simple string.
///
/// Matching is case-insensitive and `*` does not cross `/` (use `**` for that); `?` is one
/// character, `[...]`/`[!...]` a character class (the `glob` crate's syntax). Patterns that would
/// match differently than written, or never, are errors ([`GlobError`]): the same file means the
/// same on every operating system.
#[derive(Debug, Clone)]
pub struct GlobPattern(glob::Pattern);

/// Why a string is no [`GlobPattern`].
#[derive(Debug, thiserror::Error)]
pub enum GlobError {
    /// Invalid syntax: an unclosed `[`, or `**` that is not a whole path segment.
    #[error(transparent)]
    Syntax(#[from] glob::PatternError),
    /// `{a,b}` alternatives, which test names can never contain literally.
    #[error("`{{a,b}}` alternatives are not supported; list one pattern per alternative")]
    Alternatives,
    /// A trailing `/`, which no file path has.
    #[error("a pattern cannot end with `/`; `dir/**` matches the files of a directory")]
    TrailingSlash,
    /// A `\`: a separator on Windows only, a literal elsewhere, and never an escape.
    #[error("use `/` to separate directories; `\\` is not supported (`[*]` matches a literal `*`)")]
    Backslash,
    /// `[^...]`, which reads as a class containing `^` (negation is `[!...]`).
    #[error("negate a character class with `[!...]`, not `[^...]`")]
    CaretNegation,
    /// An empty pattern or segment, a leading `/`, or a `.` or `..` segment: workspace paths are
    /// relative to its root and have none of these, so such a pattern would never match.
    #[error("patterns are relative to the workspace root, e.g. `test/**/*.png`")]
    NotRelative,
}

/// How every [`GlobPattern`] matches.
const GLOB_MATCH: glob::MatchOptions = glob::MatchOptions {
    case_sensitive: false,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

impl GlobPattern {
    /// Create a new `GlobPattern` from a raw string.
    ///
    /// # Errors
    /// Returns an error if `raw` is not a syntactically valid glob pattern.
    pub fn new(raw: &str) -> Result<Self, GlobError> {
        let error = if raw.contains(['{', '}']) {
            Some(GlobError::Alternatives)
        } else if raw.contains('\\') {
            Some(GlobError::Backslash)
        } else if raw.contains("[^") {
            Some(GlobError::CaretNegation)
        } else if raw.ends_with('/') {
            Some(GlobError::TrailingSlash)
        } else if raw
            .split('/')
            .any(|segment| matches!(segment, "" | "." | ".."))
        {
            Some(GlobError::NotRelative)
        } else {
            None
        };
        error.map_or_else(|| Ok(Self(glob::Pattern::new(raw)?)), Err)
    }

    /// Get the raw string representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Check if the path matches this pattern.
    pub fn is_match<P: AsRef<Path>>(&self, path: P) -> bool {
        self.0.matches_path_with(path.as_ref(), GLOB_MATCH)
    }
}

impl PartialEq for GlobPattern {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}
impl Eq for GlobPattern {}

impl<'de> Deserialize<'de> for GlobPattern {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = std::borrow::Cow::<'de, str>::deserialize(deserializer)?;
        Self::new(&s).map_err(|e| serde::de::Error::custom(format!("glob {s:?}: {e}")))
    }
}

impl schemars::JsonSchema for GlobPattern {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "GlobPattern".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "Case-insensitive glob relative to the workspace root: `*` and `?` stay within a path segment, `**` (a whole segment) crosses segments, `[...]` is a character class; no `{a,b}` alternatives, no trailing `/`."
        })
    }
}

impl Serialize for GlobPattern {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

/// The root configuration structure for gleon.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "gleon workspace configuration (.gleon/gleon.yaml)")]
pub struct GleonConfig {
    /// The required version range of the CLI to run this configuration.
    ///
    /// Enforced by the CLI only; other readers (the Flutter package) just check its syntax.
    #[schemars(with = "String")]
    pub required_version: semver::VersionReq,
    /// The platform the CLI runs as (e.g. `macos-aarch64`, or `{renderer: impeller}` over the
    /// detected OS and architecture); its flags and `GLEON_PLATFORM` override it.
    ///
    /// Read by the CLI only: an integration (the Flutter package) runs as its process's OS and
    /// architecture, the name of the directory of that platform's own goldens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<PlatformConfig>,
    /// Optional fallback platform identifier used when current platform baselines are missing.
    ///
    /// For integrations that commit goldens as image files (the Flutter package) it is the
    /// platform of the shared goldens (`<dir>/<file>`); only its OS and architecture count, under
    /// the names a process reports (`macos-aarch64`, not `macos-arm64`: others are an error for
    /// integrations). On it text is compared by default; any other platform keeps its own goldens
    /// in `<dir>/<os>-<arch>/<file>` and, until it has one, compares the shared golden with text
    /// ignored by default (see `text_tolerance`). Without it every platform compares the shared
    /// goldens that way. `platform` and the `GLEON_*PLATFORM` variables are read by the CLI only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_platform: Option<PlatformConfig>,
    /// List of screenshot match rules.
    pub screenshots: Vec<ScreenshotRule>,
    /// Globs of paths to exclude from testing.
    #[serde(default, with = "item_or_vec")]
    #[schemars(with = "item_or_vec::OneOrMany<GlobPattern>")]
    pub exclude: Vec<GlobPattern>,
    /// Per-golden comparison metrics recorded by integrations such as the Flutter package.
    #[serde(default, skip_serializing_if = "MetricsConfig::is_default")]
    pub metrics: MetricsConfig,
    /// Directory of the images of failed cases (golden, candidate, diff) relative to the workspace
    /// root: `.gleon/runs/latest/artifacts` (the default) or a directory under `.gleon/runs/`
    /// outside `latest/`; `GLEON_ARTIFACTS_DIR` overrides it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<ArtifactsDir>,
}

/// Default [`GleonConfig::artifacts`].
pub const DEFAULT_ARTIFACTS_DIR: &str = ".gleon/runs/latest/artifacts";

/// Name of the environment variable that overrides [`GleonConfig::artifacts`].
pub const ARTIFACTS_ENV: &str = "GLEON_ARTIFACTS_DIR";

/// The directory for the images of failed cases, relative to the workspace root: the default
/// `.gleon/runs/latest/artifacts` or any directory under `.gleon/runs/` outside `latest/`.
///
/// Confined to the run output of gleon by construction: `.gleon/.gitignore` ignores it, no scanner
/// reads it, `gleon diff` leaves it alone and `gleon clean` removes it. Names are ASCII letters,
/// digits, `.`, `_` and `-`, separated by `/`, without `.` or `..`. To keep the images on a RAM
/// disk, link `.gleon/runs` (or a directory under it) there.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct ArtifactsDir(String);

/// A path that is not a valid [`ArtifactsDir`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error(
    "'{0}' must be `.gleon/runs/latest/artifacts` or a directory under `.gleon/runs/` outside \
     `latest/`: `/`-separated names of ASCII letters, digits, `.`, `_` and `-`, without `.` or `..`"
)]
pub struct InvalidArtifactsDir(pub String);

/// The directory every [`ArtifactsDir`] lies in.
const RUNS_DIR: &str = ".gleon/runs/";

impl ArtifactsDir {
    /// Validates `path`.
    ///
    /// # Errors
    /// Returns [`InvalidArtifactsDir`] unless `path` is [`DEFAULT_ARTIFACTS_DIR`] or a directory
    /// under `.gleon/runs/` outside `latest/` whose names are ASCII letters, digits, `.`, `_` and
    /// `-`, other than `.` and `..`.
    pub fn new(path: impl Into<String>) -> Result<Self, InvalidArtifactsDir> {
        let path = path.into();
        if Self::is_valid(&path) {
            Ok(Self(path))
        } else {
            Err(InvalidArtifactsDir(path))
        }
    }

    /// Whether `path` is a valid artifacts directory ([`Self::new`]), without taking it.
    #[must_use]
    pub fn is_valid(path: &str) -> bool {
        path == DEFAULT_ARTIFACTS_DIR
            || path.strip_prefix(RUNS_DIR).is_some_and(|inside| {
                // `latest/` is the output of one run (case-insensitive file systems included).
                crate::naming::is_portable_relative_path(inside)
                    && !inside
                        .split('/')
                        .next()
                        .is_some_and(|first| first.eq_ignore_ascii_case("latest"))
            })
    }

    /// The directory of [`ARTIFACTS_ENV`] (`env_value` is its raw value, `None` when unset): `None`
    /// when unset or empty, surrounding whitespace ignored.
    ///
    /// # Errors
    /// Returns [`ConfigError::InvalidArtifactsEnv`] for an invalid directory.
    pub fn from_env(env_value: Option<&str>) -> Result<Option<Self>, ConfigError> {
        env_value
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(|path| Self::new(path).map_err(ConfigError::InvalidArtifactsEnv))
            .transpose()
    }

    /// The path, `/`-separated.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The directory inside the workspace at `root`.
    #[must_use]
    pub fn to_path(&self, root: &Path) -> PathBuf {
        crate::naming::join_relative(root, &self.0)
    }
}

impl Default for ArtifactsDir {
    fn default() -> Self {
        Self(DEFAULT_ARTIFACTS_DIR.to_owned())
    }
}

impl<'de> Deserialize<'de> for ArtifactsDir {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl schemars::JsonSchema for ArtifactsDir {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ArtifactsDir".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "description": "`.gleon/runs/latest/artifacts` or a directory under `.gleon/runs/` outside `latest/`: `/`-separated names of ASCII letters, digits, `.`, `_` and `-`, without `.` or `..`.",
            "type": "string",
            "pattern": "^\\.gleon/runs/(latest/artifacts|(?![Ll][Aa][Tt][Ee][Ss][Tt](/|$))(?!\\.\\.?(/|$))[A-Za-z0-9._-]+(/(?!\\.\\.?(/|$))[A-Za-z0-9._-]+)*)$"
        })
    }
}

/// The `metrics:` section: opt-in per-golden comparison metrics.
///
/// The `GLEON_METRICS` environment variable (`1`/`true` or `0`/`false`) overrides `enabled`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MetricsConfig {
    /// Record a JSON case report per golden under `.gleon/runs/latest/cases/`.
    #[serde(default)]
    pub enabled: bool,
    /// Print one summary line per golden while metrics are enabled.
    #[serde(default = "default_true")]
    pub console: bool,
}

/// Name of the environment variable that overrides [`MetricsConfig::enabled`].
pub const METRICS_ENV: &str = "GLEON_METRICS";

/// Lines of `.gleon/.gitignore`: local caches, run output and secrets never belong in Git.
pub const GITIGNORE_LINES: &[&str] = &[
    "blobs/",
    "runs/",
    ".env",
    ".env.local",
    "credentials",
    "dashboard.html",
    "history.json",
];

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            console: default_true(),
        }
    }
}

impl MetricsConfig {
    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde passes a reference to `skip_serializing_if` functions"
    )]
    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Applies the [`METRICS_ENV`] override (`env_value` is its raw value, `None` when unset).
    ///
    /// # Errors
    /// Returns [`ConfigError::InvalidEnvFlag`] as [`Self::env_override`] does.
    pub fn effective(self, env_value: Option<&str>) -> Result<Self, ConfigError> {
        Ok(Self {
            enabled: Self::env_override(env_value)?.unwrap_or(self.enabled),
            ..self
        })
    }

    /// The value of [`METRICS_ENV`] (`env_value` is its raw value, `None` when unset): `None` when
    /// unset or empty, else whether it turns metrics on.
    ///
    /// # Errors
    /// Returns [`ConfigError::InvalidEnvFlag`] for any value other than `1`, `true`, `0` or
    /// `false` (ASCII case-insensitive, surrounding whitespace ignored).
    pub fn env_override(env_value: Option<&str>) -> Result<Option<bool>, ConfigError> {
        let Some(raw) = env_value.map(str::trim).filter(|v| !v.is_empty()) else {
            return Ok(None);
        };
        if raw == "1" || raw.eq_ignore_ascii_case("true") {
            Ok(Some(true))
        } else if raw == "0" || raw.eq_ignore_ascii_case("false") {
            Ok(Some(false))
        } else {
            Err(ConfigError::InvalidEnvFlag {
                name: METRICS_ENV,
                value: raw.to_owned(),
            })
        }
    }
}

const fn default_true() -> bool {
    true
}

/// Helper module for serde to deserialize a single item or a list of items into a `Vec<T>`.
pub mod item_or_vec {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    /// Serializes a single-element `Vec<T>` as a bare item, or a multi-element `Vec<T>` as a list.
    ///
    /// # Errors
    /// Returns an error if the underlying `serializer` fails to serialize the item(s).
    pub fn serialize<T, S>(items: &[T], serializer: S) -> Result<S::Ok, S::Error>
    where
        T: Serialize,
        S: Serializer,
    {
        match items {
            [item] => item.serialize(serializer),
            items => items.serialize(serializer),
        }
    }

    /// JSON Schema shape of a field serialized through this module.
    #[derive(schemars::JsonSchema)]
    #[serde(untagged)]
    pub enum OneOrMany<T> {
        /// A single item.
        One(T),
        /// A list of items.
        Many(Vec<T>),
    }

    /// Deserializes either a single item (a string) or a list of items into a `Vec<T>`.
    ///
    /// A visitor rather than an `untagged` enum, so the error of an invalid item (a glob that is
    /// no pattern) reaches the user instead of "did not match any variant".
    ///
    /// # Errors
    /// Returns an error if the input is neither a single `T` nor a list of `T`, or the error of
    /// the item that is invalid.
    pub fn deserialize<'de, T, D>(deserializer: D) -> Result<Vec<T>, D::Error>
    where
        T: Deserialize<'de>,
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(OneOrManyVisitor(std::marker::PhantomData))
    }

    struct OneOrManyVisitor<T>(std::marker::PhantomData<T>);

    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for OneOrManyVisitor<T> {
        type Value = Vec<T>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a string or a list of strings")
        }

        fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
            T::deserialize(serde::de::IntoDeserializer::<E>::into_deserializer(value))
                .map(|item| vec![item])
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
            Vec::deserialize(serde::de::value::SeqAccessDeserializer::new(seq))
        }
    }
}

/// A rule specifying how to match and process screenshots.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScreenshotRule {
    /// Glob patterns representing the files to include.
    #[serde(with = "item_or_vec")]
    #[schemars(with = "item_or_vec::OneOrMany<GlobPattern>")]
    pub include: Vec<GlobPattern>,
    /// Diffing mode (pixel or ssim).
    #[serde(default = "default_mode")]
    pub mode: Mode,
    /// Specific diffing configuration.
    #[serde(default)]
    pub diff: DiffConfig,
    /// Optional zones to mask out (ignore) during verification.
    #[serde(default)]
    pub masks: Vec<MaskRule>,
    /// How much text may differ, in `pixel` mode: the largest share of differing pixels in any
    /// tile of the text an integration reports, `[0, 1]`. Unset, it depends on the golden
    /// ([`crate::tolerance::TextTolerance::resolve`]): 0.05 against a golden of the platform the
    /// test runs on (see `fallback_platform`), else 1, so text never fails. A value set here
    /// applies to every golden of the rule: 1 turns text comparison off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_tolerance: Option<crate::tolerance::TextTolerance>,
}

impl ScreenshotRule {
    /// Returns all mask zones that match the given relative file path.
    #[must_use]
    pub fn matched_mask_zones(&self, relative_path: &Path) -> Vec<Zone> {
        if self.masks.is_empty() {
            return Vec::new();
        }
        self.masks
            .iter()
            .filter(|m| m.path.is_match(relative_path))
            .flat_map(|m| m.zones.clone())
            .collect()
    }
}

/// A mask rule specifying which regions of an image to ignore.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MaskRule {
    /// Image path pattern this mask applies to.
    pub path: GlobPattern,
    /// List of bounding zones to ignore.
    pub zones: Vec<Zone>,
}

const fn default_mode() -> Mode {
    Mode::Pixel
}

impl GleonConfig {
    /// Load configuration from a YAML file.
    ///
    /// Performs post-deserialization validation to catch semantically invalid
    /// configurations that serde alone cannot enforce (e.g. empty screenshot rules).
    ///
    /// # Errors
    /// Returns [`ConfigError::NotFound`] if `path` does not exist, [`ConfigError::Io`] for
    /// other I/O failures, [`ConfigError::YamlParse`] if the file is not valid YAML matching
    /// the schema, or [`ConfigError::Validation`] / [`ConfigError::InvalidPlatform`] if the parsed
    /// configuration is semantically invalid.
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        Self::load_for_cli(path, None)
    }

    /// [`Self::load_from_file`] for the CLI of `cli_version`: its `required_version` is checked
    /// first ([`Self::check_required_version`]), so a config for a newer CLI says so instead of
    /// failing on what that CLI added.
    ///
    /// # Errors
    /// As [`Self::load_from_file`], plus [`ConfigError::IncompatibleVersion`].
    pub fn load_for_cli<P: AsRef<Path>>(
        path: P,
        cli_version: Option<&semver::Version>,
    ) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        tracing::debug!("Loading configuration from {:?}", path);
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tracing::error!("Configuration file not found at {:?}", path);
                return Err(ConfigError::NotFound(path.to_path_buf()));
            }
            Err(error) => return Err(ConfigError::Io(error)),
        };
        if let Some(cli_version) = cli_version {
            Self::check_required_version(&text, cli_version)?;
        }
        Self::from_yaml_str(&text)
    }

    /// Checks the `required_version` of the config `yaml` against `cli_version`, reading that
    /// field alone (unknown keys and the rest are left to the strict parse): a config written
    /// for a newer CLI fails with the version, not with a key this CLI does not know. A
    /// pre-release CLI (`0.4.0-rc.1`) counts as its release (`0.4.0`): semver ranges never match
    /// pre-releases otherwise.
    ///
    /// # Errors
    /// Returns [`ConfigError::IncompatibleVersion`] if `cli_version` does not satisfy it. YAML
    /// that cannot be read this far passes: the strict parse reports it.
    pub fn check_required_version(
        yaml: &str,
        cli_version: &semver::Version,
    ) -> Result<(), ConfigError> {
        #[derive(Deserialize)]
        struct Required {
            required_version: Option<semver::VersionReq>,
        }
        let Ok(Required {
            required_version: Some(required),
        }) = serde_yaml::from_str::<Required>(yaml)
        else {
            return Ok(());
        };
        let release = semver::Version::new(cli_version.major, cli_version.minor, cli_version.patch);
        if required.matches(&release) {
            Ok(())
        } else {
            Err(ConfigError::IncompatibleVersion(
                required.to_string(),
                cli_version.to_string(),
            ))
        }
    }

    /// Parses and validates a configuration from YAML text (the contents of `.gleon/gleon.yaml`).
    ///
    /// # Errors
    /// Returns [`ConfigError::YamlParse`] if `yaml` does not match the schema, or
    /// [`ConfigError::Validation`] / [`ConfigError::InvalidPlatform`] if the parsed configuration
    /// is semantically invalid.
    pub fn from_yaml_str(yaml: &str) -> Result<Self, ConfigError> {
        serde_yaml::from_str::<Self>(yaml)?.validated()
    }

    /// `self` if it passes [`Self::validate`]; the single post-parse step of every loader.
    fn validated(self) -> Result<Self, ConfigError> {
        self.validate().map(|()| self)
    }

    /// The artifacts directory: `flag` (a command-line option) beats `env` (the value of
    /// [`ARTIFACTS_ENV`], see [`ArtifactsDir::from_env`]), which beats [`Self::artifacts`], which
    /// beats [`DEFAULT_ARTIFACTS_DIR`].
    #[must_use]
    pub fn artifacts_dir(
        &self,
        env: Option<&ArtifactsDir>,
        flag: Option<&ArtifactsDir>,
    ) -> ArtifactsDir {
        flag.or(env)
            .or(self.artifacts.as_ref())
            .cloned()
            .unwrap_or_default()
    }

    /// Validates semantic invariants that serde attributes cannot express.
    fn validate(&self) -> Result<(), ConfigError> {
        // Opaque keys are validated when parsed; structured fields only when turned into a key,
        // which would otherwise happen long after loading (at context resolution). `platform`
        // may leave fields (the OS too) to the detected platform; `fallback_platform` names one.
        if let Some(platform) = &self.platform {
            platform
                .validate()
                .map_err(|source| ConfigError::InvalidPlatform {
                    field: "platform",
                    source,
                })?;
        }
        if let Some(fallback) = &self.fallback_platform {
            fallback
                .key()
                .map_err(|source| ConfigError::InvalidPlatform {
                    field: "fallback_platform",
                    source,
                })?;
        }
        if self.screenshots.is_empty() {
            return Err(ConfigError::Validation(
                "'screenshots' must contain at least one rule".to_string(),
            ));
        }
        for (i, rule) in self.screenshots.iter().enumerate() {
            if rule.include.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "screenshots[{i}].include must contain at least one glob pattern"
                )));
            }
            if !(0.0..=1.0).contains(&rule.diff.threshold) {
                return Err(ConfigError::Validation(format!(
                    "screenshots[{i}].diff.threshold must be between 0.0 and 1.0 (got {})",
                    rule.diff.threshold
                )));
            }
            if !(0.0..=1.0).contains(&rule.diff.min_similarity) {
                return Err(ConfigError::Validation(format!(
                    "screenshots[{i}].diff.min_similarity must be between 0.0 and 1.0 (got {})",
                    rule.diff.min_similarity
                )));
            }
            if !gleon_engine::config::is_valid_color_tolerance(rule.diff.color_tolerance) {
                return Err(ConfigError::Validation(format!(
                    "screenshots[{i}].diff.color_tolerance must be between 0 and 255 (got {})",
                    rule.diff.color_tolerance
                )));
            }
            if let Some(text) = &rule.text_tolerance {
                if rule.mode != Mode::Pixel {
                    return Err(ConfigError::Validation(format!(
                        "screenshots[{i}].text_tolerance applies to `mode: pixel` only"
                    )));
                }
                text.validate().map_err(|e| {
                    ConfigError::Validation(format!("screenshots[{i}].text_tolerance: {e}"))
                })?;
            }
            for (j, mask) in rule.masks.iter().enumerate() {
                for (k, zone) in mask.zones.iter().enumerate() {
                    match zone.width {
                        Dimension::Percent(pct) if !(0.0..=100.0).contains(&pct) => {
                            return Err(ConfigError::Validation(format!(
                                "screenshots[{i}].masks[{j}].zones[{k}].width percentage must be between 0.0 and 100.0 (got {pct}%)"
                            )));
                        }
                        _ => {}
                    }
                    match zone.height {
                        Dimension::Percent(pct) if !(0.0..=100.0).contains(&pct) => {
                            return Err(ConfigError::Validation(format!(
                                "screenshots[{i}].masks[{j}].zones[{k}].height percentage must be between 0.0 and 100.0 (got {pct}%)"
                            )));
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }
}

/// Pre-parsed default version requirement, avoiding `unwrap()` at runtime.
const DEFAULT_VERSION_REQ: &str = ">=0.1.0";

use std::sync::LazyLock;

static DEFAULT_VERSION: LazyLock<semver::VersionReq> = LazyLock::new(|| {
    #[expect(
        clippy::expect_used,
        reason = "hard-coded default is statically valid and covered by tests"
    )]
    semver::VersionReq::parse(DEFAULT_VERSION_REQ)
        .expect("DEFAULT_VERSION_REQ must be a valid semver requirement")
});

impl Default for GleonConfig {
    fn default() -> Self {
        let required_version = DEFAULT_VERSION.clone();

        Self {
            required_version,
            platform: None,
            fallback_platform: None,
            metrics: MetricsConfig::default(),
            artifacts: None,
            screenshots: vec![ScreenshotRule {
                #[expect(
                    clippy::expect_used,
                    reason = "hard-coded default is statically valid and covered by tests"
                )]
                include: vec![
                    GlobPattern::new("**/*.png").expect("Default glob pattern must be valid"),
                ],
                mode: Mode::Pixel,
                diff: DiffConfig::default(),
                masks: vec![],
                text_tolerance: None,
            }],
            exclude: vec![
                #[expect(
                    clippy::expect_used,
                    reason = "hard-coded default is statically valid and covered by tests"
                )]
                GlobPattern::new("node_modules/**").expect("Valid pattern"),
                #[expect(
                    clippy::expect_used,
                    reason = "hard-coded default is statically valid and covered by tests"
                )]
                GlobPattern::new("target/**").expect("Valid pattern"),
                #[expect(
                    clippy::expect_used,
                    reason = "hard-coded default is statically valid and covered by tests"
                )]
                GlobPattern::new("build/**").expect("Valid pattern"),
                #[expect(
                    clippy::expect_used,
                    reason = "hard-coded default is statically valid and covered by tests"
                )]
                GlobPattern::new("example/**").expect("Valid pattern"),
            ],
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::float_cmp,
    clippy::pedantic,
    clippy::nursery,
    reason = "test code: panics are assertions, and pedantic/nursery style lints are not enforced in tests"
)]
mod tests {
    use semver::VersionReq;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn test_default_yaml_snapshot() {
        let config = GleonConfig::default();
        let generated_yaml = serde_yaml::to_string(&config).unwrap();

        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let fixture_path = manifest_dir.join("tests/fixtures/default_config.yaml");
        let expected_yaml = std::fs::read_to_string(fixture_path).unwrap();

        assert_eq!(generated_yaml.trim(), expected_yaml.trim());
    }

    #[test]
    fn test_load_config_success() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let fixture_path = manifest_dir.join("tests/fixtures/gleon.yaml");
        let config = GleonConfig::load_from_file(fixture_path).unwrap();

        assert_eq!(
            config.required_version,
            VersionReq::parse(">=0.1.0").unwrap()
        );
        assert_eq!(
            config.platform,
            Some(PlatformConfig::Opaque("macos-aarch64".to_string()))
        );
        assert_eq!(config.exclude[0].as_str(), "**/ignored/**");
        assert_eq!(config.screenshots.len(), 1);

        let rule = &config.screenshots[0];
        assert_eq!(rule.include[0].as_str(), "src/login.png");
        assert_eq!(rule.mode, Mode::Ssim);
        assert_eq!(rule.diff.threshold, 0.05);
        assert_eq!(rule.diff.min_similarity, 0.98);
        assert_eq!(rule.masks.len(), 1);

        let mask = &rule.masks[0];
        assert_eq!(mask.path.as_str(), "mask1");
        assert_eq!(mask.zones.len(), 1);

        let zone = &mask.zones[0];
        assert_eq!(zone.x, 10);
        assert_eq!(zone.y, 20);
        assert_eq!(zone.width, Dimension::Pixels(100));
        assert_eq!(zone.height, Dimension::Percent(20.0));
    }

    #[test]
    fn test_load_config_unknown_fields_rejected() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let fixture_path = manifest_dir.join("tests/fixtures/gleon_unknown_fields.yaml");
        let result = GleonConfig::load_from_file(fixture_path);
        assert!(
            result.is_err(),
            "Unknown fields should be rejected with deny_unknown_fields"
        );
    }

    #[test]
    fn test_load_config_not_found() {
        let path = PathBuf::from("nonexistent_config_file.yaml");
        let result = GleonConfig::load_from_file(&path);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ConfigError::NotFound(p) if p == path));
    }

    #[test]
    fn test_load_config_invalid_semver() {
        let invalid_yaml = "required_version: \"invalid_semver\"\nscreenshots: []";
        let result: Result<GleonConfig, _> = serde_yaml::from_str(invalid_yaml);
        assert!(result.is_err());
    }

    #[test]
    fn test_load_config_invalid_dimension() {
        // Test non-numeric dimension
        let invalid_dim_yaml = "
required_version: \">=0.1.0\"
screenshots:
  - include: \"test.png\"
    masks:
      - path: \"mask\"
        zones:
          - x: 0
            y: 0
            width: \"not_a_number\"
            height: 10
";
        let result: Result<GleonConfig, _> = serde_yaml::from_str(invalid_dim_yaml);
        assert!(result.is_err());

        // Test out of range percentage (> 100%)
        let invalid_pct_yaml = "
required_version: \">=0.1.0\"
screenshots:
  - include: \"test.png\"
    masks:
      - path: \"mask\"
        zones:
          - x: 0
            y: 0
            width: 10
            height: \"105%\"
";
        let result2: Result<GleonConfig, _> = serde_yaml::from_str(invalid_pct_yaml);
        assert!(result2.is_err());
    }

    #[test]
    fn test_load_config_invalid_ratio() {
        let invalid_yaml = "
required_version: \">=0.1.0\"
screenshots:
  - include: \"test.png\"
    diff:
      threshold: 1.5
";
        let result: Result<GleonConfig, _> = serde_yaml::from_str(invalid_yaml);
        assert!(result.is_err());
    }

    #[test]
    fn test_required_version_is_checked_before_the_strict_parse() {
        let version = |text| semver::Version::parse(text).unwrap();
        let newer = "required_version: '>=99.0.0'\nfuture_option: true\nscreenshots: []\n";
        assert!(matches!(
            GleonConfig::check_required_version(newer, &version("0.3.0")),
            Err(ConfigError::IncompatibleVersion(required, current))
                if required == ">=99.0.0" && current == "0.3.0"
        ));
        let current = "required_version: '>=0.3.0'\nscreenshots: []\n";
        assert!(GleonConfig::check_required_version(current, &version("0.3.0")).is_ok());
        // A pre-release counts as its release, which semver ranges would never match.
        assert!(GleonConfig::check_required_version(current, &version("0.3.0-rc.1")).is_ok());
        assert!(GleonConfig::check_required_version(current, &version("0.2.9")).is_err());
        // What cannot be read this far is left to the strict parse.
        for broken in ["required_version: 'nonsense'", "[", ""] {
            assert!(GleonConfig::check_required_version(broken, &version("0.3.0")).is_ok());
        }
    }

    #[test]
    fn test_default_version_req_is_valid() {
        assert!(VersionReq::parse(DEFAULT_VERSION_REQ).is_ok());
    }

    #[test]
    fn test_validation_empty_screenshots() {
        let yaml = "required_version: \">=0.1.0\"\nscreenshots: []";
        let config: GleonConfig = serde_yaml::from_str(yaml).unwrap();
        let result = config.validate();
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ConfigError::Validation(msg) if msg.contains("screenshots")
        ));
    }

    #[test]
    fn test_validation_empty_include() {
        let yaml = "
required_version: \">=0.1.0\"
screenshots:
  - include: []
";
        let config: GleonConfig = serde_yaml::from_str(yaml).unwrap();
        let result = config.validate();
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ConfigError::Validation(msg) if msg.contains("include")
        ));
    }

    #[test]
    fn test_validation_invalid_threshold() {
        let mut config = GleonConfig::default();
        config.screenshots[0].diff.threshold = 1.5;
        let result = config.validate();
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ConfigError::Validation(msg) if msg.contains("threshold must be between 0.0 and 1.0")
        ));
    }

    /// `text_tolerance` belongs to pixel rules and is a share.
    #[test]
    fn test_rule_text_tolerance() {
        let yaml = |mode: &str, text: &str| {
            format!(
                "required_version: '>=0.1.0'\nscreenshots:\n  - include: 'a/*.png'\n    mode: {mode}\n    text_tolerance: {text}\n"
            )
        };
        let config = GleonConfig::from_yaml_str(&yaml("pixel", "0.1")).unwrap();
        assert_eq!(
            config.screenshots[0].text_tolerance,
            Some(crate::tolerance::TextTolerance(0.1))
        );
        for (mode, text, needle) in [
            (
                "ssim",
                "0.1",
                "text_tolerance applies to `mode: pixel` only",
            ),
            (
                "pixel",
                "2",
                "screenshots[0].text_tolerance: `text_tolerance` must be between 0.0 and 1.0",
            ),
        ] {
            let err = GleonConfig::from_yaml_str(&yaml(mode, text)).unwrap_err();
            assert!(err.to_string().contains(needle), "{err}");
        }
    }

    #[test]
    fn test_validation_invalid_color_tolerance() {
        for bad in [f64::NAN, -1.0, 255.5, f64::INFINITY] {
            let mut config = GleonConfig::default();
            config.screenshots[0].diff.color_tolerance = bad;
            assert!(matches!(
                config.validate(),
                Err(ConfigError::Validation(msg)) if msg.contains("color_tolerance must be between 0 and 255")
            ));
        }
    }

    #[test]
    fn test_validation_invalid_min_similarity() {
        let mut config = GleonConfig::default();
        config.screenshots[0].diff.min_similarity = -0.1;
        let result = config.validate();
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ConfigError::Validation(msg) if msg.contains("min_similarity must be between 0.0 and 1.0")
        ));
    }

    #[test]
    fn test_validation_invalid_mask_percentages() {
        // Test invalid width percentage
        let mut config = GleonConfig::default();
        config.screenshots[0].masks = vec![MaskRule {
            path: GlobPattern::new("src/test.png").unwrap(),
            zones: vec![Zone {
                x: 0,
                y: 0,
                width: Dimension::Percent(150.0),
                height: Dimension::Pixels(100),
            }],
        }];
        let result = config.validate();
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ConfigError::Validation(msg) if msg.contains("width percentage must be between 0.0 and 100.0")
        ));

        // Test invalid height percentage
        let mut config2 = GleonConfig::default();
        config2.screenshots[0].masks = vec![MaskRule {
            path: GlobPattern::new("src/test.png").unwrap(),
            zones: vec![Zone {
                x: 0,
                y: 0,
                width: Dimension::Pixels(100),
                height: Dimension::Percent(-10.0),
            }],
        }];
        let result2 = config2.validate();
        assert!(result2.is_err());
        assert!(matches!(
            result2.unwrap_err(),
            ConfigError::Validation(msg) if msg.contains("height percentage must be between 0.0 and 100.0")
        ));
    }

    #[test]
    fn test_item_or_vec_errors() {
        #[derive(Deserialize, Serialize, Debug, PartialEq)]
        struct TestItem {
            #[serde(with = "item_or_vec")]
            values: Vec<String>,
        }
        // Passing an invalid type (number) where string/vec is expected
        let res: Result<TestItem, _> = serde_yaml::from_str("values: 123");
        assert!(res.is_err());
    }

    #[test]
    fn test_diff_config_invalid_type() {
        // threshold is a string, not a float
        let invalid_yaml = "
required_version: \">=0.1.0\"
screenshots:
  - include: \"test.png\"
    diff:
      threshold: \"not-a-float\"
";
        let result: Result<GleonConfig, _> = serde_yaml::from_str(invalid_yaml);
        assert!(result.is_err());
    }

    /// An invalid glob of `include` or `exclude` names itself, why and where, through the
    /// one-or-many field.
    #[test]
    fn test_invalid_globs_name_the_pattern_and_the_reason() {
        for (yaml, needles) in [
            (
                "required_version: '>=0.1.0'\nscreenshots:\n  - include: 'test/{a,b}/*.png'\n",
                [
                    "screenshots[0].include",
                    "\"test/{a,b}/*.png\"",
                    "alternatives",
                    "line 3",
                ],
            ),
            (
                "required_version: '>=0.1.0'\nscreenshots:\n  - include: ['a/*.png', 'b/[^c].png']\n",
                [
                    "screenshots[0].include",
                    "\"b/[^c].png\"",
                    "[!...]",
                    "line 3",
                ],
            ),
            (
                "required_version: '>=0.1.0'\nscreenshots:\n  - include: '**/*.png'\nexclude: build/\n",
                ["exclude", "\"build/\"", "cannot end with `/`", "line 4"],
            ),
            (
                "required_version: '>=0.1.0'\nscreenshots:\n  - include: '../other/**/*.png'\n",
                [
                    "screenshots[0].include",
                    "\"../other/**/*.png\"",
                    "relative to the workspace root",
                    "line 3",
                ],
            ),
        ] {
            let error = GleonConfig::from_yaml_str(yaml).unwrap_err().to_string();
            for needle in needles {
                assert!(error.contains(needle), "{needle:?} in {error}");
            }
        }
    }

    #[test]
    fn test_item_or_vec_parsing() {
        #[derive(Debug, Serialize, Deserialize, PartialEq)]
        struct TestStruct {
            #[serde(with = "item_or_vec")]
            values: Vec<String>,
        }

        // Test single string
        let s1: TestStruct = serde_yaml::from_str("values: \"hello\"").unwrap();
        assert_eq!(s1.values, vec!["hello".to_string()]);
        // Serializes as a single string
        assert_eq!(serde_yaml::to_string(&s1).unwrap().trim(), "values: hello");

        // Test list of strings
        let s2: TestStruct = serde_yaml::from_str("values: [\"hello\", \"world\"]").unwrap();
        assert_eq!(s2.values, vec!["hello".to_string(), "world".to_string()]);
        // Serializes as a list of strings
        assert_eq!(
            serde_yaml::to_string(&s2).unwrap().trim(),
            "values:\n- hello\n- world"
        );
    }

    #[test]
    fn test_glob_pattern_validation() {
        // 1. Literal path (no wildcards)
        let lit_pat: GlobPattern = serde_yaml::from_str("\"test/pic.png\"").unwrap();
        assert_eq!(lit_pat.as_str(), "test/pic.png");
        assert!(lit_pat.is_match("test/pic.png"));
        assert!(lit_pat.is_match("Test/PIC.png"), "case-insensitive");
        assert!(!lit_pat.is_match("test/other.png"));
        assert!(!lit_pat.is_match("test/pic.png.bak"));

        // 2. Wildcard pattern
        let wild_pat: GlobPattern = serde_yaml::from_str("\"test/*.png\"").unwrap();
        assert_eq!(wild_pat.as_str(), "test/*.png");
        assert!(wild_pat.is_match("test/pic.png"));
        assert!(wild_pat.is_match("test/other.png"));
        assert!(!wild_pat.is_match("test/dir/pic.png"));

        // 3. Double wildcard pattern
        let double_wild_pat: GlobPattern = serde_yaml::from_str("\"test/**/*.png\"").unwrap();
        assert!(double_wild_pat.is_match("test/dir/pic.png"));
        assert!(double_wild_pat.is_match("test/pic.png"));

        // 4. Invalid pattern (unclosed character class)
        let invalid: Result<GlobPattern, _> = serde_yaml::from_str("\"test/[a-z\"");
        assert!(invalid.is_err());

        // 5. Invalid pattern via new() directly
        let invalid_new = GlobPattern::new("test/[a-z");
        assert!(invalid_new.is_err());
    }

    #[test]
    fn test_diff_config_invalid_ratio_bounds() {
        // Negative threshold
        let yaml_neg_thresh = "
required_version: \">=0.1.0\"
screenshots:
  - include: \"test.png\"
    diff:
      threshold: -0.1
";
        let res1: Result<GleonConfig, _> = serde_yaml::from_str(yaml_neg_thresh);
        assert!(res1.is_err());

        // Negative similarity
        let yaml_neg_sim = "
required_version: \">=0.1.0\"
screenshots:
  - include: \"test.png\"
    diff:
      min_similarity: -0.05
";
        let res2: Result<GleonConfig, _> = serde_yaml::from_str(yaml_neg_sim);
        assert!(res2.is_err());
    }

    #[test]
    fn test_metrics_section() {
        let yaml = "
required_version: \">=0.1.0\"
screenshots:
  - include: \"test.png\"
metrics:
  enabled: true
";
        let config = GleonConfig::from_yaml_str(yaml).unwrap();
        assert_eq!(
            config.metrics,
            MetricsConfig {
                enabled: true,
                console: true
            }
        );
        let typo = yaml.replace("enabled", "enable");
        assert!(matches!(
            GleonConfig::from_yaml_str(&typo),
            Err(ConfigError::YamlParse(_))
        ));
        // The default section is omitted when serializing (the `gleon init` snapshot is unchanged).
        assert!(
            !serde_yaml::to_string(&GleonConfig::default())
                .unwrap()
                .contains("metrics")
        );
    }

    #[test]
    fn test_artifacts_section() {
        let yaml = "
required_version: \">=0.1.0\"
screenshots:
  - include: \"test.png\"
artifacts: .gleon/runs/ram
";
        let config = GleonConfig::from_yaml_str(yaml).unwrap();
        assert_eq!(
            config.artifacts,
            Some(ArtifactsDir::new(".gleon/runs/ram").unwrap())
        );
        let absolute = yaml.replace(".gleon/runs/ram", "/tmp/ram");
        let err = GleonConfig::from_yaml_str(&absolute).unwrap_err();
        assert!(
            matches!(&err, ConfigError::YamlParse(_))
                && err
                    .to_string()
                    .contains("'/tmp/ram' must be `.gleon/runs/latest/artifacts`"),
            "{err}"
        );
        // Absent by default, so the `gleon init` snapshot is unchanged.
        assert!(
            !serde_yaml::to_string(&GleonConfig::default())
                .unwrap()
                .contains("artifacts")
        );
    }

    #[test]
    fn test_artifacts_dirs_stay_inside_the_run_output() {
        for good in [
            DEFAULT_ARTIFACTS_DIR,
            ".gleon/runs/ram",
            ".gleon/runs/a/b.c/d-e_f",
            ".gleon/runs/latest.old",
        ] {
            assert_eq!(ArtifactsDir::new(good).unwrap().as_str(), good);
        }
        for bad in [
            "",
            "build",
            ".gleon",
            ".gleon/manifests",
            ".gleon/runs",
            ".gleon/runs/",
            ".gleon/runs/latest",
            ".gleon/runs/latest/cases",
            ".gleon/runs/latest/other",
            ".gleon/runs/Latest",
            ".gleon/runs/LATEST/artifacts",
            ".gleon/runs//a",
            ".gleon/runs/a/",
            ".gleon/runs/./a",
            ".gleon/runs/..",
            ".gleon/runs/../../x",
            ".gleon/runs/a\\b",
            ".gleon/runs/C:",
            ".gleon/runs/a b",
            ".gleon/runs/os=ios-sim+arch=arm",
            ".gleon/runs/a/../b",
            ".gleon/runs/a/.",
            "/.gleon/runs/a",
            "./.gleon/runs/a",
            ".GLEON/runs/a",
        ] {
            assert_eq!(
                ArtifactsDir::new(bad),
                Err(InvalidArtifactsDir(bad.to_owned())),
                "{bad}"
            );
        }
        let root = Path::new("root");
        assert_eq!(
            ArtifactsDir::new(".gleon/runs/a").unwrap().to_path(root),
            root.join(".gleon").join("runs").join("a")
        );
        assert_eq!(ArtifactsDir::default().as_str(), DEFAULT_ARTIFACTS_DIR);
    }

    #[test]
    fn test_artifacts_dir_precedence() {
        let dir = |name: &str| ArtifactsDir::new(format!(".gleon/runs/{name}")).unwrap();
        let mut config = GleonConfig::default();
        assert_eq!(config.artifacts_dir(None, None), ArtifactsDir::default());
        config.artifacts = Some(dir("config"));
        assert_eq!(config.artifacts_dir(None, None), dir("config"));
        assert_eq!(config.artifacts_dir(Some(&dir("env")), None), dir("env"));
        assert_eq!(
            config.artifacts_dir(Some(&dir("env")), Some(&dir("flag"))),
            dir("flag")
        );

        assert_eq!(ArtifactsDir::from_env(None).unwrap(), None);
        assert_eq!(ArtifactsDir::from_env(Some(" ")).unwrap(), None);
        assert_eq!(
            ArtifactsDir::from_env(Some(" .gleon/runs/ram ")).unwrap(),
            Some(dir("ram"))
        );
        let err = ArtifactsDir::from_env(Some("/tmp/ram")).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidArtifactsEnv(_)));
        assert!(err.to_string().starts_with(
            "GLEON_ARTIFACTS_DIR: '/tmp/ram' must be `.gleon/runs/latest/artifacts` or a directory"
        ));
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn test_metrics_env_override() {
        let yaml_off = MetricsConfig::default();
        let on = MetricsConfig {
            enabled: true,
            console: false,
        };
        assert_eq!(yaml_off.effective(None).unwrap(), yaml_off);
        assert_eq!(yaml_off.effective(Some("  ")).unwrap(), yaml_off);
        assert!(yaml_off.effective(Some("1")).unwrap().enabled);
        assert!(yaml_off.effective(Some("TRUE")).unwrap().enabled);
        assert_eq!(
            on.effective(Some("0")).unwrap(),
            MetricsConfig {
                enabled: false,
                console: false
            }
        );
        assert!(!on.effective(Some("false")).unwrap().enabled);
        for (value, expected) in [
            (None, None),
            (Some(""), None),
            (Some(" 1 "), Some(true)),
            (Some("False"), Some(false)),
        ] {
            assert_eq!(MetricsConfig::env_override(value).unwrap(), expected);
        }
        let err = on.effective(Some(" yes ")).unwrap_err();
        assert!(matches!(
            &err,
            ConfigError::InvalidEnvFlag { name: "GLEON_METRICS", value } if value == "yes"
        ));
        assert_eq!(
            err.to_string(),
            "GLEON_METRICS must be 1, 0, true or false (got 'yes')"
        );
    }

    #[test]
    fn test_structured_platforms_are_validated_on_load() {
        let config = |platforms: &str| {
            GleonConfig::from_yaml_str(&format!(
                "required_version: '>=0.1.0'\n{platforms}\nscreenshots:\n  - include: a.png\n"
            ))
        };
        assert!(config("platform: {os: macos, arch: aarch64, labels: {theme: dark}}").is_ok());
        // `platform` overrides fields of the detected platform; `fallback_platform` names one.
        assert!(config("platform: {arch: arm, renderer: chrome}").is_ok());
        for (yaml, field) in [
            ("platform: {os: 'mac os'}", "platform"),
            ("platform: {os: macos, labels: {'bad key': x}}", "platform"),
            ("platform: {arch: con}", "platform"),
            ("fallback_platform: {arch: 'x/y'}", "fallback_platform"),
            ("fallback_platform: {arch: x86_64}", "fallback_platform"),
        ] {
            let err = config(yaml).unwrap_err();
            assert!(
                matches!(&err, ConfigError::InvalidPlatform { field: f, .. } if *f == field),
                "{yaml}: {err:?}"
            );
            assert!(
                err.to_string()
                    .starts_with(&format!("Invalid configuration: {field}: "))
            );
            assert!(std::error::Error::source(&err).is_some());
        }
    }

    #[test]
    fn test_from_yaml_str_validates() {
        assert!(matches!(
            GleonConfig::from_yaml_str("required_version: \">=0.1.0\"\nscreenshots: []"),
            Err(ConfigError::Validation(_))
        ));
        assert!(matches!(
            GleonConfig::from_yaml_str("required_version: [1]"),
            Err(ConfigError::YamlParse(_))
        ));
    }

    #[test]
    fn test_config_defaults_applied() {
        let minimal_yaml = "
required_version: \">=0.1.0\"
screenshots:
  - include: \"test.png\"
";
        let config: GleonConfig = serde_yaml::from_str(minimal_yaml).unwrap();
        // Check structural defaults
        assert_eq!(config.platform, None);
        assert!(config.exclude.is_empty());
        assert_eq!(config.screenshots.len(), 1);

        let rule = &config.screenshots[0];
        assert_eq!(rule.mode, Mode::Pixel); // default mode
        assert_eq!(rule.masks, Vec::<MaskRule>::new()); // default empty masks

        // Check nested DiffConfig defaults
        assert_eq!(rule.diff.threshold, 0.1);
        assert_eq!(rule.diff.min_similarity, 0.8);
    }

    #[test]
    fn test_config_roundtrip() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let fixture_path = manifest_dir.join("tests/fixtures/gleon.yaml");
        let original_config = GleonConfig::load_from_file(fixture_path).unwrap();

        // Serialize to YAML string
        let serialized = serde_yaml::to_string(&original_config).unwrap();

        // Deserialize back
        let round_tripped_config: GleonConfig = serde_yaml::from_str(&serialized).unwrap();

        // Validate equality
        assert_eq!(original_config, round_tripped_config);
    }

    #[test]
    #[cfg(all(unix, not(miri)))]
    fn test_load_config_permission_denied() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let file_path = dir.path().join("unreadable_config.yaml");
        std::fs::write(&file_path, "required_version: \">=0.1.0\"").unwrap();

        std::fs::set_permissions(&file_path, std::fs::Permissions::from_mode(0o000)).unwrap();

        // If we are root, writing to 0o000 file will succeed.
        let is_root = std::fs::write(&file_path, "probe").is_ok();
        if is_root {
            return;
        }

        let err = GleonConfig::load_from_file(&file_path).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Io(e) if e.kind() == std::io::ErrorKind::PermissionDenied
        ));
    }

    #[test]
    fn test_load_config_validation_failure() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("invalid_config.yaml");
        std::fs::write(&file_path, "required_version: \">=0.1.0\"\nscreenshots: []").unwrap();

        let err = GleonConfig::load_from_file(&file_path).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
    }
}
