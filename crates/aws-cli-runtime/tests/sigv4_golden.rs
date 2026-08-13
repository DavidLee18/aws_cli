//! Pins the signer against a request captured from the reference CLI.
//!
//! SigV4 is deterministic given credentials, timestamp and request, so this validates
//! byte-for-byte agreement with botocore offline — no network, no real credentials.
//! Fixture: `tests/golden/sigv4-sts-get-caller-identity.json`, captured via
//! `aws sts get-caller-identity --debug` with the documented AWS example key.

use aws_cli_runtime::sigv4::{sign, Credentials, SigningContext, SigningRequest};
use serde_json::Value;

fn fixture() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/golden/sigv4-sts-get-caller-identity.json");
    serde_json::from_slice(&std::fs::read(&path).expect("fixture should exist")).unwrap()
}

#[test]
fn reproduces_reference_signature_exactly() {
    let f = fixture();
    let (creds, req, sig, expected) = (&f["credentials"], &f["request"], &f["signing"], &f["expected"]);

    let credentials = Credentials {
        access_key_id: creds["access_key_id"].as_str().unwrap().to_string(),
        secret_access_key: creds["secret_access_key"].as_str().unwrap().to_string(),
        session_token: None,
        expires_at: None,
    };

    let ctx = SigningContext {
        credentials: &credentials,
        region: sig["region"].as_str().unwrap(),
        service: sig["service"].as_str().unwrap(),
        timestamp: sig["timestamp"].as_str().unwrap(),
    };

    let body = req["body"].as_str().unwrap();
    let signing_request = SigningRequest {
        method: req["method"].as_str().unwrap(),
        path: req["path"].as_str().unwrap(),
        query: req["query"].as_str().unwrap(),
        // Exactly the three headers botocore signed, per the captured SignedHeaders.
        headers: vec![
            ("content-type".into(), req["content_type"].as_str().unwrap().to_string()),
            ("host".into(), req["host"].as_str().unwrap().to_string()),
            ("x-amz-date".into(), sig["timestamp"].as_str().unwrap().to_string()),
        ],
        body: body.as_bytes(),
    };

    let got = sign(&ctx, &signing_request);

    // Compare each stage, so a failure localises to the step that diverged rather than
    // just reporting a wrong final hash.
    assert_eq!(
        got.canonical_request,
        expected["canonical_request"].as_str().unwrap(),
        "canonical request diverged"
    );
    assert_eq!(
        got.string_to_sign,
        expected["string_to_sign"].as_str().unwrap(),
        "string to sign diverged"
    );
    assert_eq!(got.signed_headers, sig["signed_headers"].as_str().unwrap());
    assert_eq!(got.signature, expected["signature"].as_str().unwrap(), "signature diverged");
    assert_eq!(got.authorization, expected["authorization"].as_str().unwrap());
}

/// The fixture's own hashes must be internally consistent, so a bad regeneration is
/// caught here rather than silently redefining "correct".
#[test]
fn fixture_is_self_consistent() {
    use sha2::{Digest, Sha256};
    let f = fixture();
    let hex = |b: &[u8]| Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect::<String>();

    assert_eq!(
        hex(f["request"]["body"].as_str().unwrap().as_bytes()),
        f["expected"]["body_sha256"].as_str().unwrap()
    );
    assert_eq!(
        hex(f["expected"]["canonical_request"].as_str().unwrap().as_bytes()),
        f["expected"]["canonical_request_sha256"].as_str().unwrap()
    );
    assert!(f["expected"]["string_to_sign"]
        .as_str()
        .unwrap()
        .ends_with(f["expected"]["canonical_request_sha256"].as_str().unwrap()));
}
