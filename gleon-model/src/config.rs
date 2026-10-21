//! The `.gleon/gleon.yaml` workspace configuration, shared by the gleon CLI and the gleon
//! Flutter package (through `gleon-ffi`).

use std::path::{Path, PathBuf};

use gleon_engine::config::{DiffConfig, Dimension, Zone};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::{
    platform::{PlatformConfig, PlatformError},
    tolerance::{TextTolerance, Tolerance},
};

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
    /// A character class only `/` could match (`[/]`): classes never match a separator, so it
    /// would match nothing.
    #[error("a character class never matches `/`; separate directories with a plain `/`")]
    SeparatorClass,
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
        } else if has_separator_only_class(raw) {
            Some(GlobError::SeparatorClass)
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

/// Whether `raw` has a character class whose members are all `/` (`[/]`, `[//]`, `[/-/]`), in the
/// `glob` crate's syntax: `[!` negates, and a `]` right after `[` or `[!` is a member. An unclosed
/// class is left to the crate's own error.
fn has_separator_only_class(raw: &str) -> bool {
    let mut rest = raw;
    while let Some(open) = rest.find('[') {
        let class = &rest[open + 1..];
        let (is_negated, members) = class
            .strip_prefix('!')
            .map_or((false, class), |members| (true, members));
        let Some(first) = members.chars().next() else {
            return false;
        };
        let Some(close) = members[first.len_utf8()..]
            .find(']')
            .map(|at| at + first.len_utf8())
        else {
            return false;
        };
        let items = &members[..close];
        if !is_negated && items.replace("/-/", "").bytes().all(|byte| byte == b'/') {
            return true;
        }
        rest = &members[close + 1..];
    }
    false
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
    /// The version range of the CLI this configuration needs; `gleon init` writes the version of
    /// the CLI as the floor.
    ///
    /// Optional: a workspace of an integration alone (the Flutter package) needs none, and only
    /// the CLI enforces it; other readers just check its syntax.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub required_version: Option<semver::VersionReq>,
    /// Where the baselines of the workspace live: `manifests` (the default) for the manifests and
    /// blobs of the CLI, `goldens` for the golden files an integration (the Flutter package)
    /// commits next to its tests and compares itself.
    ///
    /// In a `goldens` workspace the CLI never scans, stages or diffs files by the `screenshots`
    /// rules: it reads the case reports of the integration's runs (`gleon test`, `gleon report`,
    /// `gleon dashboard`) and writes approved candidates over the golden files
    /// (`gleon approve`). Integrations read the rules either way.
    #[serde(default, skip_serializing_if = "Baselines::is_manifests")]
    pub baselines: Baselines,
    /// The platform the CLI runs as (e.g. `macos-aarch64`, or `{renderer: impeller}` over the
    /// detected OS and architecture); its flags and `GLEON_PLATFORM` override it.
    ///
    /// Read by the CLI only: an integration (the Flutter package) runs as its process's OS and
    /// architecture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<PlatformConfig>,
    /// The canonical platform: the CLI falls back to its baselines when the current platform has
    /// none.
    ///
    /// For integrations that commit goldens as image files (the Flutter package), only its OS and
    /// architecture count, under the names a process reports (`macos-aarch64`, `linux-x86_64`;
    /// `arm64`, `x64`, `amd64` and `darwin` are read as them; other names are an error). A
    /// golden of the workspace without a platform stamp counts as recorded there (unless the
    /// workspace excludes it). Update mode on it records again what other platforms recorded;
    /// on another platform it keeps a golden recorded elsewhere while only its text differs
    /// (without a `fallback_platform` it writes such a golden over). On it a golden recorded
    /// elsewhere draws a warning, and `GLEON_REQUIRE_CANONICAL=1` compares it as its own.
    /// `platform` and the `GLEON_*PLATFORM` variables are read by the CLI only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_platform: Option<PlatformConfig>,
    /// The screenshot rules: the first whose `include` matches a `.png` applies. May be left
    /// out or empty (a workspace naming only its `fallback_platform`): no golden is matched then,
    /// so integrations compare exactly and record nothing, and the CLI finds no screenshots.
    #[serde(default)]
    pub screenshots: Vec<ScreenshotRule>,
    /// Globs of paths to exclude from testing.
    #[serde(default, with = "item_or_vec", skip_serializing_if = "Vec::is_empty")]
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

/// Where the baselines of a workspace live ([`GleonConfig::baselines`]).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Baselines {
    /// Manifests in `.gleon/manifests/<platform>/` and blobs: `gleon stage` records the
    /// screenshots the rules match, `gleon diff` compares them.
    #[default]
    Manifests,
    /// The golden files of an integration (the Flutter package), committed next to its tests and
    /// compared by its test runner: `gleon test`, `gleon report` and `gleon approve`.
    Goldens,
}

impl Baselines {
    /// Whether these are [`Self::Manifests`], the default (left out when serialized).
    /// It takes a reference because serde passes one to `skip_serializing_if` functions.
    #[must_use]
    pub const fn is_manifests(&self) -> bool {
        matches!(self, Self::Manifests)
    }
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

/// Name of the environment variable that makes integrations on the `fallback_platform` compare
/// the goldens recorded on other platforms as their own (see [`GleonConfig::fallback_platform`]).
pub const REQUIRE_CANONICAL_ENV: &str = "GLEON_REQUIRE_CANONICAL";

/// The boolean environment variable `name` (`env_value` is its raw value, `None` when unset):
/// `None` when unset or empty, else whether it is on.
///
/// # Errors
/// Returns [`ConfigError::InvalidEnvFlag`] for any value other than `1`, `true`, `0` or `false`
/// (ASCII case-insensitive, surrounding whitespace ignored).
pub fn env_flag(name: &'static str, env_value: Option<&str>) -> Result<Option<bool>, ConfigError> {
    let Some(raw) = env_value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    if raw == "1" || raw.eq_ignore_ascii_case("true") {
        Ok(Some(true))
    } else if raw == "0" || raw.eq_ignore_ascii_case("false") {
        Ok(Some(false))
    } else {
        Err(ConfigError::InvalidEnvFlag {
            name,
            value: raw.to_owned(),
        })
    }
}

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

    /// The value of [`METRICS_ENV`] ([`env_flag`]): `None` when unset or empty, else whether it
    /// turns metrics on.
    ///
    /// # Errors
    /// Returns [`ConfigError::InvalidEnvFlag`] as [`env_flag`] does.
    pub fn env_override(env_value: Option<&str>) -> Result<Option<bool>, ConfigError> {
        env_flag(METRICS_ENV, env_value)
    }
}

const fn default_true() -> bool {
    true
}

/// Helper module for serde to deserialize a single string item or a list of items into a
/// `Vec<T>`: the lists of globs (`include`, `exclude`), where a bare item is always a string.
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

    /// Deserializes either a single string item or a list of items into a `Vec<T>`.
    ///
    /// Other bare scalars (`exclude: 123`) are an "expected a string or a list" error, as no item
    /// type of this module reads them.
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

        fn visit_borrowed_str<E: serde::de::Error>(
            self,
            value: &'de str,
        ) -> Result<Self::Value, E> {
            T::deserialize(serde::de::value::BorrowedStrDeserializer::new(value))
                .map(|item| vec![item])
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

/// The comparison of a screenshot rule: `exact`, `pixel` or `ssim`.
///
/// Required: a rule added for its masks alone means exact, while a rule without a mode once
/// compared under `pixel`, silently tolerating 1% of differing pixels.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RuleMode {
    /// Every pixel must be identical; `diff` keys are an error (nothing is tolerated).
    Exact,
    /// Up to `diff.threshold` of the pixels may differ (required), and the pixel options
    /// (`channel_tolerance`, `anti_alias`, `edge_threshold`) let some differing pixels pass.
    Pixel,
    /// The engine's SSIM policy: `diff.min_similarity` and `diff.color_tolerance`.
    Ssim,
}

/// A rule specifying how to match and process screenshots.
///
/// The files it applies to and how they compare, read from the `mode` and `diff` keys of the rule
/// in `gleon.yaml` (written back with the `diff` keys of its mode alone).
#[derive(Debug, Clone, Deserialize, PartialEq, schemars::JsonSchema)]
#[serde(try_from = "RuleFile")]
pub struct ScreenshotRule {
    /// Glob patterns representing the files to include.
    pub include: Vec<GlobPattern>,
    /// How the files compare: the rule's `mode` with the `diff` keys of that mode.
    pub tolerance: Tolerance,
    /// Optional zones to mask out (ignore) during verification.
    pub masks: Vec<MaskRule>,
    /// How much text may differ, in any mode: the largest share of differing pixels in any tile
    /// of the text an integration reports, `[0, 1]`; `None` for the default of the golden
    /// ([`TextTolerance::resolve`]).
    pub text_tolerance: Option<TextTolerance>,
}

/// A rule specifying how to match and process screenshots.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = pixel_needs_threshold)]
struct RuleFile {
    /// Glob patterns representing the files to include.
    #[serde(with = "item_or_vec")]
    #[schemars(with = "item_or_vec::OneOrMany<GlobPattern>")]
    include: Vec<GlobPattern>,
    /// The comparison: `exact`, `pixel` or `ssim` (required, see [`RuleMode`]).
    // An `Option` for serde alone (a missing one is `None`): a rule without it gets an error
    // that names the modes. Without `#[serde(default)]` the schema requires it.
    #[schemars(with = "RuleMode")]
    mode: Option<RuleMode>,
    /// The thresholds of the mode: each key belongs to one mode, and a key of another mode is an
    /// error, even at its default (it would be silently ignored).
    // `skip_serializing_if` (never serialized) keeps a `default` out of the schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "DiffConfig")]
    diff: Option<DiffFile>,
    /// Optional zones to mask out (ignore) during verification.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    masks: Vec<MaskRule>,
    /// How much text may differ, in any mode: the largest share of differing pixels in any tile
    /// of the text an integration reports, `[0, 1]`. Unset, it depends on the golden
    /// ([`crate::tolerance::TextTolerance::resolve`]): 0.05 on the platform the golden was
    /// recorded on (see `fallback_platform`), else 1, so text never fails. A value set here
    /// applies to every golden of the rule: 1 turns text comparison off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text_tolerance: Option<TextTolerance>,
}

/// The schema of a rule requires `diff.threshold` under `mode: pixel`, as parsing does
/// ([`RuleError::MissingThreshold`]), so editors flag it too.
fn pixel_needs_threshold(schema: &mut schemars::Schema) {
    schema.insert(
        "if".to_owned(),
        serde_json::json!({ "properties": { "mode": { "const": "pixel" } }, "required": ["mode"] }),
    );
    schema.insert(
        "then".to_owned(),
        serde_json::json!({
            "required": ["diff"],
            "properties": { "diff": { "required": ["threshold"] } }
        }),
    );
}

/// A screenshot rule of `gleon.yaml` spelled wrong.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RuleError {
    /// No `mode`: there is no default, a rule added for its masks alone would otherwise tolerate
    /// what `pixel` tolerates.
    #[error(
        "missing field `mode`: `exact` (every pixel identical), `pixel` (the share of the pixels \
         in `diff.threshold` may differ; `threshold: 0.01` is what a rule without a mode meant in \
         earlier versions) or `ssim`"
    )]
    MissingMode,
    /// `mode: pixel` without `diff.threshold`: the share of the pixels it tolerates is never
    /// implicit.
    #[error(
        "`mode: pixel` needs `diff.threshold`: the share of the pixels that may differ (0.0–1.0, \
         0 for none)"
    )]
    MissingThreshold,
    /// A `diff` key of another mode than the rule's (at any value: it would never apply).
    #[error("diff.{key} applies to `mode: {owner}` only")]
    ForeignKey {
        /// The key.
        key: &'static str,
        /// The mode it belongs to.
        owner: &'static str,
    },
    /// A `diff` key outside its range.
    #[error("diff.{key} must be {range} (got {value})")]
    OutOfRange {
        /// The key.
        key: &'static str,
        /// Its range, in words.
        range: &'static str,
        /// The value written.
        value: String,
    },
}

/// The `diff` keys of a rule as written, each one there or not: a key of another mode is an
/// error by being there, a key of the rule's mode left out is its default.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiffFile {
    threshold: Option<f64>,
    min_similarity: Option<f64>,
    color_tolerance: Option<f64>,
    channel_tolerance: Option<u8>,
    anti_alias: Option<bool>,
    edge_threshold: Option<u8>,
}

impl DiffFile {
    /// The first key written that does not belong to `mode`, with the mode it belongs to;
    /// `exact` owns none.
    fn foreign_key(&self, mode: RuleMode) -> Option<(&'static str, &'static str)> {
        // Destructured, so a new key cannot be left out of its mode.
        let Self {
            threshold,
            min_similarity,
            color_tolerance,
            channel_tolerance,
            anti_alias,
            edge_threshold,
        } = self;
        let pixel = [
            ("threshold", threshold.is_some()),
            ("channel_tolerance", channel_tolerance.is_some()),
            ("anti_alias", anti_alias.is_some()),
            ("edge_threshold", edge_threshold.is_some()),
        ];
        let ssim = [
            ("min_similarity", min_similarity.is_some()),
            ("color_tolerance", color_tolerance.is_some()),
        ];
        let foreign = |keys: &[(&'static str, bool)], owner: &'static str| {
            keys.iter()
                .find(|&&(_, is_written)| is_written)
                .map(|&(key, _)| (key, owner))
        };
        match mode {
            RuleMode::Exact => foreign(&pixel, "pixel").or_else(|| foreign(&ssim, "ssim")),
            RuleMode::Pixel => foreign(&ssim, "ssim"),
            RuleMode::Ssim => foreign(&pixel, "pixel"),
        }
    }

    /// The thresholds: each key written, checked against its range, else its default.
    fn config(&self) -> Result<DiffConfig, RuleError> {
        fn checked<T: Copy + ToString>(
            key: &'static str,
            range: &'static str,
            value: Option<T>,
            default: T,
            is_valid: impl Fn(T) -> bool,
        ) -> Result<T, RuleError> {
            match value {
                Some(value) if !is_valid(value) => Err(RuleError::OutOfRange {
                    key,
                    range,
                    value: value.to_string(),
                }),
                value => Ok(value.unwrap_or(default)),
            }
        }

        const RATIO: &str = "between 0.0 and 1.0";
        const PIXEL_OPTION: &str = "between 0 and 254 (0 turns it off)";
        let defaults = DiffConfig::default();
        let ratio = |value: f64| (0.0..=1.0).contains(&value);
        let pixel_option = |value: u8| value <= gleon_engine::config::MAX_PIXEL_OPTION;
        Ok(DiffConfig {
            threshold: checked(
                "threshold",
                RATIO,
                self.threshold,
                defaults.threshold,
                ratio,
            )?,
            min_similarity: checked(
                "min_similarity",
                RATIO,
                self.min_similarity,
                defaults.min_similarity,
                ratio,
            )?,
            color_tolerance: checked(
                "color_tolerance",
                "between 0 and 255 (8-bit channel units)",
                self.color_tolerance,
                defaults.color_tolerance,
                gleon_engine::config::is_valid_color_tolerance,
            )?,
            channel_tolerance: checked(
                "channel_tolerance",
                PIXEL_OPTION,
                self.channel_tolerance,
                defaults.channel_tolerance,
                pixel_option,
            )?,
            anti_alias: self.anti_alias.unwrap_or(defaults.anti_alias),
            edge_threshold: checked(
                "edge_threshold",
                PIXEL_OPTION,
                self.edge_threshold,
                defaults.edge_threshold,
                pixel_option,
            )?,
        })
    }
}

impl TryFrom<RuleFile> for ScreenshotRule {
    type Error = RuleError;

    fn try_from(file: RuleFile) -> Result<Self, Self::Error> {
        let mode = file.mode.ok_or(RuleError::MissingMode)?;
        let diff = file.diff.unwrap_or_default();
        if let Some((key, owner)) = diff.foreign_key(mode) {
            return Err(RuleError::ForeignKey { key, owner });
        }
        if mode == RuleMode::Pixel && diff.threshold.is_none() {
            return Err(RuleError::MissingThreshold);
        }
        Ok(Self {
            include: file.include,
            tolerance: Tolerance::from_rule(mode, &diff.config()?),
            masks: file.masks,
            text_tolerance: file.text_tolerance,
        })
    }
}

/// The `diff` keys a rule is written with: those of its mode, the threshold of `pixel` always
/// (the share of pixels it tolerates is never implicit), its pixel options when set.
#[derive(Serialize)]
#[serde(untagged)]
enum DiffKeys {
    Pixel {
        threshold: f64,
        #[serde(skip_serializing_if = "crate::serde_skip::is_default")]
        channel_tolerance: u8,
        #[serde(skip_serializing_if = "crate::serde_skip::is_default")]
        anti_alias: bool,
        #[serde(skip_serializing_if = "crate::serde_skip::is_default")]
        edge_threshold: u8,
    },
    Ssim {
        min_similarity: f64,
        color_tolerance: f64,
    },
}

impl Serialize for ScreenshotRule {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        /// [`RuleFile`] as written.
        #[derive(Serialize)]
        struct Written<'a> {
            #[serde(serialize_with = "item_or_vec::serialize")]
            include: &'a [GlobPattern],
            mode: RuleMode,
            #[serde(skip_serializing_if = "Option::is_none")]
            diff: Option<DiffKeys>,
            #[serde(skip_serializing_if = "<[_]>::is_empty")]
            masks: &'a [MaskRule],
            #[serde(skip_serializing_if = "Option::is_none")]
            text_tolerance: Option<TextTolerance>,
        }

        let (mode, diff) = match self.tolerance {
            Tolerance::Exact {} => (RuleMode::Exact, None),
            Tolerance::Pixel {
                max_diff_ratio,
                channel_tolerance,
                anti_alias,
                edge_threshold,
            } => (
                RuleMode::Pixel,
                Some(DiffKeys::Pixel {
                    threshold: max_diff_ratio,
                    channel_tolerance,
                    anti_alias,
                    edge_threshold,
                }),
            ),
            Tolerance::Ssim {
                min_similarity,
                color_tolerance,
            } => (
                RuleMode::Ssim,
                Some(DiffKeys::Ssim {
                    min_similarity,
                    color_tolerance,
                }),
            ),
        };
        Written {
            include: &self.include,
            mode,
            diff,
            masks: &self.masks,
            text_tolerance: self.text_tolerance,
        }
        .serialize(serializer)
    }
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
        for (i, rule) in self.screenshots.iter().enumerate() {
            if rule.include.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "screenshots[{i}].include must contain at least one glob pattern"
                )));
            }
            // A rule read from YAML is checked when parsed; one built in code is checked here.
            rule.tolerance
                .validate()
                .map_err(|e| ConfigError::Validation(format!("screenshots[{i}].diff: {e}")))?;
            if let Some(text) = &rule.text_tolerance {
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
        Self {
            required_version: Some(DEFAULT_VERSION.clone()),
            baselines: Baselines::Manifests,
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
                tolerance: Tolerance::pixel(0.01),
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

impl GleonConfig {
    /// The config `gleon init` writes for a Flutter package or app (a `pubspec.yaml` in the
    /// workspace root): the goldens under every `test/` (the root's, the packages' of a pub
    /// workspace, `example/`'s) compared exactly, their text on `canonical` (the platform they
    /// are recorded on, normally the host running `gleon init`), the images Flutter and gleon
    /// write for failures (`failures/`) excluded; the version floor of [`Self::default`].
    ///
    /// # Panics
    /// Never: the hard-coded patterns are valid.
    #[must_use]
    pub fn for_flutter(canonical: PlatformConfig) -> Self {
        #[expect(
            clippy::expect_used,
            reason = "hard-coded patterns are statically valid and covered by tests"
        )]
        let glob = |pattern| GlobPattern::new(pattern).expect("the Flutter globs must be valid");
        Self {
            baselines: Baselines::Goldens,
            fallback_platform: Some(canonical),
            screenshots: vec![ScreenshotRule {
                include: vec![glob("**/test/**/*.png")],
                tolerance: Tolerance::Exact {},
                masks: vec![],
                text_tolerance: None,
            }],
            exclude: vec![glob("**/failures/**")],
            ..Self::default()
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
    use crate::tolerance::Tolerance;

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
            Some(VersionReq::parse(">=0.1.0").unwrap())
        );
        assert_eq!(
            config.platform,
            Some(PlatformConfig::Opaque("macos-aarch64".to_string()))
        );
        assert_eq!(config.exclude[0].as_str(), "**/ignored/**");
        assert_eq!(config.screenshots.len(), 1);

        let rule = &config.screenshots[0];
        assert_eq!(rule.include[0].as_str(), "src/login.png");
        assert_eq!(
            rule.tolerance,
            Tolerance::Ssim {
                min_similarity: 0.98,
                color_tolerance: 12.0,
            }
        );
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

    /// The error of an invalid config, which names `needle`: each rule has its mode, so the
    /// intended check fails, not the missing mode.
    fn assert_config_error(yaml: &str, needle: &str) {
        let err = GleonConfig::from_yaml_str(yaml).unwrap_err().to_string();
        assert!(err.contains(needle), "expected {needle:?} in: {err}");
        assert!(!err.contains("missing field `mode`"), "{err}");
    }

    #[test]
    fn test_load_config_invalid_dimension() {
        // A non-numeric dimension.
        assert_config_error(
            "
required_version: \">=0.1.0\"
screenshots:
  - include: \"test.png\"
    mode: pixel
    diff: { threshold: 0.01 }
    masks:
      - path: \"mask\"
        zones:
          - x: 0
            y: 0
            width: \"not_a_number\"
            height: 10
",
            "not_a_number",
        );

        // A percentage beyond 100%.
        assert_config_error(
            "
required_version: \">=0.1.0\"
screenshots:
  - include: \"test.png\"
    mode: pixel
    diff: { threshold: 0.01 }
    masks:
      - path: \"mask\"
        zones:
          - x: 0
            y: 0
            width: 10
            height: \"105%\"
",
            "zones[0]: percentage must be between 0.0 and 100.0",
        );
    }

    #[test]
    fn test_load_config_invalid_ratio() {
        assert_config_error(
            "
required_version: \">=0.1.0\"
screenshots:
  - include: \"test.png\"
    mode: pixel
    diff:
      threshold: 1.5
",
            "diff.threshold must be between 0.0 and 1.0 (got 1.5)",
        );
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

    /// A workspace may name only its canonical platform: no rule, no version floor.
    #[test]
    fn test_minimal_config_has_no_rules_and_no_version() {
        let config = GleonConfig::from_yaml_str("fallback_platform: macos-aarch64\n").unwrap();
        assert_eq!(config.required_version, None);
        assert!(config.screenshots.is_empty());
        assert_eq!(
            config.fallback_platform,
            Some(PlatformConfig::Opaque("macos-aarch64".to_owned()))
        );
        assert!(
            GleonConfig::from_yaml_str("required_version: \">=0.1.0\"\nscreenshots: []").is_ok()
        );
        assert!(GleonConfig::from_yaml_str("{}").is_ok());
        // Serialized, an absent version stays absent.
        assert!(
            !serde_yaml::to_string(&config)
                .unwrap()
                .contains("required_version")
        );
    }

    /// A rule names its mode: no default tolerates a share of the pixels silently.
    #[test]
    fn test_rule_without_mode_is_an_error() {
        let yaml = "screenshots:\n  - include: 'a/*.png'\n";
        let err = GleonConfig::from_yaml_str(yaml).unwrap_err();
        assert!(matches!(err, ConfigError::YamlParse(_)), "{err:?}");
        // The modes are named, and what a rule without one meant (an upgrade adds `mode: pixel`).
        for needle in [
            "missing field `mode`",
            "`exact`",
            "`ssim`",
            "in earlier versions",
            "line 2",
        ] {
            assert!(err.to_string().contains(needle), "{needle}: {err}");
        }
        for (mode, diff, expected) in [
            ("exact", "", Tolerance::Exact {}),
            (
                "pixel",
                "    diff: { threshold: 0.01 }\n",
                Tolerance::pixel(0.01),
            ),
            (
                "ssim",
                "",
                Tolerance::Ssim {
                    min_similarity: 0.8,
                    color_tolerance: 8.0,
                },
            ),
        ] {
            let yaml = format!("screenshots:\n  - include: 'a/*.png'\n    mode: {mode}\n{diff}");
            let config = GleonConfig::from_yaml_str(&yaml).unwrap();
            assert_eq!(config.screenshots[0].tolerance, expected, "{mode}");
            // Written back with the keys of its mode alone.
            let written = serde_yaml::to_string(&config).unwrap();
            assert!(written.contains(&format!("mode: {mode}\n")), "{written}");
            assert_eq!(
                written.contains("threshold: 0.01"),
                mode == "pixel",
                "{written}"
            );
            assert_eq!(
                written.contains("min_similarity: 0.8"),
                mode == "ssim",
                "{written}"
            );
            assert_eq!(written.contains("diff"), mode != "exact", "{written}");
            assert!(!written.contains("anti_alias"), "{written}");
            assert_eq!(GleonConfig::from_yaml_str(&written).unwrap(), config);
        }
    }

    /// `pixel` names the share of the pixels it tolerates: a default would tolerate 1% silently.
    #[test]
    fn test_pixel_rule_without_threshold_is_an_error() {
        for diff in ["", "    diff: {}\n", "    diff: { anti_alias: true }\n"] {
            let yaml = format!("screenshots:\n  - include: 'a/*.png'\n    mode: pixel\n{diff}");
            let err = GleonConfig::from_yaml_str(&yaml).unwrap_err();
            assert!(matches!(err, ConfigError::YamlParse(_)), "{err:?}");
            for needle in [
                "`mode: pixel` needs `diff.threshold`",
                "0 for none",
                "line 2",
            ] {
                assert!(err.to_string().contains(needle), "{needle}: {err}");
            }
        }
        // 0 is a threshold: every pixel identical, with the pixel options.
        let yaml =
            "screenshots:\n  - include: 'a/*.png'\n    mode: pixel\n    diff: { threshold: 0 }\n";
        let config = GleonConfig::from_yaml_str(yaml).unwrap();
        assert_eq!(config.screenshots[0].tolerance, Tolerance::pixel(0.0));
    }

    /// `exact` tolerates nothing, so every `diff` key is foreign to it.
    #[test]
    fn test_exact_rule_refuses_diff_keys() {
        for (diff, key, owner) in [
            ("{ threshold: 0.5 }", "threshold", "pixel"),
            ("{ anti_alias: true }", "anti_alias", "pixel"),
            ("{ channel_tolerance: 2 }", "channel_tolerance", "pixel"),
            ("{ edge_threshold: 64 }", "edge_threshold", "pixel"),
            ("{ min_similarity: 0.9 }", "min_similarity", "ssim"),
            ("{ color_tolerance: 9 }", "color_tolerance", "ssim"),
        ] {
            let yaml = format!(
                "screenshots:\n  - include: 'a/*.png'\n    mode: exact\n    diff: {diff}\n"
            );
            let err = GleonConfig::from_yaml_str(&yaml).unwrap_err().to_string();
            assert!(
                err.contains(&format!(
                    "diff.{key} applies to `mode: {owner}` only at line 2"
                )),
                "{err}"
            );
        }
        // A key is an error by being there, even at its default: `threshold: 0.01` under `exact`
        // reads like a tolerance that would never apply. An empty `diff` sets nothing.
        let yaml = "screenshots:\n  - include: 'a/*.png'\n    mode: exact\n    diff: { threshold: 0.01 }\n";
        let err = GleonConfig::from_yaml_str(yaml).unwrap_err().to_string();
        assert!(
            err.contains("diff.threshold applies to `mode: pixel` only"),
            "{err}"
        );
        let yaml = "screenshots:\n  - include: 'a/*.png'\n    mode: exact\n    diff: {}\n";
        assert!(GleonConfig::from_yaml_str(yaml).is_ok());
    }

    /// The template of a Flutter workspace: exact goldens under every `test/` (a package of a pub
    /// workspace too), text on the canonical platform, the images of failures left out.
    #[test]
    fn test_flutter_template() {
        let config = GleonConfig::for_flutter(PlatformConfig::Opaque("macos-aarch64".to_owned()));
        assert!(config.validate().is_ok());
        assert_eq!(config.baselines, Baselines::Goldens);
        let yaml = serde_yaml::to_string(&config).unwrap();
        assert!(yaml.contains("baselines: goldens"), "{yaml}");
        assert!(yaml.contains("fallback_platform: macos-aarch64"), "{yaml}");
        assert!(yaml.contains("include: '**/test/**/*.png'"), "{yaml}");
        assert!(yaml.contains("mode: exact"), "{yaml}");
        assert!(yaml.contains("exclude: '**/failures/**'"), "{yaml}");
        assert!(!yaml.contains("masks"), "{yaml}");
        let rules = crate::rules::RuleSet::new(&config);
        for (path, matched) in [
            ("test/goldens/a.png", true),
            ("packages/app/test/goldens/a.png", true),
            ("example/test/a.png", true),
            ("test/failures/a_masterImage.png", false),
            ("packages/app/test/failures/a_isolatedDiff.png", false),
            ("lib/assets/logo.png", false),
        ] {
            assert_eq!(
                matches!(
                    rules.resolve(path).unwrap(),
                    crate::rules::RuleMatch::Matched { .. }
                ),
                matched,
                "{path}"
            );
        }
        let parsed = GleonConfig::from_yaml_str(&yaml).unwrap();
        assert_eq!(parsed, config);
        assert_eq!(parsed.screenshots[0].tolerance, Tolerance::Exact {});
    }

    /// `baselines` defaults to manifests and is then left out; `goldens` round-trips; any other
    /// value is a parse error, never a silent default.
    #[test]
    fn test_baselines_key() {
        let config = GleonConfig::from_yaml_str("fallback_platform: macos-aarch64\n").unwrap();
        assert_eq!(config.baselines, Baselines::Manifests);
        assert!(
            !serde_yaml::to_string(&config)
                .unwrap()
                .contains("baselines")
        );
        assert!(
            !serde_yaml::to_string(&GleonConfig::default())
                .unwrap()
                .contains("baselines")
        );

        let goldens = GleonConfig::from_yaml_str("baselines: goldens\n").unwrap();
        assert_eq!(goldens.baselines, Baselines::Goldens);
        let yaml = serde_yaml::to_string(&goldens).unwrap();
        assert_eq!(GleonConfig::from_yaml_str(&yaml).unwrap(), goldens);

        let explicit = GleonConfig::from_yaml_str("baselines: manifests\n").unwrap();
        assert_eq!(explicit.baselines, Baselines::Manifests);

        let err = GleonConfig::from_yaml_str("baselines: blobs\n").unwrap_err();
        assert!(matches!(err, ConfigError::YamlParse(_)), "{err}");
        assert!(err.to_string().contains("goldens"), "{err}");
    }

    #[test]
    fn test_validation_empty_include() {
        let yaml = "
required_version: \">=0.1.0\"
screenshots:
  - include: []
    mode: exact
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
        config.screenshots[0].tolerance = Tolerance::pixel(1.5);
        let result = config.validate();
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ConfigError::Validation(msg) if msg.contains("screenshots[0].diff: `max_diff_ratio` must be between 0.0 and 1.0")
        ));
        // Read from YAML, the key is checked when parsed, by its name.
        let err = GleonConfig::from_yaml_str(
            "screenshots:\n  - include: a.png\n    mode: pixel\n    diff: { threshold: 1.5 }\n",
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("diff.threshold must be between 0.0 and 1.0 (got 1.5) at line 2"),
            "{err}"
        );
    }

    /// Pixel options set on an SSIM rule would silently do nothing: a config error naming them.
    #[test]
    fn test_diff_keys_need_the_mode_that_reads_them() {
        let yaml = |mode: &str, diff: &str| {
            format!(
                "required_version: '>=0.1.0'\nscreenshots:\n  - include: 'a/*.png'\n    mode: {mode}\n    diff: {diff}\n"
            )
        };
        for (diff, key, owner, other) in [
            ("{ anti_alias: true }", "anti_alias", "pixel", "ssim"),
            (
                "{ channel_tolerance: 4 }",
                "channel_tolerance",
                "pixel",
                "ssim",
            ),
            ("{ edge_threshold: 64 }", "edge_threshold", "pixel", "ssim"),
            ("{ threshold: 0.05 }", "threshold", "pixel", "ssim"),
            ("{ min_similarity: 0.9 }", "min_similarity", "ssim", "pixel"),
            (
                "{ color_tolerance: 10 }",
                "color_tolerance",
                "ssim",
                "pixel",
            ),
        ] {
            let err = GleonConfig::from_yaml_str(&yaml(other, diff)).unwrap_err();
            assert!(
                err.to_string()
                    .contains(&format!("diff.{key} applies to `mode: {owner}` only")),
                "{err}"
            );
            // `pixel` names its threshold too.
            let own = if owner == "pixel" && key != "threshold" {
                diff.replacen("{ ", "{ threshold: 0, ", 1)
            } else {
                diff.to_owned()
            };
            assert!(
                GleonConfig::from_yaml_str(&yaml(owner, &own)).is_ok(),
                "{own}"
            );
        }
        // The keys of another mode are errors at their defaults too (the full `diff` blocks
        // `gleon init` wrote before 0.4.0 keep the keys of their mode only); a rule's own keys
        // at their defaults are fine.
        let defaults = "{ threshold: 0.01, min_similarity: 0.8, color_tolerance: 8.0, \
                        channel_tolerance: 0, anti_alias: false, edge_threshold: 0 }";
        for (mode, foreign) in [("pixel", "min_similarity"), ("ssim", "threshold")] {
            let err = GleonConfig::from_yaml_str(&yaml(mode, defaults))
                .unwrap_err()
                .to_string();
            assert!(err.contains(&format!("diff.{foreign} applies to")), "{err}");
        }
        let own = [
            (
                "pixel",
                "{ threshold: 0.01, channel_tolerance: 0, anti_alias: false, edge_threshold: 0 }",
            ),
            ("ssim", "{ min_similarity: 0.8, color_tolerance: 8.0 }"),
        ];
        for (mode, diff) in own {
            assert!(
                GleonConfig::from_yaml_str(&yaml(mode, diff)).is_ok(),
                "{mode}"
            );
        }
    }

    /// `text_tolerance` is a share, for pixel and SSIM rules alike.
    #[test]
    fn test_rule_text_tolerance() {
        let yaml = |mode: &str, text: &str| {
            format!(
                "required_version: '>=0.1.0'\nscreenshots:\n  - include: 'a/*.png'\n    mode: {mode}\n    text_tolerance: {text}\n"
            )
        };
        let pixel = "pixel\n    diff: { threshold: 0.01 }";
        for mode in [pixel, "ssim"] {
            let config = GleonConfig::from_yaml_str(&yaml(mode, "0.1")).unwrap();
            assert_eq!(
                config.screenshots[0].text_tolerance,
                Some(TextTolerance(0.1))
            );
        }
        let err = GleonConfig::from_yaml_str(&yaml(pixel, "2")).unwrap_err();
        assert!(
            err.to_string().contains(
                "screenshots[0].text_tolerance: `text_tolerance` must be between 0.0 and 1.0"
            ),
            "{err}"
        );
    }

    #[test]
    fn test_validation_invalid_color_tolerance() {
        for bad in [f64::NAN, -1.0, 255.5, f64::INFINITY] {
            let mut config = GleonConfig::default();
            config.screenshots[0].tolerance = Tolerance::Ssim {
                min_similarity: 0.8,
                color_tolerance: bad,
            };
            assert!(matches!(
                config.validate(),
                Err(ConfigError::Validation(msg)) if msg.contains("color_tolerance` must be between 0 and 255")
            ));
        }
    }

    #[test]
    fn test_validation_invalid_min_similarity() {
        let mut config = GleonConfig::default();
        config.screenshots[0].tolerance = Tolerance::Ssim {
            min_similarity: -0.1,
            color_tolerance: 8.0,
        };
        let result = config.validate();
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ConfigError::Validation(msg) if msg.contains("`min_similarity` must be between 0.0 and 1.0")
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
        // `threshold` is a string, not a float.
        assert_config_error(
            "
required_version: \">=0.1.0\"
screenshots:
  - include: \"test.png\"
    mode: pixel
    diff:
      threshold: \"not-a-float\"
",
            "not-a-float",
        );
    }

    /// An invalid glob of `include` or `exclude` names itself, why and where, through the
    /// one-or-many field.
    #[test]
    fn test_invalid_globs_name_the_pattern_and_the_reason() {
        for (yaml, needles) in [
            (
                "required_version: '>=0.1.0'\nscreenshots:\n  - include: 'test/{a,b}/*.png'\n    mode: exact\n",
                [
                    "screenshots[0].include",
                    "\"test/{a,b}/*.png\"",
                    "alternatives",
                    "line 3",
                ],
            ),
            (
                "required_version: '>=0.1.0'\nscreenshots:\n  - include: ['a/*.png', 'b/[^c].png']\n    mode: exact\n",
                [
                    "screenshots[0].include",
                    "\"b/[^c].png\"",
                    "[!...]",
                    "line 3",
                ],
            ),
            (
                "required_version: '>=0.1.0'\nscreenshots:\n  - include: '**/*.png'\n    mode: exact\nexclude: build/\n",
                ["exclude", "\"build/\"", "cannot end with `/`", "line 5"],
            ),
            (
                "required_version: '>=0.1.0'\nscreenshots:\n  - include: '../other/**/*.png'\n    mode: exact\n",
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

        // An owned string (from a YAML value) is read like a borrowed one.
        let value: serde_yaml::Value = serde_yaml::from_str("values: hello").unwrap();
        let owned: TestStruct = serde_yaml::from_value(value).unwrap();
        assert_eq!(owned, s1);

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

    /// A class only `/` could match matches nothing (classes never match a separator); one that
    /// can match anything else is a class like any other.
    #[test]
    fn test_separator_only_classes_are_errors() {
        for pattern in ["a[/]b.png", "a[//]b.png", "a[/-/]b.png", "x/[a]y[/]z.png"] {
            assert!(
                matches!(GlobPattern::new(pattern), Err(GlobError::SeparatorClass)),
                "{pattern}"
            );
        }
        for (pattern, path) in [
            ("a-[!/]-b.png", "a-x-b.png"),
            ("a-[x/]-b.png", "a-x-b.png"),
            ("a[.-0]b.png", "a.b.png"),
            ("a[]]b.png", "a]b.png"),
            ("a-[!]]-b.png", "a-x-b.png"),
            ("a-[é/]-b.png", "a-é-b.png"),
        ] {
            let glob = GlobPattern::new(pattern).unwrap();
            assert!(glob.is_match(path), "{pattern} {path}");
        }
        // Unclosed classes are the crate's syntax errors.
        for pattern in ["a[/b.png", "a.png["] {
            assert!(
                matches!(GlobPattern::new(pattern), Err(GlobError::Syntax(_))),
                "{pattern}"
            );
        }
    }

    #[test]
    fn test_diff_config_invalid_ratio_bounds() {
        assert_config_error(
            "
required_version: \">=0.1.0\"
screenshots:
  - include: \"test.png\"
    mode: pixel
    diff:
      threshold: -0.1
",
            "diff.threshold must be between 0.0 and 1.0 (got -0.1)",
        );
        assert_config_error(
            "
required_version: \">=0.1.0\"
screenshots:
  - include: \"test.png\"
    mode: ssim
    diff:
      min_similarity: -0.05
",
            "diff.min_similarity must be between 0.0 and 1.0 (got -0.05)",
        );
    }

    #[test]
    fn test_metrics_section() {
        let yaml = "
required_version: \">=0.1.0\"
screenshots:
  - include: \"test.png\"
    mode: exact
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
    mode: exact
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
                "required_version: '>=0.1.0'\n{platforms}\nscreenshots:\n  - include: a.png\n    mode: exact\n"
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
            GleonConfig::from_yaml_str("screenshots:\n  - include: []\n    mode: exact\n"),
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
    mode: pixel
    diff: { threshold: 0.02 }
";
        let config: GleonConfig = serde_yaml::from_str(minimal_yaml).unwrap();
        // Check structural defaults
        assert_eq!(config.platform, None);
        assert!(config.exclude.is_empty());
        assert_eq!(config.screenshots.len(), 1);

        let rule = &config.screenshots[0];
        assert_eq!(rule.masks, Vec::<MaskRule>::new()); // default empty masks
        // The defaults of the pixel options.
        assert_eq!(rule.tolerance, Tolerance::pixel(0.02));
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
        std::fs::write(
            &file_path,
            "required_version: \">=0.1.0\"\nscreenshots:\n  - include: []\n    mode: exact\n",
        )
        .unwrap();

        let err = GleonConfig::load_from_file(&file_path).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
    }
}
