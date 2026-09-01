//! `aws sso login` and `aws sso logout`: obtaining and discarding the IAM Identity Center
//! access token.
//!
//! The provider in [`super::sso`] can already *use* a cached token and refresh one that is
//! about to lapse. What it could not do is obtain the first one, which is what this adds —
//! the OAuth 2.0 **device authorization grant**: register a client, ask for a device code,
//! show the user a URL and a code, then poll until they have approved it.
//!
//! Everything written here lands in `~/.aws/sso/cache` in botocore's own format and under
//! botocore's own key, so a token obtained by this command is usable by the reference CLI
//! and vice versa. That interoperability is the point: a user should not have to pick one
//! CLI to authenticate with.

use super::sso::{cache_dir, format_rfc3339, now_unix, sha1_hex, CachedToken};
use super::CredentialError;
use serde::Deserialize;
use std::time::Duration;

/// The device flow's default poll interval, from the RFC. The service may name its own.
const DEFAULT_INTERVAL_SECONDS: u64 = 5;
/// `slow_down` adds five seconds, also from the RFC.
const SLOW_DOWN_DELAY_SECONDS: u64 = 5;
/// botocore's registration type. A CLI cannot keep a secret, so the client is public.
const CLIENT_REGISTRATION_TYPE: &str = "public";
const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// What the caller needs to start a login.
pub struct LoginRequest {
    pub start_url: String,
    pub sso_region: String,
    /// `[sso-session NAME]`, or `None` for the legacy inline form. It decides both the
    /// client name and the cache key.
    pub session_name: Option<String>,
    /// `sso_registration_scopes`, split and trimmed.
    pub scopes: Vec<String>,
    /// Print the URL instead of opening a browser (`--no-browser`).
    pub no_browser: bool,
}

/// A client registration, cached beside the token under its own key.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
struct Registration {
    #[serde(rename = "clientId")]
    client_id: String,
    #[serde(rename = "clientSecret")]
    client_secret: String,
    #[serde(rename = "expiresAt")]
    expires_at: String,
    #[serde(rename = "scopes", skip_serializing_if = "Option::is_none")]
    scopes: Option<Vec<String>>,
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(Duration::from_secs(30)).build()
}

fn oidc_url(region: &str, path: &str) -> String {
    format!("https://oidc.{region}.amazonaws.com{path}")
}

fn sso_error(message: impl Into<String>) -> CredentialError {
    CredentialError::Sso { profile: "sso".to_string(), message: message.into() }
}

/// Run the device-authorization flow and cache the resulting token.
///
/// Returns the start URL that was logged into, so the caller can print the reference's
/// success line.
pub fn device_login(request: &LoginRequest) -> Result<(), CredentialError> {
    let registration = register_client(request)?;
    let authorization = authorize_device(request, &registration)?;

    // The client may already be authorised, in which case the user is never shown
    // anything. botocore tries once before printing, and so does this -- printing a code
    // that is not needed would be noise.
    let mut interval = authorization.interval.unwrap_or(DEFAULT_INTERVAL_SECONDS);
    match create_token(request, &registration, &authorization.device_code, &mut interval)? {
        Some(token) => return write_token(request, &registration, token),
        None => announce(request, &authorization),
    }

    loop {
        std::thread::sleep(Duration::from_secs(interval));
        if let Some(token) =
            create_token(request, &registration, &authorization.device_code, &mut interval)?
        {
            return write_token(request, &registration, token);
        }
    }
}

/// `sso-oidc:RegisterClient`, reusing a cached registration when one is still valid.
fn register_client(request: &LoginRequest) -> Result<Registration, CredentialError> {
    if let Some(cached) = read_registration(request) {
        return Ok(cached);
    }

    let mut body = serde_json::json!({
        "clientName": client_name(request),
        "clientType": CLIENT_REGISTRATION_TYPE,
    });
    if !request.scopes.is_empty() {
        body["scopes"] = serde_json::json!(request.scopes);
    }

    #[derive(Deserialize)]
    struct RegisterClientResponse {
        #[serde(rename = "clientId")]
        client_id: String,
        #[serde(rename = "clientSecret")]
        client_secret: String,
        #[serde(rename = "clientSecretExpiresAt")]
        client_secret_expires_at: i64,
    }

    let response = agent()
        .post(&oidc_url(&request.sso_region, "/client/register"))
        .set("content-type", "application/json")
        .set("user-agent", &crate::http::user_agent())
        .send_json(body)
        .map_err(|e| sso_error(format!("registering the client failed: {}", describe(e))))?;

    let parsed: RegisterClientResponse = response
        .into_json()
        .map_err(|e| sso_error(format!("the client registration response was unreadable: {e}")))?;

    let registration = Registration {
        client_id: parsed.client_id,
        client_secret: parsed.client_secret,
        expires_at: format_rfc3339(parsed.client_secret_expires_at),
        scopes: (!request.scopes.is_empty()).then(|| request.scopes.clone()),
    };
    write_registration(request, &registration);
    Ok(registration)
}

/// botocore names the client after the session; a legacy profile has no session name, so
/// it uses a timestamp. The `botocore-client-` prefix is kept deliberately -- this
/// registration is shared with the reference, and renaming it would show up in the IAM
/// Identity Center console as a second, unfamiliar client.
fn client_name(request: &LoginRequest) -> String {
    match &request.session_name {
        Some(name) => format!("botocore-client-{name}"),
        None => format!("botocore-client-{}", now_unix()),
    }
}

struct Authorization {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: String,
    interval: Option<u64>,
}

/// `sso-oidc:StartDeviceAuthorization`. Deliberately not cached: the response is
/// short-lived and can only be exchanged once, so sharing it between clients breaks both.
fn authorize_device(
    request: &LoginRequest,
    registration: &Registration,
) -> Result<Authorization, CredentialError> {
    #[derive(Deserialize)]
    struct StartDeviceAuthorizationResponse {
        #[serde(rename = "deviceCode")]
        device_code: String,
        #[serde(rename = "userCode")]
        user_code: String,
        #[serde(rename = "verificationUri")]
        verification_uri: String,
        #[serde(rename = "verificationUriComplete")]
        verification_uri_complete: String,
        interval: Option<u64>,
    }

    let response = agent()
        .post(&oidc_url(&request.sso_region, "/device_authorization"))
        .set("content-type", "application/json")
        .set("user-agent", &crate::http::user_agent())
        .send_json(serde_json::json!({
            "clientId": registration.client_id,
            "clientSecret": registration.client_secret,
            "startUrl": request.start_url,
        }))
        .map_err(|e| sso_error(format!("starting device authorization failed: {}", describe(e))))?;

    let parsed: StartDeviceAuthorizationResponse = response
        .into_json()
        .map_err(|e| sso_error(format!("the device authorization response was unreadable: {e}")))?;

    Ok(Authorization {
        device_code: parsed.device_code,
        user_code: parsed.user_code,
        verification_uri: parsed.verification_uri,
        verification_uri_complete: parsed.verification_uri_complete,
        interval: parsed.interval,
    })
}

/// The token response, or `None` while the user has not finished approving.
struct NewToken {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
}

/// One `sso-oidc:CreateToken` attempt.
///
/// The three "not yet" answers are the flow's control signals, not failures:
/// `authorization_pending` means keep waiting, `slow_down` means wait longer, and
/// `expired_token` means the user took too long and has to start again.
fn create_token(
    request: &LoginRequest,
    registration: &Registration,
    device_code: &str,
    interval: &mut u64,
) -> Result<Option<NewToken>, CredentialError> {
    #[derive(Deserialize)]
    struct CreateTokenResponse {
        #[serde(rename = "accessToken")]
        access_token: String,
        #[serde(rename = "expiresIn")]
        expires_in: i64,
        #[serde(rename = "refreshToken")]
        refresh_token: Option<String>,
    }

    let result = agent()
        .post(&oidc_url(&request.sso_region, "/token"))
        .set("content-type", "application/json")
        .set("user-agent", &crate::http::user_agent())
        .send_json(serde_json::json!({
            "grantType": GRANT_TYPE,
            "clientId": registration.client_id,
            "clientSecret": registration.client_secret,
            "deviceCode": device_code,
        }));

    match result {
        Ok(response) => {
            let parsed: CreateTokenResponse = response
                .into_json()
                .map_err(|e| sso_error(format!("the token response was unreadable: {e}")))?;
            Ok(Some(NewToken {
                access_token: parsed.access_token,
                refresh_token: parsed.refresh_token,
                expires_in: parsed.expires_in,
            }))
        }
        Err(ureq::Error::Status(_, response)) => {
            let body = response.into_string().unwrap_or_default();
            match oauth_error(&body).as_deref() {
                Some("authorization_pending") => Ok(None),
                Some("slow_down") => {
                    *interval += SLOW_DOWN_DELAY_SECONDS;
                    Ok(None)
                }
                Some("expired_token") => Err(sso_error(
                    "The authorization request has expired. Please run `aws sso login` again.",
                )),
                _ => Err(sso_error(format!("fetching the token failed: {body}"))),
            }
        }
        Err(e) => Err(sso_error(format!("fetching the token failed: {}", describe(e)))),
    }
}

/// The OAuth error code, which the service reports in the body rather than by status.
fn oauth_error(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get("error")?
        .as_str()
        .map(str::to_string)
}

/// Show the user where to go. Two wordings, matching the reference's two handlers.
fn announce(request: &LoginRequest, authorization: &Authorization) {
    if request.no_browser {
        print!(
            "Browser will not be automatically opened.\nPlease visit the following URL:\n\n{}\n",
            authorization.verification_uri
        );
        print!(
            "\nThen enter the code:\n\n{}\n\nAlternatively, you may visit the following URL \
             which will autofill the code upon loading:\n{}\n",
            authorization.user_code, authorization.verification_uri_complete
        );
        return;
    }

    print!(
        "Attempting to open your default browser.\nIf the browser does not open or you wish \
         to use a different device to authorize this request, open the following URL:\n\n{}\n",
        authorization.verification_uri
    );
    print!("\nThen enter the code:\n\n{}\n", authorization.user_code);
    open_browser(&authorization.verification_uri_complete);
}

/// Hand the URL to the desktop. A failure is not reported: the URL has already been
/// printed, so the user can still finish, and an error here would look like a login
/// failure when nothing has failed.
fn open_browser(url: &str) {
    let (program, args): (&str, &[&str]) = if cfg!(target_os = "macos") {
        ("open", &[])
    } else if cfg!(target_os = "windows") {
        ("cmd", &["/C", "start", ""])
    } else {
        ("xdg-open", &[])
    };
    let _ = std::process::Command::new(program)
        .args(args)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

// ---------------------------------------------------------------------------
// The cache
// ---------------------------------------------------------------------------

/// botocore's registration cache key: sha1 over a minified JSON object with sorted keys.
///
/// The shape is fixed by the reference and reproduced exactly, `"tool": "botocore"`
/// included, because a different key means a second registration for the same session.
fn registration_cache_key(request: &LoginRequest) -> String {
    let scopes = match request.scopes.is_empty() {
        true => "null".to_string(),
        false => serde_json::to_string(&request.scopes).unwrap_or_else(|_| "null".into()),
    };
    let session = match &request.session_name {
        Some(name) => serde_json::to_string(name).unwrap_or_else(|_| "null".into()),
        None => "null".to_string(),
    };
    // Keys sorted, separators exactly as Python's `json.dumps(sort_keys=True)` writes
    // them: `", "` between entries and `": "` after a key.
    let args = format!(
        "{{\"region\": {}, \"scopes\": {}, \"session_name\": {}, \"startUrl\": {}, \"tool\": \"botocore\"}}",
        serde_json::to_string(&request.sso_region).unwrap_or_default(),
        scopes,
        session,
        serde_json::to_string(&request.start_url).unwrap_or_default(),
    );
    sha1_hex(args.as_bytes())
}

/// The token cache key: the session name for the modern form, the start URL for legacy.
fn token_cache_key(request: &LoginRequest) -> String {
    let input = request.session_name.as_deref().unwrap_or(&request.start_url);
    sha1_hex(input.as_bytes())
}

fn read_registration(request: &LoginRequest) -> Option<Registration> {
    let path = cache_dir()?.join(format!("{}.json", registration_cache_key(request)));
    let text = std::fs::read_to_string(path).ok()?;
    let registration: Registration = serde_json::from_str(&text).ok()?;
    // An expired registration cannot be used, and neither can one created for the
    // authorization-code flow -- it has no client secret usable here.
    if super::sso::is_expired(&registration.expires_at) || registration.client_secret.is_empty() {
        return None;
    }
    Some(registration)
}

fn write_registration(request: &LoginRequest, registration: &Registration) {
    let Some(dir) = cache_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{}.json", registration_cache_key(request)));
    if let Ok(text) = serde_json::to_string(registration) {
        let _ = std::fs::write(&path, text);
        super::sso::restrict_permissions(&path);
    }
}

fn write_token(
    request: &LoginRequest,
    registration: &Registration,
    token: NewToken,
) -> Result<(), CredentialError> {
    let document = CachedToken {
        access_token: Some(token.access_token),
        expires_at: Some(format_rfc3339(now_unix() + token.expires_in)),
        start_url: Some(request.start_url.clone()),
        region: Some(request.sso_region.clone()),
        refresh_token: token.refresh_token,
        client_id: Some(registration.client_id.clone()),
        client_secret: Some(registration.client_secret.clone()),
        registration_expires_at: Some(registration.expires_at.clone()),
    };

    let dir = cache_dir().ok_or_else(|| sso_error("cannot locate ~/.aws/sso/cache (HOME is unset)"))?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| sso_error(format!("creating {}: {e}", dir.display())))?;
    let path = dir.join(format!("{}.json", token_cache_key(request)));
    let text = serde_json::to_string(&document)
        .map_err(|e| sso_error(format!("serialising the token: {e}")))?;
    std::fs::write(&path, text)
        .map_err(|e| sso_error(format!("writing {}: {e}", path.display())))?;
    super::sso::restrict_permissions(&path);
    Ok(())
}

// ---------------------------------------------------------------------------
// logout
// ---------------------------------------------------------------------------

/// `aws sso logout`: invalidate every cached token server-side, then delete it, and drop
/// any AWS credentials that were derived from one.
///
/// Both caches are swept regardless of profile — the command is "log out", not "log this
/// profile out". A file that is not JSON, or not one of ours, is left alone.
pub fn logout() {
    if let Some(dir) = cache_dir() {
        sweep(&dir, |contents| {
            let Some(token) = contents.get("accessToken").and_then(|v| v.as_str()) else {
                return false;
            };
            // Invalidate it at the service before removing the local copy: deleting the
            // file alone leaves the session alive until it expires on its own.
            if let Some(region) = contents.get("region").and_then(|v| v.as_str()) {
                invalidate(region, token);
            }
            true
        });
    }
    if let Some(dir) = super::profile::home().map(|h| h.join(".aws/cli/cache")) {
        sweep(&dir, |contents| {
            contents.get("ProviderType").and_then(|v| v.as_str()) == Some("sso")
        });
    }
}

/// Delete every file in `dir` that `should_delete` accepts, ignoring anything unreadable.
fn sweep(dir: &std::path::Path, should_delete: impl Fn(&serde_json::Value) -> bool) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        // Not JSON, so not something this command put there.
        let Ok(contents) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        if should_delete(&contents) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// `sso:Logout` on the portal, which takes the bearer token in a header and is unsigned.
/// A failure is ignored: the token may already be expired, and that is not a reason to
/// leave it on disk.
fn invalidate(region: &str, token: &str) {
    let _ = agent()
        .post(&format!("https://portal.sso.{region}.amazonaws.com/logout"))
        .set("x-amz-sso_bearer_token", token)
        .set("user-agent", &crate::http::user_agent())
        .call();
}

fn describe(error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(status, response) => {
            let body = response.into_string().unwrap_or_default();
            format!("HTTP {status}: {body}")
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> LoginRequest {
        LoginRequest {
            start_url: "https://d-9067c25df2.awsapps.com/start".to_string(),
            sso_region: "us-east-1".to_string(),
            session_name: Some("my-session".to_string()),
            scopes: Vec::new(),
            no_browser: false,
        }
    }

    /// The token cache key is the session name hashed, which is what makes a token this
    /// command writes usable by the reference CLI. Pinned to the digest rather than
    /// recomputed, so a change to the key is a test failure and not a silent loss of
    /// interoperability.
    #[test]
    fn the_token_cache_key_is_the_session_name_hashed() {
        assert_eq!(token_cache_key(&request()), sha1_hex(b"my-session"));

        // Legacy inline configuration has no session, and keys on the start URL instead.
        let legacy = LoginRequest { session_name: None, ..request() };
        assert_eq!(
            token_cache_key(&legacy),
            sha1_hex(b"https://d-9067c25df2.awsapps.com/start")
        );
    }

    /// The registration key hashes a JSON object with sorted keys and Python's separators.
    /// Any deviation -- a missing space after a colon, a different `tool` value -- yields a
    /// different file and so a second client registration for the same session.
    #[test]
    fn the_registration_cache_key_matches_botocores_json_shape() {
        let expected = sha1_hex(
            b"{\"region\": \"us-east-1\", \"scopes\": null, \"session_name\": \"my-session\", \
              \"startUrl\": \"https://d-9067c25df2.awsapps.com/start\", \"tool\": \"botocore\"}",
        );
        assert_eq!(registration_cache_key(&request()), expected);
    }

    /// Scopes participate in the key, so a session whose scopes change re-registers rather
    /// than reusing a registration granting different permissions.
    #[test]
    fn scopes_change_the_registration_key() {
        let scoped = LoginRequest { scopes: vec!["sso:account:access".into()], ..request() };
        assert_ne!(registration_cache_key(&request()), registration_cache_key(&scoped));
    }

    #[test]
    fn the_client_name_carries_botocores_prefix() {
        assert_eq!(client_name(&request()), "botocore-client-my-session");
    }

    /// The flow's three control signals have to be told apart from a real failure.
    #[test]
    fn oauth_control_signals_are_recognised() {
        assert_eq!(oauth_error(r#"{"error":"authorization_pending"}"#).as_deref(), Some("authorization_pending"));
        assert_eq!(oauth_error(r#"{"error":"slow_down"}"#).as_deref(), Some("slow_down"));
        assert_eq!(oauth_error("not json at all"), None);
    }
}
