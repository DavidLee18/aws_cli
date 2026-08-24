//! Signing a prepared request and sending it.

use crate::endpoint::Endpoint;
use crate::sigv4::{self, Credentials, SigningContext, SigningRequest};
use crate::transport::{self};
use crate::RuntimeError;

pub use crate::transport::{Body, Response, ResponseHead, Transport};

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
    /// The request body. May be file-backed, in which case it is streamed from disk at
    /// send time and never held in memory.
    pub body: Body,
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
        headers.push((
            "x-amz-content-sha256".to_string(),
            payload_hash(&req.body, &req.endpoint.signing_name),
        ));
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

/// The payload hash that goes into the canonical request.
///
/// An in-memory body is hashed. A file-backed body is signed as `UNSIGNED-PAYLOAD`
/// instead: hashing it would mean reading every byte off disk before the upload starts,
/// doubling the I/O and adding a full SHA-256 pass over each part. S3 accepts the
/// sentinel over HTTPS, where TLS already protects the payload in transit.
///
/// Only S3 accepts it, and file-backed bodies only ever arise from S3 uploads, so the
/// mismatch is unreachable rather than merely unlikely.
pub fn payload_hash(body: &Body, signing_name: &str) -> String {
    use sha2::{Digest, Sha256};
    match body.as_bytes() {
        Some(bytes) => Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect(),
        None => {
            debug_assert!(
                requires_content_sha256(signing_name),
                "a file-backed body reached {signing_name}, which cannot sign UNSIGNED-PAYLOAD",
            );
            "UNSIGNED-PAYLOAD".to_string()
        }
    }
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
    // The endpoint may already carry a path (S3 path-style addressing), and that part is
    // signed too — signing only `req.path` would authorise a different resource.
    let path = format!("{}{}", req.endpoint.path_prefix, req.path);
    let signing_req = SigningRequest {
        method: &req.method,
        path: if path.is_empty() { "/" } else { &path },
        query: &canonical_query(&req.query),
        headers: headers.clone(),
        payload_hash: &payload_hash(&req.body, &req.endpoint.signing_name),
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

/// The status line's reason phrase, for responses that carry no error body.
///
/// `HeadBucket` and `HeadObject` answer a failure with a status and nothing else — HEAD
/// has no body by definition. Without this the error parsers fall through to their
/// "Unknown" branch and print `An error occurred (Unknown) ... :` with an empty message,
/// which tells the user nothing about what went wrong.
pub fn reason_phrase(status: u16) -> &'static str {
    match status {
        301 => "Moved Permanently",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        412 => "Precondition Failed",
        416 => "Requested Range Not Satisfiable",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "",
    }
}

pub fn user_agent() -> String {
    format!("aws-cli-rs/{}", env!("CARGO_PKG_VERSION"))
}

/// Headers for an unsigned request (`--no-sign-request`).
///
/// Everything the protocol asked for, minus the SigV4 apparatus. `host` is still listed
/// so the caller can filter it out consistently.
pub fn unsigned_headers(req: &PreparedRequest) -> Vec<(String, String)> {
    let mut out = vec![("host".to_string(), req.endpoint.host.clone())];
    if let Some(content_type) = &req.content_type {
        out.push(("content-type".to_string(), content_type.clone()));
    }
    for (k, v) in &req.extra_headers {
        out.push((k.to_ascii_lowercase(), v.clone()));
    }
    out.push(("user-agent".to_string(), user_agent()));
    out
}

/// The absolute URL for a prepared request.
fn url_of(req: &PreparedRequest) -> String {
    let base = req.endpoint.url.trim_end_matches('/');
    let path = if req.path.is_empty() { "/" } else { &req.path };
    if req.query.is_empty() {
        format!("{base}{path}")
    } else {
        format!("{base}{path}?{}", req.query)
    }
}

fn transport_request(req: &PreparedRequest, headers: &[(String, String)]) -> transport::Request {
    transport::Request {
        method: req.method.clone(),
        url: url_of(req),
        headers: headers.to_vec(),
        body: req.body.clone(),
    }
}

/// Send a request and read the response into memory.
///
/// Blocking, but backed by the shared pooled client — successive calls reuse the
/// connection rather than handshaking again.
pub fn send(
    req: &PreparedRequest,
    headers: &[(String, String)],
    transport_opts: &Transport,
) -> Result<Response, RuntimeError> {
    transport::send(&transport_request(req, headers), transport_opts)
}

/// Send a request and stream the response body straight into `sink`.
///
/// The download path: the object never exists in memory as a whole, so fetching a 5 GB
/// object costs a chunk buffer rather than 5 GB.
pub fn send_to_writer<W: std::io::Write>(
    req: &PreparedRequest,
    headers: &[(String, String)],
    transport_opts: &Transport,
    sink: &mut W,
) -> Result<ResponseHead, RuntimeError> {
    transport::send_to_writer(&transport_request(req, headers), transport_opts, sink)
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
