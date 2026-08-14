//! Shape-driven XML response parsing for the query and restXml protocols.
//!
//! The output shape decides how each element is interpreted: a `list` member repeats, a
//! `map` becomes an object, a scalar is coerced by type. Parsing blind would lose that —
//! a single-element list is indistinguishable from a scalar in XML alone.

use aws_cli_model::shape::StructureShape;
use aws_cli_model::{Model, Shape, ShapeId};
use quick_xml::events::Event;
use quick_xml::Reader;
use serde_json::{Map, Value};

use crate::ProtocolError;

/// A parsed XML element: text content plus ordered children.
#[derive(Debug, Default)]
struct Element {
    text: String,
    children: Vec<(String, Element)>,
}

impl Element {
    fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|(n, _)| n == name).map(|(_, e)| e)
    }

    fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> {
        self.children.iter().filter(move |(n, _)| n == name).map(|(_, e)| e)
    }
}

fn parse_document(xml: &str) -> Result<(String, Element), ProtocolError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut stack: Vec<(String, Element)> = Vec::new();
    let mut root: Option<(String, Element)> = None;

    loop {
        match reader.read_event() {
            Err(e) => return Err(ProtocolError::Xml(e.to_string())),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref());
                stack.push((name, Element::default()));
            }
            Ok(Event::Empty(e)) => {
                let name = local_name(e.name().as_ref());
                if let Some((_, parent)) = stack.last_mut() {
                    parent.children.push((name, Element::default()));
                }
            }
            Ok(Event::Text(t)) => {
                if let Some((_, current)) = stack.last_mut() {
                    current.text.push_str(&t.unescape().map_err(|e| ProtocolError::Xml(e.to_string()))?);
                }
            }
            Ok(Event::End(_)) => {
                let Some(finished) = stack.pop() else { continue };
                match stack.last_mut() {
                    Some((_, parent)) => parent.children.push(finished),
                    None => root = Some(finished),
                }
            }
            _ => {}
        }
    }

    root.ok_or_else(|| ProtocolError::Xml("document has no root element".into()))
}

fn local_name(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    match s.rsplit_once(':') {
        Some((_, local)) => local.to_string(),
        None => s.to_string(),
    }
}

/// Parse an operation response body into the JSON the CLI prints.
///
/// Query-protocol responses wrap the payload in `<OpNameResult>`; that wrapper is
/// unwrapped here so the output matches the reference, which prints result members at
/// the top level alongside nothing else.
pub fn parse_response(
    model: &Model,
    operation_wire_name: &str,
    output_shape: Option<&StructureShape>,
    body: &str,
) -> Result<Value, ProtocolError> {
    let Some(shape) = output_shape else { return Ok(Value::Object(Map::new())) };
    let (_, root) = parse_document(body)?;

    let result_wrapper = format!("{operation_wire_name}Result");
    let payload = root.child(&result_wrapper).unwrap_or(&root);

    parse_structure(model, shape, payload)
}

fn parse_structure(
    model: &Model,
    shape: &StructureShape,
    element: &Element,
) -> Result<Value, ProtocolError> {
    let mut out = Map::new();
    for (member_name, member) in &shape.members {
        let wire = member
            .traits
            .get("smithy.api#xmlName")
            .and_then(|v| v.as_str())
            .unwrap_or(member_name);

        let target_shape = model
            .shape(&member.target)
            .ok_or_else(|| ProtocolError::UnknownShape(member.target.to_string()))?;

        // Flattened lists have no wrapper element: their entries appear as repeated
        // siblings named after the member itself.
        if matches!(target_shape, Shape::List(_) | Shape::Set(_))
            && target_shape.traits().has("smithy.api#xmlFlattened")
        {
            let items: Vec<&Element> = element.children_named(wire).collect();
            if items.is_empty() {
                continue;
            }
            let (Shape::List(list) | Shape::Set(list)) = target_shape else { unreachable!() };
            let values = items
                .iter()
                .map(|e| parse_value(model, &list.member.target, e))
                .collect::<Result<Vec<_>, _>>()?;
            out.insert(member_name.clone(), Value::Array(values));
            continue;
        }

        let Some(child) = element.child(wire) else { continue };
        out.insert(member_name.clone(), parse_value(model, &member.target, child)?);
    }
    Ok(Value::Object(out))
}

fn parse_value(
    model: &Model,
    target: &ShapeId,
    element: &Element,
) -> Result<Value, ProtocolError> {
    let shape = model
        .shape(target)
        .ok_or_else(|| ProtocolError::UnknownShape(target.to_string()))?;

    Ok(match shape {
        Shape::Structure(s) | Shape::Union(s) => parse_structure(model, s, element)?,
        Shape::List(list) | Shape::Set(list) => {
            let wrapper = list
                .member
                .traits
                .get("smithy.api#xmlName")
                .and_then(|v| v.as_str())
                .unwrap_or("member");
            let values = element
                .children_named(wrapper)
                .map(|e| parse_value(model, &list.member.target, e))
                .collect::<Result<Vec<_>, _>>()?;
            Value::Array(values)
        }
        Shape::Map(map_shape) => {
            let mut out = Map::new();
            let entries: Vec<&Element> = if shape.traits().has("smithy.api#xmlFlattened") {
                vec![element]
            } else {
                element.children_named("entry").collect()
            };
            for entry in entries {
                let Some(k) = entry.child("key") else { continue };
                let Some(v) = entry.child("value") else { continue };
                out.insert(k.text.clone(), parse_value(model, &map_shape.value.target, v)?);
            }
            Value::Object(out)
        }
        // Timestamps are re-rendered in the CLI's print format rather than passed
        // through, so `...54.000Z` from S3 prints as `...54+00:00` like the reference.
        Shape::Timestamp(_) => match crate::shapes::parse_timestamp(&element.text) {
            Some(unix) => Value::String(crate::shapes::format_cli_output(unix)),
            None => Value::String(element.text.clone()),
        },
        Shape::Boolean(_) => Value::Bool(element.text == "true"),
        Shape::Integer(_) | Shape::Long(_) | Shape::Short(_) | Shape::Byte(_) => element
            .text
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(element.text.clone())),
        Shape::Float(_) | Shape::Double(_) => element
            .text
            .parse::<f64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(element.text.clone())),
        _ => Value::String(element.text.clone()),
    })
}

/// A service-level error returned in an XML error document.
#[derive(Debug)]
pub struct XmlError {
    pub code: String,
    pub message: String,
    pub request_id: Option<String>,
}

/// Extract an `<ErrorResponse><Error><Code/><Message/>` payload, if the body is one.
pub fn parse_error(body: &str) -> Option<XmlError> {
    let (_, root) = parse_document(body).ok()?;
    // Query errors nest under <Error>; some services return <Error> as the root.
    let err = root.child("Error").unwrap_or(&root);
    let code = err.child("Code")?.text.clone();
    Some(XmlError {
        code,
        message: err.child("Message").map(|e| e.text.clone()).unwrap_or_default(),
        request_id: root.child("RequestId").map(|e| e.text.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_document() {
        let (name, root) = parse_document(
            r#"<Outer><A>1</A><B><C>x</C></B><A>2</A></Outer>"#,
        )
        .unwrap();
        assert_eq!(name, "Outer");
        assert_eq!(root.children_named("A").count(), 2);
        assert_eq!(root.child("B").unwrap().child("C").unwrap().text, "x");
    }

    #[test]
    fn strips_namespace_prefixes() {
        let (name, root) = parse_document(r#"<ns:Outer><ns:A>v</ns:A></ns:Outer>"#).unwrap();
        assert_eq!(name, "Outer");
        assert_eq!(root.child("A").unwrap().text, "v");
    }

    #[test]
    fn extracts_query_error() {
        let body = r#"<ErrorResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">
          <Error><Type>Sender</Type><Code>InvalidClientTokenId</Code>
          <Message>The security token included in the request is invalid.</Message></Error>
          <RequestId>abc-123</RequestId></ErrorResponse>"#;
        let e = parse_error(body).unwrap();
        assert_eq!(e.code, "InvalidClientTokenId");
        assert!(e.message.starts_with("The security token"));
        assert_eq!(e.request_id.as_deref(), Some("abc-123"));
    }

    #[test]
    fn non_error_body_yields_none() {
        assert!(parse_error("<GetCallerIdentityResponse/>").is_none());
    }
}
