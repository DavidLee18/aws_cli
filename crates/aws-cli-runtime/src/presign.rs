//! SigV4 query-string signing, for URLs that carry their own authorization.
//!
//! Distinct from [`crate::http::sign_request`] in three ways that matter:
//!
//! - The auth parameters go in the query string, not the `Authorization` header.
//! - The body is always empty, so the payload hash is [`sigv4::EMPTY_SHA256`].
//! - **The emitted parameter order and the canonical order differ.** botocore appends the
//!   auth parameters in a fixed insertion order but recomputes the canonical query string
//!   by sorting the *encoded* pairs — and `X-Amz-Security-Token` sorts before
//!   `X-Amz-SignedHeaders` (`Sec` < `Sig`), so a session token lands in a different place
//!   in each. Getting this wrong produces a URL that looks right and fails to
//!   authenticate.

use crate::sigv4::{self, SigningContext};

pub struct PresignRequest<'a> {
    pub method: &'a str,
    /// The `host` header value to sign. Callers decide whether a port appears — botocore
    /// omits `:443` but keeps every other port.
    pub host: &'a str,
    /// Path, already in the form it should be canonicalized as.
    pub path: &'a str,
    /// Operation parameters, in the order they should be emitted.
    pub params: Vec<(String, String)>,
    /// Headers signed in addition to `host`, such as `x-k8s-aws-id`.
    pub extra_signed_headers: Vec<(String, String)>,
    pub expires: u32,
    /// The payload hash to sign. S3 presigns with the literal `UNSIGNED-PAYLOAD`
    /// (`S3SigV4QueryAuth.payload`), everything else with the empty-body SHA-256.
    pub payload: Payload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Payload {
    #[default]
    EmptyBody,
    Unsigned,
}

impl Payload {
    fn as_str(self) -> &'static str {
        match self {
            Payload::EmptyBody => sigv4::EMPTY_SHA256,
            Payload::Unsigned => "UNSIGNED-PAYLOAD",
        }
    }
}

/// Return the query string (without a leading `?`), auth parameters and signature
/// included.
pub fn presign(ctx: &SigningContext<'_>, req: &PresignRequest<'_>) -> String {
    let mut headers: Vec<(String, String)> = vec![("host".to_string(), req.host.to_string())];
    for (k, v) in &req.extra_signed_headers {
        headers.push((k.to_ascii_lowercase(), v.trim().to_string()));
    }
    headers.sort_by(|a, b| a.0.cmp(&b.0));
    let signed_headers = headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");

    // Insertion order, exactly as botocore builds the dict. The security token is last
    // when present.
    let mut auth: Vec<(String, String)> = vec![
        ("X-Amz-Algorithm".into(), sigv4::ALGORITHM.into()),
        (
            "X-Amz-Credential".into(),
            format!("{}/{}", ctx.credentials.access_key_id, sigv4::credential_scope(ctx)),
        ),
        ("X-Amz-Date".into(), ctx.timestamp.to_string()),
        ("X-Amz-Expires".into(), req.expires.to_string()),
        ("X-Amz-SignedHeaders".into(), signed_headers.clone()),
    ];
    if let Some(token) = &ctx.credentials.session_token {
        auth.push(("X-Amz-Security-Token".into(), token.clone()));
    }

    let emitted: Vec<(String, String)> =
        req.params.iter().cloned().chain(auth.iter().cloned()).collect();
    let query = encode_sequence(&emitted);

    // The canonical query string is recomputed from the encoded pairs and sorted, which
    // is not the emitted order.
    let mut canonical_pairs: Vec<(String, String)> = emitted
        .iter()
        .map(|(k, v)| (percent_encode(k), percent_encode(v)))
        .collect();
    canonical_pairs.sort();
    let canonical_query =
        canonical_pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");

    let canonical_headers: String =
        headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let canonical_request = format!(
        "{}\n{}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{}",
        req.method,
        req.path,
        req.payload.as_str()
    );

    let (_, signature) = sigv4::sign_canonical_request(ctx, &canonical_request);
    format!("{query}&X-Amz-Signature={signature}")
}

fn encode_sequence(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// botocore's `percent_encode`: `quote(value, safe='-._~')`, so every reserved character
/// including `/` and `+` is escaped.
pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::Credentials;

    fn creds(token: Option<&str>) -> Credentials {
        Credentials {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: token.map(str::to_string),
            expires_at: None,
            method: "test-fixture",
        }
    }

    #[test]
    fn encodes_with_botocores_safe_set() {
        assert_eq!(percent_encode("jane doe"), "jane%20doe");
        assert_eq!(percent_encode("a/b+c="), "a%2Fb%2Bc%3D");
        // Unreserved characters survive.
        assert_eq!(percent_encode("a-b._~9"), "a-b._~9");
    }

    /// The emitted order is insertion order; a session token comes last.
    #[test]
    fn emits_auth_parameters_in_insertion_order() {
        let c = creds(Some("FAKE/tok+en="));
        let ctx = SigningContext {
            credentials: &c,
            region: "us-west-2",
            service: "rds-db",
            timestamp: "20260814T015250Z",
        };
        let query = presign(
            &ctx,
            &PresignRequest {
                method: "GET",
                host: "mydb.us-west-2.rds.amazonaws.com",
                path: "/",
                params: vec![
                    ("Action".into(), "connect".into()),
                    ("DBUser".into(), "jane doe".into()),
                ],
                extra_signed_headers: Vec::new(),
                expires: 900,
                payload: Payload::EmptyBody,
            },
        );
        let names: Vec<&str> =
            query.split('&').map(|p| p.split('=').next().unwrap()).collect();
        assert_eq!(
            names,
            [
                "Action",
                "DBUser",
                "X-Amz-Algorithm",
                "X-Amz-Credential",
                "X-Amz-Date",
                "X-Amz-Expires",
                "X-Amz-SignedHeaders",
                "X-Amz-Security-Token",
                "X-Amz-Signature",
            ]
        );
        assert!(query.contains("DBUser=jane%20doe"));
        assert!(query.contains(
            "X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20260814%2Fus-west-2%2Frds-db%2Faws4_request"
        ));
        assert!(query.contains("X-Amz-Security-Token=FAKE%2Ftok%2Ben%3D"));
    }

    /// Extra signed headers appear only inside `X-Amz-SignedHeaders`, never as their own
    /// query parameter — `eks get-token` depends on this.
    #[test]
    fn extra_signed_headers_are_not_query_parameters() {
        let c = creds(None);
        let ctx = SigningContext {
            credentials: &c,
            region: "us-west-2",
            service: "sts",
            timestamp: "20260814T015254Z",
        };
        let query = presign(
            &ctx,
            &PresignRequest {
                method: "GET",
                host: "sts.us-west-2.amazonaws.com",
                path: "/",
                params: vec![
                    ("Action".into(), "GetCallerIdentity".into()),
                    ("Version".into(), "2011-06-15".into()),
                ],
                extra_signed_headers: vec![("x-k8s-aws-id".into(), "my-cluster".into())],
                expires: 60,
                payload: Payload::EmptyBody,
            },
        );
        assert!(query.contains("X-Amz-SignedHeaders=host%3Bx-k8s-aws-id"), "{query}");
        assert!(!query.contains("&x-k8s-aws-id="), "{query}");
    }

    /// The signature must change when a signed header's *value* changes, which is the
    /// whole point of binding the cluster name into the token.
    #[test]
    fn signed_header_value_binds_into_the_signature() {
        let c = creds(None);
        let ctx = SigningContext {
            credentials: &c,
            region: "us-west-2",
            service: "sts",
            timestamp: "20260814T015254Z",
        };
        let make = |cluster: &str| {
            presign(
                &ctx,
                &PresignRequest {
                    method: "GET",
                    host: "sts.us-west-2.amazonaws.com",
                    path: "/",
                    params: vec![("Action".into(), "GetCallerIdentity".into())],
                    extra_signed_headers: vec![("x-k8s-aws-id".into(), cluster.into())],
                    expires: 60,
                    payload: Payload::EmptyBody,
                },
            )
        };
        assert_ne!(make("cluster-a"), make("cluster-b"));
    }
}
