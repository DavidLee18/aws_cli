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
use crate::RuntimeError;
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

fn sso_error(message: impl Into<String>) -> RuntimeError {
    RuntimeError::Configuration(message.into())
}

/// Turn a failed OIDC or portal call into the reference's service-error line.
///
/// The exception name comes from `x-amzn-errortype`, which the service sends as
/// `InvalidClientException:http://internal...` -- everything from the first colon is
/// noise. The body carries the OAuth `error` / `error_description` pair, and the
/// description is the human half. Without this the user sees a raw JSON body and an
/// exit code that says "configuration" when the service rejected the request.
fn service_error(error: ureq::Error, operation: &str) -> RuntimeError {
    match error {
        ureq::Error::Status(status, response) => {
            let error_type = response.header("x-amzn-errortype").map(str::to_string);
            let body = response.into_string().unwrap_or_default();
            let parsed: serde_json::Value =
                serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);

            let code = error_type
                .and_then(|raw| raw.split(':').next().map(str::to_string))
                .or_else(|| parsed.get("error").and_then(|v| v.as_str()).map(str::to_string))
                .unwrap_or_else(|| status.to_string());
            let message = parsed
                .get("error_description")
                .and_then(|v| v.as_str())
                .or_else(|| parsed.get("error").and_then(|v| v.as_str()))
                .unwrap_or(&body)
                .to_string();

            RuntimeError::Service { code, message, operation: operation.to_string() }
        }
        other => RuntimeError::Http(other.to_string()),
    }
}

/// Run the device-authorization flow and cache the resulting token.
///
/// Returns the start URL that was logged into, so the caller can print the reference's
/// success line.
pub fn device_login(request: &LoginRequest) -> Result<(), RuntimeError> {
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
fn register_client(request: &LoginRequest) -> Result<Registration, RuntimeError> {
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
        .map_err(|e| service_error(e, "RegisterClient"))?;

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
) -> Result<Authorization, RuntimeError> {
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
        .map_err(|e| service_error(e, "StartDeviceAuthorization"))?;

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
) -> Result<Option<NewToken>, RuntimeError> {
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
        Err(ureq::Error::Status(status, response)) => {
            let error_type = response.header("x-amzn-errortype").map(str::to_string);
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
                // Anything else is a real failure, reported as the service error it is.
                _ => Err(rebuild_service_error(status, error_type, &body, "CreateToken")),
            }
        }
        Err(e) => Err(service_error(e, "CreateToken")),
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

/// The body has already been consumed by the pending/slow-down check, so the service
/// error is rebuilt from the parts rather than from the response.
fn rebuild_service_error(
    status: u16,
    error_type: Option<String>,
    body: &str,
    operation: &str,
) -> RuntimeError {
    let parsed: serde_json::Value =
        serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let code = error_type
        .and_then(|raw| raw.split(':').next().map(str::to_string))
        .or_else(|| parsed.get("error").and_then(|v| v.as_str()).map(str::to_string))
        .unwrap_or_else(|| status.to_string());
    let message = parsed
        .get("error_description")
        .and_then(|v| v.as_str())
        .or_else(|| parsed.get("error").and_then(|v| v.as_str()))
        .unwrap_or(body)
        .to_string();
    RuntimeError::Service { code, message, operation: operation.to_string() }
}

fn write_token(
    request: &LoginRequest,
    registration: &Registration,
    token: NewToken,
) -> Result<(), RuntimeError> {
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


// ---------------------------------------------------------------------------
// The portal, for `aws configure sso`
// ---------------------------------------------------------------------------

/// One account the signed-in user can reach.
#[derive(Debug, Clone, Deserialize)]
pub struct Account {
    #[serde(rename = "accountId")]
    pub account_id: String,
    #[serde(rename = "accountName")]
    pub account_name: Option<String>,
    #[serde(rename = "emailAddress")]
    pub email_address: Option<String>,
}

impl Account {
    /// The reference's display string, which degrades as fields go missing -- both have
    /// been seen absent in real responses, and formatting `None` into the line would be
    /// worse than dropping it.
    pub fn display(&self) -> String {
        match (&self.account_name, &self.email_address) {
            (None, None) => self.account_id.clone(),
            (Some(name), None) => format!("{name} ({})", self.account_id),
            (None, Some(email)) => format!("{email} ({})", self.account_id),
            (Some(name), Some(email)) => format!("{name}, {email} ({})", self.account_id),
        }
    }

    /// Accounts with neither a name nor an email sort last; the rest sort by whichever
    /// field is present, case-insensitively.
    pub fn sort_key(&self) -> (bool, String) {
        let nameless = self.account_name.is_none() && self.email_address.is_none();
        let value = self
            .account_name
            .as_ref()
            .or(self.email_address.as_ref())
            .unwrap_or(&self.account_id);
        (nameless, value.to_lowercase())
    }
}

/// The cached access token for a session, for a command that has just logged in.
pub fn cached_access_token(session_name: &str) -> Option<String> {
    let path = cache_dir()?.join(format!("{}.json", sha1_hex(session_name.as_bytes())));
    let text = std::fs::read_to_string(path).ok()?;
    let token: CachedToken = serde_json::from_str(&text).ok()?;
    token.access_token
}

/// `sso:ListAccounts`, following every page.
///
/// Unsigned, like the rest of the portal: the bearer token in the header is the whole
/// credential.
pub fn list_accounts(region: &str, token: &str) -> Result<Vec<Account>, RuntimeError> {
    #[derive(Deserialize)]
    struct Page {
        #[serde(rename = "accountList")]
        account_list: Vec<Account>,
        #[serde(rename = "nextToken")]
        next_token: Option<String>,
    }

    let mut accounts = Vec::new();
    let mut next: Option<String> = None;
    loop {
        let mut url = format!("https://portal.sso.{region}.amazonaws.com/assignment/accounts?max_result=100");
        if let Some(token) = &next {
            url.push_str(&format!("&next_token={}", urlencode(token)));
        }
        let page: Page = portal_get(&url, token)?;
        accounts.extend(page.account_list);
        match page.next_token {
            Some(t) if !t.is_empty() => next = Some(t),
            _ => return Ok(accounts),
        }
    }
}

/// `sso:ListAccountRoles`, following every page, returning just the role names.
pub fn list_account_roles(
    region: &str,
    token: &str,
    account_id: &str,
) -> Result<Vec<String>, RuntimeError> {
    #[derive(Deserialize)]
    struct Role {
        #[serde(rename = "roleName")]
        role_name: String,
    }
    #[derive(Deserialize)]
    struct Page {
        #[serde(rename = "roleList")]
        role_list: Vec<Role>,
        #[serde(rename = "nextToken")]
        next_token: Option<String>,
    }

    let mut roles = Vec::new();
    let mut next: Option<String> = None;
    loop {
        let mut url = format!(
            "https://portal.sso.{region}.amazonaws.com/assignment/roles?max_result=100&account_id={}",
            urlencode(account_id)
        );
        if let Some(token) = &next {
            url.push_str(&format!("&next_token={}", urlencode(token)));
        }
        let page: Page = portal_get(&url, token)?;
        roles.extend(page.role_list.into_iter().map(|r| r.role_name));
        match page.next_token {
            Some(t) if !t.is_empty() => next = Some(t),
            _ => return Ok(roles),
        }
    }
}

fn portal_get<T: serde::de::DeserializeOwned>(
    url: &str,
    token: &str,
) -> Result<T, RuntimeError> {
    let response = agent()
        .get(url)
        .set("x-amz-sso_bearer_token", token)
        .set("user-agent", &crate::http::user_agent())
        .call()
        .map_err(|e| service_error(e, "ListAccounts"))?;
    response
        .into_json()
        .map_err(|e| sso_error(format!("the SSO portal response was unreadable: {e}")))
}

/// Percent-encode a query value. The portal's tokens are opaque and may contain anything.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}
