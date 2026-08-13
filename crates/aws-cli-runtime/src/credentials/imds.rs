//! Ambient credential providers: ECS/EKS container roles and EC2 instance metadata.
//!
//! Both return `Ok(None)` when they simply do not apply (not running on EC2, no
//! container env vars), so the chain can move on; `Err` is reserved for a provider that
//! should have worked and did not.

use super::{CredentialError, Credentials};
use serde::Deserialize;
use std::time::Duration;

/// The link-local address container credentials are served from when the relative-URI
/// form is used.
const CONTAINER_HOST: &str = "http://169.254.170.2";
const IMDS_HOST: &str = "http://169.254.169.254";

/// Short by design: these endpoints are link-local, so a slow response means "not here"
/// rather than "be patient", and the CLI should not hang on a laptop.
const TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct MetadataCredentials {
    access_key_id: String,
    secret_access_key: String,
    token: Option<String>,
    expiration: Option<String>,
}

impl From<MetadataCredentials> for Credentials {
    fn from(m: MetadataCredentials) -> Self {
        Credentials {
            access_key_id: m.access_key_id,
            secret_access_key: m.secret_access_key,
            session_token: m.token,
            expires_at: m.expiration,
        }
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout_connect(TIMEOUT).timeout(TIMEOUT).build()
}

/// ECS/EKS container credentials.
pub fn from_container() -> Result<Option<Credentials>, CredentialError> {
    // RELATIVE takes precedence over FULL when both are set (botocore
    // ContainerProvider._provided_relative_uri).
    let url = match (
        std::env::var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI").ok().filter(|s| !s.is_empty()),
        std::env::var("AWS_CONTAINER_CREDENTIALS_FULL_URI").ok().filter(|s| !s.is_empty()),
    ) {
        (Some(relative), _) => format!("{CONTAINER_HOST}{relative}"),
        (None, Some(full)) => {
            // FULL_URI can point anywhere, so it is allowlisted: https, loopback, or one
            // of the fixed link-local addresses. Without this a stray env var could ship
            // the container's credentials to an arbitrary host.
            if !is_allowed_full_uri(&full) {
                return Err(CredentialError::Process {
                    profile: "container".into(),
                    message: format!(
                        "AWS_CONTAINER_CREDENTIALS_FULL_URI host is not allowed: {full}"
                    ),
                });
            }
            full
        }
        (None, None) => return Ok(None),
    };

    let mut request = agent().get(&url);
    if let Some(token) = container_auth_token() {
        request = request.set("authorization", &token);
    }

    match request.call() {
        Ok(response) => {
            let body = response.into_string().map_err(|e| CredentialError::Sso {
                profile: "container".into(),
                message: e.to_string(),
            })?;
            match serde_json::from_str::<MetadataCredentials>(&body) {
                Ok(c) => Ok(Some(c.into())),
                Err(e) => Err(CredentialError::Process {
                    profile: "container".into(),
                    message: format!("container credential response was unreadable: {e}"),
                }),
            }
        }
        // The env var was set but the endpoint failed: that is a real error, not a
        // "provider does not apply", because the caller clearly expected it to work.
        Err(e) => Err(CredentialError::Process {
            profile: "container".into(),
            message: format!("container credential endpoint failed: {e}"),
        }),
    }
}

/// `https` anywhere, or plain http only to loopback / the fixed ECS addresses.
fn is_allowed_full_uri(url: &str) -> bool {
    const ALLOWED_HOSTS: &[&str] =
        &["169.254.170.2", "169.254.170.23", "fd00:ec2::23", "localhost"];

    let Some((scheme, rest)) = url.split_once("://") else { return false };
    if scheme.eq_ignore_ascii_case("https") {
        return true;
    }
    if !scheme.eq_ignore_ascii_case("http") {
        return false;
    }

    let authority = rest.split('/').next().unwrap_or_default();
    // Strip a port, taking care not to mangle bracketed IPv6.
    let host = if let Some(end) = authority.strip_prefix('[').and_then(|r| r.split_once(']')) {
        end.0
    } else {
        authority.split(':').next().unwrap_or_default()
    };

    ALLOWED_HOSTS.iter().any(|h| host.eq_ignore_ascii_case(h)) || is_loopback(host)
}

fn is_loopback(host: &str) -> bool {
    if host == "::1" {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => false,
    }
}

fn container_auth_token() -> Option<String> {
    if let Ok(path) = std::env::var("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE") {
        if let Ok(token) = std::fs::read_to_string(&path) {
            return Some(token.trim().to_string());
        }
    }
    std::env::var("AWS_CONTAINER_AUTHORIZATION_TOKEN").ok().filter(|s| !s.is_empty())
}

/// EC2 instance metadata, IMDSv2.
///
/// Returns `Ok(None)` whenever the service is absent or disabled — the common case off
/// EC2 — so the chain reports "unable to locate credentials" rather than a confusing
/// network error.
pub fn from_instance_metadata() -> Result<Option<Credentials>, CredentialError> {
    if std::env::var("AWS_EC2_METADATA_DISABLED").is_ok_and(|v| v.eq_ignore_ascii_case("true")) {
        return Ok(None);
    }
    let base = std::env::var("AWS_EC2_METADATA_SERVICE_ENDPOINT")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| IMDS_HOST.to_string());
    let base = base.trim_end_matches('/').to_string();

    // IMDSv2: obtain a session token first. Failure here means no IMDS reachable.
    let Ok(token_response) = agent()
        .put(&format!("{base}/latest/api/token"))
        .set("x-aws-ec2-metadata-token-ttl-seconds", "21600")
        .send_string("")
    else {
        return Ok(None);
    };
    let Ok(token) = token_response.into_string() else { return Ok(None) };

    let Ok(role_response) = agent()
        .get(&format!("{base}/latest/meta-data/iam/security-credentials/"))
        .set("x-aws-ec2-metadata-token", &token)
        .call()
    else {
        return Ok(None);
    };
    let Ok(roles) = role_response.into_string() else { return Ok(None) };
    let Some(role) = roles.lines().next().map(str::trim).filter(|r| !r.is_empty()) else {
        return Ok(None);
    };

    let Ok(creds_response) = agent()
        .get(&format!("{base}/latest/meta-data/iam/security-credentials/{role}"))
        .set("x-aws-ec2-metadata-token", &token)
        .call()
    else {
        return Ok(None);
    };
    let Ok(body) = creds_response.into_string() else { return Ok(None) };

    // Reaching this point means IMDS answered, so a malformed body IS an error.
    serde_json::from_str::<MetadataCredentials>(&body)
        .map(|c| Some(c.into()))
        .map_err(|e| CredentialError::Process {
            profile: "instance-metadata".into(),
            message: format!("instance metadata response was unreadable: {e}"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_metadata_credential_documents() {
        let body = r#"{"Code":"Success","AccessKeyId":"ASIA","SecretAccessKey":"s",
            "Token":"t","Expiration":"2030-01-01T00:00:00Z"}"#;
        let c: Credentials = serde_json::from_str::<MetadataCredentials>(body).unwrap().into();
        assert_eq!(c.access_key_id, "ASIA");
        assert_eq!(c.session_token.as_deref(), Some("t"));
        assert_eq!(c.expires_at.as_deref(), Some("2030-01-01T00:00:00Z"));
    }

    /// With no container env vars set the provider must decline, not error, so the chain
    /// continues to IMDS.
    #[test]
    fn container_declines_when_not_configured() {
        // Only meaningful when the ambient environment is clean, which it is in CI and
        // on a developer laptop.
        if std::env::var("AWS_CONTAINER_CREDENTIALS_FULL_URI").is_ok()
            || std::env::var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI").is_ok()
        {
            return;
        }
        assert!(from_container().unwrap().is_none());
    }

    #[test]
    fn full_uri_allowlist_matches_botocore() {
        // https anywhere.
        assert!(is_allowed_full_uri("https://example.com/creds"));
        // http only to loopback or the fixed ECS addresses.
        assert!(is_allowed_full_uri("http://169.254.170.2/v2/credentials"));
        assert!(is_allowed_full_uri("http://169.254.170.23/creds"));
        assert!(is_allowed_full_uri("http://localhost:8080/creds"));
        assert!(is_allowed_full_uri("http://127.0.0.1/creds"));
        assert!(is_allowed_full_uri("http://[::1]:9000/creds"));
        assert!(is_allowed_full_uri("http://[fd00:ec2::23]/creds"));
        // Anything else over plain http would leak credentials off-box.
        assert!(!is_allowed_full_uri("http://evil.example.com/creds"));
        assert!(!is_allowed_full_uri("http://169.254.169.254/creds"));
        assert!(!is_allowed_full_uri("ftp://localhost/creds"));
        assert!(!is_allowed_full_uri("not-a-url"));
    }
}
