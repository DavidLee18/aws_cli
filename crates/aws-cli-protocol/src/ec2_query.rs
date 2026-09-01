//! `ec2Query`.
//!
//! Form-encoded like `awsQuery`, but with four differences that matter, each verified
//! against botocore's `EC2Serializer`/`EC2QueryParser`:
//!
//! 1. Lists are flat — `Name.1`, not `Name.member.1` — and an empty list emits nothing
//!    at all (awsQuery emits `Name=`).
//! 2. Member naming: `queryName` wins and is used verbatim; otherwise `xmlName` is used
//!    with only its **first character uppercased**; otherwise the member name as-is.
//! 3. Responses have no `<...Result>` wrapper — output members sit directly under the
//!    root element.
//! 4. The request id is `<requestId>` on success but `<RequestID>` on errors, and errors
//!    nest as `<Errors><Error>`.

use aws_cli_model::shape::{Member, StructureShape};
use aws_cli_model::{Model, Shape};
use serde_json::Value;

use crate::shapes::{self, Location, Protocol, TimestampFormat};
use crate::{xml, ProtocolError};

/// Build the form-encoded body for an ec2Query operation.
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
            if value.is_null() {
                continue;
            }
            let prefix = wire_name(member_name, member);
            flatten(model, &prefix, member, value, &mut pairs)?;
        }
    }

    Ok(pairs
        .iter()
        .map(|(k, v)| format!("{}={}", form_encode(k), form_encode(v)))
        .collect::<Vec<_>>()
        .join("&"))
}

/// `queryName` verbatim, else `xmlName` with its first character uppercased, else the
/// member name unchanged.
fn wire_name(member_name: &str, member: &Member) -> String {
    if let Some(query_name) = member.traits.get("aws.protocols#ec2QueryName").and_then(|v| v.as_str())
    {
        return query_name.to_string();
    }
    match member.traits.get("smithy.api#xmlName").and_then(|v| v.as_str()) {
        Some(xml_name) => uppercase_first(xml_name),
        None => member_name.to_string(),
    }
}

fn uppercase_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn flatten(
    model: &Model,
    prefix: &str,
    member: &Member,
    value: &Value,
    out: &mut Vec<(String, String)>,
) -> Result<(), ProtocolError> {
    let shape = model
        .shape(&member.target)
        .ok_or_else(|| ProtocolError::UnknownShape(member.target.to_string()))?;

    match shape {
        Shape::Structure(s) => {
            let Value::Object(map) = value else {
                return Err(ProtocolError::TypeMismatch {
                    path: prefix.to_string(),
                    expected: "object",
                });
            };
            for (name, child) in &s.members {
                let Some(v) = map.get(name) else { continue };
                let child_prefix = format!("{prefix}.{}", wire_name(name, child));
                flatten(model, &child_prefix, child, v, out)?;
            }
        }
        // The defining difference: a flat 1-based index, no `member` wrapper.
        Shape::List(list) | Shape::Set(list) => {
            let Value::Array(items) = value else {
                return Err(ProtocolError::TypeMismatch {
                    path: prefix.to_string(),
                    expected: "array",
                });
            };
            for (i, item) in items.iter().enumerate() {
                flatten(model, &format!("{prefix}.{}", i + 1), &list.member, item, out)?;
            }
        }
        Shape::Map(map_shape) => {
            let Value::Object(entries) = value else {
                return Err(ProtocolError::TypeMismatch {
                    path: prefix.to_string(),
                    expected: "object",
                });
            };
            for (i, (k, v)) in entries.iter().enumerate() {
                let base = format!("{prefix}.entry.{}", i + 1);
                out.push((format!("{base}.key"), k.clone()));
                flatten(model, &format!("{base}.value"), &map_shape.value, v, out)?;
            }
        }
        Shape::Timestamp(_) => {
            let format = TimestampFormat::resolve(Protocol::Ec2Query, Location::Body, member);
            let text = match value {
                Value::Number(n) => n.as_i64().map(|t| format.format(t)),
                Value::String(s) => shapes::parse_timestamp(s).map(|t| format.format(t)),
                _ => None,
            }
            .unwrap_or_else(|| scalar_to_string(value));
            out.push((prefix.to_string(), text));
        }
        // Already base64 by the time it reaches here; see `args::normalize_blobs`.
        Shape::Blob(_) => {
            let text = value.as_str().map(str::to_string);
            out.push((prefix.to_string(), text.unwrap_or_else(|| scalar_to_string(value))));
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

fn form_encode(s: &str) -> String {
    const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.~";
    s.bytes()
        .map(|b| {
            if UNRESERVED.contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Parse an ec2Query response.
///
/// Unlike awsQuery there is no `<OperationNameResult>` wrapper: output members are
/// direct children of the root element.
pub fn parse_response(
    model: &Model,
    output_shape: Option<&StructureShape>,
    body: &str,
) -> Result<Value, ProtocolError> {
    // Passing an operation name that cannot match a wrapper element makes the shared XML
    // parser read members straight off the root, which is exactly ec2's shape.
    xml::parse_response(model, "\u{0}no-wrapper", output_shape, body)
}

/// Extract an ec2Query error, which nests as `<Errors><Error>` with `<RequestID>`.
pub fn parse_error(body: &str) -> Option<xml::XmlError> {
    xml::parse_error(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_cli_model::ShapeId;
    use serde_json::json;

    fn member(target: &str, traits: serde_json::Value) -> Member {
        serde_json::from_value(json!({"target": target, "traits": traits})).unwrap()
    }

    #[test]
    fn query_name_wins_and_is_verbatim() {
        let m = member(
            "smithy.api#String",
            json!({"aws.protocols#ec2QueryName": "Ipv6Addresses",
                   "smithy.api#xmlName": "ipv6AddressesSet"}),
        );
        assert_eq!(wire_name("Ipv6Addresses", &m), "Ipv6Addresses");
    }

    #[test]
    fn xml_name_gets_only_its_first_character_uppercased() {
        let m = member("smithy.api#String", json!({"smithy.api#xmlName": "dryRun"}));
        assert_eq!(wire_name("DryRun", &m), "DryRun");
        // The REST of the name keeps its casing — not a full PascalCase conversion.
        let m2 = member("smithy.api#String", json!({"smithy.api#xmlName": "reservationSet"}));
        assert_eq!(wire_name("Reservations", &m2), "ReservationSet");
    }

    #[test]
    fn falls_back_to_the_member_name() {
        let m = member("smithy.api#String", json!({}));
        assert_eq!(wire_name("MaxResults", &m), "MaxResults");
    }

    /// Lists are flat and 1-based, with no `.member` segment — the key difference from
    /// awsQuery.
    #[test]
    fn serializes_lists_flat() {
        let model = Model::from_json(
            br#"{"smithy":"2.0","shapes":{
                "com.x#S":{"type":"service","version":"1","traits":{}},
                "com.x#Ids":{"type":"list","member":{"target":"smithy.api#String"}},
                "com.x#In":{"type":"structure","members":{
                    "InstanceIds":{"target":"com.x#Ids","traits":{"smithy.api#xmlName":"InstanceId"}}}}}}"#,
        )
        .unwrap();
        let Some(Shape::Structure(input)) =
            model.shape(&ShapeId::parse("com.x#In").unwrap())
        else {
            panic!("input shape")
        };

        let body = serialize(
            &model,
            "DescribeInstances",
            "2016-11-15",
            Some(input),
            Some(&json!({"InstanceIds": ["i-1", "i-2"]})),
        )
        .unwrap();
        assert_eq!(
            body,
            "Action=DescribeInstances&Version=2016-11-15&InstanceId.1=i-1&InstanceId.2=i-2"
        );
    }

    #[test]
    fn empty_list_emits_nothing() {
        let model = Model::from_json(
            br#"{"smithy":"2.0","shapes":{
                "com.x#S":{"type":"service","version":"1","traits":{}},
                "com.x#Ids":{"type":"list","member":{"target":"smithy.api#String"}},
                "com.x#In":{"type":"structure","members":{
                    "InstanceIds":{"target":"com.x#Ids"}}}}}"#,
        )
        .unwrap();
        let Some(Shape::Structure(input)) =
            model.shape(&ShapeId::parse("com.x#In").unwrap())
        else {
            panic!("input shape")
        };
        let body =
            serialize(&model, "D", "v", Some(input), Some(&json!({"InstanceIds": []}))).unwrap();
        // awsQuery would append `InstanceIds=` here; ec2Query appends nothing.
        assert_eq!(body, "Action=D&Version=v");
    }
}
