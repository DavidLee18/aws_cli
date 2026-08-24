//! Protocol dispatch: turning a model + parsed arguments into a wire request, and a
//! response back into printable JSON.
//!
//! Keeping this out of `main` means the binary's flow reads the same for every protocol,
//! and adding one is a matter of extending two `match` arms.

use aws_cli_model::shape::{OperationShape, StructureShape};
use aws_cli_model::{Model, Protocol};
use aws_cli_runtime::http::Body;
use aws_cli_protocol::{
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
    pub body: aws_cli_runtime::http::Body,
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
            aws_cli_model::Shape::Blob(blob) if blob.traits.has("smithy.api#streaming") => {
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
            (status.to_string(), aws_cli_runtime::http::reason_phrase(status).to_string())
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
