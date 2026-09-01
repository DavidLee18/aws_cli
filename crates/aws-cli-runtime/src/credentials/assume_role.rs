//! `sts:AssumeRole` and `sts:AssumeRoleWithWebIdentity`.
//!
//! Both are single, fixed operations, so the request is built directly rather than
//! through the model-driven protocol layer — that keeps the credential path free of
//! model loading, which would otherwise be needed before credentials exist.

use super::{CredentialError, Credentials};
use crate::sigv4::{self, SigningContext, SigningRequest};
use sha1::{Digest, Sha1};
use std::collections::BTreeMap;

const STS_API_VERSION: &str = "2011-06-15";

/// The parameters a profile can contribute to an AssumeRole call.
#[derive(Debug, Default, Clone)]
pub struct AssumeRoleRequest {
    pub role_arn: String,
    /// `None` yields botocore's `botocore-session-<epoch>` default.
    pub role_session_name: Option<String>,
    pub duration_seconds: Option<i64>,
    pub external_id: Option<String>,
    /// `mfa_serial`; when present an MFA code is required.
    pub serial_number: Option<String>,
    pub token_code: Option<String>,
}

impl AssumeRoleRequest {
    /// Wire parameters, excluding Action/Version.
    ///
    /// Also returns whether the session name was generated, because a generated name is
    /// excluded from the cache key (it changes every call and would defeat caching).
    fn parameters(&self) -> (BTreeMap<String, String>, bool) {
        let mut params = BTreeMap::new();
        params.insert("RoleArn".to_string(), self.role_arn.clone());

        let generated = self.role_session_name.is_none();
        let session_name = self
            .role_session_name
            .clone()
            .unwrap_or_else(|| format!("botocore-session-{}", now_unix()));
        params.insert("RoleSessionName".to_string(), session_name);

        if let Some(d) = self.duration_seconds {
            params.insert("DurationSeconds".to_string(), d.to_string());
        }
        if let Some(e) = &self.external_id {
            params.insert("ExternalId".to_string(), e.clone());
        }
        if let Some(s) = &self.serial_number {
            params.insert("SerialNumber".to_string(), s.clone());
        }
        if let Some(t) = &self.token_code {
            params.insert("TokenCode".to_string(), t.clone());
        }
        (params, generated)
    }
}

/// Call `sts:AssumeRole`, signed with the source credentials.
pub fn assume_role(
    source: &Credentials,
    region: &str,
    request: &AssumeRoleRequest,
    profile: &str,
) -> Result<Credentials, CredentialError> {
    let (params, generated_session_name) = request.parameters();

    // The cache is shared with the reference CLI, so a role assumed by either tool is
    // reusable by the other until it expires.
    let cache_key = cache_key(&params, generated_session_name);
    if let Some(mut cached) = super::cache::read(&cache_key) {
        cached.method = "assume-role";
        return Ok(cached);
    }

    let mut body = format!("Action=AssumeRole&Version={STS_API_VERSION}");
    for (k, v) in &params {
        body.push_str(&format!("&{}={}", form_encode(k), form_encode(v)));
    }

    let xml = sts_call(&body, region, Some(source), profile, "AssumeRole")?;
    let credentials =
        parse_credentials(&xml).ok_or_else(|| service_error(&xml, "AssumeRole"))?;

    super::cache::write(&cache_key, &credentials, None);
    Ok(credentials)
}

/// Call `sts:AssumeRoleWithWebIdentity`, which is UNSIGNED — the web identity token is
/// the credential, so there is nothing to sign with.
pub fn assume_role_with_web_identity(
    region: &str,
    role_arn: &str,
    web_identity_token: &str,
    role_session_name: Option<&str>,
    profile: &str,
) -> Result<Credentials, CredentialError> {
    let session_name = role_session_name
        .map(str::to_string)
        .unwrap_or_else(|| format!("botocore-session-{}", now_unix()));

    let body = format!(
        "Action=AssumeRoleWithWebIdentity&Version={STS_API_VERSION}\
         &RoleArn={}&RoleSessionName={}&WebIdentityToken={}",
        form_encode(role_arn),
        form_encode(&session_name),
        form_encode(web_identity_token),
    );

    let xml = sts_call(&body, region, None, profile, "AssumeRoleWithWebIdentity")?;
    parse_credentials(&xml)
        .map(|mut c| {
            c.method = "assume-role-with-web-identity";
            c
        })
        .ok_or_else(|| service_error(&xml, "AssumeRoleWithWebIdentity"))
}

/// Turn an STS error document into the service-shaped error the reference reports.
fn service_error(xml: &str, operation: &str) -> CredentialError {
    let (code, message) = sts_error(xml).unwrap_or_else(|| {
        ("Unknown".to_string(), "STS response contained no credentials".to_string())
    });
    CredentialError::AssumeRoleService { code, message, operation: operation.to_string() }
}

/// POST a form-encoded body to regional STS, signing only when credentials are supplied.
fn sts_call(
    body: &str,
    region: &str,
    source: Option<&Credentials>,
    profile: &str,
    operation: &str,
) -> Result<String, CredentialError> {
    let host = format!("sts.{region}.amazonaws.com");
    let url = format!("https://{host}/");
    let content_type = "application/x-www-form-urlencoded; charset=utf-8";

    let mut request = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .post(&url)
        .set("content-type", content_type)
        .set("user-agent", &crate::http::user_agent());

    if let Some(creds) = source {
        let timestamp = sigv4::format_timestamp(now_unix());
        let mut headers = vec![
            ("content-type".to_string(), content_type.to_string()),
            ("host".to_string(), host.clone()),
            ("x-amz-date".to_string(), timestamp.clone()),
        ];
        if let Some(token) = &creds.session_token {
            headers.push(("x-amz-security-token".to_string(), token.clone()));
        }
        let signature = sigv4::sign(
            &SigningContext {
                credentials: creds,
                region,
                service: "sts",
                timestamp: &timestamp,
            },
            &SigningRequest {
                method: "POST",
                path: "/",
                query: "",
                headers: headers.clone(),
                payload_hash: &crate::http::payload_hash(
                    &crate::transport::Body::from_vec(body.as_bytes().to_vec()),
                    "sts",
                ),
            },
        );
        for (k, v) in headers.iter().filter(|(k, _)| k != "host") {
            request = request.set(k, v);
        }
        request = request.set("authorization", &signature.authorization);
    }

    match request.send_string(body) {
        Ok(response) => response.into_string().map_err(|e| CredentialError::AssumeRole {
            profile: profile.to_string(),
            message: e.to_string(),
        }),
        // STS reports failures as XML error documents; surface them the way the
        // reference does rather than as a transport error.
        Err(ureq::Error::Status(_, response)) => {
            let body = response.into_string().unwrap_or_default();
            Err(service_error(&body, operation))
        }
        Err(e) => Err(CredentialError::AssumeRole {
            profile: profile.to_string(),
            message: e.to_string(),
        }),
    }
}

/// Extract `<Credentials>` from an STS response.
fn parse_credentials(xml: &str) -> Option<Credentials> {
    let block = element_text(xml, "Credentials")?;
    Some(Credentials {
        method: "assume-role",
        access_key_id: element_text(&block, "AccessKeyId")?,
        secret_access_key: element_text(&block, "SecretAccessKey")?,
        session_token: element_text(&block, "SessionToken"),
        expires_at: element_text(&block, "Expiration"),
    })
}

/// `(Code, Message)` from an STS `<Error>` document.
fn sts_error(xml: &str) -> Option<(String, String)> {
    let code = element_text(xml, "Code")?;
    Some((code, element_text(xml, "Message").unwrap_or_default()))
}

/// The inner text of the first `<tag>` in `xml`.
///
/// A targeted extractor rather than a full parser: STS responses here have four known
/// fields, and pulling in the model-driven XML layer would mean loading a service model
/// before credentials exist.
fn element_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].trim().to_string())
}

// --- credential cache (shared with the reference CLI) ---------------------------------

/// botocore's key: `sha1` of the JSON-serialised arguments with sorted keys, using
/// Python's default `json.dumps` separators (`", "` and `": "`). The separators matter:
/// a compact encoding would produce a different digest and silently stop sharing the
/// cache with the reference.
fn cache_key(params: &BTreeMap<String, String>, generated_session_name: bool) -> String {
    let entries: Vec<String> = params
        .iter()
        .filter(|(k, _)| !(generated_session_name && k.as_str() == "RoleSessionName"))
        .map(|(k, v)| format!("{}: {}", json_string(k), json_string(v)))
        .collect();
    let serialized = format!("{{{}}}", entries.join(", "));

    let digest: String = Sha1::digest(serialized.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    // botocore replaces path-hostile characters in the key before using it as a filename.
    digest.replace([':', '/'], "_")
}

fn json_string(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}


fn form_encode(s: &str) -> String {
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

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_credentials_from_sts_xml() {
        let xml = r#"<AssumeRoleResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">
          <AssumeRoleResult><Credentials>
            <AccessKeyId>ASIAEXAMPLE</AccessKeyId>
            <SecretAccessKey>secret</SecretAccessKey>
            <SessionToken>token</SessionToken>
            <Expiration>2030-01-01T00:00:00Z</Expiration>
          </Credentials></AssumeRoleResult></AssumeRoleResponse>"#;
        let c = parse_credentials(xml).unwrap();
        assert_eq!(c.access_key_id, "ASIAEXAMPLE");
        assert_eq!(c.session_token.as_deref(), Some("token"));
        assert_eq!(c.expires_at.as_deref(), Some("2030-01-01T00:00:00Z"));
    }

    #[test]
    fn extracts_sts_error_message() {
        let xml = r#"<ErrorResponse><Error><Code>AccessDenied</Code>
          <Message>User is not authorized</Message></Error></ErrorResponse>"#;
        let (code, message) = sts_error(xml).unwrap();
        assert_eq!(code, "AccessDenied");
        assert_eq!(message, "User is not authorized");
        assert!(parse_credentials(xml).is_none());
    }

    #[test]
    fn generated_session_name_is_excluded_from_the_cache_key() {
        let base = AssumeRoleRequest {
            role_arn: "arn:aws:iam::1:role/r".into(),
            ..Default::default()
        };
        let (p1, g1) = base.parameters();
        // A second call a notional second later gets a different generated name...
        let mut p2 = p1.clone();
        p2.insert("RoleSessionName".into(), "botocore-session-9999999999".into());

        assert!(g1, "session name should be generated when unset");
        assert_eq!(
            cache_key(&p1, true),
            cache_key(&p2, true),
            "a generated session name must not change the cache key"
        );
    }

    #[test]
    fn explicit_session_name_participates_in_the_cache_key() {
        let a = AssumeRoleRequest {
            role_arn: "arn:aws:iam::1:role/r".into(),
            role_session_name: Some("alice".into()),
            ..Default::default()
        };
        let b = AssumeRoleRequest { role_session_name: Some("bob".into()), ..a.clone() };
        let (pa, ga) = a.parameters();
        let (pb, gb) = b.parameters();
        assert!(!ga && !gb);
        assert_ne!(cache_key(&pa, false), cache_key(&pb, false));
    }

    /// The digest is over Python-style JSON, so this pins the exact serialisation the
    /// reference hashes — otherwise the shared cache silently stops being shared.
    #[test]
    fn cache_key_uses_python_json_separators() {
        let mut params = BTreeMap::new();
        params.insert("RoleArn".to_string(), "arn:aws:iam::1:role/r".to_string());
        params.insert("RoleSessionName".to_string(), "alice".to_string());

        let expected_json =
            r#"{"RoleArn": "arn:aws:iam::1:role/r", "RoleSessionName": "alice"}"#;
        let expected: String =
            Sha1::digest(expected_json.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(cache_key(&params, false), expected);
    }

    #[test]
    fn parameters_include_only_what_is_set() {
        let request = AssumeRoleRequest {
            role_arn: "arn".into(),
            role_session_name: Some("s".into()),
            duration_seconds: Some(3600),
            external_id: Some("x".into()),
            ..Default::default()
        };
        let (params, _) = request.parameters();
        assert_eq!(params["DurationSeconds"], "3600");
        assert_eq!(params["ExternalId"], "x");
        assert!(!params.contains_key("SerialNumber"));
        assert!(!params.contains_key("TokenCode"));
    }
}
