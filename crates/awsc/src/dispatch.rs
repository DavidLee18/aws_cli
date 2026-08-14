//! Protocol dispatch: turning a model + parsed arguments into a wire request, and a
//! response back into printable JSON.
//!
//! Keeping this out of `main` means the binary's flow reads the same for every protocol,
//! and adding one is a matter of extending two `match` arms.

use aws_cli_model::shape::{OperationShape, StructureShape};
use aws_cli_model::{Model, Protocol};
use aws_cli_protocol::{
    aws_json, ec2_query, http_binding, json, query, response_fixups, xml, ProtocolError,
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
    pub body: String,
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

    match protocol {
        Protocol::AwsQuery => Ok(WireRequest {
            method: "POST".into(),
            path: "/".into(),
            query: String::new(),
            content_type: Some(FORM_CONTENT_TYPE.into()),
            headers: Vec::new(),
            body: query::serialize(
                model,
                operation_wire_name,
                &api_version,
                input_shape,
                input,
            )?,
        }),

        Protocol::Ec2Query => Ok(WireRequest {
            method: "POST".into(),
            path: "/".into(),
            query: String::new(),
            content_type: Some(FORM_CONTENT_TYPE.into()),
            headers: Vec::new(),
            body: ec2_query::serialize(
                model,
                operation_wire_name,
                &api_version,
                input_shape,
                input,
            )?,
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
                body: request.body,
            })
        }

        Protocol::RestJson1 | Protocol::RestXml => serialize_rest(
            model,
            protocol,
            op,
            input_shape,
            input,
        ),

        Protocol::Rpcv2Cbor | Protocol::Unknown => Err(ProtocolError::Unsupported(format!(
            "{protocol:?}"
        ))),
    }
}

const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded; charset=utf-8";

/// The `rest*` protocols share their HTTP binding and differ only in body encoding.
fn serialize_rest(
    model: &Model,
    protocol: Protocol,
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
            body: String::new(),
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
        _ => (String::new(), None),
    };

    let query = bound
        .query
        .iter()
        .map(|(k, v)| if v.is_empty() { k.clone() } else { format!("{k}={v}") })
        .collect::<Vec<_>>()
        .join("&");

    Ok(WireRequest {
        method: http.method,
        path: bound.path,
        query,
        content_type,
        headers: bound.headers,
        body,
    })
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
    body: &str,
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
    body: &str,
) -> Result<Value, ProtocolError> {
    match protocol {
        Protocol::AwsQuery => xml::parse_response(model, operation_wire_name, output_shape, body),
        Protocol::Ec2Query => ec2_query::parse_response(model, output_shape, body),
        Protocol::AwsJson1_0 | Protocol::AwsJson1_1 | Protocol::RestJson1 => {
            aws_json::parse_response(model, protocol, output_shape, body)
        }
        Protocol::RestXml => xml::parse_response(model, operation_wire_name, output_shape, body),
        Protocol::Rpcv2Cbor | Protocol::Unknown => {
            Err(ProtocolError::Unsupported(format!("{protocol:?}")))
        }
    }
}

/// Extract `(code, message)` from an error response.
pub fn parse_error(
    protocol: Protocol,
    body: &str,
    error_type_header: Option<&str>,
) -> (String, String) {
    let fallback = || ("Unknown".to_string(), body.trim().to_string());

    match protocol {
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
