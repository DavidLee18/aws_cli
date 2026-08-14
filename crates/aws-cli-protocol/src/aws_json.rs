//! `awsJson1_0` and `awsJson1_1`.
//!
//! The simplest of the protocols: always `POST /`, the operation is named by an
//! `X-Amz-Target` header, and the body is the input structure as JSON. Only the
//! content-type version differs between the two.

use aws_cli_model::shape::StructureShape;
use aws_cli_model::Model;
use serde_json::Value;

use crate::json;
use crate::shapes::Protocol;
use crate::ProtocolError;

/// A prepared awsJson request.
#[derive(Debug)]
pub struct JsonRequest {
    pub target: String,
    pub content_type: String,
    pub body: String,
}

/// Build the request for an operation.
///
/// `target_prefix` comes from the protocol trait's `targetPrefix`, falling back to the
/// service shape's name — botocore keys `X-Amz-Target` as `{prefix}.{Operation}`.
pub fn serialize(
    model: &Model,
    protocol: Protocol,
    target_prefix: &str,
    operation_wire_name: &str,
    input_shape: Option<&StructureShape>,
    input: Option<&Value>,
) -> Result<JsonRequest, ProtocolError> {
    let body = match (input_shape, input) {
        (Some(shape), Some(value)) => {
            // Rendered with Python's separators, because the body is hashed into the
            // signature — see `json::to_python_json`.
            json::to_python_json(&json::serialize_structure(model, protocol, shape, value)?)
        }
        // An operation with no supplied input still sends an empty JSON object, not an
        // empty body — services reject the latter.
        _ => "{}".to_string(),
    };

    Ok(JsonRequest {
        target: format!("{target_prefix}.{operation_wire_name}"),
        content_type: content_type(protocol).to_string(),
        body,
    })
}

pub fn content_type(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::AwsJson1_0 => "application/x-amz-json-1.0",
        _ => "application/x-amz-json-1.1",
    }
}

/// The `targetPrefix` for a service.
///
/// Resolution order, and the order matters:
///
/// 1. the protocol trait's own `targetPrefix`, if a model ever carries one;
/// 2. the vendored table keyed by `sdkId`, which is the authority — see
///    [`aws_cli_model::protocol_metadata`] for why it cannot be derived;
/// 3. the service shape name, correct for 149 of 152 services and the only option for a
///    service newer than the vendored table.
pub fn target_prefix(model: &Model, protocol: Protocol) -> Option<String> {
    let traits = &model.service().ok()?.traits;
    let trait_id = match protocol {
        Protocol::AwsJson1_0 => "aws.protocols#awsJson1_0",
        Protocol::AwsJson1_1 => "aws.protocols#awsJson1_1",
        _ => return None,
    };
    if let Some(from_trait) = traits
        .get(trait_id)
        .and_then(|v| v.get("targetPrefix"))
        .and_then(|v| v.as_str())
    {
        return Some(from_trait.to_string());
    }
    if let Some(vendored) = model
        .sdk_id()
        .ok()
        .flatten()
        .and_then(aws_cli_model::protocol_metadata::target_prefix)
    {
        return Some(vendored.to_string());
    }
    Some(model.service_id().name().to_string())
}

/// Parse a successful response body.
pub fn parse_response(
    model: &Model,
    protocol: Protocol,
    output_shape: Option<&StructureShape>,
    body: &str,
) -> Result<Value, ProtocolError> {
    let Some(shape) = output_shape else {
        return Ok(Value::Object(serde_json::Map::new()));
    };
    let parsed: Value = serde_json::from_str(body.trim()).unwrap_or(Value::Null);
    json::parse_structure(model, protocol, shape, &parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_types_match_the_protocol_version() {
        assert_eq!(content_type(Protocol::AwsJson1_0), "application/x-amz-json-1.0");
        assert_eq!(content_type(Protocol::AwsJson1_1), "application/x-amz-json-1.1");
    }

    #[test]
    fn empty_input_still_sends_an_object() {
        let model = Model::from_json(
            br#"{"smithy":"2.0","shapes":{"com.x#S":{"type":"service","version":"1","traits":{}}}}"#,
        )
        .unwrap();
        let request =
            serialize(&model, Protocol::AwsJson1_0, "DynamoDB_20120810", "ListTables", None, None)
                .unwrap();
        assert_eq!(request.body, "{}");
        assert_eq!(request.target, "DynamoDB_20120810.ListTables");
    }
}
