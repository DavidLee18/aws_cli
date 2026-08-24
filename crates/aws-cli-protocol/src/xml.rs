//! Shape-driven XML for the query and restXml protocols: responses in, requests out.
//!
//! The output shape decides how each element is interpreted: a `list` member repeats, a
//! `map` becomes an object, a scalar is coerced by type. Parsing blind would lose that —
//! a single-element list is indistinguishable from a scalar in XML alone.

use aws_cli_model::shape::{Member, StructureShape};
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
                match stack.last_mut() {
                    Some((_, parent)) => parent.children.push((name, Element::default())),
                    // A self-closing element with nothing open above it IS the document:
                    // `get-bucket-location` in us-east-1 answers `<LocationConstraint/>`,
                    // which was being dropped and reported as having no root at all.
                    None => root = Some((name, Element::default())),
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
    let (root_name, root) = parse_document(body)?;

    // A single-member output can be serialised as the document element itself rather than
    // wrapped: `GetBucketLocation` answers `<LocationConstraint/>`, where the root IS the
    // member. Treating it as a wrapper would find no child and emit nothing at all.
    if root.children.is_empty() && shape.members.len() == 1 {
        if let Some((member_name, member)) = shape.members.iter().next() {
            let wire = member
                .traits
                .get("smithy.api#xmlName")
                .and_then(|v| v.as_str())
                .unwrap_or(member_name);
            if wire == root_name {
                let mut out = Map::new();
                let value = if root.text.is_empty() {
                    Value::Null
                } else {
                    parse_value(model, &member.target, &root)?
                };
                out.insert(member_name.clone(), value);
                return Ok(Value::Object(out));
            }
        }
    }

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
        //
        // `xmlFlattened` sits on the MEMBER, not on the list shape it targets — S3's
        // `ListObjectsV2Output$Contents` carries it while `ObjectList` does not. Checking
        // only the target made every flattened list parse as absent, so
        // `s3api list-objects-v2` reported a bucket with objects as empty. Both are
        // accepted here because a few models do annotate the shape.
        if matches!(target_shape, Shape::List(_) | Shape::Set(_))
            && (member.traits.has("smithy.api#xmlFlattened")
                || target_shape.traits().has("smithy.api#xmlFlattened"))
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

// ---------------------------------------------------------------------------
// Writing request bodies
// ---------------------------------------------------------------------------

/// The element name a member is written under: `xmlName` if it has one, else its own.
fn element_name<'a>(member_name: &'a str, member: &'a Member) -> &'a str {
    member
        .traits
        .get("smithy.api#xmlName")
        .and_then(|v| v.as_str())
        .unwrap_or(member_name)
}

/// Escape text for an element body or attribute value.
fn escape(text: &str, out: &mut String) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            other => out.push(other),
        }
    }
}

/// Serialise an operation's un-bound members as its request document.
///
/// The root element is the input shape's `xmlName` when it has one, and the operation
/// name otherwise — S3 names most of them (`Tagging`, `CORSConfiguration`), and where it
/// does not the operation name is what the service expects.
pub fn serialize_request(
    model: &Model,
    shape: &StructureShape,
    operation_wire_name: &str,
    values: &Value,
) -> Result<String, ProtocolError> {
    let root = shape
        .traits
        .get("smithy.api#xmlName")
        .and_then(|v| v.as_str())
        .unwrap_or(operation_wire_name);

    let mut out = String::new();
    out.push('<');
    out.push_str(root);
    if let Some(ns) = namespace(model, &shape.traits) {
        out.push_str(&format!(" xmlns=\"{ns}\""));
    }
    out.push_str(&attributes(shape, values));
    out.push('>');
    write_members(model, shape, values, &mut out)?;
    out.push_str(&format!("</{root}>"));
    Ok(out)
}

/// Serialise the single member that is the whole payload.
pub fn serialize_payload(
    model: &Model,
    member_name: &str,
    member: &Member,
    value: &Value,
) -> Result<String, ProtocolError> {
    let shape = model
        .shape(&member.target)
        .ok_or_else(|| ProtocolError::UnknownShape(member.target.to_string()))?;

    // A blob or string payload is the body verbatim, not an XML document.
    match shape {
        Shape::Blob(_) => return Ok(value.as_str().unwrap_or_default().to_string()),
        Shape::String(_) | Shape::Enum(_) => {
            return Ok(value.as_str().unwrap_or_default().to_string())
        }
        _ => {}
    }

    // The payload's own element name wins over the member's, since the document element
    // is the shape rather than the field that referenced it.
    let root = shape
        .traits()
        .get("smithy.api#xmlName")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| element_name(member_name, member))
        .to_string();

    let mut out = String::new();
    out.push('<');
    out.push_str(&root);
    if let Some(ns) = namespace(model, shape.traits()) {
        out.push_str(&format!(" xmlns=\"{ns}\""));
    }
    if let Shape::Structure(s) | Shape::Union(s) = shape {
        out.push_str(&attributes(s, value));
    }
    out.push('>');
    match shape {
        Shape::Structure(s) | Shape::Union(s) => write_members(model, s, value, &mut out)?,
        _ => write_scalar(value, &mut out),
    }
    out.push_str(&format!("</{root}>"));
    Ok(out)
}

/// The `xmlns` for a request document.
///
/// The shape's own namespace wins, but S3 and the other restXml services declare theirs
/// once on the *service* and expect every request body to carry it — the reference emits
/// `xmlns="http://s3.amazonaws.com/doc/2006-03-01/"` on documents whose shape says
/// nothing about namespaces at all.
fn namespace(model: &Model, traits: &aws_cli_model::shape::Traits) -> Option<String> {
    let from = |t: &aws_cli_model::shape::Traits| {
        t.get("smithy.api#xmlNamespace")
            .and_then(|v| v.get("uri").cloned())
            .and_then(|v| v.as_str().map(str::to_string))
    };
    from(traits).or_else(|| model.service().ok().and_then(|s| from(&s.traits)))
}

/// The attributes a structure contributes to its own start tag.
///
/// Exactly one member in the whole catalogue uses `xmlAttribute` — S3's `Grantee$Type`,
/// named `xsi:type` — but a grant written with it as a child element is silently the
/// wrong document, and S3 rejects the request rather than misreading it.
fn attributes(shape: &StructureShape, values: &Value) -> String {
    let Some(map) = values.as_object() else { return String::new() };
    let mut out = String::new();
    for (member_name, member) in &shape.members {
        if !member.traits.has("smithy.api#xmlAttribute") {
            continue;
        }
        let Some(value) = map.get(member_name) else { continue };
        let Some(text) = value.as_str() else { continue };
        let name = element_name(member_name, member);
        // A prefixed attribute needs its prefix bound on the same element; `xsi` is the
        // only one that occurs, and botocore emits the schema-instance URI for it.
        if let Some(prefix) = name.split_once(':').map(|(p, _)| p) {
            out.push_str(&format!(
                " xmlns:{prefix}=\"http://www.w3.org/2001/XMLSchema-instance\""
            ));
        }
        out.push_str(&format!(" {name}=\""));
        escape(text, &mut out);
        out.push('"');
    }
    out
}

fn write_members(
    model: &Model,
    shape: &StructureShape,
    values: &Value,
    out: &mut String,
) -> Result<(), ProtocolError> {
    let Some(map) = values.as_object() else { return Ok(()) };
    for (member_name, member) in &shape.members {
        // Attributes were written into the start tag already.
        if member.traits.has("smithy.api#xmlAttribute") {
            continue;
        }
        let Some(value) = map.get(member_name) else { continue };
        if value.is_null() {
            continue;
        }
        write_member(model, member_name, member, value, out)?;
    }
    Ok(())
}

fn write_member(
    model: &Model,
    member_name: &str,
    member: &Member,
    value: &Value,
    out: &mut String,
) -> Result<(), ProtocolError> {
    let name = element_name(member_name, member);
    let target = model
        .shape(&member.target)
        .ok_or_else(|| ProtocolError::UnknownShape(member.target.to_string()))?;

    match target {
        Shape::List(list) | Shape::Set(list) => {
            let Some(items) = value.as_array() else { return Ok(()) };
            // `xmlFlattened` sits on the member as often as on the list shape, and the
            // difference is whether the items are wrapped: flattened repeats the member
            // element, otherwise the items sit inside it under the list member's name.
            let flattened = member.traits.has("smithy.api#xmlFlattened")
                || target.traits().has("smithy.api#xmlFlattened");
            let item_name = element_name("member", &list.member);
            if flattened {
                for item in items {
                    write_element(model, name, &list.member, item, out)?;
                }
            } else {
                out.push_str(&format!("<{name}>"));
                for item in items {
                    write_element(model, item_name, &list.member, item, out)?;
                }
                out.push_str(&format!("</{name}>"));
            }
        }
        Shape::Map(map_shape) => {
            let Some(entries) = value.as_object() else { return Ok(()) };
            let flattened = member.traits.has("smithy.api#xmlFlattened")
                || target.traits().has("smithy.api#xmlFlattened");
            let key_name = element_name("key", &map_shape.key);
            let value_name = element_name("value", &map_shape.value);
            if !flattened {
                out.push_str(&format!("<{name}>"));
            }
            for (k, v) in entries {
                let entry = if flattened { name } else { "entry" };
                out.push_str(&format!("<{entry}>"));
                out.push_str(&format!("<{key_name}>"));
                escape(k, out);
                out.push_str(&format!("</{key_name}>"));
                write_element(model, value_name, &map_shape.value, v, out)?;
                out.push_str(&format!("</{entry}>"));
            }
            if !flattened {
                out.push_str(&format!("</{name}>"));
            }
        }
        _ => write_element(model, name, member, value, out)?,
    }
    Ok(())
}

/// One element: `<name>...</name>`, recursing for structures.
fn write_element(
    model: &Model,
    name: &str,
    member: &Member,
    value: &Value,
    out: &mut String,
) -> Result<(), ProtocolError> {
    let target = model
        .shape(&member.target)
        .ok_or_else(|| ProtocolError::UnknownShape(member.target.to_string()))?;

    match target {
        Shape::Structure(s) | Shape::Union(s) => {
            out.push_str(&format!("<{name}{}>", attributes(s, value)));
            write_members(model, s, value, out)?;
            out.push_str(&format!("</{name}>"));
        }
        Shape::List(_) | Shape::Set(_) | Shape::Map(_) => {
            // A nested collection is named by the member that holds it.
            write_member(model, name, member, value, out)?;
        }
        Shape::Blob(_) => {
            out.push_str(&format!("<{name}>"));
            // Blobs travel base64-encoded, as they do in every other protocol.
            if let Some(text) = value.as_str() {
                out.push_str(&crate::shapes::base64_encode(text.as_bytes()));
            }
            out.push_str(&format!("</{name}>"));
        }
        Shape::Timestamp(_) => {
            out.push_str(&format!("<{name}>"));
            let format = crate::shapes::TimestampFormat::resolve(
                crate::shapes::Protocol::RestXml,
                crate::shapes::Location::Body,
                member,
            );
            match value {
                Value::String(text) => match crate::shapes::parse_timestamp(text) {
                    Some(unix) => escape(&format.format(unix), out),
                    None => escape(text, out),
                },
                Value::Number(n) => {
                    if let Some(unix) = n.as_i64() {
                        escape(&format.format(unix), out);
                    }
                }
                _ => {}
            }
            out.push_str(&format!("</{name}>"));
        }
        _ => {
            out.push_str(&format!("<{name}>"));
            write_scalar(value, out);
            out.push_str(&format!("</{name}>"));
        }
    }
    Ok(())
}

fn write_scalar(value: &Value, out: &mut String) {
    match value {
        Value::String(text) => escape(text, out),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        _ => {}
    }
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
mod flattened_tests {
    use super::*;

    /// `xmlFlattened` sits on the member, not the list shape it targets. Checking only
    /// the target made every flattened list parse as absent — `s3api list-objects-v2`
    /// reported a bucket full of objects as empty, which is worse than an error because
    /// a script acts on it.
    #[test]
    fn reads_a_member_annotated_flattened_list() {
        let model = Model::from_json(
            br#"{"smithy":"2.0","shapes":{
                "com.x#S":{"type":"service","version":"1","traits":{}},
                "com.x#Item":{"type":"structure","members":{
                    "Key":{"target":"smithy.api#String"}}},
                "com.x#Items":{"type":"list","member":{"target":"com.x#Item"}},
                "com.x#Out":{"type":"structure","members":{
                    "Contents":{"target":"com.x#Items",
                                "traits":{"smithy.api#xmlFlattened":{}}},
                    "Name":{"target":"smithy.api#String"}}}}}"#,
        )
        .unwrap();
        let shape = match model.shape(&ShapeId::parse("com.x#Out").unwrap()) {
            Some(Shape::Structure(s)) => s.clone(),
            _ => panic!("output shape"),
        };
        let body = "<Result><Name>b</Name>\
                    <Contents><Key>a.txt</Key></Contents>\
                    <Contents><Key>b.txt</Key></Contents></Result>";
        let parsed = parse_response(&model, "Op", Some(&shape), body).unwrap();
        let contents = parsed.get("Contents").and_then(|v| v.as_array()).expect("Contents");
        assert_eq!(contents.len(), 2, "both repeated siblings should be collected");
        assert_eq!(contents[0].get("Key").unwrap(), "a.txt");
        assert_eq!(contents[1].get("Key").unwrap(), "b.txt");
        assert_eq!(parsed.get("Name").unwrap(), "b");
    }

    /// A non-flattened list still expects its wrapper element.
    #[test]
    fn reads_a_wrapped_list() {
        let model = Model::from_json(
            br#"{"smithy":"2.0","shapes":{
                "com.x#S":{"type":"service","version":"1","traits":{}},
                "com.x#Items":{"type":"list","member":{"target":"smithy.api#String"}},
                "com.x#Out":{"type":"structure","members":{
                    "Names":{"target":"com.x#Items"}}}}}"#,
        )
        .unwrap();
        let shape = match model.shape(&ShapeId::parse("com.x#Out").unwrap()) {
            Some(Shape::Structure(s)) => s.clone(),
            _ => panic!("output shape"),
        };
        let body = "<Result><Names><member>a</member><member>b</member></Names></Result>";
        let parsed = parse_response(&model, "Op", Some(&shape), body).unwrap();
        assert_eq!(parsed.get("Names").and_then(|v| v.as_array()).unwrap().len(), 2);
    }
}

#[cfg(test)]
mod tests {
    fn request_model() -> Model {
        Model::from_json(
            br#"{"smithy":"2.0","shapes":{
              "com.x#S":{"type":"service","version":"1",
                "traits":{"smithy.api#xmlNamespace":{"uri":"http://example.com/doc/"}}},
              "com.x#Str":{"type":"string"},
              "com.x#Num":{"type":"integer"},
              "com.x#Flag":{"type":"boolean"},
              "com.x#Tag":{"type":"structure","members":{
                "Key":{"target":"com.x#Str"},
                "Value":{"target":"com.x#Str"}}},
              "com.x#TagSet":{"type":"list","member":{"target":"com.x#Tag",
                "traits":{"smithy.api#xmlName":"Tag"}}},
              "com.x#Names":{"type":"list","member":{"target":"com.x#Str"},
                "traits":{"smithy.api#xmlFlattened":{}}},
              "com.x#Grantee":{"type":"structure","members":{
                "Type":{"target":"com.x#Str","traits":{
                  "smithy.api#xmlAttribute":{},"smithy.api#xmlName":"xsi:type"}},
                "URI":{"target":"com.x#Str"}}},
              "com.x#Config":{"type":"structure","members":{
                "TagSet":{"target":"com.x#TagSet"},
                "Names":{"target":"com.x#Names","traits":{"smithy.api#xmlName":"Name"}},
                "Enabled":{"target":"com.x#Flag"},
                "Count":{"target":"com.x#Num"},
                "Grantee":{"target":"com.x#Grantee"}}},
              "com.x#Input":{"type":"structure","members":{
                "Config":{"target":"com.x#Config",
                  "traits":{"smithy.api#httpPayload":{}}}}}}}"#,
        )
        .expect("fixture model")
    }

    fn structure_of(model: &Model, id: &str) -> StructureShape {
        let id = ShapeId::parse(id).expect("shape id");
        match model.shape(&id).expect("shape present") {
            Shape::Structure(s) | Shape::Union(s) => s.clone(),
            other => panic!("expected a structure, got {other:?}"),
        }
    }

    /// The service's namespace applies to request documents whose own shape declares
    /// none — the reference emits it on every S3 body.
    #[test]
    fn a_request_document_carries_the_service_namespace() {
        let model = request_model();
        let shape = structure_of(&model, "com.x#Config");
        let body = serialize_request(
            &model,
            &shape,
            "PutConfig",
            &serde_json::json!({"Count": 3}),
        )
        .unwrap();
        assert_eq!(
            body,
            "<PutConfig xmlns=\"http://example.com/doc/\"><Count>3</Count></PutConfig>"
        );
    }

    /// A wrapped list nests its items; a flattened one repeats the member element. Get
    /// this backwards and the service reads an empty collection.
    #[test]
    fn wraps_lists_unless_they_are_flattened() {
        let model = request_model();
        let shape = structure_of(&model, "com.x#Config");
        let body = serialize_request(
            &model,
            &shape,
            "PutConfig",
            &serde_json::json!({
                "TagSet": [{"Key": "k", "Value": "v"}],
                "Names": ["a", "b"]
            }),
        )
        .unwrap();
        assert!(
            body.contains("<TagSet><Tag><Key>k</Key><Value>v</Value></Tag></TagSet>"),
            "{body}"
        );
        assert!(body.contains("<Name>a</Name><Name>b</Name>"), "{body}");
    }

    /// `xmlAttribute` members belong in the start tag, with their prefix bound.
    #[test]
    fn writes_attribute_members_as_attributes() {
        let model = request_model();
        let shape = structure_of(&model, "com.x#Config");
        let body = serialize_request(
            &model,
            &shape,
            "PutConfig",
            &serde_json::json!({"Grantee": {"Type": "Group", "URI": "u"}}),
        )
        .unwrap();
        assert!(
            body.contains(
                "<Grantee xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" \
                 xsi:type=\"Group\"><URI>u</URI></Grantee>"
            ),
            "{body}"
        );
    }

    /// A payload member is the document itself, named by its own shape.
    #[test]
    fn a_payload_member_becomes_the_document() {
        let model = request_model();
        let input = structure_of(&model, "com.x#Input");
        let member = input.members.get("Config").expect("Config member");
        let body =
            serialize_payload(&model, "Config", member, &serde_json::json!({"Count": 1})).unwrap();
        assert_eq!(
            body,
            "<Config xmlns=\"http://example.com/doc/\"><Count>1</Count></Config>"
        );
    }

    #[test]
    fn escapes_text_that_would_close_an_element() {
        let model = request_model();
        let shape = structure_of(&model, "com.x#Config");
        let body = serialize_request(
            &model,
            &shape,
            "PutConfig",
            &serde_json::json!({"TagSet": [{"Key": "a&b", "Value": "<c>"}]}),
        )
        .unwrap();
        assert!(body.contains("<Key>a&amp;b</Key>"), "{body}");
        assert!(body.contains("<Value>&lt;c&gt;</Value>"), "{body}");
    }

    /// Booleans are `true`/`false`, not `True` — Python's spelling would be rejected.
    #[test]
    fn writes_booleans_the_way_xml_spells_them() {
        let model = request_model();
        let shape = structure_of(&model, "com.x#Config");
        let body = serialize_request(
            &model,
            &shape,
            "PutConfig",
            &serde_json::json!({"Enabled": false}),
        )
        .unwrap();
        assert!(body.contains("<Enabled>false</Enabled>"), "{body}");
    }

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
