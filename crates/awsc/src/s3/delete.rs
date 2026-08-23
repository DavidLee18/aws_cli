//! Deleting many objects in one request.
//!
//! `DeleteObject` removes exactly one key, so `rm --recursive` over a bucket of 4,000
//! objects is 4,000 signed round trips. `DeleteObjects` takes up to 1,000 keys per POST,
//! which turns the same work into four. The reference CLI does not batch — its handler
//! maps `delete_object`, singular — so this is a place to be faster rather than equal.
//!
//! The subtlety is the response. `DeleteObjects` answers **200 with per-key results**: a
//! request that deleted 999 of 1,000 keys and refused one still returns success, with the
//! refusal sitting in an `<Error>` element in the body. Treating the HTTP status as the
//! outcome would report a thousand successful deletions and exit 0. So every key is
//! accounted for individually here, and a key the service mentions in *neither* list is
//! reported as a failure rather than assumed deleted — a truncated or unexpected response
//! must not read as "all fine".

use super::conn::Conn;
use super::pool::Pool;
use super::xml;
use crate::Failure;
use aws_cli_runtime::http;

/// The API's hard limit on keys per request.
pub const MAX_KEYS: usize = 1000;

/// Below this, batching costs a larger body and gains nothing over the direct call.
const MIN_BATCH: usize = 2;

/// Escape text for an XML element body.
///
/// Only the three characters that can end an element or start an entity. Keys are
/// arbitrary bytes as far as S3 is concerned, but they arrived here from a listing that
/// was itself XML, so anything unrepresentable would already have failed upstream.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
    out
}

/// The `<Delete>` document for one batch.
///
/// `Quiet` is deliberately left off: in quiet mode S3 omits the `<Deleted>` list, and the
/// CLI needs those keys to print its `delete:` lines and to tell a deleted key from one
/// the response never mentioned.
fn request_body(keys: &[&str]) -> Vec<u8> {
    let mut body = String::from("<Delete>");
    for key in keys {
        body.push_str("<Object><Key>");
        body.push_str(&escape(key));
        body.push_str("</Key></Object>");
    }
    body.push_str("</Delete>");
    body.into_bytes()
}

/// `Content-MD5` is required on `DeleteObjects` unless a checksum header replaces it.
fn content_md5(body: &[u8]) -> String {
    use base64ct::{Base64, Encoding};
    use md5::{Digest, Md5};
    Base64::encode_string(&Md5::digest(body))
}

/// What the service said about each key in a batch, in the order they were sent.
fn outcomes(response_body: &str, keys: &[&str]) -> Vec<Result<(), String>> {
    let root = match xml::parse(response_body) {
        Ok(root) => root,
        Err(e) => return keys.iter().map(|_| Err(format!("unreadable DeleteObjects response: {e}"))).collect(),
    };

    let mut deleted: Vec<&str> = Vec::new();
    for entry in root.all("Deleted") {
        deleted.push(entry.get("Key"));
    }
    let mut errors: Vec<(&str, String)> = Vec::new();
    for entry in root.all("Error") {
        let code = entry.get("Code");
        let message = entry.get("Message");
        errors.push((
            entry.get("Key"),
            format!(
                "An error occurred ({code}) when calling the DeleteObjects operation: {message}"
            ),
        ));
    }

    keys.iter()
        .map(|key| {
            if let Some((_, message)) = errors.iter().find(|(k, _)| k == key) {
                return Err(message.clone());
            }
            if deleted.iter().any(|k| k == key) {
                return Ok(());
            }
            // Neither list mentions it. The object may well be gone, but nothing here
            // says so, and reporting an unverified deletion is the failure this module
            // exists to avoid.
            Err(format!(
                "An error occurred (NoResult) when calling the DeleteObjects operation: \
                 the response did not report an outcome for {key}"
            ))
        })
        .collect()
}

/// Send one batch and report each of its keys.
fn send_batch(conn: &Conn, keys: &[&str], report: &(impl Fn(&str, Result<(), String>) + Sync)) {
    let body = request_body(keys);
    let headers = vec![("Content-MD5".to_string(), content_md5(&body))];
    let sent = conn.send_checked(
        "DeleteObjects",
        "POST",
        "/",
        "delete",
        &headers,
        http::Body::from_vec(body),
    );

    match sent {
        // The whole request failed, so nothing in it was deleted.
        Err(failure) => {
            let message = failure.message().to_string();
            for key in keys {
                report(key, Err(message.clone()));
            }
        }
        Ok(response) => {
            let text = response.text();
            for (key, outcome) in keys.iter().zip(outcomes(&text, keys)) {
                report(key, outcome);
            }
        }
    }
}

/// Delete every key, batched, calling `report` exactly once per key.
///
/// `report` is called from worker threads and may be called for keys out of order.
pub fn batched(
    conn: &Conn,
    keys: &[String],
    concurrency: Option<usize>,
    report: impl Fn(&str, Result<(), String>) + Sync,
) {
    if keys.is_empty() {
        return;
    }
    // One key is not worth an XML document and a `Content-MD5`, and `DeleteObject`'s
    // failures name the operation the user would expect to see.
    if keys.len() < MIN_BATCH {
        let key = &keys[0];
        report(key, single(conn, key).map_err(|e| e.message().to_string()));
        return;
    }
    let batches: Vec<Vec<&str>> =
        keys.chunks(MAX_KEYS).map(|c| c.iter().map(String::as_str).collect()).collect();

    let pool = Pool::new(concurrency);
    pool.run(&batches, concurrency.is_none(), |batch| {
        send_batch(conn, batch, &report);
    });
}

/// Delete one key on its own, for the single-object `rm` path.
pub fn single(conn: &Conn, key: &str) -> Result<(), Failure> {
    conn.send_checked("DeleteObject", "DELETE", &conn.object_path(key), "", &[], http::Body::Empty)
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_escapes_keys() {
        let body = String::from_utf8(request_body(&["a&b", "c<d>"])).unwrap();
        assert_eq!(
            body,
            "<Delete><Object><Key>a&amp;b</Key></Object>\
             <Object><Key>c&lt;d&gt;</Key></Object></Delete>"
        );
    }

    /// The published example for an empty body, so a signing change that alters the
    /// header is visible.
    #[test]
    fn md5_is_base64_of_the_digest() {
        assert_eq!(content_md5(b""), "1B2M2Y8AsgTpgAmY7PhCfg==");
    }

    /// A 200 that carries an `<Error>` is a partial failure, not a success.
    #[test]
    fn per_key_errors_are_not_swallowed_by_a_200() {
        let body = "<DeleteResult>\
            <Deleted><Key>ok</Key></Deleted>\
            <Error><Key>nope</Key><Code>AccessDenied</Code><Message>Access Denied</Message></Error>\
            </DeleteResult>";
        let out = outcomes(body, &["ok", "nope"]);
        assert!(out[0].is_ok());
        assert_eq!(
            out[1].as_ref().unwrap_err(),
            "An error occurred (AccessDenied) when calling the DeleteObjects operation: Access Denied"
        );
    }

    /// A key the response never mentions is a failure, not an assumed deletion.
    #[test]
    fn unmentioned_keys_fail() {
        let body = "<DeleteResult><Deleted><Key>a</Key></Deleted></DeleteResult>";
        let out = outcomes(body, &["a", "b"]);
        assert!(out[0].is_ok());
        assert!(out[1].as_ref().unwrap_err().contains("did not report an outcome for b"));
    }

    #[test]
    fn an_unreadable_response_fails_every_key() {
        let out = outcomes("<<<not xml", &["a", "b"]);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|r| r.is_err()));
    }
}
