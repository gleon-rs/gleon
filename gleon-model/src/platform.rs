//! Platform identity: the `platform` / `fallback_platform` config values and their storage keys.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Operating system of the running process, named like the CLI auto-detection names it (`macos`,
/// `linux`, `windows`).
pub const HOST_OS: &str = std::env::consts::OS;
/// CPU architecture of the running process, named like the CLI auto-detection names it (`aarch64`,
/// `x86_64`).
pub const HOST_ARCH: &str = std::env::consts::ARCH;

/// Errors that can occur during platform resolution.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum PlatformError {
    /// A structured override (OS/arch/renderer/labels) was supplied alongside an
    /// opaque platform string, which cannot be merged with it.
    #[error("Cannot apply structured overrides ({0}) to an opaque platform configuration")]
    OpaqueConflict(String),
    /// A platform segment (OS, arch, renderer, or label key/value) contained
    /// disallowed characters or was empty.
    #[error("Invalid character or pattern in platform segment: {0}")]
    InvalidSegment(String),
    /// A `GLEON_PLATFORM`-style key-value string could not be parsed.
    #[error("Failed to parse platform string: {0}")]
    ParseError(String),
    /// A label key collided with a reserved key (e.g. `os`, `arch`).
    #[error("Label key '{0}' is reserved — use --{1} flag instead")]
    ReservedLabelKey(String, String),
}

/// Resolved platform identity, used for baseline isolation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlatformInfo {
    /// Operating system (e.g. "macos", "linux", "windows").
    pub os: String,
    /// CPU architecture (e.g. "aarch64", "`x86_64`").
    pub arch: Option<String>,
    /// Optional renderer identifier (e.g. "flutter-3.22", "chrome-126").
    pub renderer: Option<String>,
    /// Arbitrary key-value labels for additional isolation axes.
    /// Sorted alphabetically by key (`BTreeMap` guarantees this).
    pub labels: BTreeMap<String, String>,
}

/// A parsed representation of structured platform fields.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlatformFields {
    /// Operating system override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// CPU architecture override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    /// Renderer identifier override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renderer: Option<String>,
    /// Arbitrary key-value label overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<BTreeMap<String, String>>,
}

/// User- or config-supplied platform configuration, either an opaque string or
/// structured fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformConfig {
    /// Opaque string — validated and normalized (lowercased) for the storage key.
    Opaque(String),
    /// Structured fields — resolved dynamically.
    Structured(PlatformFields),
}

impl PlatformConfig {
    /// The auto-detected platform of the running process (OS and architecture only), exactly what
    /// the CLI resolves when no flag, environment variable or config value overrides it.
    #[must_use]
    pub fn host() -> Self {
        Self::Structured(PlatformFields {
            os: Some(HOST_OS.to_owned()),
            arch: Some(HOST_ARCH.to_owned()),
            renderer: None,
            labels: None,
        })
    }

    /// Resolves this configuration to a platform key string.
    ///
    /// # Errors
    /// Returns [`PlatformError`] if invalid characters are present in fields.
    pub fn to_key(&self) -> Result<String, PlatformError> {
        match self {
            Self::Opaque(s) => validate_segment(s).map(std::borrow::Cow::into_owned),
            Self::Structured(fields) => {
                let info = PlatformInfo {
                    os: fields.os.clone().unwrap_or_else(|| "unknown".to_string()),
                    arch: fields.arch.clone(),
                    renderer: fields.renderer.clone(),
                    labels: fields.labels.clone().unwrap_or_default(),
                };
                info.to_key()
            }
        }
    }
}

impl Serialize for PlatformConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Opaque(s) => serializer.serialize_str(s),
            Self::Structured(fields) => fields.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for PlatformConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = PlatformConfig;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a string or a map representing structured platform config")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                validate_segment(v)
                    .map(|c| PlatformConfig::Opaque(c.into_owned()))
                    .map_err(E::custom)
            }

            fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                validate_segment(&v)
                    .map(|c| PlatformConfig::Opaque(c.into_owned()))
                    .map_err(E::custom)
            }

            fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let fields =
                    PlatformFields::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                Ok(PlatformConfig::Structured(fields))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

// Schema twin of `PlatformConfig`'s hand-written serde representation.
/// A platform: an opaque key, or structured fields.
#[derive(schemars::JsonSchema)]
#[serde(untagged)]
#[expect(
    dead_code,
    reason = "only describes the JSON shape for the schema generator"
)]
enum PlatformConfigSchema {
    /// Opaque `[a-z0-9_.-]` platform key.
    Opaque(#[schemars(regex(pattern = r"^[A-Za-z0-9_.-]+$"))] String),
    /// Structured platform fields.
    Structured(PlatformFields),
}

impl schemars::JsonSchema for PlatformConfig {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "PlatformConfig".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        PlatformConfigSchema::json_schema(generator)
    }
}

impl PlatformFields {
    /// Parses a key-value comma-separated string or fallback simple string.
    ///
    /// # Errors
    /// Returns a descriptive `String` error if a `key=value` segment is malformed
    /// (missing `=`, empty value), or if a hyphen-separated `os-arch` string is
    /// ambiguous (contains more than one hyphen).
    pub fn parse_key_value(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.is_empty() {
            return Ok(Self::default());
        }

        let mut fields = Self::default();
        if s.contains('=') {
            for part in s.split(',') {
                let (key, val) = part
                    .split_once('=')
                    .ok_or_else(|| format!("invalid format: no '=' found in '{part}'"))?;
                let key = key.trim();
                let val = val.trim();

                if val.is_empty() {
                    return Err(format!("Empty value for key '{key}'"));
                }

                match key {
                    "os" | "platform" => fields.os = Some(val.to_string()),
                    "arch" | "architecture" => fields.arch = Some(val.to_string()),
                    "renderer" => fields.renderer = Some(val.to_string()),
                    _ => {
                        let labels = fields.labels.get_or_insert_with(BTreeMap::new);
                        labels.insert(key.to_string(), val.to_string());
                    }
                }
            }
        } else if let Some((os, arch)) = s.split_once('-') {
            if arch.contains('-') {
                return Err(format!(
                    "invalid format: ambiguous platform string '{s}'. Use 'key=value' comma-separated format for complex platforms"
                ));
            }
            fields.os = Some(os.to_string());
            fields.arch = Some(arch.to_string());
        } else {
            fields.os = Some(s.to_string());
        }

        Ok(fields)
    }
}

/// Validates that a user-provided segment contains only allowed characters.
/// Returns Ok(lowercased) or descriptive error.
///
/// # Errors
/// Returns `PlatformError::InvalidSegment` if the trimmed segment is empty, is
/// exactly `.` or `..`, or contains characters outside `[a-z0-9_.-]`.
pub fn validate_segment(s: &str) -> Result<std::borrow::Cow<'_, str>, PlatformError> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(PlatformError::InvalidSegment(
            "Segment cannot be empty".into(),
        ));
    }

    // Only ASCII case-folding is applied: the charset check below rejects any non-ASCII
    // character regardless, so Unicode-aware lowercasing would only spend extra work
    // producing a string that's still invalid.
    let lowered = if trimmed.bytes().any(|b| b.is_ascii_uppercase()) {
        std::borrow::Cow::Owned(trimmed.to_ascii_lowercase())
    } else {
        std::borrow::Cow::Borrowed(trimmed)
    };

    if lowered.as_ref() == "." || lowered.as_ref() == ".." {
        return Err(PlatformError::InvalidSegment(
            "Segment cannot be '.' or '..' to avoid directory traversal".into(),
        ));
    }
    if lowered
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        Ok(lowered)
    } else {
        let bad_chars: String = s
            .chars()
            .filter(|c| !c.is_ascii_alphanumeric() && *c != '-' && *c != '_' && *c != '.')
            .collect();
        Err(PlatformError::InvalidSegment(format!(
            "'{s}' contains invalid characters: '{bad_chars}'. Use [a-z0-9_.-] only"
        )))
    }
}

impl PlatformInfo {
    /// Generates a deterministic flat key from `PlatformInfo` fields.
    ///
    /// # Errors
    /// Returns `PlatformError::InvalidSegment` if the OS, architecture, renderer,
    /// or any label key/value fails segment validation (see [`validate_segment`]).
    pub fn to_key(&self) -> Result<String, PlatformError> {
        use std::fmt::Write;
        let mut key_out = String::new();

        match validate_segment(&self.os) {
            Ok(os) => {
                // Writing to a `String` via `fmt::Write` never fails.
                #[expect(
                    clippy::expect_used,
                    reason = "`fmt::Write` for `String` is infallible"
                )]
                write!(&mut key_out, "{}:{}", os.len(), os)
                    .expect("write! to a String cannot fail");
            }
            Err(e) => {
                return Err(PlatformError::InvalidSegment(format!(
                    "OS '{}' is empty or invalid: {}",
                    self.os, e
                )));
            }
        }

        if let Some(ref arch) = self.arch {
            match validate_segment(arch) {
                Ok(clean_arch) => {
                    // Writing to a `String` via `fmt::Write` never fails.
                    #[expect(
                        clippy::expect_used,
                        reason = "`fmt::Write` for `String` is infallible"
                    )]
                    write!(&mut key_out, "-{}:{}", clean_arch.len(), clean_arch)
                        .expect("write! to a String cannot fail");
                }
                Err(e) => {
                    return Err(PlatformError::InvalidSegment(format!(
                        "Architecture '{arch}' is invalid: {e}"
                    )));
                }
            }
        }

        if let Some(ref renderer) = self.renderer {
            match validate_segment(renderer) {
                Ok(clean_renderer) => {
                    // Writing to a `String` via `fmt::Write` never fails.
                    #[expect(
                        clippy::expect_used,
                        reason = "`fmt::Write` for `String` is infallible"
                    )]
                    write!(&mut key_out, "-{}:{}", clean_renderer.len(), clean_renderer)
                        .expect("write! to a String cannot fail");
                }
                Err(e) => {
                    return Err(PlatformError::InvalidSegment(format!(
                        "Renderer '{renderer}' is invalid: {e}"
                    )));
                }
            }
        }

        for (k, v) in &self.labels {
            let key = match validate_segment(k) {
                Ok(key) => key,
                Err(e) => {
                    return Err(PlatformError::InvalidSegment(format!(
                        "Label key '{k}' is invalid: {e}"
                    )));
                }
            };
            let val = match validate_segment(v) {
                Ok(val) => val,
                Err(e) => {
                    return Err(PlatformError::InvalidSegment(format!(
                        "Label value '{v}' is invalid for key '{k}': {e}"
                    )));
                }
            };
            // Writing to a `String` via `fmt::Write` never fails.
            #[expect(
                clippy::expect_used,
                reason = "`fmt::Write` for `String` is infallible"
            )]
            write!(&mut key_out, "-{}:{}={}:{}", key.len(), key, val.len(), val)
                .expect("write! to a String cannot fail");
        }

        Ok(key_out)
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
    fn test_to_key() {
        let info = PlatformInfo {
            os: "MacOS".to_string(),
            arch: Some("aarch64".to_string()),
            renderer: None,
            labels: BTreeMap::new(),
        };
        assert_eq!(info.to_key().unwrap(), "5:macos-7:aarch64");

        let mut labels = BTreeMap::new();
        labels.insert("theme".to_string(), "dark".to_string());
        labels.insert("locale".to_string(), "en_US".to_string());

        let info_rich = PlatformInfo {
            os: "linux".to_string(),
            arch: Some("x86_64".to_string()),
            renderer: Some("flutter-3.22".to_string()),
            labels,
        };
        // Labels are sorted alphabetically: locale, theme
        assert_eq!(
            info_rich.to_key().unwrap(),
            "5:linux-6:x86_64-12:flutter-3.22-6:locale=5:en_us-5:theme=4:dark"
        );
    }

    #[test]
    fn test_parse_key_value() {
        let fields = PlatformFields::parse_key_value(
            "platform=macos,arch=aarch64,renderer=flutter,theme=dark",
        )
        .unwrap();
        assert_eq!(fields.os.as_deref(), Some("macos"));
        assert_eq!(fields.arch.as_deref(), Some("aarch64"));
        assert_eq!(fields.renderer.as_deref(), Some("flutter"));
        assert_eq!(
            fields
                .labels
                .as_ref()
                .unwrap()
                .get("theme")
                .map(String::as_str),
            Some("dark")
        );

        let fields_simple = PlatformFields::parse_key_value("macos-aarch64").unwrap();
        assert_eq!(fields_simple.os.as_deref(), Some("macos"));
        assert_eq!(fields_simple.arch.as_deref(), Some("aarch64"));

        assert!(PlatformFields::parse_key_value("os=macos,arch=").is_err());
        assert!(PlatformFields::parse_key_value("macos-aarch64-extra").is_err());
    }

    #[test]
    fn test_deserialize_platform_config() {
        let yaml_simple = "\"macos-aarch64\"";
        let config: PlatformConfig = serde_yaml::from_str(yaml_simple).unwrap();
        assert_eq!(config, PlatformConfig::Opaque("macos-aarch64".to_string()));

        let yaml_struct = "
os: linux
arch: x86_64
renderer: chrome
labels:
  theme: dark
";
        let config_struct: PlatformConfig = serde_yaml::from_str(yaml_struct).unwrap();
        assert_eq!(
            config_struct,
            PlatformConfig::Structured(PlatformFields {
                os: Some("linux".to_string()),
                arch: Some("x86_64".to_string()),
                renderer: Some("chrome".to_string()),
                labels: {
                    let mut map = BTreeMap::new();
                    map.insert("theme".to_string(), "dark".to_string());
                    Some(map)
                }
            })
        );
    }

    #[test]
    fn test_platform_config_deserialization_from_value() {
        use serde::Deserialize;
        let val = serde_json::Value::String("custom-opaque".to_string());
        let config = PlatformConfig::deserialize(val).unwrap();
        assert_eq!(config, PlatformConfig::Opaque("custom-opaque".to_string()));
    }

    #[test]
    fn test_platform_config_expecting() {
        use serde::Deserialize;
        let val = serde_json::Value::Number(42.into());
        let err = PlatformConfig::deserialize(val).unwrap_err();
        assert!(
            err.to_string()
                .contains("a string or a map representing structured platform config")
        );
    }

    #[test]
    fn test_validate_segment_invalid() {
        assert!(validate_segment("mac os").is_err());
        assert!(validate_segment("mac/os").is_err());
        assert!(validate_segment("mac!").is_err());
        assert!(validate_segment(".").is_err());
        assert!(validate_segment("..").is_err());
    }

    #[test]
    fn test_validate_segment_empty_and_to_key_errors() {
        // Validate segment empty checks
        assert!(matches!(
            validate_segment("   "),
            Err(PlatformError::InvalidSegment(_))
        ));

        // OS invalid
        let info = PlatformInfo {
            os: "mac os".to_string(),
            arch: None,
            renderer: None,
            labels: BTreeMap::new(),
        };
        assert!(info.to_key().is_err());

        // Arch invalid
        let info = PlatformInfo {
            os: "macos".to_string(),
            arch: Some("x86 64".to_string()),
            renderer: None,
            labels: BTreeMap::new(),
        };
        assert!(info.to_key().is_err());

        // Renderer invalid
        let info = PlatformInfo {
            os: "macos".to_string(),
            arch: None,
            renderer: Some("chrome/126".to_string()),
            labels: BTreeMap::new(),
        };
        assert!(info.to_key().is_err());

        // Label key invalid
        let mut labels = BTreeMap::new();
        labels.insert("theme name".to_string(), "dark".to_string());
        let info = PlatformInfo {
            os: "macos".to_string(),
            arch: None,
            renderer: None,
            labels,
        };
        assert!(info.to_key().is_err());

        // Label value invalid
        let mut labels = BTreeMap::new();
        labels.insert("theme".to_string(), "dark side".to_string());
        let info = PlatformInfo {
            os: "macos".to_string(),
            arch: None,
            renderer: None,
            labels,
        };
        assert!(info.to_key().is_err());
    }

    #[test]
    fn test_platform_config_serialization() {
        let opaque = PlatformConfig::Opaque("custom-opaque".to_string());
        let serialized_opaque = serde_yaml::to_string(&opaque).unwrap();
        assert_eq!(serialized_opaque.trim(), "custom-opaque");

        let structured = PlatformConfig::Structured(PlatformFields {
            os: Some("linux".to_string()),
            arch: Some("x86_64".to_string()),
            renderer: None,
            labels: None,
        });
        let serialized_struct = serde_yaml::to_string(&structured).unwrap();
        assert!(serialized_struct.contains("os: linux"));
        assert!(serialized_struct.contains("arch: x86_64"));
    }

    #[test]
    fn test_platform_config_json_deserialization() {
        let json = "\"custom-opaque\"";
        let config: PlatformConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config, PlatformConfig::Opaque("custom-opaque".to_string()));
    }

    #[test]
    fn test_parse_key_value_edge_cases() {
        let empty = PlatformFields::parse_key_value("").unwrap();
        assert_eq!(empty, PlatformFields::default());

        let simple_no_hyphen = PlatformFields::parse_key_value("macos").unwrap();
        assert_eq!(simple_no_hyphen.os.as_deref(), Some("macos"));
        assert_eq!(simple_no_hyphen.arch, None);

        assert_eq!(
            PlatformFields::parse_key_value("os=macos,arch").unwrap_err(),
            "invalid format: no '=' found in 'arch'"
        );
    }

    #[test]
    fn test_opaque_key_is_validated_and_lowercased() {
        assert_eq!(
            PlatformConfig::Opaque("Custom-Env".to_owned())
                .to_key()
                .unwrap(),
            "custom-env"
        );
        assert!(
            PlatformConfig::Opaque("bad key".to_owned())
                .to_key()
                .is_err()
        );
        assert_eq!(
            PlatformConfig::host().to_key().unwrap(),
            format!(
                "{}:{HOST_OS}-{}:{HOST_ARCH}",
                HOST_OS.len(),
                HOST_ARCH.len()
            )
        );
    }

    #[test]
    fn test_to_key_length_prefixed_format() {
        let info = PlatformInfo {
            os: "linux".to_string(),
            arch: Some("x86_64".to_string()),
            renderer: Some("chrome".to_string()),
            labels: {
                let mut map = BTreeMap::new();
                map.insert("theme".to_string(), "dark".to_string());
                map
            },
        };
        // Expect: 5:linux-6:x86_64-6:chrome-5:theme=4:dark
        assert_eq!(
            info.to_key().unwrap(),
            "5:linux-6:x86_64-6:chrome-5:theme=4:dark"
        );
    }
}
