//! Protocol dispatch: turning a model + parsed arguments into a wire request, and a
//! response back into printable JSON.
//!
//! Keeping this out of `main` means the binary's flow reads the same for every protocol,
//! and adding one is a matter of extending two `match` arms.

use awsc_model::shape::{OperationShape, StructureShape};
use awsc_model::{Model, Protocol};
use awsc_runtime::http::Body;
use awsc_protocol::{
    aws_json, cbor, ec2_query, http_binding, json, query, response_fixups, xml, ProtocolError,
};
use serde_json::Value;

/// Everything needed to issue the HTTP request, independent of protocol.
pub struct WireRequest {
    pub method: String,
    /// Path relative to the endpoint, starting with `/`.
    pub path: String,
    /// Already-encoded query string, without the leading `?`.
    pub query: String,
    pub content_type: Option<String>,
    /// Protocol-specific headers, such as `X-Amz-Target`.
    pub headers: Vec<(String, String)>,
    /// Bytes, not text: `rpcv2Cbor` bodies are binary — and a streaming payload is not
    /// bytes at all, but a file read while the request is in flight.
    pub body: awsc_runtime::http::Body,
}

/// Whether the service requires a checksum header on this operation's request body.
///
/// Two spellings: `aws.protocols#httpChecksum` with `requestChecksumRequired` (S3), and
/// the older `smithy.api#httpChecksumRequired` (S3 Control). Without it the service
/// refuses the request outright — `PutBucketTagging` answers "Missing required header for
/// this request: Content-MD5 OR x-amz-checksum-*".
fn requires_request_checksum(op: &OperationShape) -> bool {
    if op.traits.has("smithy.api#httpChecksumRequired") {
        return true;
    }
    op.traits
        .get("aws.protocols#httpChecksum")
        .and_then(|v| v.get("requestChecksumRequired").cloned())
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Copy `input`, filling any idempotency-token member the caller left out.
///
/// Returns `None` when nothing was missing, so the caller's document is serialized
/// unchanged. botocore's `generate_idempotent_uuid` does the same: a top-level member
/// carrying `smithy.api#idempotencyToken` (on the member, or on the shape it targets)
/// becomes `uuid4()` when the input does not name it. Secrets Manager's
/// `ClientRequestToken` is one of these. The service requires the value on the wire,
/// and the reference CLI generates it.
fn fill_idempotency_tokens(
    model: &Model,
    input_shape: Option<&StructureShape>,
    input: Option<&Value>,
) -> Result<Option<Value>, ProtocolError> {
    let Some(shape) = input_shape else { return Ok(None) };
    // A non-object is the serializer's error to report. Inventing a token beside it
    // would hide that.
    let present = match input {
        Some(Value::Object(map)) => Some(map),
        Some(_) => return Ok(None),
        None => None,
    };
    let missing: Vec<&str> = shape
        .members
        .iter()
        .filter(|(name, member)| {
            is_idempotency_token(model, member) && present.is_none_or(|map| !map.contains_key(*name))
        })
        .map(|(name, _)| name.as_str())
        .collect();
    if missing.is_empty() {
        return Ok(None);
    }
    let mut object = input.cloned().unwrap_or_else(|| Value::Object(Default::default()));
    let Some(map) = object.as_object_mut() else { return Ok(None) };
    for name in missing {
        map.insert(name.to_string(), Value::String(uuid_v4()?));
    }
    Ok(Some(object))
}

/// The trait lives on the member in most models and on the target string in a few.
/// botocore merges the two before it looks, so either place counts.
fn is_idempotency_token(model: &Model, member: &awsc_model::shape::Member) -> bool {
    member.traits.has("smithy.api#idempotencyToken")
        || model
            .shape(&member.target)
            .is_some_and(|shape| shape.traits().has("smithy.api#idempotencyToken"))
}

/// `str(uuid.uuid4())`: 36 lowercase hex characters, hyphens, version 4, RFC 4122 variant.
fn uuid_v4() -> Result<String, ProtocolError> {
    use aws_lc_rs::rand::{SecureRandom, SystemRandom};
    let mut bytes = [0u8; 16];
    SystemRandom::new().fill(&mut bytes).map_err(|_| ProtocolError::Random)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5],
        bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    ))
}

/// Attach `Content-MD5` when the operation demands a checksum.
///
/// Only for a body already in memory: a streaming payload would have to be read twice,
/// and no operation that requires a checksum takes one.
fn add_request_checksum(op: &OperationShape, wire: &mut WireRequest) {
    if !requires_request_checksum(op) {
        return;
    }
    let Some(bytes) = wire.body.as_bytes() else { return };
    use base64ct::{Base64, Encoding};
    use md5::{Digest, Md5};
    wire
        .headers
        .push(("Content-MD5".to_string(), Base64::encode_string(&Md5::digest(bytes))));
}

/// Build the request for an operation.
pub fn serialize(
    model: &Model,
    protocol: Protocol,
    operation_wire_name: &str,
    op: &OperationShape,
    input_shape: Option<&StructureShape>,
    input: Option<&Value>,
) -> Result<WireRequest, ProtocolError> {
    let api_version = model.service().map(|s| s.version.clone()).unwrap_or_default();

    // One token for the whole call, including its retries: the wire document is built
    // once, here, and every attempt resends it. A fresh token per attempt would make
    // the retry a different request.
    let generated = fill_idempotency_tokens(model, input_shape, input)?;
    let input = generated.as_ref().or(input);

    let mut wire = serialize_for_protocol(
        model,
        protocol,
        &api_version,
        operation_wire_name,
        op,
        input_shape,
        input,
    )?;
    add_request_checksum(op, &mut wire);
    Ok(wire)
}

#[allow(clippy::too_many_arguments)]
fn serialize_for_protocol(
    model: &Model,
    protocol: Protocol,
    api_version: &str,
    operation_wire_name: &str,
    op: &OperationShape,
    input_shape: Option<&StructureShape>,
    input: Option<&Value>,
) -> Result<WireRequest, ProtocolError> {
    match protocol {
        Protocol::AwsQuery => Ok(WireRequest {
            method: "POST".into(),
            path: "/".into(),
            query: String::new(),
            content_type: Some(FORM_CONTENT_TYPE.into()),
            headers: Vec::new(),
            body: Body::from_vec(
                query::serialize(model, operation_wire_name, api_version, input_shape, input)?
                    .into_bytes(),
            ),
        }),

        Protocol::Ec2Query => Ok(WireRequest {
            method: "POST".into(),
            path: "/".into(),
            query: String::new(),
            content_type: Some(FORM_CONTENT_TYPE.into()),
            headers: Vec::new(),
            body: Body::from_vec(
                ec2_query::serialize(model, operation_wire_name, api_version, input_shape, input)?
                    .into_bytes(),
            ),
        }),

        Protocol::AwsJson1_0 | Protocol::AwsJson1_1 => {
            let prefix = aws_json::target_prefix(model, protocol).unwrap_or_default();
            let request = aws_json::serialize(
                model,
                protocol,
                &prefix,
                operation_wire_name,
                input_shape,
                input,
            )?;
            Ok(WireRequest {
                method: "POST".into(),
                path: "/".into(),
                query: String::new(),
                content_type: Some(request.content_type),
                headers: vec![("x-amz-target".into(), request.target)],
                body: Body::from_vec(request.body.into_bytes()),
            })
        }

        Protocol::RestJson1 | Protocol::RestXml => serialize_rest(
            model,
            protocol,
            operation_wire_name,
            op,
            input_shape,
            input,
        ),

        // Smithy RPC v2 CBOR names the operation in the URL rather than a header, and
        // `Accept` is required: the service will not assume the client wants CBOR back.
        Protocol::Rpcv2Cbor => Ok(WireRequest {
            method: "POST".into(),
            path: cbor::request_path(model.service_id().name(), operation_wire_name),
            query: String::new(),
            content_type: Some(cbor::CONTENT_TYPE.into()),
            headers: vec![
                (cbor::PROTOCOL_HEADER.0.into(), cbor::PROTOCOL_HEADER.1.into()),
                ("accept".into(), cbor::CONTENT_TYPE.into()),
            ],
            body: Body::from_vec(cbor::serialize(model, input_shape, input)?),
        }),

        Protocol::Unknown => Err(ProtocolError::Unsupported(format!("{protocol:?}"))),
    }
}

const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded; charset=utf-8";

/// The `rest*` protocols share their HTTP binding and differ only in body encoding.
fn serialize_rest(
    model: &Model,
    protocol: Protocol,
    operation_wire_name: &str,
    op: &OperationShape,
    input_shape: Option<&StructureShape>,
    input: Option<&Value>,
) -> Result<WireRequest, ProtocolError> {
    let http = http_binding::http_trait(op).ok_or_else(|| {
        ProtocolError::Unsupported("operation has no smithy.api#http trait".into())
    })?;

    let empty = Value::Object(serde_json::Map::new());
    let values = input.unwrap_or(&empty);

    let Some(shape) = input_shape else {
        return Ok(WireRequest {
            method: http.method,
            path: http.uri,
            query: String::new(),
            content_type: None,
            headers: Vec::new(),
            body: Body::Empty,
        });
    };

    let bound = http_binding::bind(model, protocol, &http, shape, values)?;

    // Body members are whatever the HTTP bindings did not claim.
    let body_value = match &bound.payload_member {
        Some(name) => values.get(name).cloned().unwrap_or(Value::Null),
        None => {
            let mut object = serde_json::Map::new();
            for name in &bound.body_members {
                if let Some(v) = values.get(name) {
                    object.insert(name.clone(), v.clone());
                }
            }
            Value::Object(object)
        }
    };

    // A streaming payload is a file to send, not a value to encode. The member's value is
    // the path, and it is described rather than read so a 5 GB upload stays a file handle
    // instead of a 5 GB allocation.
    let streaming_payload = bound.payload_member.as_deref().and_then(|name| {
        let member = shape.members.get(name)?;
        let target = model.shape(&member.target)?;
        match target {
            awsc_model::Shape::Blob(blob) if blob.traits.has("smithy.api#streaming") => {
                values.get(name)?.as_str().map(str::to_string)
            }
            _ => None,
        }
    });
    if let Some(path) = streaming_payload {
        let len = std::fs::metadata(&path)
            .map_err(|e| ProtocolError::Unsupported(format!("{path}: {e}")))?
            .len();
        return Ok(WireRequest {
            method: http.method,
            path: bound.path,
            query: encode_query(&bound.query),
            content_type: None,
            headers: bound.headers,
            body: Body::FileRange { path: path.into(), offset: 0, len },
        });
    }

    // A non-streaming blob payload is the request body *as bytes*. The argument layer
    // normalised it to base64, so it decodes here rather than being sent as its own
    // encoding -- `lambda invoke --payload` puts the decoded JSON on the wire, not the
    // base64 text of it. Done before the per-protocol encoders because it is the same
    // answer for all of them.
    if let Some(name) = bound.payload_member.as_deref() {
        let is_blob = shape
            .members
            .get(name)
            .and_then(|m| model.shape(&m.target))
            .is_some_and(|t| matches!(t, awsc_model::Shape::Blob(_)));
        if is_blob {
            let decoded = body_value
                .as_str()
                .and_then(awsc_protocol::shapes::base64_decode)
                .unwrap_or_default();
            return Ok(WireRequest {
                method: http.method,
                path: bound.path,
                query: encode_query(&bound.query),
                content_type: None,
                headers: bound.headers,
                body: Body::from_vec(decoded),
            });
        }
    }

    let (body, content_type) = match protocol {
        Protocol::RestJson1 => {
            let encoded = if bound.payload_member.is_some() {
                json::to_python_json(&body_value)
            } else {
                json::to_python_json(&json::serialize_structure(
                    model, protocol, shape, &body_value,
                )?)
            };
            // botocore sets application/json whenever a body is present, including `{}`.
            (encoded, Some("application/json".to_string()))
        }
        // botocore sends NO Content-Type for restXml, even with an XML body; the spec
        // says application/xml but the reference omits it, and services accept that.
        Protocol::RestXml => (
            xml_request_body(model, shape, &bound, values, operation_wire_name)?,
            None,
        ),
        _ => (String::new(), None),
    };

    Ok(WireRequest {
        method: http.method,
        path: bound.path,
        query: encode_query(&bound.query),
        content_type,
        headers: bound.headers,
        body: Body::from_vec(body.into_bytes()),
    })
}

fn encode_query(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| if v.is_empty() { k.clone() } else { format!("{k}={v}") })
        .collect::<Vec<_>>()
        .join("&")
}

/// The XML request body for a `restXml` operation.
///
/// Either the payload member serialised on its own, or the members the HTTP bindings did
/// not claim wrapped in the operation's request element. Returning an empty string when
/// there is nothing to send is right; returning one when there *is* something was the bug
/// — `s3api put-bucket-tagging` sent no body at all and the service saw an empty request.
fn xml_request_body(
    model: &Model,
    shape: &StructureShape,
    bound: &http_binding::BoundRequest,
    values: &Value,
    operation_wire_name: &str,
) -> Result<String, ProtocolError> {
    if let Some(name) = &bound.payload_member {
        let Some(member) = shape.members.get(name) else { return Ok(String::new()) };
        let Some(value) = values.get(name) else { return Ok(String::new()) };
        if value.is_null() {
            return Ok(String::new());
        }
        return xml::serialize_payload(model, name, member, value);
    }

    let mut object = serde_json::Map::new();
    for name in &bound.body_members {
        if let Some(v) = values.get(name) {
            if !v.is_null() {
                object.insert(name.clone(), v.clone());
            }
        }
    }
    if object.is_empty() {
        return Ok(String::new());
    }
    xml::serialize_request(model, shape, operation_wire_name, &Value::Object(object))
}

/// Parse a successful response body into the JSON the CLI prints.
///
/// Applies the per-service `after-call` fix-ups afterwards, since those change what is
/// printed even though they sit outside the protocol.
pub fn parse_response(
    model: &Model,
    protocol: Protocol,
    operation_wire_name: &str,
    output_shape: Option<&StructureShape>,
    body: &[u8],
) -> Result<Value, ProtocolError> {
    let mut value = parse_body(model, protocol, operation_wire_name, output_shape, body)?;
    if let Some(shape) = output_shape {
        // IAM policy documents arrive URL-encoded; the reference decodes them.
        response_fixups::decode_policy_documents(model, shape, &mut value);
    }
    Ok(value)
}

fn parse_body(
    model: &Model,
    protocol: Protocol,
    operation_wire_name: &str,
    output_shape: Option<&StructureShape>,
    body: &[u8],
) -> Result<Value, ProtocolError> {
    // Only CBOR needs the raw bytes; the rest are text protocols. Lossy rather than
    // strict, because a response that is almost UTF-8 should still be parsed as far as
    // it goes instead of failing whole.
    let text = || String::from_utf8_lossy(body);
    match protocol {
        Protocol::AwsQuery => {
            xml::parse_response(model, operation_wire_name, output_shape, &text())
        }
        Protocol::Ec2Query => ec2_query::parse_response(model, output_shape, &text()),
        Protocol::AwsJson1_0 | Protocol::AwsJson1_1 | Protocol::RestJson1 => {
            aws_json::parse_response(model, protocol, output_shape, &text())
        }
        Protocol::RestXml => xml::parse_response(model, operation_wire_name, output_shape, &text()),
        Protocol::Rpcv2Cbor => cbor::parse_response(model, output_shape, body),
        Protocol::Unknown => Err(ProtocolError::Unsupported(format!("{protocol:?}"))),
    }
}

/// Extract `(code, message)` from an error response.
pub fn parse_error(
    protocol: Protocol,
    status: u16,
    raw: &[u8],
    error_type_header: Option<&str>,
) -> (String, String) {
    let body = &String::from_utf8_lossy(raw);
    // A body-less failure — every HEAD operation — carries the status and nothing else.
    // Reporting that as `(Unknown)` with an empty message describes nothing; the status
    // is the only fact available, so use it.
    let fallback = || {
        let text = body.trim();
        if text.is_empty() {
            (status.to_string(), awsc_runtime::http::reason_phrase(status).to_string())
        } else {
            // A CBOR body is binary, so its lossy text is noise rather than a message.
            let readable = if protocol == Protocol::Rpcv2Cbor { "" } else { text };
            ("Unknown".to_string(), readable.to_string())
        }
    };

    match protocol {
        Protocol::Rpcv2Cbor => match cbor::parse_error(raw) {
            Some(e) => (e.code, e.message),
            None => fallback(),
        },
        Protocol::AwsQuery | Protocol::Ec2Query | Protocol::RestXml => match xml::parse_error(body)
        {
            Some(e) => (e.code, e.message),
            None => fallback(),
        },
        // Only restJson1 consults X-Amzn-Errortype; the awsJson parsers read `__type`
        // from the body alone, which is what botocore does.
        Protocol::AwsJson1_0 | Protocol::AwsJson1_1 => match json::parse_error(body, None) {
            Some(e) => (e.code, e.message),
            None => fallback(),
        },
        Protocol::RestJson1 => match json::parse_error(body, error_type_header) {
            Some(e) => (e.code, e.message),
            None => fallback(),
        },
        _ => fallback(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const MODEL: &str = r#"{
      "smithy": "2.0",
      "shapes": {
        "com.x#S": {"type": "service", "version": "2017-10-17",
          "operations": [
            {"target": "com.x#CreateSecret"},
            {"target": "com.x#PutThing"}
          ],
          "traits": {"aws.protocols#awsJson1_1": {}}},
        "com.x#Str": {"type": "string"},
        "com.x#Token": {"type": "string", "traits": {"smithy.api#idempotencyToken": {}}},
        "com.x#CreateSecret": {"type": "operation",
          "input": {"target": "com.x#CreateSecretRequest"}},
        "com.x#CreateSecretRequest": {"type": "structure", "members": {
          "Name": {"target": "com.x#Str", "traits": {"smithy.api#required": {}}},
          "ClientRequestToken": {"target": "com.x#Str",
            "traits": {"smithy.api#idempotencyToken": {}}},
          "SecretString": {"target": "com.x#Str"}
        }},
        "com.x#PutThing": {"type": "operation",
          "input": {"target": "com.x#PutThingRequest"}},
        "com.x#PutThingRequest": {"type": "structure", "members": {
          "ClientToken": {"target": "com.x#Token"}
        }}
      }
    }"#;

    fn model() -> Model {
        Model::from_json(MODEL.as_bytes()).expect("model")
    }

    /// The body `serialize` would send for `operation` and `input`.
    fn body(operation: &str, input: Option<&Value>) -> Value {
        let model = model();
        let (id, op) = model.operation(operation).expect("operation");
        let input_shape = model.operation_input(op).expect("input shape");
        let wire = serialize(&model, model.protocol().unwrap(), id.name(), op, input_shape, input)
            .expect("serialize");
        serde_json::from_slice(wire.body.as_bytes().expect("body")).expect("json")
    }

    fn is_uuid_v4(value: &str) -> bool {
        let bytes = value.as_bytes();
        if bytes.len() != 36 {
            return false;
        }
        let hex = |i: usize| bytes[i].is_ascii_hexdigit();
        let hyphen = |i: usize| bytes[i] == b'-';
        (0..8).all(hex)
            && hyphen(8)
            && (9..13).all(hex)
            && hyphen(13)
            && bytes[14] == b'4'
            && (15..18).all(hex)
            && hyphen(18)
            && matches!(bytes[19], b'8' | b'9' | b'a' | b'b')
            && (20..23).all(hex)
            && hyphen(23)
            && (24..36).all(hex)
    }

    #[test]
    fn a_missing_client_request_token_is_a_uuid() {
        let document = body("create-secret", Some(&json!({"Name": "app", "SecretString": "value"})));
        let token = document["ClientRequestToken"].as_str().expect("token");
        assert!(is_uuid_v4(token), "{token}");
        assert_eq!(document["Name"], "app");
        assert_eq!(document["SecretString"], "value");
    }

    #[test]
    fn an_omitted_document_still_gets_a_token() {
        let document = body("create-secret", None);
        assert!(is_uuid_v4(document["ClientRequestToken"].as_str().unwrap_or("")));
    }

    #[test]
    fn a_caller_supplied_token_is_kept() {
        let document = body(
            "create-secret",
            Some(&json!({"Name": "app", "ClientRequestToken": "already-chosen-by-the-caller-ok"})),
        );
        assert_eq!(document["ClientRequestToken"], "already-chosen-by-the-caller-ok");
    }

    #[test]
    fn each_call_gets_its_own_token() {
        let first = body("create-secret", None);
        let second = body("create-secret", None);
        assert_ne!(first["ClientRequestToken"], second["ClientRequestToken"]);
    }

    /// A few models put the trait on the string shape rather than on the member.
    #[test]
    fn a_token_trait_on_the_target_shape_counts() {
        let document = body("put-thing", None);
        assert!(is_uuid_v4(document["ClientToken"].as_str().unwrap_or("")));
    }
}
