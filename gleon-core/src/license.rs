use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};

const OFFICIAL_PUBLIC_KEYS: &[(u8, [u8; 32])] = &[(
    1,
    [
        186, 132, 188, 52, 188, 5, 242, 79, 251, 253, 13, 9, 176, 13, 22, 216, 150, 171, 163, 233,
        208, 202, 87, 244, 75, 108, 20, 228, 50, 141, 108, 27,
    ],
)];

/// Where commercial licenses are offered.
const PRICING_URL: &str = "https://gleon.rs/#pricing";
/// The official releases of the CLI.
const RELEASES_URL: &str = "https://github.com/gleon-rs/gleon/releases";
/// The first and last line of a notice, so it stands out in a CI log.
const RULE: &str = "====================================================";

#[cfg(not(test))]
/// Get the public key for a given kid.
#[must_use]
pub fn get_public_key(kid: u8) -> Option<[u8; 32]> {
    OFFICIAL_PUBLIC_KEYS
        .iter()
        .find(|(k, _)| *k == kid)
        .map(|(_, bytes)| *bytes)
}

#[cfg(test)]
std::thread_local! {
    /// Test-only mutable override of the embedded Ed25519 public keys, used so the test suite
    /// can sign license tokens with a key it controls.
    pub static PUBLIC_KEYS: std::cell::RefCell<Vec<(u8, [u8; 32])>> = std::cell::RefCell::new(OFFICIAL_PUBLIC_KEYS.to_vec());
}

#[cfg(test)]
/// Get the public key for a given kid.
#[must_use]
pub fn get_public_key(kid: u8) -> Option<[u8; 32]> {
    PUBLIC_KEYS.with(|keys| {
        keys.borrow()
            .iter()
            .find(|(k, _)| *k == kid)
            .map(|(_, bytes)| *bytes)
    })
}

/// The signed payload embedded in a license key, containing ownership and validity details.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct LicensePayload {
    /// Schema version (expected 1).
    pub v: u8,
    /// The name or organization the license was issued to.
    pub owner: String,
    /// Glob pattern matching the repositories this license is valid for.
    pub repo_pattern: String,
    /// Unix timestamp (seconds) after which the license is no longer valid without grace period.
    pub expires_at: u64,
    /// Unique identifier of this license.
    pub license_id: String,
}

/// Validity status of a cryptographic license key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LicenseValidity {
    /// The license is valid and active.
    Valid,
    /// The license expired within the last 14 days and is in grace period.
    GracePeriod {
        /// Reason for grace period.
        reason: String,
    },
}

/// Outcome of license/compliance verification for the current execution context.
#[derive(Debug, PartialEq, Eq)]
pub enum LicenseStatus {
    /// A valid license was verified for this environment.
    Valid,
    /// The environment is a public repository or otherwise granted use, not requiring a
    /// commercial license.
    PublicOrGrantedUse,
    /// A commercial license expired within the last 14 days and is in the grace period.
    GracePeriod {
        /// Human-readable explanation of the grace period.
        reason: String,
    },
    /// Unlicensed use was detected but is only soft-enforced (e.g. non-strict mode in private CI).
    UnlicensedSoft {
        /// Human-readable explanation of why the license was rejected.
        reason: String,
    },
    /// A self-compiled (non-official) binary is running in a private CI without a valid license.
    UnofficialBuildInPrivateCI {
        /// Reason for license failure.
        reason: String,
    },
    /// An official binary older than the enforcement window is running unlicensed in private CI.
    ExpiredUnlicensedBinary {
        /// Reason for license failure.
        reason: String,
    },
}

/// Detailed error conditions encountered during license token processing.
#[derive(Debug, thiserror::Error)]
pub enum LicenseError {
    /// The token string is larger than the 8192 byte limit.
    #[error("License token exceeds maximum allowed length")]
    TokenTooLong,
    /// The base64 token could not be parsed using any common base64 engine.
    #[error("Invalid base64 encoding")]
    InvalidBase64,
    /// The parsed binary token is smaller than a 1-byte kid + 64-byte signature.
    #[error("License key payload too short")]
    PayloadTooShort,
    /// The 64-byte signature tail is not a valid Ed25519 signature format.
    #[error("Invalid Ed25519 signature format")]
    InvalidSignatureFormat,
    /// The kid byte is not recognized among the official keys.
    #[error("Unknown key ID (kid: {0})")]
    UnknownKeyId(u8),
    /// The embedded official public key is corrupt.
    #[error("Invalid embedded public key")]
    InvalidEmbeddedPublicKey,
    /// The token's signature does not match its contents.
    #[error("Cryptographic signature verification failed")]
    SignatureVerificationFailed(#[source] ed25519_dalek::SignatureError),
    /// The JSON payload could not be parsed.
    #[error("Invalid license payload JSON")]
    InvalidPayloadJson(#[source] serde_json::Error),
    /// The schema version of the token is not supported.
    #[error("Unsupported license version: {0}")]
    UnsupportedVersion(u8),
    /// The repository pattern contained in the token is an invalid glob.
    #[error("Invalid license repo pattern")]
    InvalidRepoPattern(#[source] globset::Error),
    /// A generic CI provider was detected but the repository could not be identified.
    #[error(
        "Repository name could not be automatically detected for this CI. Please set GLEON_PROJECT_PATH environment variable."
    )]
    MissingCiRepo,
    /// The repository glob pattern does not match the execution context.
    #[error("License pattern '{pattern}' does not match repository '{repo}'")]
    PatternMismatch {
        /// The glob pattern from the token.
        pattern: String,
        /// The repository name from the context.
        repo: String,
    },
    /// The license has passed its expiration and grace period.
    #[error("License has expired")]
    Expired,
    /// The `GLEON_LICENSE_KEY` environment variable was not found or was empty.
    #[error("No GLEON_LICENSE_KEY environment variable provided")]
    MissingKeyEnv,
}

/// Identifies the environment gleon is currently executing in, for license enforcement purposes.
#[derive(Debug, PartialEq, Eq)]
pub enum ExecutionContext {
    /// Running inside GitHub Actions.
    GitHubActions {
        /// The `owner/repo` slug.
        repo: String,
        /// Whether the repository is private.
        is_private: bool,
    },
    /// Running inside a recognized non-GitHub CI provider.
    GenericCI {
        /// The best-effort detected repository identifier, or empty if unknown.
        repo: String,
    },
    /// Running outside of any recognized CI environment (local development).
    LocalDev,
}

#[derive(Deserialize)]
struct GithubEventPayload {
    repository: Option<GithubRepository>,
}

#[derive(Deserialize)]
struct GithubRepository {
    private: bool,
}

/// Determines whether the current GitHub Actions repository is private by reading the event
/// payload referenced by the `GITHUB_EVENT_PATH` environment variable.
///
/// Fails closed: if the payload is missing, unreadable, unparsable, or lacks repository
/// information, this returns `true` (private).
pub fn parse_github_event_payload_is_private(env_provider: &dyn crate::env::EnvProvider) -> bool {
    let Some(path) = env_provider.get_var("GITHUB_EVENT_PATH") else {
        return true;
    };

    fs::read_to_string(&path)
        .map_err(|e| tracing::debug!("Failed to read GitHub event payload at {}: {}", path, e))
        .and_then(|content| {
            serde_json::from_str::<GithubEventPayload>(&content)
                .map_err(|e| tracing::warn!("Failed to parse GitHub event payload: {}", e))
        })
        .ok()
        .and_then(|payload| payload.repository)
        .is_none_or(|repo| repo.private) // Fail closed: if event payload exists but fails to parse, treat as private
}

/// Identifies the current execution context (GitHub Actions, another CI provider, or local
/// development) by inspecting well-known environment variables.
pub fn identify_context(env_provider: &dyn crate::env::EnvProvider) -> ExecutionContext {
    use crate::env::get_trimmed_var;

    const OTHER_CI_MARKER_VARS: &[&str] = &[
        "CI",
        "CONTINUOUS_INTEGRATION",
        "CIRCLECI",
        "TRAVIS",
        "GITLAB_CI",
        "TF_BUILD",
        "BUILDKITE",
        "DRONE",
        "TEAMCITY_VERSION",
        "BITBUCKET_COMMIT",
    ];

    // 1. GitHub Actions
    if get_trimmed_var(env_provider, "GITHUB_ACTIONS").as_deref() == Some("true")
        && let Some(repo) = get_trimmed_var(env_provider, "GITHUB_REPOSITORY")
    {
        let is_private = parse_github_event_payload_is_private(env_provider);
        return ExecutionContext::GitHubActions { repo, is_private };
    }

    // 2. Resolve provider repository from trusted CI provider metadata
    let provider_repo = get_trimmed_var(env_provider, "CI_PROJECT_PATH")
        .or_else(|| get_trimmed_var(env_provider, "TRAVIS_REPO_SLUG"))
        .or_else(|| get_trimmed_var(env_provider, "BITBUCKET_REPO_FULL_NAME"))
        .or_else(|| {
            let user = get_trimmed_var(env_provider, "CIRCLE_PROJECT_USERNAME")?;
            let repo = get_trimmed_var(env_provider, "CIRCLE_PROJECT_REPONAME")?;
            Some(format!("{user}/{repo}"))
        });

    let override_repo = get_trimmed_var(env_provider, "GLEON_PROJECT_PATH");

    let repo = provider_repo.unwrap_or_else(|| override_repo.unwrap_or_default());

    // 3. Fallback for other CIs
    if !repo.is_empty()
        || OTHER_CI_MARKER_VARS
            .iter()
            .any(|var| env_provider.has_var(var))
    {
        return ExecutionContext::GenericCI { repo };
    }

    // 4. Local Dev
    ExecutionContext::LocalDev
}

/// Entry point for license verification.
pub struct LicenseGate;

impl LicenseGate {
    /// Verifies the license/compliance status for the current process environment.
    ///
    /// Whether this is an official build is decided when it is compiled (`build.rs` sets the
    /// `gleon_official` cfg), so nothing of that check is part of the binary.
    pub fn verify(env_provider: &dyn crate::env::EnvProvider) -> LicenseStatus {
        let build_timestamp_str = option_env!("GLEON_BUILD_TIMESTAMP").unwrap_or("0");
        let build_timestamp: u64 = build_timestamp_str.trim().parse().unwrap_or(0);
        Self::verify_internal(env_provider, cfg!(gleon_official), build_timestamp)
    }

    fn verify_internal(
        env_provider: &dyn crate::env::EnvProvider,
        is_official: bool,
        build_timestamp: u64,
    ) -> LicenseStatus {
        let context = identify_context(env_provider);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // If local dev, we always pass silently.
        if context == ExecutionContext::LocalDev {
            return LicenseStatus::Valid;
        }

        let is_private_ci = match context {
            ExecutionContext::GitHubActions { is_private, .. } => is_private,
            _ => true,
        };

        // If it's a known public repo on GitHub, it passes silently.
        if let ExecutionContext::GitHubActions {
            is_private: false, ..
        } = context
        {
            return LicenseStatus::PublicOrGrantedUse;
        }

        let has_valid_license = env_provider
            .get_var("GLEON_LICENSE_KEY")
            .filter(|k| !k.trim().is_empty())
            .map_or_else(
                || Err(LicenseError::MissingKeyEnv),
                |key| Self::verify_key(&key, &context, now),
            );

        match has_valid_license {
            Ok(LicenseValidity::Valid) => LicenseStatus::Valid,
            Ok(LicenseValidity::GracePeriod { reason }) => LicenseStatus::GracePeriod { reason },
            Err(e) => {
                let e_msg = e.to_string();
                // An official binary MUST have both the official secret and a valid timestamp (not 0 and not in far future).
                let is_valid_official_build =
                    is_official && build_timestamp > 0 && build_timestamp <= now + 86400;

                if !is_valid_official_build && is_private_ci {
                    return LicenseStatus::UnofficialBuildInPrivateCI { reason: e_msg };
                }

                // Time-bomb check: > 90 days old (approx 90 * 24 * 60 * 60 = 7_776_000 seconds)
                if is_valid_official_build && is_private_ci && now > build_timestamp + 7_776_000 {
                    return LicenseStatus::ExpiredUnlicensedBinary { reason: e_msg };
                }

                LicenseStatus::UnlicensedSoft { reason: e_msg }
            }
        }
    }

    fn verify_token_integrity_internal(
        key: &str,
        resolve_pubkey: impl FnOnce(u8) -> Result<[u8; 32], LicenseError>,
    ) -> Result<(u8, LicensePayload), LicenseError> {
        if key.len() > 8192 {
            return Err(LicenseError::TokenTooLong);
        }
        let (kid, message, signature) = Self::decode_and_parse_token(key)?;

        let pub_key_bytes = resolve_pubkey(kid)?;
        let pub_key = VerifyingKey::from_bytes(&pub_key_bytes)
            .map_err(|_| LicenseError::InvalidEmbeddedPublicKey)?;

        pub_key
            .verify_strict(&message, &signature)
            .map_err(LicenseError::SignatureVerificationFailed)?;

        let payload_bytes = &message[1..];
        let payload: LicensePayload =
            serde_json::from_slice(payload_bytes).map_err(LicenseError::InvalidPayloadJson)?;

        if payload.v != 1 {
            return Err(LicenseError::UnsupportedVersion(payload.v));
        }

        globset::GlobBuilder::new(&payload.repo_pattern)
            .case_insensitive(true)
            .build()
            .map_err(LicenseError::InvalidRepoPattern)?;

        Ok((kid, payload))
    }

    /// Verifies the cryptographic integrity and schema of a token against official public keys.
    ///
    /// Returns `(u8, LicensePayload)` if signature and payload are cryptographically valid.
    ///
    /// # Errors
    ///
    /// Returns a [`LicenseError`] if token is oversized, base64 is invalid, kid is unknown,
    /// signature is invalid, JSON payload is corrupted, or schema version is unsupported.
    pub fn verify_token_integrity(key: &str) -> Result<(u8, LicensePayload), LicenseError> {
        Self::verify_token_integrity_internal(key, |kid| {
            get_public_key(kid).ok_or(LicenseError::UnknownKeyId(kid))
        })
    }

    /// Verifies the cryptographic integrity and schema of a token against an explicit public key.
    ///
    /// Returns `(u8, LicensePayload)` if signature and payload are cryptographically valid.
    ///
    /// # Errors
    ///
    /// Returns a [`LicenseError`] if token is oversized, base64 is invalid,
    /// signature is invalid, JSON payload is corrupted, or schema version is unsupported.
    pub fn verify_token_integrity_with_public_key(
        key: &str,
        public_key: &[u8; 32],
    ) -> Result<(u8, LicensePayload), LicenseError> {
        Self::verify_token_integrity_internal(key, |_kid| Ok(*public_key))
    }

    /// Verifies a raw Base64 license token against the given execution context and timestamp.
    ///
    /// # Errors
    ///
    /// Returns a [`LicenseError`] if decoding, cryptographic signature, JSON payload parsing,
    /// repository pattern matching, or expiration validation fails.
    pub fn verify_key(
        key: &str,
        context: &ExecutionContext,
        now: u64,
    ) -> Result<LicenseValidity, LicenseError> {
        let (_kid, payload) = Self::verify_token_integrity(key)?;
        Self::verify_parsed_payload(&payload, context, now)
    }

    /// Verifies a raw Base64 license token against an explicit public key, execution context, and timestamp.
    ///
    /// # Errors
    ///
    /// Returns a [`LicenseError`] if decoding, cryptographic signature, JSON payload parsing,
    /// repository pattern matching, or expiration validation fails.
    pub fn verify_key_with_public_key(
        key: &str,
        context: &ExecutionContext,
        now: u64,
        public_key: &[u8; 32],
    ) -> Result<LicenseValidity, LicenseError> {
        let (_kid, payload) = Self::verify_token_integrity_with_public_key(key, public_key)?;
        Self::verify_parsed_payload(&payload, context, now)
    }

    fn decode_and_parse_token(key: &str) -> Result<(u8, Vec<u8>, Signature), LicenseError> {
        let mut decoded = decode_base64_flexible(key).ok_or(LicenseError::InvalidBase64)?;
        if decoded.len() <= 65 {
            return Err(LicenseError::PayloadTooShort);
        }

        let signature_bytes = decoded.split_off(decoded.len() - 64);
        let signature = Signature::from_slice(&signature_bytes)
            .map_err(|_| LicenseError::InvalidSignatureFormat)?;

        let kid = decoded[0];
        Ok((kid, decoded, signature))
    }

    fn verify_parsed_payload(
        payload: &LicensePayload,
        context: &ExecutionContext,
        now: u64,
    ) -> Result<LicenseValidity, LicenseError> {
        let repo_to_check = match context {
            ExecutionContext::GitHubActions { repo, .. } => Some(repo),
            ExecutionContext::GenericCI { repo } => {
                if repo.trim().is_empty() {
                    return Err(LicenseError::MissingCiRepo);
                }
                Some(repo)
            }
            ExecutionContext::LocalDev => None,
        };

        if let Some(repo) = repo_to_check {
            let matcher = globset::GlobBuilder::new(&payload.repo_pattern)
                .case_insensitive(true)
                .build()
                .map_err(LicenseError::InvalidRepoPattern)?
                .compile_matcher();
            if !matcher.is_match(repo) {
                return Err(LicenseError::PatternMismatch {
                    pattern: payload.repo_pattern.clone(),
                    repo: repo.clone(),
                });
            }
        }

        let day = 24 * 60 * 60;
        if now > payload.expires_at {
            let grace_ends = payload.expires_at.saturating_add(14 * day);
            if now <= grace_ends {
                let expired = match (now - payload.expires_at) / day {
                    0 => "less than a day ago".to_owned(),
                    days => format!("{days} day(s) ago"),
                };
                return Ok(LicenseValidity::GracePeriod {
                    reason: format!(
                        "it expired {expired} and stops working in {} day(s)",
                        (grace_ends - now).div_ceil(day)
                    ),
                });
            }
            return Err(LicenseError::Expired);
        }

        Ok(LicenseValidity::Valid)
    }
}

/// Serializes the given payload, signs it using the provided Ed25519 signing key,
/// and returns the Base64-encoded license token (1-byte kid followed by JSON payload bytes followed by 64 signature bytes).
///
/// # Errors
///
/// Returns an error if JSON serialization fails.
#[cfg(any(feature = "keygen", test))]
pub fn generate_license_token(
    kid: u8,
    payload: &LicensePayload,
    signing_key: &ed25519_dalek::SigningKey,
) -> Result<String, serde_json::Error> {
    use ed25519_dalek::Signer;
    let payload_bytes = serde_json::to_vec(payload)?;

    let mut message = Vec::with_capacity(1 + payload_bytes.len() + 64);
    message.push(kid);
    message.extend_from_slice(&payload_bytes);

    let signature = signing_key.sign(&message);

    let mut combined = message;
    combined.extend_from_slice(&signature.to_bytes());

    Ok(base64::engine::general_purpose::STANDARD.encode(combined))
}

/// Decodes a Base64 string trying `STANDARD`, `URL_SAFE`, `STANDARD_NO_PAD`, and `URL_SAFE_NO_PAD` engines.
///
/// Returns `None` if all decoders fail or input is invalid.
#[must_use]
fn decode_base64_flexible(raw: &str) -> Option<Vec<u8>> {
    let trimmed = raw.trim();
    let engines = [
        base64::engine::general_purpose::STANDARD,
        base64::engine::general_purpose::URL_SAFE,
        base64::engine::general_purpose::STANDARD_NO_PAD,
        base64::engine::general_purpose::URL_SAFE_NO_PAD,
    ];
    for engine in engines {
        if let Ok(d) = engine.decode(trimmed) {
            return Some(d);
        }
    }
    None
}

/// Outcome of enforcing licensing policy.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum EnforcementAction {
    /// Execution is allowed to proceed without restriction.
    Allow,
    /// Execution proceeds, but a compliance warning was emitted.
    Warn,
    /// Execution should be blocked due to a compliance violation.
    Block,
}

/// Structured outcome of evaluating license enforcement policy.
///
/// Carries what the caller should *do* (`action`) separately from what it should *print*
/// (`message`, `gha_annotation`), so the library itself performs no I/O side effects — the
/// binary decides how and where to display them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyDecision {
    /// Whether the caller should allow, warn, or block execution.
    pub action: EnforcementAction,
    /// Human-readable compliance notice lines to print to stderr, if any.
    pub message: Vec<String>,
    /// A GitHub Actions workflow command annotation (`::error ...`/`::warning ...`), if
    /// running in GitHub Actions and a notice applies.
    pub gha_annotation: Option<String>,
}

/// Applies enforcement rules for a given [`LicenseStatus`], returning a structured decision
/// describing what action to take and what (if anything) to display — printing is left to
/// the caller.
#[must_use]
pub fn enforce_policy(
    status: LicenseStatus,
    strict_mode: bool,
    env_provider: &dyn crate::env::EnvProvider,
) -> PolicyDecision {
    let in_github = env_provider.has_var("GITHUB_ACTIONS");
    match status {
        LicenseStatus::Valid | LicenseStatus::PublicOrGrantedUse => PolicyDecision {
            action: EnforcementAction::Allow,
            message: Vec::new(),
            gha_annotation: None,
        },
        LicenseStatus::GracePeriod { reason } => PolicyDecision {
            action: EnforcementAction::Warn,
            message: framed(vec![
                format!("[gleon license] The license key has expired: {reason}."),
                format!("Renew the license: {PRICING_URL}"),
            ]),
            gha_annotation: in_github.then(|| {
                format!("::warning title=gleon license::The license key has expired: {reason}.")
            }),
        },
        LicenseStatus::UnlicensedSoft { reason } => {
            let mut lines = vec![
                "[gleon license] This CI run has no valid license key (GLEON_LICENSE_KEY)."
                    .to_owned(),
                format!("Reason: {reason}"),
                format!(
                    "gleon is free for all users until December 31, 2026. After that, commercial CI/CD use needs a commercial license: {PRICING_URL}"
                ),
            ];
            if strict_mode {
                lines.push(
                    "--strict (GLEON_STRICT) is set, so gleon stops here with exit code 42."
                        .to_owned(),
                );
            }
            PolicyDecision {
                action: if strict_mode {
                    EnforcementAction::Block
                } else {
                    EnforcementAction::Warn
                },
                message: framed(lines),
                gha_annotation: in_github.then(|| {
                    if strict_mode {
                        format!(
                            "::error title=gleon license::This CI run has no valid license key ({reason}). --strict stops gleon with exit code 42."
                        )
                    } else {
                        format!(
                            "::warning title=gleon license::This CI run has no valid license key ({reason}). gleon is free for all users until December 31, 2026."
                        )
                    }
                }),
            }
        }
        LicenseStatus::UnofficialBuildInPrivateCI { reason } => blocked(
            "This gleon binary is not an official release",
            &reason,
            in_github,
        ),
        LicenseStatus::ExpiredUnlicensedBinary { reason } => blocked(
            "This official release of gleon is more than 90 days old",
            &reason,
            in_github,
        ),
    }
}

/// `lines` between two [`RULE`]s.
fn framed(lines: Vec<String>) -> Vec<String> {
    std::iter::once(RULE.to_owned())
        .chain(lines)
        .chain(std::iter::once(RULE.to_owned()))
        .collect()
}

/// The decision for a binary that cannot run in private CI without a key: `binary` says why.
fn blocked(binary: &str, reason: &str, in_github: bool) -> PolicyDecision {
    let advice = format!(
        "Without a key, private CI needs an official release less than 90 days old: {RELEASES_URL}"
    );
    PolicyDecision {
        action: EnforcementAction::Block,
        gha_annotation: in_github.then(|| {
            format!(
                "::error title=gleon license::{binary}, and this CI run has no valid license key. {advice}"
            )
        }),
        message: framed(vec![
            "[gleon license] gleon stops here with exit code 42.".to_owned(),
            format!("{binary}, and this CI run has no valid license key (GLEON_LICENSE_KEY)."),
            format!("Reason: {reason}"),
            advice,
        ]),
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

    struct MockEnv {
        vars: std::collections::HashMap<String, String>,
    }

    impl crate::env::EnvProvider for MockEnv {
        fn get_var(&self, key: &str) -> Option<String> {
            self.vars.get(key).cloned()
        }
    }

    #[test]
    fn test_identify_context_github_actions_public() {
        let mut vars = std::collections::HashMap::new();
        vars.insert("GITHUB_ACTIONS".to_string(), "true".to_string());
        vars.insert("GITHUB_REPOSITORY".to_string(), "foo/bar".to_string());

        let path = std::env::temp_dir().join(format!(
            "github_payload_ctx_{:?}.json",
            std::thread::current().id()
        ));
        fs::write(&path, r#"{"repository":{"private":false}}"#).unwrap();
        vars.insert(
            "GITHUB_EVENT_PATH".to_string(),
            path.to_string_lossy().into_owned(),
        );

        let env = MockEnv { vars };

        let ctx = identify_context(&env);
        assert_eq!(
            ctx,
            ExecutionContext::GitHubActions {
                repo: "foo/bar".to_string(),
                is_private: false
            }
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn test_identify_context_github_actions_missing_payload() {
        let mut vars = std::collections::HashMap::new();
        vars.insert("GITHUB_ACTIONS".to_string(), "true".to_string());
        vars.insert("GITHUB_REPOSITORY".to_string(), "foo/bar".to_string());
        // Do not insert GITHUB_EVENT_PATH
        let env = MockEnv { vars };

        let ctx = identify_context(&env);
        // Should fail-closed to private: true
        assert_eq!(
            ctx,
            ExecutionContext::GitHubActions {
                repo: "foo/bar".to_string(),
                is_private: true
            }
        );
    }

    #[test]
    fn test_identify_context_github_actions_requires_marker() {
        let mut vars = std::collections::HashMap::new();
        // Simulate a fake GITHUB_REPOSITORY set in another CI
        vars.insert("GITHUB_REPOSITORY".to_string(), "foo/bar".to_string());
        // And a generic CI provider variable
        vars.insert("CI_PROJECT_PATH".to_string(), "gitlab/project".to_string());

        // Create a fake public event payload
        let path = std::env::temp_dir().join(format!(
            "github_payload_fake_{:?}.json",
            std::thread::current().id()
        ));
        fs::write(&path, r#"{"repository":{"private":false}}"#).unwrap();
        vars.insert(
            "GITHUB_EVENT_PATH".to_string(),
            path.to_string_lossy().into_owned(),
        );

        let env = MockEnv { vars };

        let ctx = identify_context(&env);
        // It must NOT match GitHubActions and should fall through to GenericCI
        assert_eq!(
            ctx,
            ExecutionContext::GenericCI {
                repo: "gitlab/project".to_string()
            }
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn test_identify_context_gitlab() {
        let mut vars = std::collections::HashMap::new();
        vars.insert("CI_PROJECT_PATH".to_string(), "foo/bar".to_string());
        let env = MockEnv { vars };

        let ctx = identify_context(&env);
        assert_eq!(
            ctx,
            ExecutionContext::GenericCI {
                repo: "foo/bar".to_string()
            }
        );
    }

    #[test]
    fn test_identify_context_local_dev() {
        let env = MockEnv {
            vars: std::collections::HashMap::new(),
        };
        let ctx = identify_context(&env);
        assert_eq!(ctx, ExecutionContext::LocalDev);
    }

    #[test]
    fn test_identify_context_generic_ci_providers_and_precedence() {
        // CI_PROJECT_PATH
        let mut vars = std::collections::HashMap::new();
        vars.insert("CI_PROJECT_PATH".to_string(), "gitlab/project".to_string());
        assert_eq!(
            identify_context(&MockEnv { vars: vars.clone() }),
            ExecutionContext::GenericCI {
                repo: "gitlab/project".to_string()
            }
        );

        // TRAVIS_REPO_SLUG
        let mut vars_travis = std::collections::HashMap::new();
        vars_travis.insert("TRAVIS_REPO_SLUG".to_string(), "travis/project".to_string());
        assert_eq!(
            identify_context(&MockEnv { vars: vars_travis }),
            ExecutionContext::GenericCI {
                repo: "travis/project".to_string()
            }
        );

        // BITBUCKET_REPO_FULL_NAME
        let mut vars_bb = std::collections::HashMap::new();
        vars_bb.insert(
            "BITBUCKET_REPO_FULL_NAME".to_string(),
            "bitbucket/project".to_string(),
        );
        assert_eq!(
            identify_context(&MockEnv { vars: vars_bb }),
            ExecutionContext::GenericCI {
                repo: "bitbucket/project".to_string()
            }
        );

        // CircleCI two-part path
        let mut vars_circle = std::collections::HashMap::new();
        vars_circle.insert(
            "CIRCLE_PROJECT_USERNAME".to_string(),
            "circle_user".to_string(),
        );
        vars_circle.insert(
            "CIRCLE_PROJECT_REPONAME".to_string(),
            "circle_repo".to_string(),
        );
        assert_eq!(
            identify_context(&MockEnv {
                vars: vars_circle.clone()
            }),
            ExecutionContext::GenericCI {
                repo: "circle_user/circle_repo".to_string()
            }
        );

        // CircleCI incomplete (one component blank) falls back
        vars_circle.insert("CIRCLE_PROJECT_REPONAME".to_string(), "   ".to_string());
        vars_circle.insert("CI".to_string(), "true".to_string());
        assert_eq!(
            identify_context(&MockEnv { vars: vars_circle }),
            ExecutionContext::GenericCI {
                repo: String::new()
            }
        );

        // Blank higher-priority GLEON_PROJECT_PATH falls back to provider repo
        let mut vars_blank_override = std::collections::HashMap::new();
        vars_blank_override.insert("GLEON_PROJECT_PATH".to_string(), "   ".to_string());
        vars_blank_override.insert("CI_PROJECT_PATH".to_string(), "fallback/repo".to_string());
        assert_eq!(
            identify_context(&MockEnv {
                vars: vars_blank_override
            }),
            ExecutionContext::GenericCI {
                repo: "fallback/repo".to_string()
            }
        );

        // Override mismatch against provider repo prefers provider repo for security
        let mut vars_mismatch = std::collections::HashMap::new();
        vars_mismatch.insert("GLEON_PROJECT_PATH".to_string(), "spoofed/repo".to_string());
        vars_mismatch.insert("CI_PROJECT_PATH".to_string(), "authentic/repo".to_string());
        assert_eq!(
            identify_context(&MockEnv {
                vars: vars_mismatch
            }),
            ExecutionContext::GenericCI {
                repo: "authentic/repo".to_string()
            }
        );
    }

    #[test]
    fn test_generic_ci_verify_key_empty_and_whitespace_repo_fails() {
        let token = generate_test_license("foo/*", 2000);

        let empty_ctx = ExecutionContext::GenericCI {
            repo: String::new(),
        };
        let res_empty = LicenseGate::verify_key(&token, &empty_ctx, 100);
        assert!(res_empty.is_err());
        assert!(matches!(
            res_empty.unwrap_err(),
            LicenseError::MissingCiRepo
        ));

        let whitespace_ctx = ExecutionContext::GenericCI {
            repo: "   ".to_string(),
        };
        let res_ws = LicenseGate::verify_key(&token, &whitespace_ctx, 100);
        assert!(res_ws.is_err());
        assert!(matches!(res_ws.unwrap_err(), LicenseError::MissingCiRepo));
    }

    #[test]
    fn test_parse_github_event_payload_is_private() {
        use std::io::Write;
        let temp = tempfile::tempdir().unwrap();
        let payload_path = temp.path().join("event.json");
        let mut file = fs::File::create(&payload_path).unwrap();
        file.write_all(b"{\"repository\": {\"private\": true}}")
            .unwrap();

        let mut vars = std::collections::HashMap::new();
        vars.insert(
            "GITHUB_EVENT_PATH".to_string(),
            payload_path.to_string_lossy().into_owned(),
        );
        let env = MockEnv { vars };

        assert!(parse_github_event_payload_is_private(&env));
    }

    #[test]
    fn test_parse_github_event_payload_edge_cases() {
        let temp = tempfile::tempdir().unwrap();

        // 1. Non-existent file path returns true (fail closed)
        let mut vars = std::collections::HashMap::new();
        vars.insert(
            "GITHUB_EVENT_PATH".to_string(),
            temp.path()
                .join("missing.json")
                .to_string_lossy()
                .into_owned(),
        );
        assert!(parse_github_event_payload_is_private(&MockEnv {
            vars: vars.clone()
        }));

        // 2. Malformed JSON returns true (fail closed)
        let bad_json_path = temp.path().join("bad.json");
        fs::write(&bad_json_path, "{ invalid json }").unwrap();
        vars.insert(
            "GITHUB_EVENT_PATH".to_string(),
            bad_json_path.to_string_lossy().into_owned(),
        );
        assert!(parse_github_event_payload_is_private(&MockEnv {
            vars: vars.clone()
        }));

        // 3. JSON without repository returns true (fail closed)
        let no_repo_path = temp.path().join("norepo.json");
        fs::write(&no_repo_path, "{}").unwrap();
        vars.insert(
            "GITHUB_EVENT_PATH".to_string(),
            no_repo_path.to_string_lossy().into_owned(),
        );
        assert!(parse_github_event_payload_is_private(&MockEnv { vars }));
    }

    fn generate_test_license(repo_pattern: &str, expires_at: u64) -> String {
        use ed25519_dalek::SigningKey;
        let secret = [42u8; 32];
        let signing_key = SigningKey::from_bytes(&secret);
        let public_key = signing_key.verifying_key();

        PUBLIC_KEYS.with(|keys| *keys.borrow_mut() = vec![(1, public_key.to_bytes())]);

        let payload = LicensePayload {
            v: 1,
            owner: "test".to_string(),
            repo_pattern: repo_pattern.to_string(),
            expires_at,
            license_id: "test-id".to_string(),
        };

        generate_license_token(1, &payload, &signing_key).unwrap()
    }

    #[test]
    fn test_verify_key_valid() {
        let now = 1000;
        let token = generate_test_license("foo/*", now + 100);
        let ctx = ExecutionContext::GenericCI {
            repo: "foo/bar".to_string(),
        };
        let res = LicenseGate::verify_key(&token, &ctx, now).unwrap();
        assert!(matches!(res, LicenseValidity::Valid));
    }

    #[test]
    fn test_verify_key_grace_period() {
        let now = 20 * 24 * 60 * 60;
        let expires_at = now - (5 * 24 * 60 * 60); // Expired 5 days ago (within 14 days)
        let token = generate_test_license("foo/*", expires_at);
        let ctx = ExecutionContext::GenericCI {
            repo: "foo/bar".to_string(),
        };
        let res = LicenseGate::verify_key(&token, &ctx, now).unwrap();
        assert_eq!(
            res,
            LicenseValidity::GracePeriod {
                reason: "it expired 5 day(s) ago and stops working in 9 day(s)".to_string()
            }
        );
        // Part of a day left counts as a day.
        let res = LicenseGate::verify_key(&token, &ctx, now + 3600).unwrap();
        assert_eq!(
            res,
            LicenseValidity::GracePeriod {
                reason: "it expired 5 day(s) ago and stops working in 9 day(s)".to_string()
            }
        );
    }

    #[test]
    fn test_verify_key_grace_period_bounds() {
        let day = 24 * 60 * 60;
        let expires_at = 20 * day;
        let token = generate_test_license("foo/*", expires_at);
        let ctx = ExecutionContext::GenericCI {
            repo: "foo/bar".to_string(),
        };
        let at = |now: u64| LicenseGate::verify_key(&token, &ctx, now).ok();
        let grace = |reason: &str| {
            Some(LicenseValidity::GracePeriod {
                reason: reason.to_owned(),
            })
        };
        assert_eq!(
            at(expires_at + 1),
            grace("it expired less than a day ago and stops working in 14 day(s)")
        );
        assert_eq!(
            at(expires_at + day - 1),
            grace("it expired less than a day ago and stops working in 14 day(s)")
        );
        assert_eq!(
            at(expires_at + day),
            grace("it expired 1 day(s) ago and stops working in 13 day(s)")
        );
        assert_eq!(
            at(expires_at + 14 * day),
            grace("it expired 14 day(s) ago and stops working in 0 day(s)")
        );
        assert!(matches!(
            LicenseGate::verify_key(&token, &ctx, expires_at),
            Ok(LicenseValidity::Valid)
        ));
        assert!(matches!(
            LicenseGate::verify_key(&token, &ctx, expires_at + 14 * day + 1),
            Err(LicenseError::Expired)
        ));
    }

    #[test]
    fn test_verify_key_expired() {
        let now = 20 * 24 * 60 * 60;
        let expires_at = now - (15 * 24 * 60 * 60); // Expired 15 days ago (> 14 days)
        let token = generate_test_license("foo/*", expires_at);
        let ctx = ExecutionContext::GenericCI {
            repo: "foo/bar".to_string(),
        };
        let res = LicenseGate::verify_key(&token, &ctx, now);
        assert!(res.is_err());
    }

    #[test]
    fn test_verify_key_wrong_repo() {
        let now = 1000;
        let token = generate_test_license("baz/*", now + 100);
        let ctx = ExecutionContext::GenericCI {
            repo: "foo/bar".to_string(),
        };
        let res = LicenseGate::verify_key(&token, &ctx, now);
        assert!(res.is_err());
    }

    #[test]
    fn test_verify_key_error_branches() {
        use ed25519_dalek::{Signer, SigningKey};

        let ctx = ExecutionContext::GenericCI {
            repo: "foo/bar".to_string(),
        };

        // 1. Invalid base64
        let err_b64 = LicenseGate::verify_key("not_valid_b64!@#$", &ctx, 1000);
        assert!(err_b64.is_err());
        assert!(matches!(err_b64.unwrap_err(), LicenseError::InvalidBase64));

        // 2. Payload too short (<= 64 bytes)
        let short_b64 = base64::engine::general_purpose::STANDARD.encode([0u8; 32]);
        let err_short = LicenseGate::verify_key(&short_b64, &ctx, 1000);
        assert!(err_short.is_err());
        assert!(matches!(
            err_short.unwrap_err(),
            LicenseError::PayloadTooShort
        ));

        // 3. Cryptographic signature verification failed & Invalid license payload JSON
        // Initialize PUBLIC_KEY_BYTES for current thread
        let _valid_token = generate_test_license("foo/*", 2000);

        // Signature mismatch with valid signature format (signed with a different key)
        let payload = LicensePayload {
            v: 1,
            owner: "test".to_string(),
            repo_pattern: "foo/*".to_string(),
            expires_at: 2000,
            license_id: "test-id".to_string(),
        };
        let payload_bytes = serde_json::to_vec(&payload).unwrap();
        let other_key = SigningKey::from_bytes(&[99u8; 32]);

        let mut message = Vec::with_capacity(1 + payload_bytes.len());
        message.push(1);
        message.extend_from_slice(&payload_bytes);

        let sig = other_key.sign(&message);
        let mut invalid_sig_payload = message;
        invalid_sig_payload.extend_from_slice(&sig.to_bytes());
        let invalid_token = base64::engine::general_purpose::STANDARD.encode(invalid_sig_payload);
        let err_sig_verify = LicenseGate::verify_key(&invalid_token, &ctx, 1000);
        assert!(err_sig_verify.is_err());
        assert!(matches!(
            err_sig_verify.unwrap_err(),
            LicenseError::SignatureVerificationFailed(_)
        ));

        // Invalid license payload JSON (signed with matching key but bad JSON)
        let matching_key = SigningKey::from_bytes(&[42u8; 32]);
        let bad_json = b"{ not valid json }";

        let mut message = Vec::with_capacity(1 + bad_json.len());
        message.push(1);
        message.extend_from_slice(bad_json);

        let bad_json_sig = matching_key.sign(&message);
        let mut bad_json_payload = message;
        bad_json_payload.extend_from_slice(&bad_json_sig.to_bytes());
        let bad_json_token = base64::engine::general_purpose::STANDARD.encode(bad_json_payload);
        let err_json = LicenseGate::verify_key(&bad_json_token, &ctx, 1000);
        assert!(err_json.is_err());
        assert!(matches!(
            err_json.unwrap_err(),
            LicenseError::InvalidPayloadJson(_)
        ));

        // 4. Invalid glob pattern in repo_pattern
        let token_bad_glob = generate_test_license("[invalid", 2000);
        let err_glob = LicenseGate::verify_key(&token_bad_glob, &ctx, 1000);
        assert!(err_glob.is_err());
        assert!(matches!(
            err_glob.unwrap_err(),
            LicenseError::InvalidRepoPattern(_)
        ));
    }

    #[test]
    fn test_enforce_policy_outcomes() {
        let mut vars = std::collections::HashMap::new();
        vars.insert("GITHUB_ACTIONS".to_string(), "true".to_string());
        let env = MockEnv { vars };

        assert_eq!(
            enforce_policy(LicenseStatus::Valid, false, &env).action,
            EnforcementAction::Allow
        );
        assert_eq!(
            enforce_policy(LicenseStatus::PublicOrGrantedUse, false, &env).action,
            EnforcementAction::Allow
        );

        let soft_warn = enforce_policy(
            LicenseStatus::UnlicensedSoft {
                reason: "soft test".to_string(),
            },
            false,
            &env,
        );
        assert_eq!(soft_warn.action, EnforcementAction::Warn);
        assert!(soft_warn.message.iter().any(|l| l.contains("soft test")));
        assert!(
            soft_warn
                .message
                .iter()
                .any(|l| l.contains("free for all users until December 31, 2026"))
        );
        assert!(!soft_warn.message.iter().any(|l| l.contains("exit code 42")));
        assert_eq!(soft_warn.message.first().map(String::as_str), Some(RULE));
        assert_eq!(soft_warn.message.last().map(String::as_str), Some(RULE));
        assert_eq!(
            soft_warn.gha_annotation.as_deref(),
            Some(
                "::warning title=gleon license::This CI run has no valid license key (soft test). gleon is free for all users until December 31, 2026."
            )
        );

        let soft_block = enforce_policy(
            LicenseStatus::UnlicensedSoft {
                reason: "strict test".to_string(),
            },
            true,
            &env,
        );
        assert_eq!(soft_block.action, EnforcementAction::Block);
        assert!(
            soft_block
                .message
                .iter()
                .any(|l| l.contains("exit code 42"))
        );
        assert_eq!(
            soft_block.gha_annotation.as_deref(),
            Some(
                "::error title=gleon license::This CI run has no valid license key (strict test). --strict stops gleon with exit code 42."
            )
        );

        let unofficial = enforce_policy(
            LicenseStatus::UnofficialBuildInPrivateCI {
                reason: "test reason".to_string(),
            },
            false,
            &env,
        );
        assert_eq!(unofficial.action, EnforcementAction::Block);
        assert!(
            unofficial
                .message
                .iter()
                .any(|l| l.contains("not an official release"))
        );
        assert!(
            unofficial
                .message
                .iter()
                .any(|l| l == "Reason: test reason")
        );
        assert_eq!(
            unofficial.gha_annotation.as_deref(),
            Some(
                "::error title=gleon license::This gleon binary is not an official release, and this CI run has no valid license key. Without a key, private CI needs an official release less than 90 days old: https://github.com/gleon-rs/gleon/releases"
            )
        );

        let expired = enforce_policy(
            LicenseStatus::ExpiredUnlicensedBinary {
                reason: "test reason".to_string(),
            },
            false,
            &env,
        );
        assert_eq!(expired.action, EnforcementAction::Block);
        assert!(
            expired
                .message
                .iter()
                .any(|l| l.contains("more than 90 days old"))
        );

        // Non-GitHub environment: no annotation, but still a printable message.
        let env_non_gh = MockEnv {
            vars: std::collections::HashMap::new(),
        };
        let soft_non_gh = enforce_policy(
            LicenseStatus::UnlicensedSoft {
                reason: "soft test non gh".to_string(),
            },
            false,
            &env_non_gh,
        );
        assert_eq!(soft_non_gh.action, EnforcementAction::Warn);
        assert!(!soft_non_gh.message.is_empty());
        assert!(soft_non_gh.gha_annotation.is_none());

        let unofficial_non_gh = enforce_policy(
            LicenseStatus::UnofficialBuildInPrivateCI {
                reason: "test reason".to_string(),
            },
            false,
            &env_non_gh,
        );
        assert_eq!(unofficial_non_gh.action, EnforcementAction::Block);
        assert!(unofficial_non_gh.gha_annotation.is_none());
    }

    #[test]
    fn test_verify_internal_local_dev() {
        let env = MockEnv {
            vars: std::collections::HashMap::new(),
        };
        let status = LicenseGate::verify_internal(&env, false, 0);
        assert_eq!(status, LicenseStatus::Valid);
    }

    #[test]
    fn test_verify_internal_public_ci() {
        let mut vars = std::collections::HashMap::new();
        vars.insert("GITHUB_ACTIONS".to_string(), "true".to_string());
        vars.insert("GITHUB_REPOSITORY".to_string(), "foo/bar".to_string());

        let path = std::env::temp_dir().join(format!(
            "github_payload_{:?}.json",
            std::thread::current().id()
        ));
        fs::write(&path, r#"{"repository":{"private":false}}"#).unwrap();
        vars.insert(
            "GITHUB_EVENT_PATH".to_string(),
            path.to_string_lossy().into_owned(),
        );

        let env = MockEnv { vars };

        let status = LicenseGate::verify_internal(&env, false, 0);
        assert_eq!(status, LicenseStatus::PublicOrGrantedUse);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn test_verify_internal_unofficial_private_ci() {
        let mut vars = std::collections::HashMap::new();
        vars.insert("CI".to_string(), "true".to_string());
        let env = MockEnv { vars };

        let status = LicenseGate::verify_internal(&env, false, 0); // is_official = false
        assert!(matches!(
            status,
            LicenseStatus::UnofficialBuildInPrivateCI { .. }
        ));
    }

    #[test]
    fn test_verify_internal_official_valid_timestamp() {
        let mut vars = std::collections::HashMap::new();
        vars.insert("CI".to_string(), "true".to_string());
        let env = MockEnv { vars };

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Valid if timestamp is within last 24h (e.g. now - 1 hour)
        let build_timestamp = now - 3600;

        let status = LicenseGate::verify_internal(&env, true, build_timestamp);
        assert!(matches!(status, LicenseStatus::UnlicensedSoft { .. }));
    }

    #[test]
    fn test_verify_internal_official_expired_timestamp() {
        let mut vars = std::collections::HashMap::new();
        vars.insert("CI".to_string(), "true".to_string());
        let env = MockEnv { vars };

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Expired if timestamp is older than 90 days (e.g. now - 100 days)
        let build_timestamp = now.saturating_sub(100 * 24 * 60 * 60);

        let status = LicenseGate::verify_internal(&env, true, build_timestamp);
        assert!(matches!(
            status,
            LicenseStatus::ExpiredUnlicensedBinary { .. }
        ));
    }

    #[test]
    fn test_verify_internal_official_future_timestamp_blocked() {
        let mut vars = std::collections::HashMap::new();
        vars.insert("CI".to_string(), "true".to_string());
        let env = MockEnv { vars };

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Invalid if timestamp is > 24h in the future
        let build_timestamp = now + 48 * 3600;

        let status = LicenseGate::verify_internal(&env, true, build_timestamp);
        assert!(matches!(
            status,
            LicenseStatus::UnofficialBuildInPrivateCI { .. }
        ));
    }

    #[test]
    fn test_verify_public_api() {
        let env = MockEnv {
            vars: std::collections::HashMap::new(),
        };
        // Verify public LicenseGate::verify entrypoint
        let status = LicenseGate::verify(&env);
        assert_eq!(status, LicenseStatus::Valid);
    }

    #[test]
    fn test_generate_license_token_and_verify_key() {
        use ed25519_dalek::SigningKey;
        let secret = [99u8; 32];
        let signing_key = SigningKey::from_bytes(&secret);
        let public_key = signing_key.verifying_key();

        PUBLIC_KEYS.with(|keys| *keys.borrow_mut() = vec![(1, public_key.to_bytes())]);

        let now = 5000;
        let payload = LicensePayload {
            v: 1,
            owner: "acme".to_string(),
            repo_pattern: "acme/*".to_string(),
            expires_at: now + 86400,
            license_id: "lic-123".to_string(),
        };

        let token = generate_license_token(1, &payload, &signing_key).unwrap();
        let ctx = ExecutionContext::GenericCI {
            repo: "acme/web".to_string(),
        };

        let result = LicenseGate::verify_key(&token, &ctx, now).unwrap();
        assert_eq!(result, LicenseValidity::Valid);
    }

    #[test]
    fn test_decode_base64_flexible() {
        let sample = b"hello ed25519 world";
        let std_b64 = base64::engine::general_purpose::STANDARD.encode(sample);
        let url_b64 = base64::engine::general_purpose::URL_SAFE.encode(sample);
        let std_no_pad = base64::engine::general_purpose::STANDARD_NO_PAD.encode(sample);
        let url_no_pad = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sample);

        assert_eq!(decode_base64_flexible(&std_b64).unwrap(), sample);
        assert_eq!(decode_base64_flexible(&url_b64).unwrap(), sample);
        assert_eq!(decode_base64_flexible(&std_no_pad).unwrap(), sample);
        assert_eq!(decode_base64_flexible(&url_no_pad).unwrap(), sample);
        assert!(decode_base64_flexible("not valid @#$ b64").is_none());
    }

    #[test]
    fn test_embedded_public_key_is_valid_ed25519_curve_point() {
        // Test that the production public key embedded in the binary is a mathematically valid Edwards curve point
        for (_, bytes) in OFFICIAL_PUBLIC_KEYS {
            assert!(VerifyingKey::from_bytes(bytes).is_ok());
        }
    }

    #[test]
    fn test_verify_internal_with_valid_and_grace_period_license() {
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);
        PUBLIC_KEYS
            .with(|keys| *keys.borrow_mut() = vec![(255, signing_key.verifying_key().to_bytes())]);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // 1. Valid token
        let valid_payload = LicensePayload {
            v: 1,
            owner: "acme".to_string(),
            repo_pattern: "org/repo".to_string(),
            expires_at: now + 3600,
            license_id: "lic_1".to_string(),
        };
        let valid_token = generate_license_token(255, &valid_payload, &signing_key).unwrap();

        let mut vars = std::collections::HashMap::new();
        vars.insert("CI".to_string(), "true".to_string());
        vars.insert("CI_PROJECT_PATH".to_string(), "org/repo".to_string());
        vars.insert("GLEON_LICENSE_KEY".to_string(), valid_token);
        let env = MockEnv { vars };

        let status = LicenseGate::verify_internal(&env, false, 0);
        assert_eq!(status, LicenseStatus::Valid);

        // 2. Grace period token (expired 2 days ago, within 14-day window)
        let grace_payload = LicensePayload {
            v: 1,
            owner: "acme".to_string(),
            repo_pattern: "org/repo".to_string(),
            expires_at: now - 2 * 86400,
            license_id: "lic_grace".to_string(),
        };
        let grace_token = generate_license_token(255, &grace_payload, &signing_key).unwrap();

        let mut vars_grace = std::collections::HashMap::new();
        vars_grace.insert("CI".to_string(), "true".to_string());
        vars_grace.insert("CI_PROJECT_PATH".to_string(), "org/repo".to_string());
        vars_grace.insert("GLEON_LICENSE_KEY".to_string(), grace_token);
        let env_grace = MockEnv { vars: vars_grace };

        let status_grace = LicenseGate::verify_internal(&env_grace, false, 0);
        assert!(matches!(status_grace, LicenseStatus::GracePeriod { .. }));
    }

    #[test]
    fn test_enforce_policy_grace_period() {
        let mut vars = std::collections::HashMap::new();
        vars.insert("GITHUB_ACTIONS".to_string(), "true".to_string());
        let env_gha = MockEnv { vars };

        let decision_gha = enforce_policy(
            LicenseStatus::GracePeriod {
                reason: "expired recently".to_string(),
            },
            false,
            &env_gha,
        );
        assert_eq!(decision_gha.action, EnforcementAction::Warn);
        assert!(
            decision_gha
                .message
                .iter()
                .any(|l| l.contains("expired recently"))
        );
        assert_eq!(
            decision_gha.gha_annotation.as_deref(),
            Some("::warning title=gleon license::The license key has expired: expired recently.")
        );

        let env_non_gha = MockEnv {
            vars: std::collections::HashMap::new(),
        };
        let decision_non_gha = enforce_policy(
            LicenseStatus::GracePeriod {
                reason: "expired recently non gha".to_string(),
            },
            false,
            &env_non_gha,
        );
        assert_eq!(decision_non_gha.action, EnforcementAction::Warn);
        assert!(decision_non_gha.gha_annotation.is_none());
    }

    #[test]
    fn test_verify_token_too_long() {
        let long_key = "a".repeat(8193);
        let err = LicenseGate::verify_key(&long_key, &ExecutionContext::LocalDev, 0).unwrap_err();
        assert!(matches!(err, LicenseError::TokenTooLong));
    }

    #[test]
    fn test_verify_unsupported_version() {
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);
        PUBLIC_KEYS
            .with(|keys| *keys.borrow_mut() = vec![(255, signing_key.verifying_key().to_bytes())]);
        let payload = LicensePayload {
            v: 2,
            owner: "acme".to_string(),
            repo_pattern: "org/repo".to_string(),
            expires_at: 1_000_000,
            license_id: "lic_v2".to_string(),
        };
        let token = generate_license_token(255, &payload, &signing_key).unwrap();
        let err = LicenseGate::verify_key(&token, &ExecutionContext::LocalDev, 0).unwrap_err();
        assert!(matches!(err, LicenseError::UnsupportedVersion(2)));
    }

    #[test]
    fn test_verify_parsed_payload_contexts() {
        let payload = LicensePayload {
            v: 1,
            owner: "acme".to_string(),
            repo_pattern: "acme/*".to_string(),
            expires_at: 2_000_000,
            license_id: "lic_1".to_string(),
        };
        // 1. GitHubActions matching pattern
        let gha_ctx = ExecutionContext::GitHubActions {
            repo: "acme/app".to_string(),
            is_private: true,
        };
        let valid = LicenseGate::verify_parsed_payload(&payload, &gha_ctx, 1_000_000).unwrap();
        assert_eq!(valid, LicenseValidity::Valid);

        // 2. LocalDev context (repo_to_check is None)
        let local_ctx = ExecutionContext::LocalDev;
        let valid_local =
            LicenseGate::verify_parsed_payload(&payload, &local_ctx, 1_000_000).unwrap();
        assert_eq!(valid_local, LicenseValidity::Valid);
    }
}
