//! Credential resolution.
//!
//! Mirrors botocore's provider chain: environment variables first, then the selected
//! profile's mechanism (static keys, SSO, `credential_process`, assume-role), then the
//! ambient providers (container, then IMDS).
//!
//! Mechanisms that are not implemented are reported *by name* rather than skipped. A
//! chain that silently falls through to the next provider can authenticate as the wrong
//! identity, which is far worse than a clear error.

pub mod imds;
pub mod process;
pub mod profile;
pub mod sso;

use profile::{Config, Section};

#[derive(Debug, Clone)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    /// RFC 3339 or the sigv4 compact form; informational only today.
    pub expires_at: Option<String>,
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
    /// The reference's own wording for the case users hit most, plus the specific reason
    /// (the reference gives none, and "which of the eleven cache files was stale" is
    /// exactly what you want to know).
    #[error(
        "The SSO session associated with this profile has expired or is otherwise \
         invalid. To refresh this SSO session run `aws sso login --profile {profile}`. \
         ({detail})"
    )]
    SsoExpired { profile: String, detail: String },
    #[error("profile `{profile}` uses {mechanism}, which is not implemented yet")]
    Unsupported { profile: String, mechanism: &'static str },
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
}

/// Resolve credentials for the given profile.
///
/// Chain order follows botocore's `create_credential_resolver`; see [`from_profile`] for
/// the within-profile ordering, which is *not* the intuitive one.
pub fn resolve(explicit_profile: Option<&str>) -> Result<Credentials, CredentialError> {
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
        return from_profile(&config, &name);
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
fn from_profile(config: &Config, name: &str) -> Result<Credentials, CredentialError> {
    let merged = config.profile(name).unwrap_or_default();

    // 1 & 2: assume-role variants, before anything else.
    if merged.contains_key("role_arn") {
        let mechanism = if merged.contains_key("web_identity_token_file") {
            "web-identity assume-role"
        } else {
            "assume-role"
        };
        return Err(CredentialError::Unsupported { profile: name.to_string(), mechanism });
    }

    // 3: SSO.
    if let Some(sso_config) = sso_config_for(config, &merged) {
        return sso::resolve(&sso_config, name);
    }

    // 4: static keys from the credentials file.
    if let Some(creds) = static_keys(config.credentials_profiles.get(name)) {
        return Ok(creds);
    }

    // 5: credential_process.
    if let Some(command) = merged.get("credential_process") {
        return process::resolve(command, name);
    }

    // 6: static keys from the config file.
    if let Some(creds) = static_keys(config.config_profiles.get(name)) {
        return Ok(creds);
    }

    // The profile exists but declares no credential mechanism (a region-only profile is
    // common); fall through to the ambient providers rather than failing outright.
    from_ambient()
}

/// Static keys from one section. The access key alone triggers the provider; a missing
/// secret is an error rather than a skip, matching botocore's `PartialCredentialsError`.
fn static_keys(section: Option<&Section>) -> Option<Credentials> {
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
    let config = Config::load().ok()?;
    let name = profile::profile_name(explicit_profile);
    config.profile(&name)?.get("region").cloned()
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

        assert_eq!(from_profile(&config, "p").unwrap().access_key_id, "AKIA");
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
        match from_profile(&config, "p") {
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

        match from_profile(&config, "p") {
            Err(CredentialError::Unsupported { mechanism, .. }) => {
                assert_eq!(mechanism, "assume-role")
            }
            other => panic!("assume-role should take precedence, got {other:?}"),
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
        assert_eq!(static_keys(Some(&s)).unwrap().session_token.as_deref(), Some("legacy"));
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
        match from_profile(&config, "p") {
            Err(CredentialError::Unsupported { mechanism, .. }) => {
                assert_eq!(mechanism, "web-identity assume-role")
            }
            other => panic!("expected an explicit refusal, got {other:?}"),
        }
    }
}
