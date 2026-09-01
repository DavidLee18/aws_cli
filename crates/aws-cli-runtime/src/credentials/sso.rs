//! The AWS SSO (IAM Identity Center) credential provider.
//!
//! Two steps: read the bearer token the `aws sso login` flow cached on disk, then
//! exchange it for temporary credentials via the SSO portal.
//!
//! A token inside the 15-minute refresh window is renewed in place via
//! `sso-oidc:CreateToken` and written back to the cache.
//!
//! The cache is shared with the reference CLI in both directions: `aws sso login`
//! produces a token this provider uses, and a refresh performed here is picked up by the
//! reference. Performing the initial login (OIDC device / PKCE authorization) is out of
//! scope — a token that cannot be refreshed is reported with the reference's own wording.

use super::{CredentialError, Credentials};
use serde::Deserialize;
use sha1::{Digest, Sha1};
use std::path::PathBuf;

/// The cached token document written by the login flow.
///
/// Field names are the reference's; only `accessToken`/`expiresAt` are required here.
/// `refreshToken` is present for sessions registered with the newer scopes, but using it
/// requires the OIDC `CreateToken` call that only the login flow performs.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
struct CachedToken {
    #[serde(rename = "accessToken", skip_serializing_if = "Option::is_none")]
    access_token: Option<String>,
    #[serde(rename = "expiresAt", skip_serializing_if = "Option::is_none")]
    expires_at: Option<String>,
    #[serde(rename = "startUrl", skip_serializing_if = "Option::is_none")]
    start_url: Option<String>,
    #[serde(rename = "region", skip_serializing_if = "Option::is_none")]
    region: Option<String>,
    // Refresh inputs. All four must be present, and the registration unexpired, for a
    // refresh to be attempted.
    #[serde(rename = "refreshToken", skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    #[serde(rename = "clientId", skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
    #[serde(rename = "clientSecret", skip_serializing_if = "Option::is_none")]
    client_secret: Option<String>,
    #[serde(rename = "registrationExpiresAt", skip_serializing_if = "Option::is_none")]
    registration_expires_at: Option<String>,
}

/// Refresh when under this much validity remains, matching botocore's window.
const REFRESH_WINDOW_SECONDS: i64 = 15 * 60;

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
    // The role credentials are cached, not just the bearer token. Without this every
    // invocation calls GetRoleCredentials against the *SSO* region, which is often not the
    // region the command works in -- measured from Korea against a us-east-1 SSO endpoint,
    // that was about a second of dead time in front of every command.
    let key = role_cache_key(config);
    if let Some(mut cached) = super::cache::read(&key) {
        cached.method = "sso";
        return Ok(cached);
    }
    let token = load_token(config, profile)?;
    let credentials = fetch_role_credentials(config, &token, profile)?;
    super::cache::write(&key, &credentials, Some("sso"));
    Ok(credentials)
}

/// botocore's SSO cache key: `sha1` of the arguments as *minified* JSON with sorted keys.
///
/// Note the separators differ from the assume-role fetcher, which uses Python's default
/// `", "`/`": "` spacing. botocore's own source calls this out as an inconsistency it
/// cannot fix without invalidating existing caches. Matching it exactly is what lets
/// `aws` and `awsc` share credentials rather than each re-fetching.
fn role_cache_key(config: &SsoConfig) -> String {
    use sha1::{Digest, Sha1};
    // Sorted keys: accountId, roleName, then sessionName or startUrl.
    let mut entries = vec![
        format!("{}:{}", json_string("accountId"), json_string(&config.account_id)),
        format!("{}:{}", json_string("roleName"), json_string(&config.role_name)),
    ];
    match &config.session_name {
        Some(name) => {
            entries.push(format!("{}:{}", json_string("sessionName"), json_string(name)))
        }
        None => entries
            .push(format!("{}:{}", json_string("startUrl"), json_string(&config.start_url))),
    }
    let serialized = format!("{{{}}}", entries.join(","));
    let digest: String =
        Sha1::digest(serialized.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    digest.replace([':', '/'], "_")
}

fn json_string(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
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

    // The specific reason is preserved in `detail` for --debug even though the
    // user-facing message is the reference's fixed wording.
    let expired = |detail: &str| CredentialError::SsoTokenExpired {
        profile: profile.to_string(),
        detail: detail.to_string(),
    };

    let bytes = std::fs::read(&path).map_err(|_| expired("no cached token was found"))?;
    let token: CachedToken =
        serde_json::from_slice(&bytes).map_err(|e| CredentialError::Sso {
            profile: profile.to_string(),
            message: format!("cached token at {} is unreadable: {e}", path.display()),
        })?;

    if token.access_token.is_none() {
        // Registration-only cache entries have no accessToken; they are not a token.
        return Err(expired("the cached entry holds no access token"));
    }

    // The cache is keyed by session, so a mismatched startUrl means stale state rather
    // than a usable token.
    if let (Some(cached_url), false) = (&token.start_url, config.start_url.is_empty()) {
        if cached_url != &config.start_url {
            return Err(expired("the cached token is for a different start URL"));
        }
    }

    let remaining = token.expires_at.as_deref().and_then(parse_rfc3339).map(|e| e - now_unix());

    // Still comfortably valid.
    if remaining.is_none_or(|r| r > REFRESH_WINDOW_SECONDS) {
        return Ok(token.access_token.clone().unwrap_or_default());
    }

    // Inside the refresh window: try to renew before giving up.
    match refresh(&token, config) {
        Some(refreshed) => {
            write_token(config, &refreshed);
            Ok(refreshed.access_token.unwrap_or_default())
        }
        // Refresh unavailable or failed. A token that has not actually expired is still
        // usable; one that has is not.
        None if remaining.is_some_and(|r| r > 0) => {
            Ok(token.access_token.clone().unwrap_or_default())
        }
        None => Err(expired("the cached token has expired and could not be refreshed")),
    }
}

/// Exchange a refresh token for a new access token via `sso-oidc:CreateToken`.
///
/// Returns `None` — rather than an error — when refresh is not possible, so the caller
/// can fall back to the existing token or report expiry. Unsigned, against `sso_region`.
fn refresh(token: &CachedToken, config: &SsoConfig) -> Option<CachedToken> {
    let refresh_token = token.refresh_token.as_ref()?;
    let client_id = token.client_id.as_ref()?;
    let client_secret = token.client_secret.as_ref()?;

    // A lapsed client registration cannot be used to refresh.
    if let Some(expiry) = token.registration_expires_at.as_deref() {
        if is_expired(expiry) {
            return None;
        }
    }

    let body = serde_json::json!({
        "grantType": "refresh_token",
        "clientId": client_id,
        "clientSecret": client_secret,
        "refreshToken": refresh_token,
    });

    let response = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .post(&format!("https://oidc.{}.amazonaws.com/token", config.sso_region))
        .set("content-type", "application/json")
        .set("user-agent", &crate::http::user_agent())
        .send_json(body)
        .ok()?;

    #[derive(Deserialize)]
    struct CreateTokenResponse {
        #[serde(rename = "accessToken")]
        access_token: String,
        #[serde(rename = "expiresIn")]
        expires_in: i64,
        #[serde(rename = "refreshToken")]
        refresh_token: Option<String>,
    }
    let parsed: CreateTokenResponse = response.into_json().ok()?;

    let mut refreshed = token.clone();
    refreshed.access_token = Some(parsed.access_token);
    refreshed.expires_at = Some(format_rfc3339(now_unix() + parsed.expires_in));
    if parsed.refresh_token.is_some() {
        refreshed.refresh_token = parsed.refresh_token;
    }
    Some(refreshed)
}

/// Write the refreshed token back, so the reference CLI benefits from it too.
fn write_token(config: &SsoConfig, token: &CachedToken) {
    let Some(dir) = cache_dir() else { return };
    let path = dir.join(format!("{}.json", cache_key(config)));
    let Ok(text) = serde_json::to_string(token) else { return };
    // A cache write failing must not fail the command.
    if std::fs::write(&path, text).is_ok() {
        restrict_permissions(&path);
    }
}

#[cfg(unix)]
fn restrict_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &std::path::Path) {}

/// The `%Y-%m-%dT%H:%M:%SZ` form the cache uses.
fn format_rfc3339(unix: i64) -> String {
    let compact = crate::sigv4::format_timestamp(unix); // YYYYMMDDTHHMMSSZ
    format!(
        "{}-{}-{}T{}:{}:{}Z",
        &compact[0..4],
        &compact[4..6],
        &compact[6..8],
        &compact[9..11],
        &compact[11..13],
        &compact[13..15],
    )
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
        // The portal rejecting the token is botocore's UnauthorizedSSOTokenError, a
        // different message from a locally-detected expiry.
        Err(ureq::Error::Status(401 | 403, _)) => {
            return Err(CredentialError::SsoUnauthorized { profile: profile.to_string() })
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
        method: "sso",
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

    /// Against values computed with botocore's own algorithm:
    ///
    ///   json.dumps({'roleName':..,'accountId':..,'sessionName':..},
    ///              sort_keys=True, separators=(',', ':'))  -> sha1 hexdigest
    ///
    /// A mismatch here does not fail anything visibly -- it just means `aws` and `awsc`
    /// stop sharing the cache and each re-fetch credentials, which looks like nothing more
    /// than being mysteriously slow.
    #[test]
    fn role_cache_key_matches_botocore() {
        let session = SsoConfig {
            session_name: Some("amplify-admin".into()),
            start_url: "https://d-9067c25df2.awsapps.com/start".into(),
            sso_region: "us-east-1".into(),
            account_id: "147475613246".into(),
            role_name: "amplify-admin".into(),
        };
        assert_eq!(role_cache_key(&session), "fa3955c26a3285bb71b32e8b0262014665c0ccad");

        // The legacy inline form keys on the start URL instead, and the session name must
        // not leak into the digest.
        let legacy = SsoConfig { session_name: None, ..session };
        assert_eq!(role_cache_key(&legacy), "cd609e275dc9a0b9b9579d21f3d3db11d977dccc");
    }
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
