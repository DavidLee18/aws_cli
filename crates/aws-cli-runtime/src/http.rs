//! Signing a prepared request and sending it.

use crate::endpoint::Endpoint;
use crate::sigv4::{self, Credentials, SigningContext, SigningRequest};
use crate::RuntimeError;

/// A request built by the protocol layer, ready to sign and send.
pub struct PreparedRequest {
    pub method: String,
    pub endpoint: Endpoint,
    /// Absolute path, already URI-encoded, starting with `/`.
    pub path: String,
    /// Already-encoded query string without the leading `?`.
    pub query: String,
    /// `None` when the protocol sends no Content-Type — restXml genuinely omits it.
    pub content_type: Option<String>,
    /// Protocol headers such as `X-Amz-Target`, plus any bound by the operation.
    pub extra_headers: Vec<(String, String)>,
    pub body: String,
}

pub struct Response {
    pub status: u16,
    pub body: String,
    headers: Vec<(String, String)>,
}

impl Response {
    /// Case-insensitive header lookup.
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    }
}

/// The headers that participate in the signature.
///
/// Content-type, host and x-amz-date always; the session token when present; and every
/// protocol/operation header, because those genuinely affect the request (`X-Amz-Target`
/// selects the operation, so leaving it unsigned would let it be tampered with).
/// User-agent and the `amz-sdk-*` retry headers stay unsigned, matching the reference.
fn signed_headers(
    req: &PreparedRequest,
    timestamp: &str,
    creds: &Credentials,
) -> Vec<(String, String)> {
    let mut headers = vec![("host".to_string(), req.endpoint.host.clone())];
    if let Some(content_type) = &req.content_type {
        headers.push(("content-type".to_string(), content_type.clone()));
    }
    headers.push(("x-amz-date".to_string(), timestamp.to_string()));
    if let Some(token) = &creds.session_token {
        headers.push(("x-amz-security-token".to_string(), token.clone()));
    }
    // S3 rejects requests without an explicit payload hash header ("Missing required
    // header for this request: x-amz-content-sha256"), unlike every other service, which
    // is happy with the hash appearing only inside the canonical request.
    if requires_content_sha256(&req.endpoint.signing_name) {
        headers.push(("x-amz-content-sha256".to_string(), payload_hash(req.body.as_bytes())));
    }
    for (k, v) in &req.extra_headers {
        headers.push((k.to_ascii_lowercase(), v.clone()));
    }
    headers
}

fn requires_content_sha256(signing_name: &str) -> bool {
    // Covers `s3`, `s3-object-lambda`, `s3-outposts`, and the express variants.
    signing_name == "s3" || signing_name.starts_with("s3-")
}

fn payload_hash(body: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(body).iter().map(|b| format!("{b:02x}")).collect()
}

/// Sign `req` and return the headers to send, including Authorization.
pub fn sign_request(
    req: &PreparedRequest,
    creds: &Credentials,
    timestamp: &str,
) -> (Vec<(String, String)>, sigv4::Signature) {
    let headers = signed_headers(req, timestamp, creds);
    let ctx = SigningContext {
        credentials: creds,
        region: &req.endpoint.signing_region,
        service: &req.endpoint.signing_name,
        timestamp,
    };
    let signing_req = SigningRequest {
        method: &req.method,
        path: if req.path.is_empty() { "/" } else { &req.path },
        query: &canonical_query(&req.query),
        headers: headers.clone(),
        body: req.body.as_bytes(),
    };
    let signature = sigv4::sign(&ctx, &signing_req);

    let mut out = headers;
    out.push(("authorization".to_string(), signature.authorization.clone()));
    out.push(("user-agent".to_string(), user_agent()));
    (out, signature)
}

/// SigV4 requires query parameters sorted by name, then value, with `=` present even for
/// valueless keys.
fn canonical_query(query: &str) -> String {
    if query.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (p.to_string(), String::new()),
        })
        .collect();
    pairs.sort();
    pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

pub fn user_agent() -> String {
    format!("aws-cli-rs/{}", env!("CARGO_PKG_VERSION"))
}

/// Send a signed request. Blocking, since the CLI makes one call per invocation and an
/// async runtime would buy nothing here.
pub fn send(req: &PreparedRequest, headers: &[(String, String)]) -> Result<Response, RuntimeError> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(60))
        .build();

    let base = req.endpoint.url.trim_end_matches('/');
    let path = if req.path.is_empty() { "/" } else { &req.path };
    let url = if req.query.is_empty() {
        format!("{base}{path}")
    } else {
        format!("{base}{path}?{}", req.query)
    };

    let mut call = agent.request(&req.method, &url);
    for (k, v) in headers {
        // `host` is set by the transport; sending it explicitly duplicates the header.
        if k != "host" {
            call = call.set(k, v);
        }
    }

    let collect = |resp: ureq::Response| {
        let status = resp.status();
        let names: Vec<String> = resp.headers_names();
        let headers: Vec<(String, String)> = names
            .iter()
            .filter_map(|n| resp.header(n).map(|v| (n.clone(), v.to_string())))
            .collect();
        resp.into_string().map(|body| Response { status, body, headers })
    };

    match call.send_string(&req.body) {
        Ok(resp) => collect(resp).map_err(|e| RuntimeError::Http(e.to_string())),
        // A 4xx/5xx is a normal outcome carrying a modelled error document, not a
        // transport failure — hand the body back for the protocol layer to parse.
        Err(ureq::Error::Status(_, resp)) => {
            collect(resp).map_err(|e| RuntimeError::Http(e.to_string()))
        }
        Err(e) => Err(RuntimeError::Http(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalises_query_strings() {
        assert_eq!(canonical_query(""), "");
        // Sorted by key, and a valueless key still gets `=`.
        assert_eq!(canonical_query("b=2&a=1"), "a=1&b=2");
        assert_eq!(canonical_query("acl"), "acl=");
        assert_eq!(canonical_query("list-type=2&prefix=x"), "list-type=2&prefix=x");
    }
}
