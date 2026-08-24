//! Smithy RPC v2 CBOR.
//!
//! A binary sibling of the awsJson protocols: same idea — the whole input structure goes
//! in the body — but CBOR instead of JSON, and the operation is named by the URL rather
//! than an `X-Amz-Target` header. Requests go to
//! `/service/{ServiceName}/operation/{OperationName}` with `smithy-protocol: rpc-v2-cbor`.
//!
//! Two places where it deliberately differs from the JSON protocols, and where copying
//! `json.rs` would be wrong:
//!
//! - **`jsonName` is ignored.** Members are keyed by their model names. A service that
//!   sets `jsonName` on a member would be mis-keyed by reusing the JSON path.
//! - **`timestampFormat` is ignored.** Every timestamp is tag 1 over epoch seconds,
//!   whatever the model says.
//!
//! The encoder is shape-driven rather than a generic `serde_json::Value` -> CBOR
//! conversion, because the type distinctions CBOR makes are exactly the ones JSON has
//! already thrown away: a blob is a byte string rather than base64 text, and a timestamp
//! is a tagged number rather than a string.

use aws_cli_model::shape::StructureShape;
use aws_cli_model::{Model, Shape, ShapeId};
use serde_json::{Map, Value};

use crate::shapes;
use crate::ProtocolError;

/// The `smithy-protocol` header every request carries.
pub const PROTOCOL_HEADER: (&str, &str) = ("smithy-protocol", "rpc-v2-cbor");
pub const CONTENT_TYPE: &str = "application/cbor";

/// The request path for an operation.
///
/// `service_name` is the service *shape* name — the part after `#` in its shape id — not
/// the CLI's name for it and not the endpoint prefix.
pub fn request_path(service_name: &str, operation_wire_name: &str) -> String {
    format!("/service/{service_name}/operation/{operation_wire_name}")
}

// ---------------------------------------------------------------------------
// The value model
// ---------------------------------------------------------------------------

/// As much of CBOR as this protocol uses.
#[derive(Debug, Clone, PartialEq)]
pub enum Cbor {
    Uint(u64),
    /// Already negative; CBOR stores `-1 - n`.
    Nint(i64),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Cbor>),
    Map(Vec<(Cbor, Cbor)>),
    Tag(u64, Box<Cbor>),
    Bool(bool),
    Null,
    Undefined,
    Float(f64),
}

impl Cbor {
    fn as_f64(&self) -> Option<f64> {
        match self {
            Cbor::Uint(n) => Some(*n as f64),
            Cbor::Nint(n) => Some(*n as f64),
            Cbor::Float(f) => Some(*f),
            Cbor::Tag(_, inner) => inner.as_f64(),
            _ => None,
        }
    }

    fn as_text(&self) -> Option<&str> {
        match self {
            Cbor::Text(s) => Some(s),
            Cbor::Tag(_, inner) => inner.as_text(),
            _ => None,
        }
    }

    fn entries(&self) -> Option<&[(Cbor, Cbor)]> {
        match self {
            Cbor::Map(entries) => Some(entries),
            Cbor::Tag(_, inner) => inner.entries(),
            _ => None,
        }
    }

    /// The value of a text-keyed member.
    fn get(&self, key: &str) -> Option<&Cbor> {
        self.entries()?.iter().find(|(k, _)| k.as_text() == Some(key)).map(|(_, v)| v)
    }
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// Write a major type and its argument, using the shortest encoding that fits.
fn write_head(out: &mut Vec<u8>, major: u8, argument: u64) {
    let high = major << 5;
    match argument {
        0..=23 => out.push(high | argument as u8),
        24..=0xff => {
            out.push(high | 24);
            out.push(argument as u8);
        }
        0x100..=0xffff => {
            out.push(high | 25);
            out.extend_from_slice(&(argument as u16).to_be_bytes());
        }
        0x10000..=0xffff_ffff => {
            out.push(high | 26);
            out.extend_from_slice(&(argument as u32).to_be_bytes());
        }
        _ => {
            out.push(high | 27);
            out.extend_from_slice(&argument.to_be_bytes());
        }
    }
}

fn write_integer(out: &mut Vec<u8>, value: i64) {
    if value < 0 {
        // CBOR negatives store -1 - n, so -1 is argument 0.
        write_head(out, 1, (-1 - value) as u64);
    } else {
        write_head(out, 0, value as u64);
    }
}

fn write_float(out: &mut Vec<u8>, value: f64) {
    out.push((7 << 5) | 27);
    out.extend_from_slice(&value.to_be_bytes());
}

fn write_text(out: &mut Vec<u8>, value: &str) {
    write_head(out, 3, value.len() as u64);
    out.extend_from_slice(value.as_bytes());
}

fn write_bytes(out: &mut Vec<u8>, value: &[u8]) {
    write_head(out, 2, value.len() as u64);
    out.extend_from_slice(value);
}

/// Encode a `Cbor` value. Used for `document` shapes and by the tests.
pub fn write(value: &Cbor, out: &mut Vec<u8>) {
    match value {
        Cbor::Uint(n) => write_head(out, 0, *n),
        Cbor::Nint(n) => write_integer(out, *n),
        Cbor::Bytes(b) => write_bytes(out, b),
        Cbor::Text(s) => write_text(out, s),
        Cbor::Array(items) => {
            write_head(out, 4, items.len() as u64);
            for item in items {
                write(item, out);
            }
        }
        Cbor::Map(entries) => {
            write_head(out, 5, entries.len() as u64);
            for (k, v) in entries {
                write(k, out);
                write(v, out);
            }
        }
        Cbor::Tag(tag, inner) => {
            write_head(out, 6, *tag);
            write(inner, out);
        }
        Cbor::Bool(false) => out.push((7 << 5) | 20),
        Cbor::Bool(true) => out.push((7 << 5) | 21),
        Cbor::Null => out.push((7 << 5) | 22),
        Cbor::Undefined => out.push((7 << 5) | 23),
        Cbor::Float(f) => write_float(out, *f),
    }
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

/// The additional-information value that means "indefinite length".
const INDEFINITE: u8 = 31;
/// The one-byte "break" that closes an indefinite-length item.
const BREAK: u8 = 0xff;

impl<'a> Reader<'a> {
    fn error(what: &str) -> ProtocolError {
        ProtocolError::Unsupported(format!("malformed CBOR response: {what}"))
    }

    fn byte(&mut self) -> Result<u8, ProtocolError> {
        let byte = *self.bytes.get(self.at).ok_or_else(|| Self::error("truncated"))?;
        self.at += 1;
        Ok(byte)
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], ProtocolError> {
        let end = self.at.checked_add(len).ok_or_else(|| Self::error("length overflow"))?;
        let slice = self.bytes.get(self.at..end).ok_or_else(|| Self::error("truncated"))?;
        self.at = end;
        Ok(slice)
    }

    fn argument(&mut self, info: u8) -> Result<Option<u64>, ProtocolError> {
        Ok(match info {
            0..=23 => Some(info as u64),
            24 => Some(self.byte()? as u64),
            25 => Some(u16::from_be_bytes(self.take(2)?.try_into().expect("2 bytes")) as u64),
            26 => Some(u32::from_be_bytes(self.take(4)?.try_into().expect("4 bytes")) as u64),
            27 => Some(u64::from_be_bytes(self.take(8)?.try_into().expect("8 bytes"))),
            INDEFINITE => None,
            _ => return Err(Self::error("reserved additional information")),
        })
    }

    fn value(&mut self) -> Result<Cbor, ProtocolError> {
        let initial = self.byte()?;
        let major = initial >> 5;
        let info = initial & 0x1f;

        match major {
            0 => Ok(Cbor::Uint(self.definite(info)?)),
            1 => {
                let n = self.definite(info)?;
                // -1 - n, saturating rather than wrapping for values past i64.
                Ok(Cbor::Nint(-1i64 - i64::try_from(n).unwrap_or(i64::MAX)))
            }
            2 => Ok(Cbor::Bytes(self.byte_string(info)?)),
            3 => {
                let raw = self.byte_string(info)?;
                Ok(Cbor::Text(String::from_utf8_lossy(&raw).into_owned()))
            }
            4 => {
                let mut items = Vec::new();
                match self.argument(info)? {
                    Some(len) => {
                        for _ in 0..len {
                            items.push(self.value()?);
                        }
                    }
                    None => {
                        while !self.at_break()? {
                            items.push(self.value()?);
                        }
                    }
                }
                Ok(Cbor::Array(items))
            }
            5 => {
                let mut entries = Vec::new();
                match self.argument(info)? {
                    Some(len) => {
                        for _ in 0..len {
                            let key = self.value()?;
                            entries.push((key, self.value()?));
                        }
                    }
                    None => {
                        while !self.at_break()? {
                            let key = self.value()?;
                            entries.push((key, self.value()?));
                        }
                    }
                }
                Ok(Cbor::Map(entries))
            }
            6 => {
                let tag = self.definite(info)?;
                Ok(Cbor::Tag(tag, Box::new(self.value()?)))
            }
            7 => match info {
                20 => Ok(Cbor::Bool(false)),
                21 => Ok(Cbor::Bool(true)),
                22 => Ok(Cbor::Null),
                23 => Ok(Cbor::Undefined),
                25 => Ok(Cbor::Float(half_to_f64(u16::from_be_bytes(
                    self.take(2)?.try_into().expect("2 bytes"),
                )))),
                26 => Ok(Cbor::Float(
                    f32::from_be_bytes(self.take(4)?.try_into().expect("4 bytes")) as f64,
                )),
                27 => Ok(Cbor::Float(f64::from_be_bytes(
                    self.take(8)?.try_into().expect("8 bytes"),
                ))),
                _ => Err(Self::error("unsupported simple value")),
            },
            _ => Err(Self::error("unknown major type")),
        }
    }

    fn definite(&mut self, info: u8) -> Result<u64, ProtocolError> {
        self.argument(info)?.ok_or_else(|| Self::error("indefinite length where one is required"))
    }

    /// Byte or text payload, joining the chunks of an indefinite-length string.
    fn byte_string(&mut self, info: u8) -> Result<Vec<u8>, ProtocolError> {
        match self.argument(info)? {
            Some(len) => Ok(self.take(len as usize)?.to_vec()),
            None => {
                let mut out = Vec::new();
                while !self.at_break()? {
                    let initial = self.byte()?;
                    let chunk_len = self.definite(initial & 0x1f)?;
                    out.extend_from_slice(self.take(chunk_len as usize)?);
                }
                Ok(out)
            }
        }
    }

    /// Consume a break if it is next.
    fn at_break(&mut self) -> Result<bool, ProtocolError> {
        match self.bytes.get(self.at) {
            Some(&BREAK) => {
                self.at += 1;
                Ok(true)
            }
            Some(_) => Ok(false),
            None => Err(Self::error("truncated inside an indefinite-length item")),
        }
    }
}

/// IEEE 754 half precision, which CBOR allows for floats.
fn half_to_f64(bits: u16) -> f64 {
    let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exponent = ((bits >> 10) & 0x1f) as i32;
    let mantissa = (bits & 0x3ff) as f64;
    match exponent {
        0 => sign * mantissa * 2f64.powi(-24),
        0x1f if mantissa == 0.0 => sign * f64::INFINITY,
        0x1f => f64::NAN,
        _ => sign * (mantissa / 1024.0 + 1.0) * 2f64.powi(exponent - 15),
    }
}

pub fn decode(bytes: &[u8]) -> Result<Cbor, ProtocolError> {
    Reader { bytes, at: 0 }.value()
}

// ---------------------------------------------------------------------------
// Shape-driven serialization
// ---------------------------------------------------------------------------

/// Encode an operation's input.
///
/// An operation with no input shape sends no body at all — the spec is explicit that a
/// request for an operation with no input must not carry one.
pub fn serialize(
    model: &Model,
    input_shape: Option<&StructureShape>,
    input: Option<&Value>,
) -> Result<Vec<u8>, ProtocolError> {
    let mut out = Vec::new();
    match (input_shape, input) {
        (Some(shape), Some(value)) => serialize_structure(model, shape, value, &mut out)?,
        // An input shape with nothing supplied still sends an empty map: the operation
        // accepts members, they are simply all absent.
        (Some(_), None) => write_head(&mut out, 5, 0),
        _ => {}
    }
    Ok(out)
}

fn serialize_structure(
    model: &Model,
    shape: &StructureShape,
    input: &Value,
    out: &mut Vec<u8>,
) -> Result<(), ProtocolError> {
    let Value::Object(map) = input else {
        return Err(ProtocolError::TypeMismatch { path: "<input>".into(), expected: "object" });
    };

    // The length prefix has to be known before the entries are written, so collect the
    // present members first rather than writing an indefinite-length map: definite
    // lengths are what every server implementation is exercised against.
    let present: Vec<_> = shape
        .members
        .iter()
        .filter_map(|(name, member)| match map.get(name) {
            Some(value) if !value.is_null() => Some((name, member, value)),
            _ => None,
        })
        .collect();

    write_head(out, 5, present.len() as u64);
    for (name, member, value) in present {
        // `jsonName` is deliberately not consulted: RPC v2 CBOR keys by member name.
        write_text(out, name);
        serialize_value(model, &member.target, value, out)?;
    }
    Ok(())
}

fn serialize_value(
    model: &Model,
    target: &ShapeId,
    value: &Value,
    out: &mut Vec<u8>,
) -> Result<(), ProtocolError> {
    let shape =
        model.shape(target).ok_or_else(|| ProtocolError::UnknownShape(target.to_string()))?;

    match shape {
        Shape::Structure(s) | Shape::Union(s) => serialize_structure(model, s, value, out)?,
        Shape::List(list) | Shape::Set(list) => {
            let Value::Array(items) = value else {
                return Err(ProtocolError::TypeMismatch {
                    path: target.to_string(),
                    expected: "array",
                });
            };
            write_head(out, 4, items.len() as u64);
            for item in items {
                serialize_value(model, &list.member.target, item, out)?;
            }
        }
        Shape::Map(map_shape) => {
            let Value::Object(entries) = value else {
                return Err(ProtocolError::TypeMismatch {
                    path: target.to_string(),
                    expected: "object",
                });
            };
            write_head(out, 5, entries.len() as u64);
            for (k, v) in entries {
                write_text(out, k);
                serialize_value(model, &map_shape.value.target, v, out)?;
            }
        }
        // A blob is raw bytes here, not the base64 text a JSON protocol would carry.
        Shape::Blob(_) => match value.as_str() {
            Some(s) => write_bytes(out, s.as_bytes()),
            None => write_json_value(value, out),
        },
        // Tag 1 over epoch seconds, regardless of any `timestampFormat` on the member.
        Shape::Timestamp(_) => match timestamp_seconds(value) {
            Some(unix) => {
                write_head(out, 6, 1);
                write_integer(out, unix);
            }
            None => write_json_value(value, out),
        },
        Shape::Boolean(_) => match value.as_bool() {
            Some(b) => write(&Cbor::Bool(b), out),
            None => write_json_value(value, out),
        },
        Shape::Byte(_) | Shape::Short(_) | Shape::Integer(_) | Shape::Long(_)
        | Shape::IntEnum(_) => match value.as_i64() {
            Some(n) => write_integer(out, n),
            None => write_json_value(value, out),
        },
        Shape::Float(_) | Shape::Double(_) => match value.as_f64() {
            Some(f) => write_float(out, f),
            None => write_json_value(value, out),
        },
        Shape::String(_) | Shape::Enum(_) => match value.as_str() {
            Some(s) => write_text(out, s),
            None => write_json_value(value, out),
        },
        // `document` is by definition unmodelled, so it takes the generic mapping.
        _ => write_json_value(value, out),
    }
    Ok(())
}

fn timestamp_seconds(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => shapes::parse_timestamp(s),
        _ => None,
    }
}

/// The shape-free mapping, for `document` members and for values that do not match the
/// type their shape claims.
fn write_json_value(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => write(&Cbor::Null, out),
        Value::Bool(b) => write(&Cbor::Bool(*b), out),
        Value::Number(n) => match n.as_i64() {
            Some(i) => write_integer(out, i),
            None => write_float(out, n.as_f64().unwrap_or(0.0)),
        },
        Value::String(s) => write_text(out, s),
        Value::Array(items) => {
            write_head(out, 4, items.len() as u64);
            for item in items {
                write_json_value(item, out);
            }
        }
        Value::Object(entries) => {
            write_head(out, 5, entries.len() as u64);
            for (k, v) in entries {
                write_text(out, k);
                write_json_value(v, out);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Shape-driven parsing
// ---------------------------------------------------------------------------

/// Decode a response body into the JSON the CLI prints.
pub fn parse_response(
    model: &Model,
    output_shape: Option<&StructureShape>,
    body: &[u8],
) -> Result<Value, ProtocolError> {
    let Some(shape) = output_shape else { return Ok(Value::Object(Map::new())) };
    // An operation that returns nothing sends nothing.
    if body.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let decoded = decode(body)?;
    Ok(parse_structure(model, shape, &decoded))
}

fn parse_structure(model: &Model, shape: &StructureShape, value: &Cbor) -> Value {
    let Some(entries) = value.entries() else { return Value::Object(Map::new()) };

    let mut out = Map::new();
    for (member_name, member) in &shape.members {
        let Some(found) = entries
            .iter()
            .find(|(k, _)| k.as_text() == Some(member_name.as_str()))
            .map(|(_, v)| v)
        else {
            continue;
        };
        if matches!(found, Cbor::Null | Cbor::Undefined) {
            continue;
        }
        out.insert(member_name.clone(), parse_value(model, &member.target, found));
    }
    Value::Object(out)
}

fn parse_value(model: &Model, target: &ShapeId, value: &Cbor) -> Value {
    let Some(shape) = model.shape(target) else { return to_json(value) };

    match shape {
        Shape::Structure(s) | Shape::Union(s) => parse_structure(model, s, value),
        Shape::List(list) | Shape::Set(list) => match value {
            Cbor::Array(items) => Value::Array(
                items.iter().map(|v| parse_value(model, &list.member.target, v)).collect(),
            ),
            _ => to_json(value),
        },
        Shape::Map(map_shape) => match value.entries() {
            Some(entries) => {
                let mut out = Map::new();
                for (k, v) in entries {
                    let Some(key) = k.as_text() else { continue };
                    out.insert(key.to_string(), parse_value(model, &map_shape.value.target, v));
                }
                Value::Object(out)
            }
            None => to_json(value),
        },
        // Base64, matching what the JSON protocols hand back: the CLI prints the wire
        // form of a blob rather than decoding it, and a user should see the same text
        // whichever protocol the service happens to speak.
        Shape::Blob(_) => match value {
            Cbor::Bytes(b) => Value::String(shapes::base64_encode(b)),
            _ => to_json(value),
        },
        Shape::Timestamp(_) => match value.as_f64() {
            Some(seconds) => Value::String(shapes::format_cli_output(seconds as i64)),
            None => to_json(value),
        },
        _ => to_json(value),
    }
}

/// The shape-free mapping, for `document` members and unmodelled shapes.
fn to_json(value: &Cbor) -> Value {
    match value {
        Cbor::Uint(n) => Value::from(*n),
        Cbor::Nint(n) => Value::from(*n),
        // No JSON type for raw bytes; base64 is what the rest of the CLI shows.
        Cbor::Bytes(b) => Value::String(shapes::base64_encode(b)),
        Cbor::Text(s) => Value::String(s.clone()),
        Cbor::Array(items) => Value::Array(items.iter().map(to_json).collect()),
        Cbor::Map(entries) => {
            let mut out = Map::new();
            for (k, v) in entries {
                let Some(key) = k.as_text() else { continue };
                out.insert(key.to_string(), to_json(v));
            }
            Value::Object(out)
        }
        Cbor::Tag(_, inner) => to_json(inner),
        Cbor::Bool(b) => Value::Bool(*b),
        Cbor::Null | Cbor::Undefined => Value::Null,
        Cbor::Float(f) => serde_json::Number::from_f64(*f).map(Value::Number).unwrap_or(Value::Null),
    }
}

/// Extract a modeled error from a CBOR error response.
pub fn parse_error(body: &[u8]) -> Option<crate::json::JsonError> {
    let decoded = decode(body).ok()?;
    let code = ["__type", "code", "Code"].iter().find_map(|k| decoded.get(k)?.as_text())?;
    let message = ["message", "Message", "errorMessage"]
        .iter()
        .find_map(|k| decoded.get(k)?.as_text())
        .unwrap_or_default();
    Some(crate::json::JsonError {
        code: crate::json::normalize_error_code(code),
        message: message.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A service with one structure covering each type the encoder special-cases.
    fn model() -> Model {
        Model::from_json(
            br#"{"smithy":"2.0","shapes":{
              "com.x#S":{"type":"service","version":"1","traits":{}},
              "com.x#Str":{"type":"string"},
              "com.x#Num":{"type":"integer"},
              "com.x#Blob":{"type":"blob"},
              "com.x#Time":{"type":"timestamp"},
              "com.x#Flag":{"type":"boolean"},
              "com.x#Items":{"type":"list","member":{"target":"com.x#Str"}},
              "com.x#Tags":{"type":"map","key":{"target":"com.x#Str"},
                            "value":{"target":"com.x#Num"}},
              "com.x#In":{"type":"structure","members":{
                "Name":{"target":"com.x#Str",
                        "traits":{"smithy.api#jsonName":"nameJson"}},
                "Count":{"target":"com.x#Num"},
                "Data":{"target":"com.x#Blob"},
                "When":{"target":"com.x#Time",
                        "traits":{"smithy.api#timestampFormat":"date-time"}},
                "On":{"target":"com.x#Flag"},
                "Items":{"target":"com.x#Items"},
                "Tags":{"target":"com.x#Tags"}}}}}"#,
        )
        .expect("fixture model")
    }

    fn input_shape(model: &Model) -> aws_cli_model::shape::StructureShape {
        let id = aws_cli_model::ShapeId::parse("com.x#In").expect("shape id");
        match model.shape(&id).expect("shape present") {
            Shape::Structure(s) => s.clone(),
            other => panic!("expected a structure, got {other:?}"),
        }
    }

    /// Every head length boundary, since a wrong one shifts the whole rest of the body.
    #[test]
    fn writes_the_shortest_head_that_fits() {
        let cases: &[(u64, &[u8])] = &[
            (0, &[0x00]),
            (23, &[0x17]),
            (24, &[0x18, 24]),
            (255, &[0x18, 0xff]),
            (256, &[0x19, 0x01, 0x00]),
            (65535, &[0x19, 0xff, 0xff]),
            (65536, &[0x1a, 0x00, 0x01, 0x00, 0x00]),
            (4294967296, &[0x1b, 0, 0, 0, 1, 0, 0, 0, 0]),
        ];
        for (argument, expected) in cases {
            let mut out = Vec::new();
            write_head(&mut out, 0, *argument);
            assert_eq!(&out, expected, "argument {argument}");
        }
    }

    /// CBOR stores a negative as -1 - n, so -1 is argument 0 and an off-by-one here is
    /// silent — it produces a valid document with the wrong number in it.
    #[test]
    fn writes_negatives_as_the_offset_form() {
        let mut out = Vec::new();
        write_integer(&mut out, -1);
        assert_eq!(out, vec![0x20]);
        out.clear();
        write_integer(&mut out, -500);
        assert_eq!(out, vec![0x39, 0x01, 0xf3]);
        assert_eq!(decode(&out).unwrap(), Cbor::Nint(-500));
    }

    /// `jsonName` applies to the JSON protocols and must NOT be applied here.
    #[test]
    fn keys_by_member_name_not_json_name() {
        let model = model();
        let bytes =
            serialize(&model, Some(&input_shape(&model)), Some(&json!({"Name": "x"}))).unwrap();
        let decoded = decode(&bytes).unwrap();
        assert!(decoded.get("Name").is_some(), "expected the model member name");
        assert!(decoded.get("nameJson").is_none(), "jsonName must not be applied");
    }

    /// A blob is raw bytes, not the base64 text a JSON protocol would send — and it
    /// comes back base64 so the printed output matches the other protocols.
    #[test]
    fn blobs_travel_as_byte_strings() {
        let model = model();
        let bytes =
            serialize(&model, Some(&input_shape(&model)), Some(&json!({"Data": "hi"}))).unwrap();
        assert_eq!(decode(&bytes).unwrap().get("Data"), Some(&Cbor::Bytes(b"hi".to_vec())));

        let parsed = parse_response(&model, Some(&input_shape(&model)), &bytes).unwrap();
        assert_eq!(parsed["Data"], json!("aGk="));
    }

    /// The member carries `timestampFormat: date-time`, which this protocol ignores:
    /// every timestamp is tag 1 over epoch seconds.
    #[test]
    fn timestamps_ignore_the_format_trait() {
        let model = model();
        let bytes = serialize(
            &model,
            Some(&input_shape(&model)),
            Some(&json!({"When": "2026-08-24T00:00:00Z"})),
        )
        .unwrap();
        let when = decode(&bytes).unwrap().get("When").cloned().expect("When present");
        match when {
            Cbor::Tag(1, inner) => assert_eq!(*inner, Cbor::Uint(1787529600)),
            other => panic!("expected tag 1, got {other:?}"),
        }
    }

    #[test]
    fn round_trips_a_whole_structure() {
        let model = model();
        let shape = input_shape(&model);
        let input = json!({
            "Name": "thing",
            "Count": -7,
            "On": true,
            "Items": ["a", "b"],
            "Tags": {"k": 1}
        });
        let bytes = serialize(&model, Some(&shape), Some(&input)).unwrap();
        let parsed = parse_response(&model, Some(&shape), &bytes).unwrap();
        assert_eq!(parsed, input);
    }

    /// Absent members are omitted rather than sent as null, so the map length must match
    /// what was actually written.
    #[test]
    fn omits_absent_members() {
        let model = model();
        let bytes = serialize(
            &model,
            Some(&input_shape(&model)),
            Some(&json!({"Name": "x", "Count": null})),
        )
        .unwrap();
        // 0xa1 is a definite-length map of exactly one pair.
        assert_eq!(bytes[0], 0xa1);
    }

    /// Services are free to send indefinite-length containers, and a decoder that only
    /// handles definite lengths fails on a response that is perfectly valid.
    #[test]
    fn decodes_indefinite_length_containers() {
        // {_ "a": [_ 1, 2], "b": (_ "he", "llo")}
        let bytes = [
            0xbf, 0x61, b'a', 0x9f, 0x01, 0x02, 0xff, 0x61, b'b', 0x7f, 0x62, b'h', b'e', 0x63,
            b'l', b'l', b'o', 0xff, 0xff,
        ];
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.get("a"), Some(&Cbor::Array(vec![Cbor::Uint(1), Cbor::Uint(2)])));
        assert_eq!(decoded.get("b"), Some(&Cbor::Text("hello".into())));
    }

    #[test]
    fn decodes_the_three_float_widths() {
        // 1.5 as half, single and double.
        assert_eq!(decode(&[0xf9, 0x3e, 0x00]).unwrap(), Cbor::Float(1.5));
        assert_eq!(decode(&[0xfa, 0x3f, 0xc0, 0x00, 0x00]).unwrap(), Cbor::Float(1.5));
        assert_eq!(
            decode(&[0xfb, 0x3f, 0xf8, 0, 0, 0, 0, 0, 0]).unwrap(),
            Cbor::Float(1.5)
        );
    }

    #[test]
    fn a_truncated_body_is_an_error_not_a_panic() {
        assert!(decode(&[0x19, 0x01]).is_err());
        assert!(decode(&[0xbf, 0x61, b'a']).is_err());
        assert!(decode(&[]).is_err());
    }

    #[test]
    fn reads_the_error_code_and_message() {
        // {"__type": "com.x#ValidationException", "message": "bad"}
        let mut bytes = Vec::new();
        write(
            &Cbor::Map(vec![
                (Cbor::Text("__type".into()), Cbor::Text("com.x#ValidationException".into())),
                (Cbor::Text("message".into()), Cbor::Text("bad".into())),
            ]),
            &mut bytes,
        );
        let error = parse_error(&bytes).expect("error document");
        assert_eq!(error.code, "ValidationException");
        assert_eq!(error.message, "bad");

        // A body with no code is not an error document.
        let mut other = Vec::new();
        write(&Cbor::Map(vec![(Cbor::Text("x".into()), Cbor::Uint(1))]), &mut other);
        assert!(parse_error(&other).is_none());
    }

    #[test]
    fn names_the_operation_in_the_path() {
        assert_eq!(
            request_path("RevenueMeasurement", "GetRevenue"),
            "/service/RevenueMeasurement/operation/GetRevenue"
        );
    }

    /// An operation with no input shape sends no body at all.
    #[test]
    fn no_input_shape_means_no_body() {
        assert!(serialize(&model(), None, None).unwrap().is_empty());
    }
}
