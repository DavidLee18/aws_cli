//! JSON body serialization and parsing.
//!
//! Shared by `awsJson1_0`, `awsJson1_1` and `restJson1`. The three differ in framing
//! (target header vs HTTP binding), not in how a structure becomes JSON.

use aws_cli_model::shape::StructureShape;
use aws_cli_model::{Model, Shape, ShapeId};
use serde_json::{Map, Value};

use crate::shapes::{self, Location, Protocol, TimestampFormat};
use crate::ProtocolError;

/// Serialize a structure into its JSON body representation.
///
/// The input `Value` is the CLI's own JSON, keyed by *model* member names; the output is
/// keyed by wire names, with `jsonName` applied and typed values coerced.
pub fn serialize_structure(
    model: &Model,
    protocol: Protocol,
    shape: &StructureShape,
    input: &Value,
) -> Result<Value, ProtocolError> {
    let Value::Object(map) = input else {
        return Err(ProtocolError::TypeMismatch { path: "<input>".into(), expected: "object" });
    };

    let mut out = Map::new();
    for (member_name, member) in &shape.members {
        let Some(value) = map.get(member_name) else { continue };
        if value.is_null() {
            continue;
        }
        let format = TimestampFormat::resolve(protocol, Location::Body, member);
        let encoded = serialize_value(model, protocol, &member.target, value, format)?;
        out.insert(shapes::json_name(member_name, member).to_string(), encoded);
    }
    Ok(Value::Object(out))
}

fn serialize_value(
    model: &Model,
    protocol: Protocol,
    target: &ShapeId,
    value: &Value,
    timestamp_format: TimestampFormat,
) -> Result<Value, ProtocolError> {
    let shape = model
        .shape(target)
        .ok_or_else(|| ProtocolError::UnknownShape(target.to_string()))?;

    Ok(match shape {
        Shape::Structure(s) | Shape::Union(s) => {
            serialize_structure(model, protocol, s, value)?
        }
        Shape::List(list) | Shape::Set(list) => {
            let Value::Array(items) = value else {
                return Err(ProtocolError::TypeMismatch {
                    path: target.to_string(),
                    expected: "array",
                });
            };
            let format = TimestampFormat::resolve(protocol, Location::Body, &list.member);
            Value::Array(
                items
                    .iter()
                    .map(|v| serialize_value(model, protocol, &list.member.target, v, format))
                    .collect::<Result<_, _>>()?,
            )
        }
        Shape::Map(map_shape) => {
            let Value::Object(entries) = value else {
                return Err(ProtocolError::TypeMismatch {
                    path: target.to_string(),
                    expected: "object",
                });
            };
            let format = TimestampFormat::resolve(protocol, Location::Body, &map_shape.value);
            let mut out = Map::new();
            for (k, v) in entries {
                out.insert(
                    k.clone(),
                    serialize_value(model, protocol, &map_shape.value.target, v, format)?,
                );
            }
            Value::Object(out)
        }
        // Blobs travel base64-encoded in JSON.
        Shape::Blob(_) => match value.as_str() {
            Some(s) => Value::String(shapes::base64_encode(s.as_bytes())),
            None => value.clone(),
        },
        Shape::Timestamp(_) => match timestamp_value(value) {
            Some(unix) => match timestamp_format {
                TimestampFormat::EpochSeconds => Value::from(unix),
                other => Value::String(other.format(unix)),
            },
            None => value.clone(),
        },
        // `document` shapes pass through untouched — that is the point of them.
        Shape::Document(_) => value.clone(),
        _ => value.clone(),
    })
}

fn timestamp_value(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => shapes::parse_timestamp(s),
        _ => None,
    }
}

/// Parse a JSON body into the CLI's representation, keyed by model member names.
pub fn parse_structure(
    model: &Model,
    protocol: Protocol,
    shape: &StructureShape,
    body: &Value,
) -> Result<Value, ProtocolError> {
    let Value::Object(map) = body else {
        // A non-object body for a structure output is not an error worth failing on;
        // an empty result is the useful interpretation.
        return Ok(Value::Object(Map::new()));
    };

    let mut out = Map::new();
    for (member_name, member) in &shape.members {
        let wire = shapes::json_name(member_name, member);
        let Some(value) = map.get(wire) else { continue };
        if value.is_null() {
            continue;
        }
        let format = TimestampFormat::resolve(protocol, Location::Body, member);
        out.insert(
            member_name.clone(),
            parse_value(model, protocol, &member.target, value, format)?,
        );
    }
    Ok(Value::Object(out))
}

fn parse_value(
    model: &Model,
    protocol: Protocol,
    target: &ShapeId,
    value: &Value,
    // Deliberately unused: botocore sniffs the wire format on parse rather than trusting
    // the modeled one, and the CLI then prints ISO-8601 regardless. Kept in the
    // signature so the call sites stay symmetric with serialization.
    _timestamp_format: TimestampFormat,
) -> Result<Value, ProtocolError> {
    let shape = model
        .shape(target)
        .ok_or_else(|| ProtocolError::UnknownShape(target.to_string()))?;

    Ok(match shape {
        Shape::Structure(s) | Shape::Union(s) => parse_structure(model, protocol, s, value)?,
        Shape::List(list) | Shape::Set(list) => {
            let Value::Array(items) = value else { return Ok(value.clone()) };
            let format = TimestampFormat::resolve(protocol, Location::Body, &list.member);
            Value::Array(
                items
                    .iter()
                    .map(|v| parse_value(model, protocol, &list.member.target, v, format))
                    .collect::<Result<_, _>>()?,
            )
        }
        Shape::Map(map_shape) => {
            let Value::Object(entries) = value else { return Ok(value.clone()) };
            let format = TimestampFormat::resolve(protocol, Location::Body, &map_shape.value);
            let mut out = Map::new();
            for (k, v) in entries {
                out.insert(
                    k.clone(),
                    parse_value(model, protocol, &map_shape.value.target, v, format)?,
                );
            }
            Value::Object(out)
        }
        // Blobs are NOT decoded. botocore's default parser would base64-decode, but the
        // CLI replaces it with `identity` (customizations/binaryformat.py), so what the
        // user sees is the raw base64 from the wire. Decoding here would print different
        // bytes than the reference.
        Shape::Blob(_) => value.clone(),
        // Whatever the wire format, the CLI prints Python's isoformat (`+00:00`).
        Shape::Timestamp(_) => match timestamp_value(value) {
            Some(unix) => Value::String(shapes::format_cli_output(unix)),
            None => value.clone(),
        },
        _ => value.clone(),
    })
}

/// Render a request body the way Python's `json.dumps` does by default.
///
/// This is NOT cosmetic. botocore serializes request bodies with `json.dumps`, whose
/// default separators are `", "` and `": "` — so the body is `{"A": 1, "B": 2}`, not the
/// compact `{"A":1,"B":2}` that `serde_json::to_string` produces. The body is hashed into
/// the SigV4 signature, so a compact encoding signs a different request.
///
/// `ensure_ascii` is also on by default in Python, so non-ASCII is escaped as `\uXXXX`.
pub fn to_python_json(value: &Value) -> String {
    let mut out = String::new();
    write_python_json(value, &mut out);
    out
}

fn write_python_json(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            out.push('{');
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_python_string(k, out);
                out.push_str(": ");
                write_python_json(v, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_python_json(v, out);
            }
            out.push(']');
        }
        Value::String(s) => write_python_string(s, out),
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
    }
}

/// A JSON string with `ensure_ascii=True` semantics: every non-ASCII scalar becomes a
/// `\uXXXX` escape, with astral characters written as a surrogate pair.
fn write_python_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if c.is_ascii() => out.push(c),
            c => {
                let mut buffer = [0u16; 2];
                for unit in c.encode_utf16(&mut buffer) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}

/// A service error carried in a JSON body.
#[derive(Debug)]
pub struct JsonError {
    pub code: String,
    pub message: String,
}

/// Extract a modeled error from a JSON error response.
///
/// `error_type_header` is the `X-Amzn-Errortype` value, which **only restJson1 consults**
/// — botocore's awsJson parser reads the code from the body's `__type` alone, and
/// reproducing that means passing `None` here for awsJson even though the service does
/// send the header. Verified by exhaustive grep: `errortype` appears only in
/// `RestJSONParser._inject_error_code`.
pub fn parse_error(body: &str, error_type_header: Option<&str>) -> Option<JsonError> {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);

    let raw_code = error_type_header
        .map(str::to_string)
        .or_else(|| field(&parsed, &["__type", "code", "Code"]))?;

    // botocore prefers the lowercase `message` spelling over `Message`.
    let message = field(&parsed, &["message", "Message", "errorMessage"]).unwrap_or_default();
    Some(JsonError { code: normalize_error_code(&raw_code), message })
}

fn field(value: &Value, names: &[&str]) -> Option<String> {
    let object = value.as_object()?;
    names
        .iter()
        .find_map(|n| object.get(*n))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Strip the decoration AWS puts around error codes.
///
/// ORDER MATTERS and matches botocore: split on `:` first and keep the head (dropping
/// the `...Exception:http://internal...` URL suffix), then split on `#` and keep the
/// tail (dropping the `com.amazonaws.dynamodb#` namespace). Doing it the other way round
/// gives a different answer whenever a code contains both.
pub fn normalize_error_code(raw: &str) -> String {
    let before_colon = raw.split(':').next().unwrap_or(raw);
    let after_hash = before_colon.rsplit('#').next().unwrap_or(before_colon);
    after_hash.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_error_codes() {
        assert_eq!(normalize_error_code("ResourceNotFound"), "ResourceNotFound");
        assert_eq!(
            normalize_error_code("com.amazonaws.dynamodb#ResourceNotFoundException"),
            "ResourceNotFoundException"
        );
        assert_eq!(
            normalize_error_code("ThrottlingException:http://internal.amazon.com/x"),
            "ThrottlingException"
        );
        // Both decorations at once: colon is stripped first, then the namespace.
        assert_eq!(normalize_error_code("com.aws.vAPI#Throttled:http://x"), "Throttled");
    }

    /// The request body is hashed into the SigV4 signature, so matching Python's
    /// `json.dumps` output exactly is a correctness requirement, not a style choice.
    #[test]
    fn renders_bodies_the_way_python_does() {
        use serde_json::json;
        assert_eq!(
            to_python_json(&json!({"TableName": "t", "Limit": 5})),
            r#"{"TableName": "t", "Limit": 5}"#
        );
        assert_eq!(to_python_json(&json!({})), "{}");
        assert_eq!(to_python_json(&json!({"a": [1, 2]})), r#"{"a": [1, 2]}"#);
        assert_eq!(to_python_json(&json!({"a": null, "b": true})), r#"{"a": null, "b": true}"#);
    }

    /// Expectations captured from `python3 -c "import json; json.dumps(...)"`.
    #[test]
    fn escapes_non_ascii_like_ensure_ascii() {
        use serde_json::json;
        assert_eq!(to_python_json(&json!({"k": "é"})), r#"{"k": "\u00e9"}"#);
        // Astral-plane characters become a UTF-16 surrogate pair, as Python writes them.
        assert_eq!(to_python_json(&json!("😀")), r#""\ud83d\ude00""#);
        assert_eq!(to_python_json(&json!("a\"b\\c\nd")), r#""a\"b\\c\nd""#);
    }

    #[test]
    fn reads_error_from_header_or_body() {
        let from_header =
            parse_error(r#"{"message":"nope"}"#, Some("com.amazonaws#AccessDenied")).unwrap();
        assert_eq!(from_header.code, "AccessDenied");
        assert_eq!(from_header.message, "nope");

        let from_body =
            parse_error(r#"{"__type":"ValidationException","Message":"bad"}"#, None).unwrap();
        assert_eq!(from_body.code, "ValidationException");
        assert_eq!(from_body.message, "bad");

        // No code anywhere is not an error document.
        assert!(parse_error(r#"{"other":1}"#, None).is_none());
    }
}
