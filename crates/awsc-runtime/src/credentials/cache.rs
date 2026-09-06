//! The on-disk credential cache at `~/.aws/cli/cache`, shared with the reference CLI.
//!
//! Both the assume-role and SSO providers cache here, and both formerly would have needed
//! their own copy of the read/write/expiry logic. They differ only in how the cache *key*
//! is derived — botocore uses different JSON separators for the two, a documented
//! inconsistency in its own source — so the key stays with each provider and everything
//! else lives here.
//!
//! Caching matters more than it looks. Without it an SSO profile calls
//! `GetRoleCredentials` on every single invocation, and that call goes to the SSO region
//! rather than the one you are working in: measured from Korea against a us-east-1 SSO
//! endpoint, it put roughly a second in front of every command before any real work
//! started.

use super::Credentials;
use crate::credentials::sso::parse_rfc3339;
use std::path::PathBuf;

/// botocore treats an entry with less than this left on it as expired.
const EXPIRY_WINDOW_SECONDS: i64 = 15 * 60;

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn dir() -> Option<PathBuf> {
    super::profile::home().map(|h| h.join(".aws/cli/cache"))
}

/// Load cached credentials, or `None` if absent, unreadable, or too close to expiry.
pub fn read(key: &str) -> Option<Credentials> {
    let path = dir()?.join(format!("{key}.json"));
    let bytes = std::fs::read(path).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let c = value.get("Credentials")?;

    let expiration = c.get("Expiration").and_then(|e| e.as_str()).map(str::to_string);
    if let Some(expiry) = &expiration {
        let remaining = parse_rfc3339(expiry)? - now_unix();
        if remaining < EXPIRY_WINDOW_SECONDS {
            return None;
        }
    }
    Some(Credentials {
        access_key_id: c.get("AccessKeyId")?.as_str()?.to_string(),
        secret_access_key: c.get("SecretAccessKey")?.as_str()?.to_string(),
        session_token: c.get("SessionToken").and_then(|t| t.as_str()).map(str::to_string),
        expires_at: expiration,
        // The cache does not record which provider filled it, and the caller is the only
        // one that knows; it overwrites this.
        method: "cache",
    })
}

/// Write credentials to the cache. `provider_type` is recorded when the reference does so
/// — it writes `"sso"` for SSO entries and nothing for assume-role.
pub fn write(key: &str, credentials: &Credentials, provider_type: Option<&str>) {
    let Some(dir) = dir() else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let mut document = serde_json::json!({
        "Credentials": {
            "AccessKeyId": credentials.access_key_id,
            "SecretAccessKey": credentials.secret_access_key,
            "SessionToken": credentials.session_token,
            "Expiration": credentials.expires_at,
        }
    });
    if let Some(kind) = provider_type {
        document["ProviderType"] = serde_json::Value::String(kind.to_string());
    }
    // A cache write failing is not worth failing the command over.
    let path = dir.join(format!("{key}.json"));
    if let Ok(text) = serde_json::to_string_pretty(&document) {
        let _ = std::fs::write(&path, text);
        restrict_permissions(&path);
    }
}

/// These are live credentials on disk; nobody else on the machine should be able to read
/// them.
#[cfg(unix)]
pub fn restrict_permissions(path: &PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
pub fn restrict_permissions(_path: &PathBuf) {}
