//! `awsQuery` request serialization.
//!
//! The operation input becomes a form-encoded body: `Action=<Op>&Version=<ver>` plus one
//! entry per supplied member, flattened with dotted paths. Used by STS, IAM, SQS, ELB and
//! the other older query-protocol services.

use aws_cli_model::shape::StructureShape;
use aws_cli_model::{Model, Shape};
use serde_json::Value;

use crate::ProtocolError;

/// Build the form-encoded body for an operation call.
///
/// `input` is the JSON object the CLI assembled from the command line; `None` (or an
/// empty object) yields just the Action/Version pair.
pub fn serialize(
    model: &Model,
    operation_wire_name: &str,
    api_version: &str,
    input_shape: Option<&StructureShape>,
    input: Option<&Value>,
) -> Result<String, ProtocolError> {
    let mut pairs: Vec<(String, String)> = vec![
        ("Action".to_string(), operation_wire_name.to_string()),
        ("Version".to_string(), api_version.to_string()),
    ];

    if let (Some(shape), Some(Value::Object(map))) = (input_shape, input) {
        for (member_name, member) in &shape.members {
            let Some(value) = map.get(member_name) else { continue };
            // `locationName` overrides the member name on the wire where present.
            let wire = member
                .traits
                .get("smithy.api#xmlName")
                .and_then(|v| v.as_str())
                .unwrap_or(member_name);
            flatten(model, wire, &member.target, value, &mut pairs)?;
        }
    }

    Ok(pairs
        .iter()
        .map(|(k, v)| format!("{}={}", form_encode(k), form_encode(v)))
        .collect::<Vec<_>>()
        .join("&"))
}

fn flatten(
    model: &Model,
    prefix: &str,
    target: &aws_cli_model::ShapeId,
    value: &Value,
    out: &mut Vec<(String, String)>,
) -> Result<(), ProtocolError> {
    let shape = model
        .shape(target)
        .ok_or_else(|| ProtocolError::UnknownShape(target.to_string()))?;

    match shape {
        Shape::Structure(s) => {
            let Value::Object(map) = value else {
                return Err(ProtocolError::TypeMismatch {
                    path: prefix.to_string(),
                    expected: "object",
                });
            };
            for (name, member) in &s.members {
                let Some(v) = map.get(name) else { continue };
                let wire = member
                    .traits
                    .get("smithy.api#xmlName")
                    .and_then(|x| x.as_str())
                    .unwrap_or(name);
                flatten(model, &format!("{prefix}.{wire}"), &member.target, v, out)?;
            }
        }
        Shape::List(list) | Shape::Set(list) => {
            let Value::Array(items) = value else {
                return Err(ProtocolError::TypeMismatch {
                    path: prefix.to_string(),
                    expected: "array",
                });
            };
            // Flattened lists index directly off the member name; otherwise the wire
            // form interposes the member wrapper (`Name.member.1`).
            let flattened = shape.traits().has("smithy.api#xmlFlattened");
            let wrapper = list
                .member
                .traits
                .get("smithy.api#xmlName")
                .and_then(|x| x.as_str())
                .unwrap_or("member");
            for (i, item) in items.iter().enumerate() {
                let key = if flattened {
                    format!("{prefix}.{}", i + 1)
                } else {
                    format!("{prefix}.{wrapper}.{}", i + 1)
                };
                flatten(model, &key, &list.member.target, item, out)?;
            }
        }
        Shape::Map(map_shape) => {
            let Value::Object(map) = value else {
                return Err(ProtocolError::TypeMismatch {
                    path: prefix.to_string(),
                    expected: "object",
                });
            };
            let flattened = shape.traits().has("smithy.api#xmlFlattened");
            for (i, (k, v)) in map.iter().enumerate() {
                let base = if flattened {
                    format!("{prefix}.{}", i + 1)
                } else {
                    format!("{prefix}.entry.{}", i + 1)
                };
                out.push((format!("{base}.key"), k.clone()));
                flatten(model, &format!("{base}.value"), &map_shape.value.target, v, out)?;
            }
        }
        _ => out.push((prefix.to_string(), scalar_to_string(value))),
    }
    Ok(())
}

fn scalar_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// `application/x-www-form-urlencoded` percent-encoding.
///
/// AWS requires the strict RFC 3986 unreserved set — note that space becomes `%20`, not
/// `+`, and `~` is NOT escaped. Getting this wrong breaks the signature, not just the
/// request.
fn form_encode(s: &str) -> String {
    const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.~";
    let mut out = String::with_capacity(s.len());
    for byte in s.as_bytes() {
        if UNRESERVED.contains(byte) {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_unreserved_set_strictly() {
        assert_eq!(form_encode("abcXYZ091-_.~"), "abcXYZ091-_.~");
        assert_eq!(form_encode("a b"), "a%20b");
        assert_eq!(form_encode("a/b"), "a%2Fb");
        assert_eq!(form_encode("a+b"), "a%2Bb");
        assert_eq!(form_encode("é"), "%C3%A9");
    }
}
