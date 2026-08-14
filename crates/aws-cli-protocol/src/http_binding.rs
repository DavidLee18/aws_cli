//! HTTP request binding for the `rest*` protocols.
//!
//! An operation's `smithy.api#http` trait gives a method and a URI template; individual
//! members are then bound to the path, query string, headers, or body by their own
//! traits. Whatever is left over forms the structured body.

use aws_cli_model::shape::{Member, StructureShape};
use aws_cli_model::shape::OperationShape;
use aws_cli_model::{Model, Shape};
use serde_json::Value;

use crate::shapes::{self, Location, Protocol, TimestampFormat};
use crate::ProtocolError;

/// Where a member goes in the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Binding {
    /// `{Name}` in the URI template.
    Label,
    /// `{Name+}` — a greedy label, which keeps its slashes unencoded.
    GreedyLabel,
    Query(String),
    /// A map of arbitrary query parameters.
    QueryParams,
    Header(String),
    /// A map bound to a header name prefix.
    PrefixHeaders(String),
    /// This member alone is the body.
    Payload,
    /// Part of the structured body.
    Body,
}

/// Classify a member from its traits.
pub fn binding_of(member_name: &str, member: &Member) -> Binding {
    let t = &member.traits;
    if t.has("smithy.api#httpLabel") {
        // Greediness is a property of the URI template, resolved by the caller which
        // has the template; assume non-greedy here and let `build_uri` upgrade it.
        return Binding::Label;
    }
    if let Some(name) = t.get("smithy.api#httpQuery").and_then(|v| v.as_str()) {
        return Binding::Query(name.to_string());
    }
    if t.has("smithy.api#httpQueryParams") {
        return Binding::QueryParams;
    }
    if let Some(name) = t.get("smithy.api#httpHeader").and_then(|v| v.as_str()) {
        return Binding::Header(name.to_string());
    }
    if let Some(prefix) = t.get("smithy.api#httpPrefixHeaders").and_then(|v| v.as_str()) {
        return Binding::PrefixHeaders(prefix.to_string());
    }
    if t.has("smithy.api#httpPayload") {
        return Binding::Payload;
    }
    let _ = member_name;
    Binding::Body
}

/// The `http` trait's method, URI template and success status.
#[derive(Debug, Clone)]
pub struct HttpTrait {
    pub method: String,
    pub uri: String,
    pub code: u16,
}

pub fn http_trait(op: &OperationShape) -> Option<HttpTrait> {
    let t = op.traits.get("smithy.api#http")?;
    Some(HttpTrait {
        method: t.get("method")?.as_str()?.to_string(),
        uri: t.get("uri")?.as_str()?.to_string(),
        code: t.get("code").and_then(|c| c.as_u64()).unwrap_or(200) as u16,
    })
}

/// A request assembled from an operation's bindings.
#[derive(Debug, Default)]
pub struct BoundRequest {
    pub path: String,
    /// Already-encoded `k=v` pairs, in template-then-member order.
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    /// Members that were not bound elsewhere, for the protocol to encode as a body.
    pub body_members: Vec<String>,
    /// Set when a single member is the whole payload.
    pub payload_member: Option<String>,
}

/// Apply an operation's HTTP bindings to the supplied input.
pub fn bind(
    model: &Model,
    protocol: Protocol,
    http: &HttpTrait,
    input_shape: &StructureShape,
    input: &Value,
) -> Result<BoundRequest, ProtocolError> {
    let empty = serde_json::Map::new();
    let values = input.as_object().unwrap_or(&empty);
    let mut bound = BoundRequest::default();

    // The template may carry literal query parameters (`/path?x=y`); split them off
    // before filling labels.
    let (template, literal_query) = match http.uri.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (http.uri.as_str(), None),
    };
    if let Some(query) = literal_query {
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            match pair.split_once('=') {
                Some((k, v)) => bound.query.push((k.to_string(), v.to_string())),
                None => bound.query.push((pair.to_string(), String::new())),
            }
        }
    }

    bound.path = build_uri(model, protocol, template, input_shape, values)?;

    for (member_name, member) in &input_shape.members {
        let value = values.get(member_name);
        match binding_of(member_name, member) {
            // Labels were consumed by build_uri.
            Binding::Label | Binding::GreedyLabel => {}
            Binding::Query(name) => {
                if let Some(v) = value {
                    push_query(model, protocol, &mut bound.query, &name, member, v)?;
                }
            }
            Binding::QueryParams => {
                if let Some(Value::Object(map)) = value {
                    for (k, v) in map {
                        bound.query.push((
                            uri_encode(k, false),
                            uri_encode(&scalar_to_string(v), false),
                        ));
                    }
                }
            }
            Binding::Header(name) => {
                if let Some(v) = value {
                    let format = TimestampFormat::resolve(protocol, Location::Header, member);
                    bound.headers.push((name, header_value(model, member, v, format)?));
                }
            }
            Binding::PrefixHeaders(prefix) => {
                if let Some(Value::Object(map)) = value {
                    for (k, v) in map {
                        bound.headers.push((format!("{prefix}{k}"), scalar_to_string(v)));
                    }
                }
            }
            Binding::Payload => {
                if value.is_some() {
                    bound.payload_member = Some(member_name.clone());
                }
            }
            Binding::Body => {
                if value.is_some() {
                    bound.body_members.push(member_name.clone());
                }
            }
        }
    }
    Ok(bound)
}

/// Fill `{Label}` and `{Greedy+}` placeholders in a URI template.
fn build_uri(
    model: &Model,
    protocol: Protocol,
    template: &str,
    input_shape: &StructureShape,
    values: &serde_json::Map<String, Value>,
) -> Result<String, ProtocolError> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('}') else {
            out.push_str(&rest[open..]);
            return Ok(out);
        };
        let raw = &rest[open + 1..open + close];
        // A trailing `+` marks a greedy label, which keeps `/` unencoded.
        let (name, greedy) = match raw.strip_suffix('+') {
            Some(n) => (n, true),
            None => (raw, false),
        };

        let value = values.get(name).map(|v| scalar_to_string(v)).unwrap_or_default();
        let format = input_shape
            .members
            .get(name)
            .map(|m| TimestampFormat::resolve(protocol, Location::Label, m));
        let rendered = match (format, input_shape.members.get(name)) {
            (Some(f), Some(m)) if is_timestamp(model, m) => {
                shapes::parse_timestamp(&value).map(|t| f.format(t)).unwrap_or(value)
            }
            _ => value,
        };
        out.push_str(&uri_encode(&rendered, greedy));
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn push_query(
    model: &Model,
    protocol: Protocol,
    query: &mut Vec<(String, String)>,
    name: &str,
    member: &Member,
    value: &Value,
) -> Result<(), ProtocolError> {
    let format = TimestampFormat::resolve(protocol, Location::Query, member);
    let encoded_name = uri_encode(name, false);

    // A bound list repeats the key rather than joining values.
    if let Some(Shape::List(list) | Shape::Set(list)) = model.shape(&member.target) {
        if let Value::Array(items) = value {
            let item_is_timestamp = is_timestamp(model, &list.member);
            for item in items {
                let text = render_scalar(item, item_is_timestamp, format);
                query.push((encoded_name.clone(), uri_encode(&text, false)));
            }
            return Ok(());
        }
    }

    let text = render_scalar(value, is_timestamp(model, member), format);
    query.push((encoded_name, uri_encode(&text, false)));
    Ok(())
}

fn header_value(
    model: &Model,
    member: &Member,
    value: &Value,
    format: TimestampFormat,
) -> Result<String, ProtocolError> {
    // A list in a header is a comma-separated value.
    if let Some(Shape::List(list) | Shape::Set(list)) = model.shape(&member.target) {
        if let Value::Array(items) = value {
            let is_ts = is_timestamp(model, &list.member);
            return Ok(items
                .iter()
                .map(|i| render_scalar(i, is_ts, format))
                .collect::<Vec<_>>()
                .join(", "));
        }
    }
    Ok(render_scalar(value, is_timestamp(model, member), format))
}

fn render_scalar(value: &Value, is_timestamp: bool, format: TimestampFormat) -> String {
    if is_timestamp {
        if let Some(unix) = match value {
            Value::Number(n) => n.as_i64(),
            Value::String(s) => shapes::parse_timestamp(s),
            _ => None,
        } {
            return format.format(unix);
        }
    }
    scalar_to_string(value)
}

fn is_timestamp(model: &Model, member: &Member) -> bool {
    matches!(model.shape(&member.target), Some(Shape::Timestamp(_)))
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

/// RFC 3986 encoding. A greedy label keeps `/` literal; everything else escapes it.
pub fn uri_encode(s: &str, keep_slashes: bool) -> String {
    const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.~";
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        if UNRESERVED.contains(b) || (keep_slashes && *b == b'/') {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_cli_model::shape::Members;
    use serde_json::json;

    fn member(target: &str, traits: serde_json::Value) -> Member {
        serde_json::from_value(json!({"target": target, "traits": traits})).unwrap()
    }

    fn shape_with(members: &[(&str, Member)]) -> StructureShape {
        let mut m = Members::new();
        for (name, member) in members {
            m.insert((*name).to_string(), member.clone());
        }
        StructureShape { members: m, traits: Default::default() }
    }

    #[test]
    fn classifies_bindings_from_traits() {
        assert_eq!(
            binding_of("B", &member("smithy.api#String", json!({"smithy.api#httpLabel": {}}))),
            Binding::Label
        );
        assert_eq!(
            binding_of("P", &member("smithy.api#String", json!({"smithy.api#httpQuery": "prefix"}))),
            Binding::Query("prefix".into())
        );
        assert_eq!(
            binding_of("H", &member("smithy.api#String", json!({"smithy.api#httpHeader": "x-a"}))),
            Binding::Header("x-a".into())
        );
        assert_eq!(
            binding_of("D", &member("smithy.api#Blob", json!({"smithy.api#httpPayload": {}}))),
            Binding::Payload
        );
        assert_eq!(binding_of("X", &member("smithy.api#String", json!({}))), Binding::Body);
    }

    /// The greedy-label distinction is the one that bites: `{Key+}` must keep its
    /// slashes, or every S3 key with a `/` in it addresses the wrong object.
    #[test]
    fn encodes_greedy_labels_differently() {
        assert_eq!(uri_encode("a/b c", false), "a%2Fb%20c");
        assert_eq!(uri_encode("a/b c", true), "a/b%20c");
    }

    #[test]
    fn fills_uri_template_labels() {
        let model = Model::from_json(br#"{"smithy":"2.0","shapes":{
            "com.x#S":{"type":"service","version":"1","traits":{}}}}"#)
            .unwrap();
        let shape = shape_with(&[
            ("Bucket", member("smithy.api#String", json!({"smithy.api#httpLabel": {}}))),
            ("Key", member("smithy.api#String", json!({"smithy.api#httpLabel": {}}))),
        ]);
        let values = json!({"Bucket": "my-bucket", "Key": "nested/path/obj.txt"});
        let uri = build_uri(
            &model,
            Protocol::RestXml,
            "/{Bucket}/{Key+}",
            &shape,
            values.as_object().unwrap(),
        )
        .unwrap();
        assert_eq!(uri, "/my-bucket/nested/path/obj.txt");
    }

    #[test]
    fn splits_literal_query_from_the_template() {
        let model = Model::from_json(br#"{"smithy":"2.0","shapes":{
            "com.x#S":{"type":"service","version":"1","traits":{}}}}"#)
            .unwrap();
        let shape = shape_with(&[(
            "Bucket",
            member("smithy.api#String", json!({"smithy.api#httpLabel": {}})),
        )]);
        let http = HttpTrait { method: "GET".into(), uri: "/{Bucket}?acl".into(), code: 200 };
        let bound =
            bind(&model, Protocol::RestXml, &http, &shape, &json!({"Bucket": "b"})).unwrap();
        assert_eq!(bound.path, "/b");
        assert_eq!(bound.query, vec![("acl".to_string(), String::new())]);
    }
}
