//! End-to-end protocol tests against the real STS model.
//!
//! The live success path needs credentials, so it is covered here instead: a genuine
//! STS response body is parsed through the actual model and asserted to produce exactly
//! the JSON the reference prints.
//!
//! Skips cleanly when `models/` has not been fetched.

use aws_cli_model::Model;
use aws_cli_protocol::{query, xml};
use serde_json::json;

fn sts_model() -> Option<Model> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models/sts.json");
    Model::from_json(&std::fs::read(path).ok()?).ok()
}

#[test]
fn serializes_get_caller_identity_like_the_reference() {
    let Some(model) = sts_model() else { return };
    let (id, op) = model.operation("get-caller-identity").unwrap();
    let input = model.operation_input(op).unwrap();

    let body = query::serialize(&model, id.name(), &model.service().unwrap().version, input, None)
        .unwrap();

    // Byte-identical to the body captured from `aws sts get-caller-identity --debug`.
    assert_eq!(body, "Action=GetCallerIdentity&Version=2011-06-15");
}

#[test]
fn parses_get_caller_identity_response() {
    let Some(model) = sts_model() else { return };
    let (id, op) = model.operation("get-caller-identity").unwrap();
    let output = model.operation_output(op).unwrap();

    // A real STS response, including the namespace and the ResponseMetadata sibling
    // that must NOT appear in the output.
    let body = r#"<GetCallerIdentityResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">
      <GetCallerIdentityResult>
        <Arn>arn:aws:iam::123456789012:user/example</Arn>
        <UserId>AIDACKCEVSQ6C2EXAMPLE</UserId>
        <Account>123456789012</Account>
      </GetCallerIdentityResult>
      <ResponseMetadata>
        <RequestId>01234567-89ab-cdef-0123-456789abcdef</RequestId>
      </ResponseMetadata>
    </GetCallerIdentityResponse>"#;

    let value = xml::parse_response(&model, id.name(), output, body).unwrap();

    assert_eq!(
        value,
        json!({
            "UserId": "AIDACKCEVSQ6C2EXAMPLE",
            "Account": "123456789012",
            "Arn": "arn:aws:iam::123456789012:user/example"
        }),
        "should unwrap GetCallerIdentityResult and drop ResponseMetadata"
    );

    // Field order is user-visible in the printed JSON and must follow the MODEL, not the
    // alphabet: the reference prints UserId, Account, Arn. Verified byte-identical
    // against a live `aws sts get-caller-identity`.
    let keys: Vec<&str> = value.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    assert_eq!(keys, ["UserId", "Account", "Arn"]);
}

/// An operation with actual input members, to exercise scalar serialization beyond the
/// empty-input case.
#[test]
fn serializes_scalar_members() {
    let Some(model) = sts_model() else { return };
    let (id, op) = model.operation("assume-role").unwrap();
    let input = model.operation_input(op).unwrap();

    let body = query::serialize(
        &model,
        id.name(),
        &model.service().unwrap().version,
        input,
        Some(&json!({"RoleArn": "arn:aws:iam::1:role/r", "RoleSessionName": "s", "DurationSeconds": 900})),
    )
    .unwrap();

    assert!(body.starts_with("Action=AssumeRole&Version=2011-06-15&"), "got: {body}");
    assert!(body.contains("RoleArn=arn%3Aaws%3Aiam%3A%3A1%3Arole%2Fr"), "ARN must be encoded: {body}");
    assert!(body.contains("RoleSessionName=s"));
    assert!(body.contains("DurationSeconds=900"));
}

#[test]
fn parses_structured_response_with_nested_members() {
    let Some(model) = sts_model() else { return };
    let (id, op) = model.operation("assume-role").unwrap();
    let output = model.operation_output(op).unwrap();

    let body = r#"<AssumeRoleResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">
      <AssumeRoleResult>
        <Credentials>
          <AccessKeyId>ASIAEXAMPLE</AccessKeyId>
          <SecretAccessKey>secret</SecretAccessKey>
          <SessionToken>token</SessionToken>
          <Expiration>2026-08-13T22:00:00Z</Expiration>
        </Credentials>
        <AssumedRoleUser>
          <Arn>arn:aws:sts::1:assumed-role/r/s</Arn>
          <AssumedRoleId>AROAID:s</AssumedRoleId>
        </AssumedRoleUser>
      </AssumeRoleResult>
    </AssumeRoleResponse>"#;

    let value = xml::parse_response(&model, id.name(), output, body).unwrap();

    assert_eq!(value["Credentials"]["AccessKeyId"], "ASIAEXAMPLE");
    assert_eq!(value["AssumedRoleUser"]["AssumedRoleId"], "AROAID:s");
    assert!(value.get("PackedPolicySize").is_none(), "absent members must be omitted");
}
