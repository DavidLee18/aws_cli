//! Credential resolution.
//!
//! Mirrors botocore's provider chain: environment variables first, then the selected
//! profile's mechanism (static keys, SSO, `credential_process`, assume-role), then the
//! ambient providers (container, then IMDS).
//!
//! Mechanisms that are not implemented are reported *by name* rather than skipped. A
//! chain that silently falls through to the next provider can authenticate as the wrong
//! identity, which is far worse than a clear error.

pub mod assume_role;
pub mod cache;
pub mod imds;
pub mod process;
pub mod profile;
pub mod sso;

use assume_role::AssumeRoleRequest;
use profile::{Config, Section};
use std::collections::BTreeSet;

#[derive(Debug, Clone)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    /// RFC 3339 or the sigv4 compact form; informational only today.
    pub expires_at: Option<String>,
    /// Which provider in the chain supplied these, using botocore's own names for them
    /// (`env`, `shared-credentials-file`, `config-file`, `custom-process`, `sso`,
    /// `assume-role`, `assume-role-with-web-identity`, `iam-role`, `container-role`).
    ///
    /// Only `configure list` reads it, and it has to: the TYPE column there is the
    /// provider name, not where the value was looked up, so there is no way to derive it
    /// after the fact from the credentials themselves.
    pub method: &'static str,
}

#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("Unable to locate credentials. You can configure credentials by running \"aws configure\".")]
    NotFound,
    #[error("The config profile ({0}) could not be found")]
    UnknownProfile(String),
    #[error("profile `{profile}`: {message}")]
    Process { profile: String, message: String },
    #[error("profile `{profile}`: {message}")]
    Sso { profile: String, message: String },
    /// The reference has TWO distinct SSO failure messages and which one appears depends
    /// on where the failure happened. Collapsing them into one would be a stderr
    /// divergence, so both are reproduced verbatim.
    ///
    /// This one is botocore's `TokenRetrievalError` (`tokens.py`): the cached token is
    /// missing, unusable, or expired with refresh unavailable or unsuccessful.
    #[error("Error when retrieving token from sso: Token has expired and refresh failed")]
    SsoTokenExpired { profile: String, detail: String },
    /// botocore's `UnauthorizedSSOTokenError`: the portal itself rejected the token.
    #[error(
        "The SSO session associated with this profile has expired or is otherwise \
         invalid. To refresh this SSO session run aws sso login with the corresponding \
         profile."
    )]
    SsoUnauthorized { profile: String },
    #[error("profile `{profile}` uses {mechanism}, which is not implemented yet")]
    Unsupported { profile: String, mechanism: &'static str },
    #[error("profile `{profile}`: {message}")]
    AssumeRole { profile: String, message: String },
    /// STS rejected the AssumeRole call. The reference reports this exactly like any
    /// other service error — same wording, same exit code 254 — rather than as a
    /// credential-chain failure, so it is kept as its own variant.
    #[error("An error occurred ({code}) when calling the {operation} operation: {message}")]
    AssumeRoleService { code: String, message: String, operation: String },
    /// botocore's `InvalidConfigError` cases, kept distinct so the message can name the
    /// exact misconfiguration.
    #[error("profile `{profile}`: {message}")]
    InvalidConfig { profile: String, message: String },
    #[error(
        "Infinite loop in credential configuration detected. \
         Attempting to load from profile `{profile}` which was already visited."
    )]
    ProfileCycle { profile: String },
    #[error(transparent)]
    Config(#[from] profile::ConfigError),
}

impl CredentialError {
    /// Whether the reference treats this as a *configuration* error (253) or a general
    /// one (255).
    ///
    /// Only "no credentials could be found anywhere" is 253 — botocore's
    /// `NoCredentialsError`. A profile that exists but cannot produce credentials, an
    /// unknown profile, or an expired SSO token are all 255. Verified by running the
    /// reference against real profiles.
    pub fn is_configuration_error(&self) -> bool {
        matches!(self, CredentialError::NotFound)
    }

    /// A rejected AssumeRole is a *client* error (254), like any other service failure.
    pub fn is_client_error(&self) -> bool {
        matches!(self, CredentialError::AssumeRoleService { .. })
    }
}

/// Resolve credentials for the given profile.
///
/// `region` is the caller's resolved region, used for any `sts:AssumeRole` call — botocore
/// uses the session's first client region for this, not a per-profile setting.
///
/// Chain order follows botocore's `create_credential_resolver`; see [`from_profile`] for
/// the within-profile ordering, which is *not* the intuitive one.
pub fn resolve(
    explicit_profile: Option<&str>,
    region: Option<&str>,
) -> Result<Credentials, CredentialError> {
    // An explicitly-selected profile REMOVES the environment provider from the chain
    // entirely (botocore credentials.py:92 + :151-171). Without this, `--profile foo`
    // would silently authenticate as whatever AWS_ACCESS_KEY_ID happened to be exported.
    let profile_is_explicit = explicit_profile.is_some()
        || std::env::var("AWS_PROFILE").is_ok_and(|v| !v.is_empty())
        || std::env::var("AWS_DEFAULT_PROFILE").is_ok_and(|v| !v.is_empty());

    if !profile_is_explicit {
        if let Some(creds) = from_environment() {
            return Ok(creds);
        }
    }

    let config = Config::load()?;
    let name = profile::profile_name(explicit_profile);

    if config.profile_exists(&name) {
        let mut visited = BTreeSet::new();
        return from_profile(&config, &name, region, &mut visited);
    }
    // An explicitly named profile that does not exist is an error; a missing `default`
    // just means "try the ambient providers".
    if profile_is_explicit {
        return Err(CredentialError::UnknownProfile(name));
    }
    from_ambient()
}

fn from_environment() -> Option<Credentials> {
    let id = std::env::var("AWS_ACCESS_KEY_ID").ok().filter(|s| !s.is_empty())?;
    let secret = std::env::var("AWS_SECRET_ACCESS_KEY").ok().filter(|s| !s.is_empty())?;
    Some(Credentials {
        access_key_id: id,
        secret_access_key: secret,
        session_token: std::env::var("AWS_SESSION_TOKEN").ok().filter(|s| !s.is_empty()),
        expires_at: None,
        method: "env",
    })
}

/// Ambient providers, used when no profile supplies credentials: container first, then
/// instance metadata — the order botocore uses.
fn from_ambient() -> Result<Credentials, CredentialError> {
    if let Some(creds) = imds::from_container()? {
        return Ok(creds);
    }
    if let Some(creds) = imds::from_instance_metadata()? {
        return Ok(creds);
    }
    Err(CredentialError::NotFound)
}

/// Dispatch on whichever mechanism the profile declares.
///
/// The order is botocore's, and is deliberately NOT the intuitive one — a profile that
/// carries several mechanisms resolves by provider position, not by which key looks most
/// specific:
///
/// 1. `role_arn` (assume-role) — position 2, so it beats static keys in the same profile
/// 2. `web_identity_token_file` + `role_arn`
/// 3. SSO
/// 4. static keys from `~/.aws/credentials`
/// 5. `credential_process`
/// 6. static keys from `~/.aws/config`
///
/// Steps 4 and 6 are why the two files are not merged: static keys in `config` lose to
/// `credential_process`, while the same keys in `credentials` win.
fn from_profile(
    config: &Config,
    name: &str,
    region: Option<&str>,
    visited: &mut BTreeSet<String>,
) -> Result<Credentials, CredentialError> {
    let merged = config.profile(name).unwrap_or_default();

    // 1 & 2: assume-role variants, before anything else.
    if merged.contains_key("role_arn") {
        return resolve_assume_role(config, name, &merged, region, visited);
    }

    // 3: SSO.
    if let Some(sso_config) = sso_config_for(config, &merged) {
        return sso::resolve(&sso_config, name);
    }

    // 4: static keys from the credentials file.
    if let Some(creds) = static_keys(config.credentials_profiles.get(name), "shared-credentials-file") {
        return Ok(creds);
    }

    // 5: credential_process.
    if let Some(command) = merged.get("credential_process") {
        return process::resolve(command, name);
    }

    // 6: static keys from the config file.
    if let Some(creds) = static_keys(config.config_profiles.get(name), "config-file") {
        return Ok(creds);
    }

    // The profile exists but declares no credential mechanism (a region-only profile is
    // common); fall through to the ambient providers rather than failing outright.
    from_ambient()
}

/// Resolve a `role_arn` profile.
///
/// Source credentials come from `source_profile` (recursive, so role chaining works) or
/// `credential_source` (one of the flat ambient providers). Exactly one must be present.
fn resolve_assume_role(
    config: &Config,
    name: &str,
    section: &Section,
    region: Option<&str>,
    visited: &mut BTreeSet<String>,
) -> Result<Credentials, CredentialError> {
    // Cycles are unbounded in depth but must terminate.
    if !visited.insert(name.to_string()) {
        return Err(CredentialError::ProfileCycle { profile: name.to_string() });
    }

    let role_arn = section.get("role_arn").cloned().unwrap_or_default();
    let region = region.ok_or_else(|| CredentialError::InvalidConfig {
        profile: name.to_string(),
        message: "assuming a role requires a region".to_string(),
    })?;

    // Web identity is a different API and needs no source credentials.
    if let Some(token_file) = section.get("web_identity_token_file") {
        let token = std::fs::read_to_string(token_file).map_err(|e| {
            CredentialError::InvalidConfig {
                profile: name.to_string(),
                message: format!("cannot read web_identity_token_file `{token_file}`: {e}"),
            }
        })?;
        return assume_role::assume_role_with_web_identity(
            region,
            &role_arn,
            token.trim(),
            section.get("role_session_name").map(String::as_str),
            name,
        );
    }

    let has_source = section.contains_key("source_profile");
    let has_credential_source = section.contains_key("credential_source");
    if has_source && has_credential_source {
        return Err(CredentialError::InvalidConfig {
            profile: name.to_string(),
            message: "contains both source_profile and credential_source".to_string(),
        });
    }

    let source = if let Some(source_profile) = section.get("source_profile") {
        if !config.profile_exists(source_profile) {
            return Err(CredentialError::InvalidConfig {
                profile: name.to_string(),
                message: format!("source_profile `{source_profile}` does not exist"),
            });
        }
        from_profile(config, source_profile, Some(region), visited)?
    } else if let Some(credential_source) = section.get("credential_source") {
        // Matched case-insensitively, as botocore does.
        match credential_source.to_ascii_lowercase().as_str() {
            "environment" => from_environment().ok_or(CredentialError::NotFound)?,
            "ecscontainer" => imds::from_container()?.ok_or(CredentialError::NotFound)?,
            "ec2instancemetadata" => {
                imds::from_instance_metadata()?.ok_or(CredentialError::NotFound)?
            }
            _ => {
                return Err(CredentialError::InvalidConfig {
                    profile: name.to_string(),
                    message: format!("credential_source `{credential_source}` is not valid"),
                })
            }
        }
    } else {
        return Err(CredentialError::InvalidConfig {
            profile: name.to_string(),
            message: "role_arn requires source_profile or credential_source".to_string(),
        });
    };

    let request = AssumeRoleRequest {
        role_arn,
        role_session_name: section.get("role_session_name").cloned(),
        // botocore coerces with int() and silently drops an unparseable value.
        duration_seconds: section.get("duration_seconds").and_then(|d| d.parse().ok()),
        external_id: section.get("external_id").cloned(),
        serial_number: section.get("mfa_serial").cloned(),
        token_code: match section.get("mfa_serial") {
            Some(serial) => Some(prompt_mfa(serial)?),
            None => None,
        },
    };

    assume_role::assume_role(&source, region, &request, name)
}

/// Prompt for an MFA code.
///
/// The reference hides the input via `getpass`; this does not, which is a deliberate
/// divergence rather than an oversight — hiding it needs terminal control this crate
/// otherwise has no reason to carry, and the code is single-use and short-lived.
fn prompt_mfa(serial: &str) -> Result<String, CredentialError> {
    use std::io::{BufRead, Write};
    eprint!("Enter MFA code for {serial}: ");
    let _ = std::io::stderr().flush();

    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).map_err(|e| CredentialError::AssumeRole {
        profile: serial.to_string(),
        message: format!("could not read MFA code: {e}"),
    })?;
    Ok(line.trim().to_string())
}

/// Static keys from one section. The access key alone triggers the provider; a missing
/// secret is an error rather than a skip, matching botocore's `PartialCredentialsError`.
fn static_keys(section: Option<&Section>, method: &'static str) -> Option<Credentials> {
    let section = section?;
    let id = section.get("aws_access_key_id")?;
    let secret = section.get("aws_secret_access_key")?;
    Some(Credentials {
        access_key_id: id.clone(),
        secret_access_key: secret.clone(),
        // botocore accepts the legacy `aws_security_token` spelling first.
        session_token: section
            .get("aws_security_token")
            .or_else(|| section.get("aws_session_token"))
            .cloned(),
        expires_at: None,
        method,
    })
}

/// Build an [`sso::SsoConfig`] from a profile, supporting both the `sso-session` form and
/// the legacy inline form.
fn sso_config_for(config: &Config, section: &Section) -> Option<sso::SsoConfig> {
    let account_id = section.get("sso_account_id")?.clone();
    let role_name = section.get("sso_role_name")?.clone();

    // Modern form: the profile names a session that carries the URL and region.
    if let Some(session_name) = section.get("sso_session") {
        let session = config.sso_session(session_name)?;
        return Some(sso::SsoConfig {
            session_name: Some(session_name.clone()),
            start_url: session.get("sso_start_url").cloned().unwrap_or_default(),
            sso_region: session.get("sso_region").cloned().unwrap_or_default(),
            account_id,
            role_name,
        });
    }

    // Legacy inline form.
    Some(sso::SsoConfig {
        session_name: None,
        start_url: section.get("sso_start_url")?.clone(),
        sso_region: section.get("sso_region")?.clone(),
        account_id,
        role_name,
    })
}

/// The region configured for a profile, for the endpoint layer's last fallback.
pub fn profile_region(explicit_profile: Option<&str>) -> Option<String> {
    profile_setting("region", explicit_profile)
}

/// Any single setting from the resolved profile, for the config-variable chains that
/// end at `~/.aws/config` — `cli_error_format` and `cli_binary_format` among them.
///
/// A missing or unreadable config file yields `None` rather than an error: every caller
/// has a default to fall back to, and failing to *print an error* because the config file
/// is unreadable would be a poor trade.
pub fn profile_setting(key: &str, explicit_profile: Option<&str>) -> Option<String> {
    let config = Config::load().ok()?;
    let name = profile::profile_name(explicit_profile);
    config.profile(&name)?.get(key).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(pairs: &[(&str, &str)]) -> Section {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// Static keys in `~/.aws/credentials` outrank `credential_process`...
    #[test]
    fn credentials_file_keys_beat_credential_process() {
        let mut config = Config::default();
        config.credentials_profiles.insert(
            "p".into(),
            section(&[("aws_access_key_id", "AKIA"), ("aws_secret_access_key", "secret")]),
        );
        config
            .config_profiles
            .insert("p".into(), section(&[("credential_process", "should-not-run")]));

        assert_eq!(from_profile(&config, "p", Some("us-east-1"), &mut BTreeSet::new()).unwrap().access_key_id, "AKIA");
    }

    /// ...but the same keys in `~/.aws/config` lose to it, because the providers sit on
    /// either side of `credential_process` in the chain.
    #[test]
    fn config_file_keys_lose_to_credential_process() {
        let mut config = Config::default();
        config.config_profiles.insert(
            "p".into(),
            section(&[
                ("aws_access_key_id", "FROM_CONFIG"),
                ("aws_secret_access_key", "s"),
                ("credential_process", "false"),
            ]),
        );
        // `false` exits non-zero, so reaching the process provider surfaces its error
        // rather than returning the config-file keys.
        match from_profile(&config, "p", Some("us-east-1"), &mut BTreeSet::new()) {
            Err(CredentialError::Process { .. }) => {}
            other => panic!("credential_process should have run first, got {other:?}"),
        }
    }

    /// `role_arn` sits at chain position 2, ahead of every profile-based provider.
    #[test]
    fn role_arn_outranks_static_keys_in_the_same_profile() {
        let mut config = Config::default();
        config.credentials_profiles.insert(
            "p".into(),
            section(&[("aws_access_key_id", "AKIA"), ("aws_secret_access_key", "s")]),
        );
        config
            .config_profiles
            .insert("p".into(), section(&[("role_arn", "arn:aws:iam::1:role/r")]));

        // Reaching the assume-role path at all is the point: it complains about the
        // missing source rather than quietly returning the static keys below it.
        match from_profile(&config, "p", Some("us-east-1"), &mut BTreeSet::new()) {
            Err(CredentialError::InvalidConfig { message, .. }) => {
                assert!(message.contains("source_profile"), "got: {message}")
            }
            other => panic!("assume-role should take precedence, got {other:?}"),
        }
    }

    #[test]
    fn rejects_both_source_profile_and_credential_source() {
        let mut config = Config::default();
        config.config_profiles.insert(
            "p".into(),
            section(&[
                ("role_arn", "arn:aws:iam::1:role/r"),
                ("source_profile", "base"),
                ("credential_source", "Environment"),
            ]),
        );
        match from_profile(&config, "p", Some("us-east-1"), &mut BTreeSet::new()) {
            Err(CredentialError::InvalidConfig { message, .. }) => {
                assert!(message.contains("both"), "got: {message}")
            }
            other => panic!("expected an InvalidConfig, got {other:?}"),
        }
    }

    #[test]
    fn rejects_unknown_credential_source() {
        let mut config = Config::default();
        config.config_profiles.insert(
            "p".into(),
            section(&[
                ("role_arn", "arn:aws:iam::1:role/r"),
                ("credential_source", "NotAThing"),
            ]),
        );
        match from_profile(&config, "p", Some("us-east-1"), &mut BTreeSet::new()) {
            Err(CredentialError::InvalidConfig { message, .. }) => {
                assert!(message.contains("not valid"), "got: {message}")
            }
            other => panic!("expected an InvalidConfig, got {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_source_profile() {
        let mut config = Config::default();
        config.config_profiles.insert(
            "p".into(),
            section(&[("role_arn", "arn:aws:iam::1:role/r"), ("source_profile", "nope")]),
        );
        match from_profile(&config, "p", Some("us-east-1"), &mut BTreeSet::new()) {
            Err(CredentialError::InvalidConfig { message, .. }) => {
                assert!(message.contains("does not exist"), "got: {message}")
            }
            other => panic!("expected an InvalidConfig, got {other:?}"),
        }
    }

    /// Role chaining is unbounded in depth, so a cycle must be caught rather than
    /// recursing until the stack runs out.
    #[test]
    fn detects_source_profile_cycles() {
        let mut config = Config::default();
        config.config_profiles.insert(
            "a".into(),
            section(&[("role_arn", "arn:aws:iam::1:role/a"), ("source_profile", "b")]),
        );
        config.config_profiles.insert(
            "b".into(),
            section(&[("role_arn", "arn:aws:iam::1:role/b"), ("source_profile", "a")]),
        );
        match from_profile(&config, "a", Some("us-east-1"), &mut BTreeSet::new()) {
            Err(CredentialError::ProfileCycle { .. }) => {}
            other => panic!("expected a cycle error, got {other:?}"),
        }
    }

    #[test]
    fn prefers_legacy_security_token_spelling() {
        let s = section(&[
            ("aws_access_key_id", "A"),
            ("aws_secret_access_key", "B"),
            ("aws_security_token", "legacy"),
            ("aws_session_token", "modern"),
        ]);
        assert_eq!(static_keys(Some(&s), "config-file").unwrap().session_token.as_deref(), Some("legacy"));
    }

    #[test]
    fn builds_sso_config_from_session() {
        let mut config = Config::default();
        config.sso_sessions.insert(
            "corp".to_string(),
            section(&[
                ("sso_start_url", "https://example.awsapps.com/start"),
                ("sso_region", "us-east-1"),
            ]),
        );
        let s = section(&[
            ("sso_session", "corp"),
            ("sso_account_id", "123456789012"),
            ("sso_role_name", "Admin"),
        ]);
        let sso = sso_config_for(&config, &s).expect("should build");
        assert_eq!(sso.session_name.as_deref(), Some("corp"));
        assert_eq!(sso.sso_region, "us-east-1");
        assert_eq!(sso.account_id, "123456789012");
    }

    #[test]
    fn builds_sso_config_from_legacy_inline_form() {
        let config = Config::default();
        let s = section(&[
            ("sso_start_url", "https://example.awsapps.com/start"),
            ("sso_region", "eu-west-1"),
            ("sso_account_id", "1"),
            ("sso_role_name", "R"),
        ]);
        let sso = sso_config_for(&config, &s).expect("should build");
        assert!(sso.session_name.is_none());
        assert_eq!(sso.sso_region, "eu-west-1");
    }

    #[test]
    fn incomplete_sso_profile_is_not_treated_as_sso() {
        let config = Config::default();
        // Missing sso_role_name.
        let s = section(&[("sso_session", "corp"), ("sso_account_id", "1")]);
        assert!(sso_config_for(&config, &s).is_none());
    }

    #[test]
    fn web_identity_is_distinguished_from_plain_assume_role() {
        let mut config = Config::default();
        config.config_profiles.insert(
            "p".into(),
            section(&[
                ("role_arn", "arn:aws:iam::1:role/r"),
                ("web_identity_token_file", "/tmp/token"),
            ]),
        );
        // The web-identity branch reads the token file first, so a missing file proves
        // that path was taken rather than the source_profile one.
        match from_profile(&config, "p", Some("us-east-1"), &mut BTreeSet::new()) {
            Err(CredentialError::InvalidConfig { message, .. }) => {
                assert!(message.contains("web_identity_token_file"), "got: {message}")
            }
            other => panic!("expected the web-identity path, got {other:?}"),
        }
    }
}
