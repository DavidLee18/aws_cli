//! `aws sso login` and `aws sso logout`.
//!
//! Custom commands on the modelled `sso` service: neither corresponds to an operation on
//! it, so both are dispatched before the model is consulted. The work itself lives in
//! [`aws_cli_runtime::credentials::sso_login`], next to the provider that consumes what
//! they produce.

use crate::args::{Arity, Parsed};
use crate::exit;
use crate::Failure;
use aws_cli_runtime::credentials::profile::Config;
use aws_cli_runtime::credentials::sso_login::{self, LoginRequest};
use aws_cli_runtime::RuntimeError;
use std::process::ExitCode;

/// `sso login`'s flags. `--sso-session` takes a value; the other two are switches.
pub fn flag_arity(flag: &str) -> Arity {
    match flag {
        "--sso-session" => Arity::One,
        _ => Arity::None,
    }
}

pub fn login(parsed: &Parsed) -> Result<ExitCode, Failure> {
    let config = Config::load().map_err(|e| Failure::new(exit::CONFIGURATION, e))?;
    let profile = aws_cli_runtime::credentials::profile::profile_name(parsed.profile.as_deref());
    let explicit_session = parsed.parameters.get("--sso-session").and_then(Clone::clone);

    let request = build_request(&config, &profile, explicit_session, parsed)?;
    let start_url = request.start_url.clone();

    sso_login::device_login(&request)
        .map_err(|e| Failure::new(exit::CONFIGURATION, RuntimeError::Configuration(e.to_string())))?;

    println!("Successfully logged into Start URL: {start_url}");
    Ok(exit::code(exit::SUCCESS))
}

pub fn logout(_parsed: &Parsed) -> Result<ExitCode, Failure> {
    sso_login::logout();
    Ok(exit::code(exit::SUCCESS))
}

/// Resolve where to log in to: an explicit `--sso-session`, else the profile's
/// `sso_session`, else the legacy keys written directly into the profile.
fn build_request(
    config: &Config,
    profile: &str,
    explicit_session: Option<String>,
    parsed: &Parsed,
) -> Result<LoginRequest, Failure> {
    let no_browser = parsed.parameters.contains_key("--no-browser");
    let scoped = config.profile(profile).unwrap_or_default();
    let session_name = explicit_session.or_else(|| scoped.get("sso_session").cloned());

    let (source, session_name) = match session_name {
        Some(name) => {
            let session = config.sso_sessions.get(&name).ok_or_else(|| {
                Failure::new(
                    exit::CONFIGURATION,
                    aws_cli_runtime::RuntimeError::Configuration(format!(
                        "The specified sso-session does not exist: \"{name}\""
                    )),
                )
            })?;
            (session.clone(), Some(name))
        }
        None => (scoped, None),
    };

    // Both are required, and the reference names *all* the missing ones rather than
    // stopping at the first -- someone fixing a config wants the whole list.
    let missing: Vec<&str> = ["sso_start_url", "sso_region"]
        .into_iter()
        .filter(|key| !source.contains_key(*key))
        .collect();
    if !missing.is_empty() {
        // The legacy form gets an extra sentence, because `aws configure sso` is what
        // writes it; a broken sso-session is edited by hand.
        let hint = match session_name {
            Some(_) => String::new(),
            None => " To make sure this profile is properly configured to use SSO, \
                     please run: aws configure sso"
                .to_string(),
        };
        return Err(Failure::new(
            exit::CONFIGURATION,
            aws_cli_runtime::RuntimeError::Configuration(format!(
                "Missing the following required SSO configuration values: {}.{hint}",
                missing.join(", ")
            )),
        ));
    }

    Ok(LoginRequest {
        start_url: source["sso_start_url"].clone(),
        sso_region: source["sso_region"].clone(),
        session_name,
        scopes: source
            .get("sso_registration_scopes")
            .map(|raw| parse_scopes(raw))
            .unwrap_or_default(),
        no_browser,
    })
}

/// `sso_registration_scopes` is a comma-separated list, trimmed, with empties dropped.
fn parse_scopes(raw: &str) -> Vec<String> {
    raw.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_are_split_and_trimmed() {
        assert_eq!(
            parse_scopes("sso:account:access, codewhisperer:completions ,"),
            vec!["sso:account:access", "codewhisperer:completions"]
        );
        assert!(parse_scopes("  ").is_empty());
    }
}
