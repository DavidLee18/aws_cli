//! The AWS SSO (IAM Identity Center) credential provider.
//!
//! Two steps: read the bearer token the `aws sso login` flow cached on disk, then
//! exchange it for temporary credentials via the SSO portal.
//!
//! The token cache is shared with the reference CLI, so `aws sso login` refreshes a
//! token this provider can then use. Performing the login flow itself (OIDC device
//! authorization) is out of scope — an expired token is reported with the same
//! instruction the reference gives.

use super::{CredentialError, Credentials};
use serde::Deserialize;
use sha1::{Digest, Sha1};
use std::path::PathBuf;

/// The cached token document written by the login flow.
///
/// Field names are the reference's; only `accessToken`/`expiresAt` are required here.
/// `refreshToken` is present for sessions registered with the newer scopes, but using it
/// requires the OIDC `CreateToken` call that only the login flow performs.
#[derive(Debug, Deserialize)]
struct CachedToken {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    #[serde(rename = "expiresAt")]
    expires_at: Option<String>,
    #[serde(rename = "startUrl")]
    start_url: Option<String>,
    #[serde(rename = "region")]
    region: Option<String>,
}

/// How a profile identifies its SSO configuration.
pub struct SsoConfig {
    /// `[sso-session NAME]` for the modern form; `None` for the legacy inline form.
    pub session_name: Option<String>,
    pub start_url: String,
    pub sso_region: String,
    pub account_id: String,
    pub role_name: String,
}

/// Resolve SSO credentials for a profile.
pub fn resolve(config: &SsoConfig, profile: &str) -> Result<Credentials, CredentialError> {
    let token = load_token(config, profile)?;
    fetch_role_credentials(config, &token, profile)
}

/// The cache key is the SSO session name for the modern form, or the start URL for the
/// legacy inline form — hashed with SHA-1 and hex encoded.
fn cache_key(config: &SsoConfig) -> String {
    let input = match &config.session_name {
        Some(name) => name.clone(),
        None => config.start_url.clone(),
    };
    let digest = Sha1::digest(input.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn cache_dir() -> Option<PathBuf> {
    super::profile::home().map(|h| h.join(".aws/sso/cache"))
}

fn load_token(config: &SsoConfig, profile: &str) -> Result<String, CredentialError> {
    let dir = cache_dir().ok_or_else(|| CredentialError::Sso {
        profile: profile.to_string(),
        message: "cannot locate ~/.aws/sso/cache (HOME is unset)".to_string(),
    })?;
    let path = dir.join(format!("{}.json", cache_key(config)));

    let expired = |detail: &str| CredentialError::SsoExpired {
        profile: profile.to_string(),
        detail: detail.to_string(),
    };

    let bytes = std::fs::read(&path).map_err(|_| expired("no cached token was found"))?;
    let token: CachedToken =
        serde_json::from_slice(&bytes).map_err(|e| CredentialError::Sso {
            profile: profile.to_string(),
            message: format!("cached token at {} is unreadable: {e}", path.display()),
        })?;

    let Some(access_token) = token.access_token else {
        // Registration-only cache entries have no accessToken; they are not a token.
        return Err(expired("the cached entry holds no access token"));
    };

    // The cache is keyed by session, so a mismatched startUrl means stale state rather
    // than a usable token.
    if let (Some(cached_url), false) = (&token.start_url, config.start_url.is_empty()) {
        if cached_url != &config.start_url {
            return Err(expired("the cached token is for a different start URL"));
        }
    }
    if let Some(expires_at) = &token.expires_at {
        if is_expired(expires_at) {
            return Err(expired("the cached token has expired"));
        }
    }
    let _ = token.region;
    Ok(access_token)
}

/// Exchange the bearer token for role credentials.
///
/// `GetRoleCredentials` on the SSO portal is unsigned — the bearer token in the
/// `x-amz-sso_bearer_token` header is the entire credential — so it needs no sigv4 and
/// no bootstrap credentials.
fn fetch_role_credentials(
    config: &SsoConfig,
    token: &str,
    profile: &str,
) -> Result<Credentials, CredentialError> {
    let url = format!(
        "https://portal.sso.{}.amazonaws.com/federation/credentials?account_id={}&role_name={}",
        config.sso_region,
        urlencode(&config.account_id),
        urlencode(&config.role_name),
    );

    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(30))
        .build();

    let response = agent
        .get(&url)
        .set("x-amz-sso_bearer_token", token)
        .set("user-agent", &crate::http::user_agent())
        .call();

    let body = match response {
        Ok(r) => r.into_string().map_err(|e| CredentialError::Sso {
            profile: profile.to_string(),
            message: e.to_string(),
        })?,
        // 401/403 means the token is no longer accepted, whatever the cache said.
        Err(ureq::Error::Status(401 | 403, _)) => {
            return Err(CredentialError::SsoExpired {
                profile: profile.to_string(),
                detail: "the SSO portal rejected the cached token".to_string(),
            })
        }
        Err(ureq::Error::Status(status, r)) => {
            let detail = r.into_string().unwrap_or_default();
            return Err(CredentialError::Sso {
                profile: profile.to_string(),
                message: format!("SSO portal returned {status}: {detail}"),
            });
        }
        Err(e) => {
            return Err(CredentialError::Sso {
                profile: profile.to_string(),
                message: e.to_string(),
            })
        }
    };

    parse_role_credentials(&body).ok_or_else(|| CredentialError::Sso {
        profile: profile.to_string(),
        message: "SSO portal response did not contain roleCredentials".to_string(),
    })
}

#[derive(Debug, Deserialize)]
struct RoleCredentialsEnvelope {
    #[serde(rename = "roleCredentials")]
    role_credentials: RoleCredentials,
}

#[derive(Debug, Deserialize)]
struct RoleCredentials {
    #[serde(rename = "accessKeyId")]
    access_key_id: String,
    #[serde(rename = "secretAccessKey")]
    secret_access_key: String,
    #[serde(rename = "sessionToken")]
    session_token: Option<String>,
    /// Milliseconds since the epoch, unlike every other expiry in the CLI.
    #[serde(rename = "expiration")]
    expiration: Option<i64>,
}

fn parse_role_credentials(body: &str) -> Option<Credentials> {
    let envelope: RoleCredentialsEnvelope = serde_json::from_str(body).ok()?;
    let c = envelope.role_credentials;
    Some(Credentials {
        access_key_id: c.access_key_id,
        secret_access_key: c.secret_access_key,
        session_token: c.session_token,
        expires_at: c.expiration.map(|ms| crate::sigv4::format_timestamp(ms / 1000)),
    })
}

/// Whether an RFC 3339 / ISO 8601 instant is in the past.
///
/// Unparseable values are treated as NOT expired: the portal is the authority, and
/// wrongly discarding a good token would be worse than one rejected round trip.
fn is_expired(timestamp: &str) -> bool {
    match parse_rfc3339(timestamp) {
        Some(unix) => unix <= now_unix(),
        None => false,
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Parse the subset of RFC 3339 these caches use: `YYYY-MM-DDTHH:MM:SS[.fff][Z|±HH:MM]`.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let num = |range: std::ops::Range<usize>| s.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);

    let days = days_from_civil(year, month, day);
    let mut unix = days * 86_400 + hour * 3600 + minute * 60 + second;

    // Apply a numeric offset if present; `Z` and a missing offset both mean UTC.
    let rest = &s[19..];
    if let Some(idx) = rest.find(['+', '-']) {
        let sign = if rest.as_bytes()[idx] == b'+' { -1 } else { 1 };
        let off = &rest[idx + 1..];
        if off.len() >= 5 {
            let oh = off.get(0..2)?.parse::<i64>().ok()?;
            let om = off.get(3..5)?.parse::<i64>().ok()?;
            unix += sign * (oh * 3600 + om * 60);
        }
    }
    Some(unix)
}

/// Howard Hinnant's `days_from_civil`.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn urlencode(s: &str) -> String {
    const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.~";
    s.bytes()
        .map(|b| {
            if UNRESERVED.contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cache is shared with the reference CLI, so this derivation has to match it
    /// exactly. Verified against a real `~/.aws/sso/cache` directory: for every
    /// configured `[sso-session NAME]`, `sha1(NAME)` was the on-disk filename, and
    /// `sha1(start_url)` was not.
    #[test]
    fn cache_key_hashes_session_name_then_start_url() {
        let modern = SsoConfig {
            session_name: Some("my-sso".into()),
            start_url: "https://example.awsapps.com/start".into(),
            sso_region: "us-east-1".into(),
            account_id: "1".into(),
            role_name: "r".into(),
        };
        let modern_key = cache_key(&modern);
        assert_eq!(modern_key, "0ad374308c5a4e22f723adf10145eafad7c4031c");

        // The legacy inline form has no session, so it keys on the start URL instead.
        let legacy = SsoConfig { session_name: None, ..modern };
        assert_eq!(cache_key(&legacy).len(), 40);
        assert_ne!(cache_key(&legacy), modern_key);
    }

    #[test]
    fn parses_rfc3339_variants() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2026-08-13T21:48:16Z"), Some(1_786_657_696));
        // Fractional seconds are ignored, offsets are applied.
        assert_eq!(parse_rfc3339("2026-08-13T21:48:16.123Z"), Some(1_786_657_696));
        assert_eq!(
            parse_rfc3339("2026-08-13T22:48:16+01:00"),
            Some(1_786_657_696),
            "a +01:00 offset is one hour behind the same wall clock in UTC"
        );
        assert_eq!(parse_rfc3339("nonsense"), None);
    }

    #[test]
    fn unparseable_expiry_is_not_treated_as_expired() {
        assert!(!is_expired("not-a-date"));
        assert!(is_expired("2000-01-01T00:00:00Z"));
        assert!(!is_expired("2099-01-01T00:00:00Z"));
    }

    #[test]
    fn parses_portal_response() {
        let body = r#"{"roleCredentials":{"accessKeyId":"ASIA","secretAccessKey":"s",
            "sessionToken":"t","expiration":1786657696000}}"#;
        let c = parse_role_credentials(body).unwrap();
        assert_eq!(c.access_key_id, "ASIA");
        assert_eq!(c.session_token.as_deref(), Some("t"));
        // Milliseconds are converted to the sigv4 timestamp format.
        assert_eq!(c.expires_at.as_deref(), Some("20260813T214816Z"));
    }

    #[test]
    fn rejects_unexpected_portal_body() {
        assert!(parse_role_credentials(r#"{"message":"Forbidden"}"#).is_none());
    }
}
