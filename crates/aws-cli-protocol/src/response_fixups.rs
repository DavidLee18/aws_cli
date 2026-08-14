//! Per-service response fix-ups that botocore applies after parsing.
//!
//! These are registered as `after-call.<service>` handlers rather than being part of any
//! protocol, so they sit outside the serializer/parser but still change what the CLI
//! prints. Each is keyed off the model, not a hardcoded operation list.

use aws_cli_model::shape::StructureShape;
use aws_cli_model::{Model, Shape, ShapeId};
use serde_json::Value;

/// IAM returns policy documents as URL-encoded JSON strings; the reference decodes them
/// into objects so users get something usable.
///
/// The rule is model-driven, not a member-name list: any **string member whose target
/// shape is named `policyDocumentType`** is decoded, wherever it appears in the response
/// (`botocore/handlers.py:513-533`, registered on `after-call.iam`).
pub fn decode_policy_documents(model: &Model, shape: &StructureShape, value: &mut Value) {
    decode_structure(model, shape, value);
}

const POLICY_SHAPE: &str = "policyDocumentType";

fn decode_structure(model: &Model, shape: &StructureShape, value: &mut Value) {
    let Some(object) = value.as_object_mut() else { return };
    for (member_name, member) in &shape.members {
        let Some(child) = object.get_mut(member_name) else { continue };
        if is_policy_document(model, &member.target) {
            if let Some(decoded) = decode_quoted_json(child) {
                *child = decoded;
            }
            continue;
        }
        decode_value(model, &member.target, child);
    }
}

fn decode_value(model: &Model, target: &ShapeId, value: &mut Value) {
    match model.shape(target) {
        Some(Shape::Structure(s) | Shape::Union(s)) => decode_structure(model, s, value),
        Some(Shape::List(list) | Shape::Set(list)) => {
            let member_target = list.member.target.clone();
            let is_policy = is_policy_document(model, &member_target);
            if let Some(items) = value.as_array_mut() {
                for item in items {
                    if is_policy {
                        if let Some(decoded) = decode_quoted_json(item) {
                            *item = decoded;
                        }
                    } else {
                        decode_value(model, &member_target, item);
                    }
                }
            }
        }
        Some(Shape::Map(map_shape)) => {
            let value_target = map_shape.value.target.clone();
            if let Some(entries) = value.as_object_mut() {
                for (_, v) in entries.iter_mut() {
                    decode_value(model, &value_target, v);
                }
            }
        }
        _ => {}
    }
}

fn is_policy_document(model: &Model, target: &ShapeId) -> bool {
    target.name() == POLICY_SHAPE && matches!(model.shape(target), Some(Shape::String(_)))
}

/// `urldecode` then `json.loads`. A value that fails either step is left alone, which is
/// what botocore does — it logs and moves on rather than failing the call.
fn decode_quoted_json(value: &Value) -> Option<Value> {
    let text = value.as_str()?;
    let unquoted = percent_decode(text)?;
    serde_json::from_str(&unquoted).ok()
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            // `+` is a space only in form encoding, and these documents are
            // `quote`d rather than `quote_plus`ed, so it stays literal.
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn iam_like_model() -> Model {
        Model::from_json(
            br#"{"smithy":"2.0","shapes":{
                "com.amazonaws.iam#S":{"type":"service","version":"1","traits":{}},
                "com.amazonaws.iam#policyDocumentType":{"type":"string"},
                "com.amazonaws.iam#Role":{"type":"structure","members":{
                    "RoleName":{"target":"smithy.api#String"},
                    "AssumeRolePolicyDocument":{"target":"com.amazonaws.iam#policyDocumentType"}}},
                "com.amazonaws.iam#Roles":{"type":"list","member":{"target":"com.amazonaws.iam#Role"}},
                "com.amazonaws.iam#Out":{"type":"structure","members":{
                    "Roles":{"target":"com.amazonaws.iam#Roles"}}}}}"#,
        )
        .unwrap()
    }

    fn out_shape(model: &Model) -> StructureShape {
        match model.shape(&ShapeId::parse("com.amazonaws.iam#Out").unwrap()) {
            Some(Shape::Structure(s)) => s.clone(),
            _ => panic!("output shape"),
        }
    }

    /// The real shape: policy documents nested inside a list of structures.
    #[test]
    fn decodes_policy_documents_inside_lists() {
        let model = iam_like_model();
        let mut value = json!({"Roles": [{
            "RoleName": "r",
            "AssumeRolePolicyDocument":
                "%7B%22Version%22%3A%222012-10-17%22%2C%22Statement%22%3A%5B%5D%7D"
        }]});

        decode_policy_documents(&model, &out_shape(&model), &mut value);

        assert_eq!(
            value["Roles"][0]["AssumeRolePolicyDocument"],
            json!({"Version": "2012-10-17", "Statement": []})
        );
        // Unrelated members are untouched.
        assert_eq!(value["Roles"][0]["RoleName"], json!("r"));
    }

    #[test]
    fn leaves_undecodable_values_alone() {
        let model = iam_like_model();
        let mut value = json!({"Roles": [{"AssumeRolePolicyDocument": "not-encoded-json"}]});
        decode_policy_documents(&model, &out_shape(&model), &mut value);
        assert_eq!(value["Roles"][0]["AssumeRolePolicyDocument"], json!("not-encoded-json"));
    }

    #[test]
    fn percent_decoding_handles_the_real_encoding() {
        assert_eq!(percent_decode("%7B%22a%22%3A1%7D").unwrap(), r#"{"a":1}"#);
        assert_eq!(percent_decode("plain").unwrap(), "plain");
        // `+` is literal here, not a space.
        assert_eq!(percent_decode("a+b").unwrap(), "a+b");
    }
}
