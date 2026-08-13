//! Signing a prepared request and sending it.

use crate::endpoint::Endpoint;
use crate::sigv4::{self, Credentials, SigningContext, SigningRequest};
use crate::RuntimeError;

/// A request built by the protocol layer, ready to sign and send.
pub struct PreparedRequest {
    pub method: String,
    pub endpoint: Endpoint,
    pub content_type: String,
    pub body: String,
}

pub struct Response {
    pub status: u16,
    pub body: String,
}

/// Headers exactly as botocore signs them for a query-protocol POST.
///
/// Only content-type, host and x-amz-date are signed — user-agent and the `amz-sdk-*`
/// retry headers are sent unsigned, confirmed from the reference's SignedHeaders.
fn signed_headers(req: &PreparedRequest, timestamp: &str, creds: &Credentials) -> Vec<(String, String)> {
    let mut headers = vec![
        ("content-type".to_string(), req.content_type.clone()),
        ("host".to_string(), req.endpoint.host.clone()),
        ("x-amz-date".to_string(), timestamp.to_string()),
    ];
    // Session tokens must be signed, or STS rejects the request.
    if let Some(token) = &creds.session_token {
        headers.push(("x-amz-security-token".to_string(), token.clone()));
    }
    headers
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
        path: "/",
        query: "",
        headers: headers.clone(),
        body: req.body.as_bytes(),
    };
    let signature = sigv4::sign(&ctx, &signing_req);

    let mut out = headers;
    out.push(("authorization".to_string(), signature.authorization.clone()));
    out.push(("user-agent".to_string(), user_agent()));
    (out, signature)
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

    let mut call = agent.post(&req.endpoint.url);
    for (k, v) in headers {
        // `host` is set by the transport; sending it explicitly duplicates the header.
        if k != "host" {
            call = call.set(k, v);
        }
    }

    match call.send_string(&req.body) {
        Ok(resp) => Ok(Response {
            status: resp.status(),
            body: resp.into_string().map_err(|e| RuntimeError::Http(e.to_string()))?,
        }),
        // A 4xx/5xx is a normal outcome carrying a modelled error document, not a
        // transport failure — hand the body back for the protocol layer to parse.
        Err(ureq::Error::Status(status, resp)) => Ok(Response {
            status,
            body: resp.into_string().map_err(|e| RuntimeError::Http(e.to_string()))?,
        }),
        Err(e) => Err(RuntimeError::Http(e.to_string())),
    }
}
