//! Which screenshot rule of a [`GleonConfig`] applies to a golden.
//!
//! This is the CLI scanner's own selection (the scanner calls [`RuleSet::select`] for every file
//! it walks), so the CLI and other integrations can never disagree: paths are matched in canonical
//! form (lowercase, `/` separators); a file inside a pruned directory (`build/`, `.git/`, ...) or
//! under a directory matched by `exclude`, or matched by `exclude` itself, is excluded; otherwise
//! the **first** rule whose `include` matches a `.png` applies, with its masks whose `path`
//! matches.

use std::sync::Arc;

use gleon_engine::config::Zone;
use globset::{GlobSet, GlobSetBuilder};
use serde::Serialize;

use crate::{
    config::{GleonConfig, GlobPattern, ScreenshotRule},
    naming::{DEFAULT_PRUNED_DIRECTORIES, TestNameError, normalize_test_name, validate_test_name},
    tolerance::Tolerance,
};

/// Compiles a `GlobSet` from a list of patterns.
///
/// # Errors
/// Returns the underlying `globset::Error` if any pattern fails to compile (this should not
/// happen for patterns already validated by [`GlobPattern`]).
pub fn build_globset(patterns: &[GlobPattern]) -> Result<GlobSet, globset::Error> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(pattern.as_glob().clone());
    }
    builder.build()
}

/// A golden path that cannot become a gleon test name.
#[derive(Debug, thiserror::Error)]
#[error("'{path}' is not a valid gleon test path: {source}")]
pub struct InvalidTestPath {
    /// The path as given.
    pub path: String,
    /// Why its canonical name is invalid.
    #[source]
    pub source: TestNameError,
}

/// The rule selection of one path (see [`RuleSet::select`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    /// `screenshots[index]` applies.
    Rule(usize),
    /// Excluded by a pruned directory or an `exclude` glob.
    Excluded,
    /// No rule includes it, or it is not a `.png`: the CLI does not track it.
    Unmatched,
}

/// How a golden relates to the configured rules.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuleMatch {
    /// `screenshots[index]` applies.
    Matched {
        /// Index of the first matching rule in `screenshots`.
        index: usize,
        /// Canonical test name: the path without `.png`, lowercase, `/`-separated.
        name: String,
        /// The rule's tolerance.
        tolerance: Tolerance,
        /// The rule's tolerance of text (`pixel` mode only).
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<crate::tolerance::TextTolerance>,
        /// Mask zones of the rule's masks whose `path` matches, in declaration order.
        masks: Vec<Zone>,
    },
    /// Excluded by a pruned directory or an `exclude` glob.
    Excluded,
    /// No rule includes it (or it is not a `.png`), so the CLI does not track it.
    Unmatched,
}

/// The screenshot rules of a [`GleonConfig`] with their globs compiled for repeated matching.
///
/// Owns shared copies of the rules, so it can be cached independently of the config it came
/// from.
#[derive(Debug)]
pub struct RuleSet {
    exclude: GlobSet,
    rules: Vec<(GlobSet, Arc<ScreenshotRule>)>,
}

impl RuleSet {
    /// Compiles the include and exclude globs of `config`.
    ///
    /// # Errors
    /// Returns the underlying `globset::Error` if a glob set fails to compile.
    pub fn new(config: &GleonConfig) -> Result<Self, globset::Error> {
        let rules = config
            .screenshots
            .iter()
            .map(|rule| build_globset(&rule.include).map(|set| (set, Arc::new(rule.clone()))))
            .collect::<Result<_, _>>();
        build_globset(&config.exclude)
            .and_then(|exclude| rules.map(|rules| Self { exclude, rules }))
    }

    /// The compiled `exclude` globs (the scanner prunes directories with them while walking).
    #[must_use]
    pub const fn exclude_set(&self) -> &GlobSet {
        &self.exclude
    }

    /// The rule at `index` of `screenshots`, as returned by [`Selection::Rule`].
    ///
    /// # Panics
    /// Panics if `index` is out of range.
    #[must_use]
    pub fn rule(&self, index: usize) -> &Arc<ScreenshotRule> {
        &self.rules[index].1
    }

    /// Selects the rule of `path` (relative to the workspace root, either separator, any case).
    ///
    /// Pruned directory names are compared as written (case-sensitively, like the scanner's
    /// walker); globs match the canonical path and every ancestor directory of it.
    #[must_use]
    pub fn select(&self, path: &str) -> Selection {
        self.select_normalized(path, &normalize_test_name(path))
    }

    fn select_normalized(&self, path: &str, normalized: &str) -> Selection {
        let mut dirs = path.split(['/', '\\']).rev().skip(1);
        let is_pruned = dirs.any(|dir| DEFAULT_PRUNED_DIRECTORIES.contains(&dir));
        let is_excluded = is_pruned
            || self.exclude.is_match(normalized)
            || normalized
                .match_indices('/')
                .any(|(end, _)| self.exclude.is_match(&normalized[..end]));
        if is_excluded {
            return Selection::Excluded;
        }
        // Exactly the scanner's check (`Path::extension`, so a bare `.png` file has none); the
        // path is already lowercase.
        let is_png = std::path::Path::new(normalized)
            .extension()
            .is_some_and(|ext| ext == "png");
        self.rules
            .iter()
            .position(|(include, _)| is_png && include.is_match(normalized))
            .map_or(Selection::Unmatched, Selection::Rule)
    }

    /// Resolves the rule of a golden `path` (relative to the workspace root, either separator,
    /// any case), with its tolerance, masks and canonical test name.
    ///
    /// # Errors
    /// Returns [`InvalidTestPath`] if a matched golden's canonical name fails
    /// [`validate_test_name`], exactly where the CLI scanner rejects it. Excluded and unmatched
    /// paths are never validated, as the scanner ignores them.
    pub fn resolve(&self, path: &str) -> Result<RuleMatch, InvalidTestPath> {
        let normalized = normalize_test_name(path);
        let index = match self.select_normalized(path, &normalized) {
            Selection::Rule(index) => index,
            Selection::Excluded => return Ok(RuleMatch::Excluded),
            Selection::Unmatched => return Ok(RuleMatch::Unmatched),
        };
        let name = test_name(&normalized).map_err(|source| InvalidTestPath {
            path: path.to_owned(),
            source,
        })?;
        let rule = self.rule(index);
        Ok(RuleMatch::Matched {
            index,
            name: name.to_owned(),
            tolerance: Tolerance::from_rule(rule.mode, &rule.diff),
            text: rule
                .text
                .map(crate::tolerance::TextTolerance::without_negative_zero),
            masks: rule.matched_mask_zones(std::path::Path::new(normalized.as_ref())),
        })
    }
}

/// The validated test name of a selected screenshot: its canonical path (see
/// [`normalize_test_name`]) without the `.png` extension.
///
/// # Errors
/// Returns the [`TestNameError`] of [`validate_test_name`].
pub fn test_name(normalized: &str) -> Result<&str, TestNameError> {
    let name = normalized.strip_suffix(".png").unwrap_or(normalized);
    validate_test_name(name).map(|()| name)
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
    use gleon_engine::config::Dimension;

    use super::*;

    const YAML: &str = r#"
required_version: ">=0.1.0"
exclude: ["test/goldens/wip/**", "**/drafts"]
screenshots:
  - include: "test/goldens/**/*.png"
    mode: ssim
    diff: { min_similarity: 0.6, color_tolerance: 64 }
    masks:
      - path: "**/clock*.png"
        zones: [{ x: 0, y: 0, width: 10, height: "25%" }]
      - path: "**/other.png"
        zones: [{ x: 5, y: 5, width: 1, height: 1 }]
  - include: "test/**/*.png"
    diff: { threshold: 0.02 }
"#;

    fn rules() -> RuleSet {
        RuleSet::new(&GleonConfig::from_yaml_str(YAML).unwrap()).unwrap()
    }

    fn resolve(path: &str) -> RuleMatch {
        rules().resolve(path).unwrap()
    }

    #[test]
    fn test_first_matching_rule_wins_with_its_masks() {
        assert_eq!(
            resolve("test/goldens/Clock_Face.png"),
            RuleMatch::Matched {
                index: 0,
                name: "test/goldens/clock_face".to_owned(),
                tolerance: Tolerance::Ssim {
                    min_similarity: 0.6,
                    color_tolerance: 64.0
                },
                text: None,
                masks: vec![Zone {
                    x: 0,
                    y: 0,
                    width: Dimension::Pixels(10),
                    height: Dimension::Percent(25.0),
                }],
            }
        );
        assert_eq!(
            resolve("test/unit/a.png"),
            RuleMatch::Matched {
                index: 1,
                name: "test/unit/a".to_owned(),
                tolerance: Tolerance::Pixel {
                    max_diff_ratio: 0.02
                },
                text: None,
                masks: vec![],
            }
        );
    }

    #[test]
    fn test_windows_separators_and_case_are_canonicalized() {
        assert!(matches!(
            resolve(r"Test\Goldens\clock.PNG"),
            RuleMatch::Matched { index: 0, name, .. } if name == "test/goldens/clock"
        ));
    }

    #[test]
    fn test_exclude_and_pruned_directories_win() {
        for excluded in [
            "test/goldens/wip/a.png",
            "build/test/a.png",
            "test/.dart_tool/a.png",
            // A directory matched by `exclude` is pruned by the scanner's walker, so everything
            // below it is excluded although the glob does not match the file itself.
            "test/goldens/drafts/a.png",
            "test/goldens/drafts/deeper/a.png",
        ] {
            assert_eq!(resolve(excluded), RuleMatch::Excluded, "{excluded}");
            assert_eq!(rules().select(excluded), Selection::Excluded, "{excluded}");
        }
        // Pruned names are compared as written, like the walker does.
        assert!(matches!(
            resolve("test/Build/a.png"),
            RuleMatch::Matched { .. }
        ));
    }

    #[test]
    fn test_unmatched_paths_and_non_png_files() {
        for unmatched in [
            "lib/a.png",
            "test/goldens/a.jpg",
            "test/goldens/noext",
            "test/goldens.png/noext",
            "test/goldens/.png",
        ] {
            assert_eq!(resolve(unmatched), RuleMatch::Unmatched, "{unmatched}");
        }
    }

    #[test]
    fn test_only_matched_names_are_validated_like_the_scanner() {
        let rules = rules();
        for bad in [
            "test/goldens/with space.png",
            "test/goldens/../a.png",
            "test/goldens/ü.png",
        ] {
            let err = rules.resolve(bad).unwrap_err();
            assert_eq!(err.path, bad);
            assert!(
                err.to_string().contains("not a valid gleon test path"),
                "{err}"
            );
        }
        // The scanner ignores files no rule includes, whatever their names.
        assert_eq!(
            rules.resolve("lib/with space.png").unwrap(),
            RuleMatch::Unmatched
        );
        assert_eq!(
            rules.resolve("test/goldens/drafts/with space.png").unwrap(),
            RuleMatch::Excluded
        );
    }

    #[test]
    fn test_test_name_strips_the_extension_and_validates() {
        assert_eq!(test_name("test/goldens/a.png").unwrap(), "test/goldens/a");
        assert_eq!(test_name("a.b.png").unwrap(), "a.b");
        assert!(test_name("a b.png").is_err());
    }

    #[test]
    fn test_json_shape() {
        assert_eq!(
            serde_json::to_value(resolve("test/goldens/wip/a.png")).unwrap(),
            serde_json::json!({"kind": "excluded"})
        );
        assert_eq!(
            serde_json::to_value(resolve("test/unit/a.png")).unwrap(),
            serde_json::json!({
                "kind": "matched",
                "index": 1,
                "name": "test/unit/a",
                "tolerance": {"kind": "pixel", "max_diff_ratio": 0.02},
                "masks": []
            })
        );
    }

    #[test]
    fn test_build_globset_matches_patterns() {
        let patterns = vec![GlobPattern::new("*.png").unwrap()];
        let set = build_globset(&patterns).unwrap();
        assert!(set.is_match("a.png"));
        assert!(!set.is_match("dir/a.png"));
    }
}
