//! S3 sends a request aimed at the wrong region back with the region the bucket
//! actually lives in. The reference follows that (`S3RegionRedirectorv2`) and retries
//! immediately, signing for the region it was just told. Ignoring it is worse than an
//! error: a 301 is not a failure, so the redirect document is parsed as an empty listing
//! and `s3 ls` prints nothing.
//!
//! The learned region is remembered for the rest of the process. A transfer makes one
//! request per object; paying the redirect on every one of them would double the
//! round trips.

use awsc_runtime::endpoint::{self, Endpoint};
use awsc_runtime::http::Response;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

/// Bucket, plus any `--endpoint-url`, to the endpoint a redirect resolved for it.
///
/// The endpoint is cached rather than the region alone: re-resolving parses the S3
/// ruleset, and a transfer would otherwise do that once per object.
fn cache() -> &'static Mutex<HashMap<String, Endpoint>> {
    // `HashMap::new` is not const on the 1.85 MSRV, so the map is built on first use.
    static CACHE: LazyLock<Mutex<HashMap<String, Endpoint>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    &CACHE
}

fn cache_key(bucket: &str, endpoint_url: Option<&str>) -> String {
    format!("{bucket}\n{}", endpoint_url.unwrap_or(""))
}

fn remember(bucket: &str, endpoint_url: Option<&str>, endpoint: Endpoint) {
    let mut guard = cache().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.insert(cache_key(bucket, endpoint_url), endpoint);
}

fn remembered(bucket: &str, endpoint_url: Option<&str>) -> Option<Endpoint> {
    let guard = cache().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.get(&cache_key(bucket, endpoint_url)).cloned()
}

/// The endpoint to send to, using one learned from an earlier redirect when we have it.
pub fn prepare(bucket: Option<&str>, endpoint_url: Option<&str>, current: &Endpoint) -> Endpoint {
    let Some(bucket) = bucket else { return current.clone() };
    if current.signing_name != "s3" {
        return current.clone();
    }
    remembered(bucket, endpoint_url).unwrap_or_else(|| current.clone())
}

/// `true` when `response` names a different region and that region resolved to an endpoint.
///
/// The resolved endpoint is cached, so the caller's next attempt — and every later
/// request for this bucket — sends there directly. One redirect per request is the
/// caller's bound; this only decides whether *this* response is one.
pub fn note(
    ruleset: Option<&serde_json::Value>,
    response: &Response,
    attempt_region: &str,
    bucket: Option<&str>,
    endpoint_url: Option<&str>,
    method: &str,
) -> bool {
    let (Some(bucket), Some(ruleset)) = (bucket, ruleset) else { return false };
    // An access-point ARN is not a bucket the redirector can retarget.
    if bucket.starts_with("arn:") {
        return false;
    }
    let Some(region) = region_of(
        response.status,
        response.header("x-amz-bucket-region").as_deref(),
        &response.text(),
        attempt_region,
        method,
        true,
    ) else {
        return false;
    };
    let Ok(endpoint) = endpoint::resolve_from_ruleset(
        ruleset,
        "s3",
        &endpoint::EndpointParams {
            region: Some(region),
            bucket: Some(bucket.to_string()),
            endpoint_url: endpoint_url.map(str::to_string),
            ..Default::default()
        },
    ) else {
        return false;
    };
    // Resolving back to the region we just signed for is not progress.
    if endpoint.signing_region == attempt_region {
        return false;
    }
    remember(bucket, endpoint_url, endpoint);
    true
}

/// The region to retry in, or `None` when this response is not a region redirect.
pub fn region_of(
    status: u16,
    bucket_region_header: Option<&str>,
    body: &str,
    signing_region: &str,
    method: &str,
    allow_location_constraint: bool,
) -> Option<String> {
    // A 2xx already succeeded. S3 also sends `x-amz-bucket-region` on those, and
    // following it would resend a request that worked. Checked before the body is
    // parsed: a listing page would otherwise be read twice.
    if (200..300).contains(&status) {
        return None;
    }
    let (code, body_region) = error_fields(body);
    let header = bucket_region_header.filter(|r| !r.is_empty());

    // botocore's `S3RegionRedirectorv2.redirect_from_error`, minus the HeadBucket
    // fallback request. A 301 carries `x-amz-bucket-region`; the cases that do not
    // are the ones that fall back, and they are reported rather than guessed.
    let redirect_status = matches!(status, 301 | 302 | 307);
    let permanent = code.as_deref() == Some("PermanentRedirect");
    let wrong_region =
        code.as_deref() == Some("AuthorizationHeaderMalformed") && body_region.is_some();
    let opt_in = allow_location_constraint
        && code.as_deref() == Some("IllegalLocationConstraintException");
    // A HEAD has no body, so the error code arrives as the status. The header is what
    // makes it a region mismatch rather than a validation failure.
    let head = method.eq_ignore_ascii_case("HEAD")
        && matches!(status, 301 | 400)
        && header.is_some();
    if !(redirect_status || permanent || wrong_region || opt_in || head) {
        return None;
    }

    let region = header.or(body_region.as_deref())?;
    if !valid_region(region) || region == signing_region {
        return None;
    }
    Some(region.to_string())
}

/// `Code` and `Region` out of an S3 `<Error>` document. Absent rather than empty.
fn error_fields(body: &str) -> (Option<String>, Option<String>) {
    let Ok(root) = super::xml::parse(body) else { return (None, None) };
    let err = root.child("Error").unwrap_or(&root);
    (non_empty(err.get("Code")), non_empty(err.get("Region")))
}

fn non_empty(value: &str) -> Option<String> {
    if value.is_empty() { None } else { Some(value.to_string()) }
}

/// botocore's `validate_region_name`: a DNS label, not all digits, no leading or trailing
/// hyphen. A region that fails it is not followed — the value came from the network, and
/// it is about to become a host label and a signing region.
fn valid_region(region: &str) -> bool {
    if region.is_empty() || region.len() > 63 || region.starts_with('-') || region.ends_with('-')
    {
        return false;
    }
    if region.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    region.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    const REDIRECT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
        <Error>
          <Code>PermanentRedirect</Code>
          <Message>Please send all future requests to this endpoint.</Message>
          <Bucket>movies</Bucket>
        </Error>"#;

    const MALFORMED: &str = r#"<Error>
          <Code>AuthorizationHeaderMalformed</Code>
          <Message>the region 'us-west-2' is wrong; expecting 'us-east-1'</Message>
          <Region>us-east-1</Region>
        </Error>"#;

    #[test]
    fn a_301_uses_the_bucket_region_header() {
        let region = region_of(301, Some("us-east-1"), REDIRECT, "us-west-2", "GET", true);
        assert_eq!(region.as_deref(), Some("us-east-1"));
    }

    #[test]
    fn a_malformed_authorization_header_uses_the_body() {
        let region = region_of(400, None, MALFORMED, "us-west-2", "GET", true);
        assert_eq!(region.as_deref(), Some("us-east-1"));
    }

    /// A successful listing also carries `x-amz-bucket-region`. Following it would
    /// resend a request that already worked.
    #[test]
    fn a_success_is_not_a_redirect() {
        let body = "<ListBucketResult><Name>movies</Name></ListBucketResult>";
        assert!(region_of(200, Some("us-east-1"), body, "us-west-2", "GET", true).is_none());
    }

    #[test]
    fn the_region_we_already_signed_for_is_not_a_redirect() {
        assert!(region_of(301, Some("us-west-2"), REDIRECT, "us-west-2", "GET", true).is_none());
    }

    #[test]
    fn an_access_denied_is_not_a_redirect() {
        let body = "<Error><Code>AccessDenied</Code><Message>no</Message></Error>";
        assert!(region_of(403, Some("us-east-1"), body, "us-west-2", "GET", true).is_none());
    }

    #[test]
    fn a_region_that_is_not_a_dns_label_is_refused() {
        assert!(region_of(301, Some("us-east-1.amazonaws.com"), REDIRECT, "us-west-2", "GET", true).is_none());
        assert!(region_of(301, Some("-east-1"), REDIRECT, "us-west-2", "GET", true).is_none());
        assert!(region_of(301, Some("123"), REDIRECT, "us-west-2", "GET", true).is_none());
    }

    /// `CreateBucket`'s constraint error is the user asking for one region and the
    /// endpoint being another. Following it would create the bucket in the endpoint's
    /// region instead.
    #[test]
    fn a_location_constraint_is_a_redirect_except_when_told_not_to_be() {
        let body = "<Error><Code>IllegalLocationConstraintException</Code>\
                    <Region>eu-west-1</Region></Error>";
        assert_eq!(
            region_of(400, None, body, "us-west-2", "PUT", true).as_deref(),
            Some("eu-west-1")
        );
        assert!(region_of(400, None, body, "us-west-2", "PUT", false).is_none());
    }

    #[test]
    fn a_head_with_no_body_still_redirects() {
        assert_eq!(
            region_of(400, Some("eu-central-1"), "", "us-west-2", "HEAD", true).as_deref(),
            Some("eu-central-1")
        );
        // The same status on a GET is a real error unless the body says otherwise.
        assert!(region_of(400, Some("eu-central-1"), "", "us-west-2", "GET", true).is_none());
    }
}
