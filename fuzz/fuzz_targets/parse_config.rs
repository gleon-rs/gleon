//! Any `.gleon/gleon.yaml` text parses or fails with an error (globs, platforms, tolerances,
//! the lenient `required_version` pre-parse), never panics.
#![no_main]

use gleon_model::config::GleonConfig;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|text: &str| {
    let version = semver::Version::new(0, 3, 0);
    let _ = GleonConfig::check_required_version(text, &version);
    if let Ok(config) = GleonConfig::from_yaml_str(text) {
        let rules = gleon_model::rules::RuleSet::new(&config);
        let _ = rules.resolve("test/goldens/a.png");
    }
});
