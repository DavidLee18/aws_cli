//! `application/vnd.amazon.eventstream` framing.
//!
//! The wire format behind every AWS streaming response — Kinesis `SubscribeToShard`,
//! Bedrock `ConverseStream`, `SelectObjectContent`, CloudWatch Logs `StartLiveTail`. The
//! body is not one document but a sequence of self-delimiting binary frames that arrive
//! as the service produces them.
//!
//! A frame is:
//!
//! ```text
//! +----------------+----------------+----------------+
//! | total_length   | headers_length | prelude_crc    |   12-byte prelude
//! +----------------+----------------+----------------+
//! | headers ...                    (headers_length)  |
//! +--------------------------------------------------+
//! | payload ...                                      |
//! +--------------------------------------------------+
//! | message_crc                                      |   4 bytes
//! +--------------------------------------------------+
//! ```
//!
//! Both CRCs are checked. They are the only thing standing between a truncated or
//! mis-framed response and a stream that silently decodes into plausible nonsense: every
//! length in the format comes from the bytes themselves, so one bad length would be
//! followed by frames read at the wrong offsets rather than by an obvious failure.

use crate::ProtocolError;

/// The fixed part before the headers: two lengths and their checksum.
const PRELUDE: usize = 12;
/// The trailing message checksum.
const TRAILER: usize = 4;
/// A frame is capped by the format at 16 MiB, and by services well below that. Bounding
/// it means a corrupt length cannot ask us to reserve gigabytes.
const MAX_FRAME: u32 = 16 * 1024 * 1024;

/// A header value. The wire distinguishes these types; the semantics layer above mostly
/// wants the string form.
#[derive(Debug, Clone, PartialEq)]
pub enum HeaderValue {
    Bool(bool),
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Bytes(Vec<u8>),
    String(String),
    /// Milliseconds since the epoch.
    Timestamp(i64),
    Uuid([u8; 16]),
}

impl HeaderValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            HeaderValue::String(s) => Some(s),
            _ => None,
        }
    }
}

/// One decoded frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub headers: Vec<(String, HeaderValue)>,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn header(&self, name: &str) -> Option<&HeaderValue> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }

    /// A `:`-prefixed protocol header, as a string.
    pub fn header_str(&self, name: &str) -> Option<&str> {
        self.header(name)?.as_str()
    }
}

fn malformed(what: &str) -> ProtocolError {
    ProtocolError::Unsupported(format!("malformed event stream: {what}"))
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// How many bytes the frame starting at `bytes[0]` occupies, if that is knowable yet.
///
/// `Ok(None)` means "not enough bytes to tell" — the caller should read more rather than
/// treat it as an error. This is what makes incremental decoding possible.
pub fn frame_length(bytes: &[u8]) -> Result<Option<usize>, ProtocolError> {
    if bytes.len() < PRELUDE {
        return Ok(None);
    }
    let total = u32_at(bytes, 0);
    if total < (PRELUDE + TRAILER) as u32 {
        return Err(malformed("frame shorter than its own framing"));
    }
    if total > MAX_FRAME {
        return Err(malformed("frame longer than the format allows"));
    }
    // The prelude CRC is checked here rather than in `decode`, so a corrupt length is
    // caught before it is used to wait for bytes that will never come.
    if crc32(&bytes[..8]) != u32_at(bytes, 8) {
        return Err(malformed("prelude checksum mismatch"));
    }
    Ok(Some(total as usize))
}

/// Decode exactly one frame, which must be `frame_length` bytes long.
pub fn decode(frame: &[u8]) -> Result<Frame, ProtocolError> {
    let Some(total) = frame_length(frame)? else {
        return Err(malformed("truncated prelude"));
    };
    if frame.len() != total {
        return Err(malformed("frame length does not match the buffer"));
    }

    let expected = u32_at(frame, total - TRAILER);
    if crc32(&frame[..total - TRAILER]) != expected {
        return Err(malformed("message checksum mismatch"));
    }

    let headers_len = u32_at(frame, 4) as usize;
    let headers_end = PRELUDE
        .checked_add(headers_len)
        .filter(|end| *end <= total - TRAILER)
        .ok_or_else(|| malformed("headers longer than the frame"))?;

    Ok(Frame {
        headers: decode_headers(&frame[PRELUDE..headers_end])?,
        payload: frame[headers_end..total - TRAILER].to_vec(),
    })
}

fn decode_headers(mut bytes: &[u8]) -> Result<Vec<(String, HeaderValue)>, ProtocolError> {
    let mut out = Vec::new();
    while !bytes.is_empty() {
        let name_len = *bytes.first().ok_or_else(|| malformed("truncated header"))? as usize;
        bytes = &bytes[1..];
        let name = bytes.get(..name_len).ok_or_else(|| malformed("truncated header name"))?;
        let name = String::from_utf8_lossy(name).into_owned();
        bytes = &bytes[name_len..];

        let kind = *bytes.first().ok_or_else(|| malformed("missing header type"))?;
        bytes = &bytes[1..];

        let (value, rest) = decode_header_value(kind, bytes)?;
        out.push((name, value));
        bytes = rest;
    }
    Ok(out)
}

fn decode_header_value(kind: u8, bytes: &[u8]) -> Result<(HeaderValue, &[u8]), ProtocolError> {
    fn fixed<const N: usize>(bytes: &[u8]) -> Result<([u8; N], &[u8]), ProtocolError> {
        let head = bytes.get(..N).ok_or_else(|| malformed("truncated header value"))?;
        Ok((head.try_into().expect("slice of the requested length"), &bytes[N..]))
    }

    Ok(match kind {
        0 => (HeaderValue::Bool(true), bytes),
        1 => (HeaderValue::Bool(false), bytes),
        2 => {
            let (b, rest) = fixed::<1>(bytes)?;
            (HeaderValue::Byte(b[0] as i8), rest)
        }
        3 => {
            let (b, rest) = fixed::<2>(bytes)?;
            (HeaderValue::Short(i16::from_be_bytes(b)), rest)
        }
        4 => {
            let (b, rest) = fixed::<4>(bytes)?;
            (HeaderValue::Int(i32::from_be_bytes(b)), rest)
        }
        5 => {
            let (b, rest) = fixed::<8>(bytes)?;
            (HeaderValue::Long(i64::from_be_bytes(b)), rest)
        }
        6 | 7 => {
            let (len, rest) = fixed::<2>(bytes)?;
            let len = u16::from_be_bytes(len) as usize;
            let value = rest.get(..len).ok_or_else(|| malformed("truncated header value"))?;
            let rest = &rest[len..];
            if kind == 6 {
                (HeaderValue::Bytes(value.to_vec()), rest)
            } else {
                (HeaderValue::String(String::from_utf8_lossy(value).into_owned()), rest)
            }
        }
        8 => {
            let (b, rest) = fixed::<8>(bytes)?;
            (HeaderValue::Timestamp(i64::from_be_bytes(b)), rest)
        }
        9 => {
            let (b, rest) = fixed::<16>(bytes)?;
            (HeaderValue::Uuid(b), rest)
        }
        other => return Err(malformed(&format!("unknown header type {other}"))),
    })
}

/// Encode one header's name, type and value.
fn encode_header(name: &str, value: &HeaderValue, out: &mut Vec<u8>) {
    out.push(name.len() as u8);
    out.extend_from_slice(name.as_bytes());
    match value {
        HeaderValue::Bool(true) => out.push(0),
        HeaderValue::Bool(false) => out.push(1),
        HeaderValue::Byte(n) => {
            out.push(2);
            out.push(*n as u8);
        }
        HeaderValue::Short(n) => {
            out.push(3);
            out.extend_from_slice(&n.to_be_bytes());
        }
        HeaderValue::Int(n) => {
            out.push(4);
            out.extend_from_slice(&n.to_be_bytes());
        }
        HeaderValue::Long(n) => {
            out.push(5);
            out.extend_from_slice(&n.to_be_bytes());
        }
        HeaderValue::Bytes(b) => {
            out.push(6);
            out.extend_from_slice(&(b.len() as u16).to_be_bytes());
            out.extend_from_slice(b);
        }
        HeaderValue::String(s) => {
            out.push(7);
            out.extend_from_slice(&(s.len() as u16).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        HeaderValue::Timestamp(n) => {
            out.push(8);
            out.extend_from_slice(&n.to_be_bytes());
        }
        HeaderValue::Uuid(u) => {
            out.push(9);
            out.extend_from_slice(u);
        }
    }
}

/// The header block on its own, which event-stream signing hashes separately.
pub fn encode_headers(headers: &[(String, HeaderValue)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, value) in headers {
        encode_header(name, value, &mut out);
    }
    out
}

/// Build a complete frame, both checksums included.
pub fn encode(headers: &[(String, HeaderValue)], payload: &[u8]) -> Vec<u8> {
    let header_bytes = encode_headers(headers);
    let total = (PRELUDE + header_bytes.len() + payload.len() + TRAILER) as u32;

    let mut out = Vec::with_capacity(total as usize);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&(header_bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(&crc32(&out).to_be_bytes());
    out.extend_from_slice(&header_bytes);
    out.extend_from_slice(payload);
    out.extend_from_slice(&crc32(&out).to_be_bytes());
    out
}

/// Buffers bytes as they arrive and hands back whole frames.
///
/// A streaming response is read in whatever chunk sizes the network produces, which have
/// nothing to do with frame boundaries: one read may carry half a frame, or three frames
/// and a fragment.
#[derive(Debug, Default)]
pub struct Decoder {
    buffer: Vec<u8>,
}

impl Decoder {
    pub fn new() -> Decoder {
        Decoder::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// The next complete frame, or `None` while one is still arriving.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, ProtocolError> {
        let Some(len) = frame_length(&self.buffer)? else { return Ok(None) };
        if self.buffer.len() < len {
            return Ok(None);
        }
        let frame = decode(&self.buffer[..len])?;
        self.buffer.drain(..len);
        Ok(Some(frame))
    }

    /// Whether anything is left over. A stream that ends mid-frame is truncated, and
    /// saying so beats reporting a clean end.
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }
}

/// CRC-32, the IEEE polynomial in its reflected form (`0xEDB88320`).
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= *byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

// ---------------------------------------------------------------------------
// Turning frames into the documents the CLI prints
// ---------------------------------------------------------------------------

use aws_cli_model::shape::StructureShape;
use aws_cli_model::{Model, Shape, ShapeId};
use serde_json::{Map, Value};

use crate::shapes::Protocol;

/// What one frame meant.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A modelled event: the union member's name and its decoded document.
    Event { name: String, value: Value },
    /// A modelled exception delivered mid-stream. The stream ends here.
    Exception { code: String, message: String, value: Value },
    /// An unmodelled error frame, which carries everything in its headers.
    Error { code: String, message: String },
    /// A frame this build has no meaning for. Skipped rather than fatal: services add
    /// event types over time, and an unknown one should not end a working stream.
    Unknown { message_type: String, event_type: Option<String> },
}

/// The member of an operation's output that carries the event stream, if it has one.
pub fn stream_member<'a>(
    model: &'a Model,
    output_shape: &'a StructureShape,
) -> Option<(&'a str, &'a StructureShape)> {
    output_shape.members.iter().find_map(|(name, member)| {
        let shape = model.shape(&member.target)?;
        match shape {
            Shape::Union(u) if shape.traits().has("smithy.api#streaming") => {
                Some((name.as_str(), u))
            }
            _ => None,
        }
    })
}

/// Interpret one frame against the stream's union shape.
pub fn interpret(
    model: &Model,
    protocol: Protocol,
    union_shape: &StructureShape,
    frame: &Frame,
) -> Result<Event, ProtocolError> {
    let message_type = frame.header_str(":message-type").unwrap_or("event");
    match message_type {
        "event" => {
            let Some(name) = frame.header_str(":event-type") else {
                return Ok(Event::Unknown { message_type: message_type.into(), event_type: None });
            };
            let Some(member) = union_shape.members.get(name) else {
                return Ok(Event::Unknown {
                    message_type: message_type.into(),
                    event_type: Some(name.to_string()),
                });
            };
            let value = decode_event(model, protocol, &member.target, frame)?;
            Ok(Event::Event { name: name.to_string(), value })
        }
        "exception" => {
            let code = frame.header_str(":exception-type").unwrap_or("Unknown").to_string();
            let value = match union_shape.members.get(code.as_str()) {
                Some(member) => decode_event(model, protocol, &member.target, frame)?,
                None => Value::Object(Map::new()),
            };
            // The message lives in the payload for a modelled exception, under whichever
            // of the two spellings the service happens to use.
            let message = ["message", "Message"]
                .iter()
                .find_map(|k| value.get(*k)?.as_str())
                .unwrap_or_default()
                .to_string();
            Ok(Event::Exception { code, message, value })
        }
        "error" => Ok(Event::Error {
            code: frame.header_str(":error-code").unwrap_or("Unknown").to_string(),
            message: frame.header_str(":error-message").unwrap_or_default().to_string(),
        }),
        other => Ok(Event::Unknown {
            message_type: other.to_string(),
            event_type: frame.header_str(":event-type").map(str::to_string),
        }),
    }
}

/// Build the frame for one outgoing event.
///
/// The mirror of [`interpret`]: `name` picks the union member, and `value` is the same
/// document shape this module hands back for an incoming event of that type. That
/// symmetry is the point — what the CLI prints for a response event is what it accepts
/// for a request event.
pub fn encode_event(
    model: &Model,
    protocol: Protocol,
    union_shape: &StructureShape,
    name: &str,
    value: &Value,
) -> Result<Vec<u8>, ProtocolError> {
    let member = union_shape.members.get(name).ok_or_else(|| {
        ProtocolError::Unsupported(format!("`{name}` is not an event of this stream"))
    })?;

    let mut headers = vec![
        (":message-type".to_string(), HeaderValue::String("event".to_string())),
        (":event-type".to_string(), HeaderValue::String(name.to_string())),
    ];

    let Some(Shape::Structure(shape)) = model.shape(&member.target) else {
        return Ok(encode(&headers, &[]));
    };

    let empty = Map::new();
    let members = value.as_object().unwrap_or(&empty);

    let mut payload = Vec::new();
    let mut body = Map::new();
    let mut content_type = None;

    for (member_name, event_member) in &shape.members {
        let Some(field) = members.get(member_name) else { continue };
        if field.is_null() {
            continue;
        }
        if event_member.traits.has("smithy.api#eventHeader") {
            if let Some(header) = json_header(field) {
                headers.push((member_name.clone(), header));
            }
        } else if event_member.traits.has("smithy.api#eventPayload") {
            content_type = Some(match model.shape(&event_member.target) {
                // A blob member arrives as base64, the same way it is printed.
                Some(Shape::Blob(_)) => {
                    payload = field
                        .as_str()
                        .and_then(crate::shapes::base64_decode)
                        .unwrap_or_default();
                    "application/octet-stream"
                }
                Some(Shape::Structure(_)) => {
                    payload = serialize_body(model, protocol, &event_member.target, field)?;
                    body_content_type(protocol)
                }
                _ => {
                    payload = field.as_str().unwrap_or_default().as_bytes().to_vec();
                    "text/plain"
                }
            });
        } else {
            body.insert(member_name.clone(), field.clone());
        }
    }

    if content_type.is_none() && !body.is_empty() {
        let document = Value::Object(body);
        payload = serialize_structure_body(model, protocol, shape, &document)?;
        content_type = Some(body_content_type(protocol));
    }


    headers.push((
        ":content-type".to_string(),
        HeaderValue::String(content_type.unwrap_or("application/octet-stream").to_string()),
    ));
    Ok(encode(&headers, &payload))
}

fn body_content_type(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::RestXml | Protocol::AwsQuery | Protocol::Ec2Query => "text/xml",
        _ => "application/json",
    }
}

fn serialize_body(
    model: &Model,
    protocol: Protocol,
    target: &ShapeId,
    value: &Value,
) -> Result<Vec<u8>, ProtocolError> {
    match model.shape(target) {
        Some(Shape::Structure(shape)) => serialize_structure_body(model, protocol, shape, value),
        _ => Ok(Vec::new()),
    }
}

/// Every duplex operation modelled today is `restJson1`, so the body is JSON. An XML
/// input stream would need a serializer that does not exist yet, and saying so beats
/// sending an empty frame.
fn serialize_structure_body(
    model: &Model,
    protocol: Protocol,
    shape: &StructureShape,
    value: &Value,
) -> Result<Vec<u8>, ProtocolError> {
    if matches!(protocol, Protocol::RestXml | Protocol::AwsQuery | Protocol::Ec2Query) {
        return Err(ProtocolError::Unsupported(
            "sending an XML event stream is not implemented".to_string(),
        ));
    }
    let mut wire = crate::json::serialize_structure(model, protocol, shape, value)?;
    pass_blobs_through(model, shape, value, &mut wire);
    Ok(crate::json::to_python_json(&wire).into_bytes())
}

/// Put blob members back as the caller wrote them.
///
/// Elsewhere in the CLI a blob parameter is raw text that gets base64-encoded on the way
/// out, while a blob in a *response* is printed as the base64 from the wire — an
/// asymmetry inherited from botocore. Event streams cannot live with it: their blobs are
/// audio and model output, which have no text form, and a stream is something you feed
/// back what you were just given. So here a blob is base64 in both directions, and this
/// undoes the generic serializer's second encoding.
fn pass_blobs_through(
    model: &Model,
    shape: &StructureShape,
    input: &Value,
    wire: &mut Value,
) {
    let (Some(members), Some(out)) = (input.as_object(), wire.as_object_mut()) else { return };
    for (name, member) in &shape.members {
        if !matches!(model.shape(&member.target), Some(Shape::Blob(_))) {
            continue;
        }
        let Some(Value::String(original)) = members.get(name) else { continue };
        let wire_name = crate::shapes::json_name(name, member);
        if let Some(slot) = out.get_mut(wire_name) {
            *slot = Value::String(original.clone());
        }
    }
}

/// A JSON value as an event-stream header. Types the format cannot carry are skipped.
fn json_header(value: &Value) -> Option<HeaderValue> {
    Some(match value {
        Value::Bool(b) => HeaderValue::Bool(*b),
        Value::String(s) => HeaderValue::String(s.clone()),
        Value::Number(n) => HeaderValue::Long(n.as_i64()?),
        _ => return None,
    })
}

/// Decode one event's structure: header-bound members from the frame headers, and the
/// payload according to whether a single member claims it.
fn decode_event(
    model: &Model,
    protocol: Protocol,
    target: &ShapeId,
    frame: &Frame,
) -> Result<Value, ProtocolError> {
    let Some(Shape::Structure(shape)) = model.shape(target) else {
        // Not a structure: the whole payload is the value.
        return Ok(payload_value(frame));
    };

    let mut out = Map::new();
    let mut payload_member = None;
    let mut has_body_members = false;

    for (name, member) in &shape.members {
        if member.traits.has("smithy.api#eventHeader") {
            if let Some(value) = frame.header(name) {
                out.insert(name.clone(), header_json(value));
            }
        } else if member.traits.has("smithy.api#eventPayload") {
            payload_member = Some((name.clone(), member));
        } else {
            has_body_members = true;
        }
    }

    match payload_member {
        // One member owns the whole payload: a blob of audio, a chunk of records, or a
        // nested structure serialised on its own.
        Some((name, member)) => {
            let value = match model.shape(&member.target) {
                Some(Shape::Blob(_)) => Value::String(crate::shapes::base64_encode(&frame.payload)),
                Some(Shape::Structure(inner)) => {
                    parse_payload_structure(model, protocol, inner, frame)?
                }
                _ => Value::String(String::from_utf8_lossy(&frame.payload).into_owned()),
            };
            out.insert(name, value);
        }
        None if has_body_members => {
            let parsed = parse_payload_structure(model, protocol, shape, frame)?;
            if let Value::Object(members) = parsed {
                out.extend(members);
            }
        }
        None => {}
    }

    Ok(Value::Object(out))
}

/// Parse the frame payload as a structure, in whichever encoding the stream uses.
fn parse_payload_structure(
    model: &Model,
    protocol: Protocol,
    shape: &StructureShape,
    frame: &Frame,
) -> Result<Value, ProtocolError> {
    if frame.payload.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let text = String::from_utf8_lossy(&frame.payload);
    // `:content-type` is advisory; the protocol is what actually decides, and a service
    // that omits the header still sends its own protocol's encoding.
    let xml = matches!(protocol, Protocol::RestXml | Protocol::AwsQuery | Protocol::Ec2Query)
        || frame.header_str(":content-type") == Some("application/xml");
    if xml {
        return crate::xml::parse_response(model, "", Some(shape), &text);
    }
    let json: Value = serde_json::from_str(&text).unwrap_or(Value::Object(Map::new()));
    crate::json::parse_structure(model, protocol, shape, &json)
}

/// A frame whose event shape is unknown: hand back the payload rather than nothing.
fn payload_value(frame: &Frame) -> Value {
    match std::str::from_utf8(&frame.payload) {
        Ok(text) => Value::String(text.to_string()),
        Err(_) => Value::String(crate::shapes::base64_encode(&frame.payload)),
    }
}

fn header_json(value: &HeaderValue) -> Value {
    match value {
        HeaderValue::Bool(b) => Value::Bool(*b),
        HeaderValue::Byte(n) => Value::from(*n),
        HeaderValue::Short(n) => Value::from(*n),
        HeaderValue::Int(n) => Value::from(*n),
        HeaderValue::Long(n) => Value::from(*n),
        HeaderValue::Bytes(b) => Value::String(crate::shapes::base64_encode(b)),
        HeaderValue::String(s) => Value::String(s.clone()),
        // Header timestamps are milliseconds; the CLI prints seconds-based ISO-8601.
        HeaderValue::Timestamp(ms) => {
            Value::String(crate::shapes::format_cli_output(ms.div_euclid(1000)))
        }
        HeaderValue::Uuid(u) => Value::String(
            u.iter().map(|b| format!("{b:02x}")).collect::<String>(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a frame the way a service would, so the tests exercise the real checksums
    /// rather than a decoder talking to itself.
    fn frame(headers: &[(&str, HeaderValue)], payload: &[u8]) -> Vec<u8> {
        let mut header_bytes = Vec::new();
        for (name, value) in headers {
            header_bytes.push(name.len() as u8);
            header_bytes.extend_from_slice(name.as_bytes());
            match value {
                HeaderValue::Bool(true) => header_bytes.push(0),
                HeaderValue::Bool(false) => header_bytes.push(1),
                HeaderValue::Byte(b) => {
                    header_bytes.push(2);
                    header_bytes.push(*b as u8);
                }
                HeaderValue::Short(n) => {
                    header_bytes.push(3);
                    header_bytes.extend_from_slice(&n.to_be_bytes());
                }
                HeaderValue::Int(n) => {
                    header_bytes.push(4);
                    header_bytes.extend_from_slice(&n.to_be_bytes());
                }
                HeaderValue::Long(n) => {
                    header_bytes.push(5);
                    header_bytes.extend_from_slice(&n.to_be_bytes());
                }
                HeaderValue::Bytes(b) => {
                    header_bytes.push(6);
                    header_bytes.extend_from_slice(&(b.len() as u16).to_be_bytes());
                    header_bytes.extend_from_slice(b);
                }
                HeaderValue::String(s) => {
                    header_bytes.push(7);
                    header_bytes.extend_from_slice(&(s.len() as u16).to_be_bytes());
                    header_bytes.extend_from_slice(s.as_bytes());
                }
                HeaderValue::Timestamp(n) => {
                    header_bytes.push(8);
                    header_bytes.extend_from_slice(&n.to_be_bytes());
                }
                HeaderValue::Uuid(u) => {
                    header_bytes.push(9);
                    header_bytes.extend_from_slice(u);
                }
            }
        }

        let total = (PRELUDE + header_bytes.len() + payload.len() + TRAILER) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(&total.to_be_bytes());
        out.extend_from_slice(&(header_bytes.len() as u32).to_be_bytes());
        let prelude_crc = crc32(&out);
        out.extend_from_slice(&prelude_crc.to_be_bytes());
        out.extend_from_slice(&header_bytes);
        out.extend_from_slice(payload);
        let message_crc = crc32(&out);
        out.extend_from_slice(&message_crc.to_be_bytes());
        out
    }

    /// The published check value for CRC-32, so a reflected/unreflected mix-up shows up
    /// here rather than as "every frame is corrupt".
    #[test]
    fn crc32_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn decodes_headers_of_every_type() {
        let bytes = frame(
            &[
                ("t", HeaderValue::Bool(true)),
                ("f", HeaderValue::Bool(false)),
                ("b", HeaderValue::Byte(-2)),
                ("s", HeaderValue::Short(-300)),
                ("i", HeaderValue::Int(-70000)),
                ("l", HeaderValue::Long(-5_000_000_000)),
                ("raw", HeaderValue::Bytes(vec![1, 2, 3])),
                (":event-type", HeaderValue::String("Records".into())),
                ("ts", HeaderValue::Timestamp(1_700_000_000_000)),
                ("id", HeaderValue::Uuid([7u8; 16])),
            ],
            b"payload",
        );
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.payload, b"payload");
        assert_eq!(decoded.header("t"), Some(&HeaderValue::Bool(true)));
        assert_eq!(decoded.header("f"), Some(&HeaderValue::Bool(false)));
        assert_eq!(decoded.header("b"), Some(&HeaderValue::Byte(-2)));
        assert_eq!(decoded.header("s"), Some(&HeaderValue::Short(-300)));
        assert_eq!(decoded.header("i"), Some(&HeaderValue::Int(-70000)));
        assert_eq!(decoded.header("l"), Some(&HeaderValue::Long(-5_000_000_000)));
        assert_eq!(decoded.header("raw"), Some(&HeaderValue::Bytes(vec![1, 2, 3])));
        assert_eq!(decoded.header_str(":event-type"), Some("Records"));
        assert_eq!(decoded.header("ts"), Some(&HeaderValue::Timestamp(1_700_000_000_000)));
        assert_eq!(decoded.header("id"), Some(&HeaderValue::Uuid([7u8; 16])));
    }

    #[test]
    fn decodes_a_frame_with_no_headers_and_no_payload() {
        let decoded = decode(&frame(&[], b"")).unwrap();
        assert!(decoded.headers.is_empty());
        assert!(decoded.payload.is_empty());
    }

    /// The point of the checksums: a flipped byte must fail loudly, not decode into
    /// something plausible.
    #[test]
    fn rejects_a_corrupted_payload() {
        let mut bytes = frame(&[(":event-type", HeaderValue::String("E".into()))], b"hello");
        let last = bytes.len() - 6;
        bytes[last] ^= 0xff;
        let error = decode(&bytes).unwrap_err().to_string();
        assert!(error.contains("message checksum"), "{error}");
    }

    /// A corrupt length is caught by the prelude CRC before it is used to size a read.
    #[test]
    fn rejects_a_corrupted_length() {
        let mut bytes = frame(&[], b"hello");
        bytes[1] ^= 0x40;
        let error = frame_length(&bytes).unwrap_err().to_string();
        assert!(error.contains("prelude checksum"), "{error}");
    }

    /// Network reads have nothing to do with frame boundaries, so the decoder must
    /// tolerate being fed one byte at a time.
    #[test]
    fn reassembles_frames_split_across_reads() {
        let mut stream = Vec::new();
        for i in 0..3 {
            stream.extend_from_slice(&frame(
                &[(":event-type", HeaderValue::String(format!("E{i}")))],
                format!("body-{i}").as_bytes(),
            ));
        }

        let mut decoder = Decoder::new();
        let mut seen = Vec::new();
        for byte in &stream {
            decoder.push(&[*byte]);
            while let Some(frame) = decoder.next_frame().unwrap() {
                seen.push(frame);
            }
        }
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[1].header_str(":event-type"), Some("E1"));
        assert_eq!(seen[2].payload, b"body-2");
        assert!(decoder.is_empty());
    }

    /// The whole stream in one read is the other extreme, and must work equally.
    #[test]
    fn reads_several_frames_from_one_chunk() {
        let mut stream = frame(&[("a", HeaderValue::Int(1))], b"x");
        stream.extend_from_slice(&frame(&[("a", HeaderValue::Int(2))], b"y"));
        let mut decoder = Decoder::new();
        decoder.push(&stream);
        assert_eq!(decoder.next_frame().unwrap().unwrap().payload, b"x");
        assert_eq!(decoder.next_frame().unwrap().unwrap().payload, b"y");
        assert!(decoder.next_frame().unwrap().is_none());
        assert!(decoder.is_empty());
    }

    /// A stream cut off mid-frame leaves bytes behind, which is how the caller can tell
    /// a truncated response from a complete one.
    #[test]
    fn a_truncated_stream_leaves_a_partial_frame() {
        let bytes = frame(&[], b"hello");
        let mut decoder = Decoder::new();
        decoder.push(&bytes[..bytes.len() - 3]);
        assert!(decoder.next_frame().unwrap().is_none());
        assert!(!decoder.is_empty());
    }

    // ---------------- the semantics layer ----------------

    /// A JSON-protocol stream shaped like Kinesis's: one event with body members, one
    /// with a blob that owns the payload, one header-bound member, and an exception.
    fn json_model() -> Model {
        Model::from_json(
            br#"{"smithy":"2.0","shapes":{
              "com.x#S":{"type":"service","version":"1","traits":{}},
              "com.x#Str":{"type":"string"},
              "com.x#Num":{"type":"integer"},
              "com.x#Bin":{"type":"blob"},
              "com.x#Rec":{"type":"structure","members":{
                "SequenceNumber":{"target":"com.x#Str"},
                "Count":{"target":"com.x#Num"}}},
              "com.x#Chunk":{"type":"structure","members":{
                "Kind":{"target":"com.x#Str","traits":{"smithy.api#eventHeader":{}}},
                "Bytes":{"target":"com.x#Bin","traits":{"smithy.api#eventPayload":{}}}}},
              "com.x#Boom":{"type":"structure","traits":{"smithy.api#error":"client"},
                "members":{"message":{"target":"com.x#Str"}}},
              "com.x#Stream":{"type":"union","traits":{"smithy.api#streaming":{}},
                "members":{
                  "Rec":{"target":"com.x#Rec"},
                  "Chunk":{"target":"com.x#Chunk"},
                  "Boom":{"target":"com.x#Boom"}}},
              "com.x#Out":{"type":"structure","members":{
                "EventStream":{"target":"com.x#Stream"}}}}}"#,
        )
        .expect("fixture model")
    }

    fn structure(model: &Model, id: &str) -> StructureShape {
        let id = ShapeId::parse(id).expect("shape id");
        match model.shape(&id).expect("shape present") {
            Shape::Structure(s) | Shape::Union(s) => s.clone(),
            other => panic!("expected a structure or union, got {other:?}"),
        }
    }

    /// The stream member has to be found by its `streaming` union target, not by name.
    #[test]
    fn finds_the_streaming_member_of_an_output() {
        let model = json_model();
        let out = structure(&model, "com.x#Out");
        let (name, union) = stream_member(&model, &out).expect("a streaming member");
        assert_eq!(name, "EventStream");
        assert!(union.members.contains_key("Rec"));
    }

    #[test]
    fn decodes_an_event_whose_members_are_in_the_payload() {
        let model = json_model();
        let union = structure(&model, "com.x#Stream");
        let bytes = frame(
            &[
                (":message-type", HeaderValue::String("event".into())),
                (":event-type", HeaderValue::String("Rec".into())),
            ],
            br#"{"SequenceNumber":"42","Count":7}"#,
        );
        let event =
            interpret(&model, Protocol::AwsJson1_1, &union, &decode(&bytes).unwrap()).unwrap();
        match event {
            Event::Event { name, value } => {
                assert_eq!(name, "Rec");
                assert_eq!(value["SequenceNumber"], "42");
                assert_eq!(value["Count"], 7);
            }
            other => panic!("expected an event, got {other:?}"),
        }
    }

    /// `eventHeader` members come from the frame headers and `eventPayload` takes the
    /// whole body — reading either from the other place yields an empty event.
    #[test]
    fn splits_header_bound_members_from_the_payload() {
        let model = json_model();
        let union = structure(&model, "com.x#Stream");
        let bytes = frame(
            &[
                (":message-type", HeaderValue::String("event".into())),
                (":event-type", HeaderValue::String("Chunk".into())),
                ("Kind", HeaderValue::String("audio".into())),
            ],
            b"raw",
        );
        let event =
            interpret(&model, Protocol::AwsJson1_1, &union, &decode(&bytes).unwrap()).unwrap();
        match event {
            Event::Event { value, .. } => {
                assert_eq!(value["Kind"], "audio");
                // The blob is base64, as blobs are everywhere else in the output.
                assert_eq!(value["Bytes"], "cmF3");
            }
            other => panic!("expected an event, got {other:?}"),
        }
    }

    #[test]
    fn reads_a_modelled_exception() {
        let model = json_model();
        let union = structure(&model, "com.x#Stream");
        let bytes = frame(
            &[
                (":message-type", HeaderValue::String("exception".into())),
                (":exception-type", HeaderValue::String("Boom".into())),
            ],
            br#"{"message":"shard gone"}"#,
        );
        let event =
            interpret(&model, Protocol::AwsJson1_1, &union, &decode(&bytes).unwrap()).unwrap();
        assert_eq!(
            event,
            Event::Exception {
                code: "Boom".into(),
                message: "shard gone".into(),
                value: serde_json::json!({"message": "shard gone"}),
            }
        );
    }

    #[test]
    fn reads_an_unmodelled_error_frame() {
        let model = json_model();
        let union = structure(&model, "com.x#Stream");
        let bytes = frame(
            &[
                (":message-type", HeaderValue::String("error".into())),
                (":error-code", HeaderValue::String("InternalError".into())),
                (":error-message", HeaderValue::String("try again".into())),
            ],
            b"",
        );
        let event =
            interpret(&model, Protocol::AwsJson1_1, &union, &decode(&bytes).unwrap()).unwrap();
        assert_eq!(
            event,
            Event::Error { code: "InternalError".into(), message: "try again".into() }
        );
    }

    /// Services add event types over time. An unrecognised one must not end a stream
    /// that is otherwise working.
    #[test]
    fn an_unknown_event_type_is_skipped_not_fatal() {
        let model = json_model();
        let union = structure(&model, "com.x#Stream");
        let bytes = frame(
            &[
                (":message-type", HeaderValue::String("event".into())),
                (":event-type", HeaderValue::String("SomethingNew".into())),
            ],
            b"{}",
        );
        let event =
            interpret(&model, Protocol::AwsJson1_1, &union, &decode(&bytes).unwrap()).unwrap();
        assert_eq!(
            event,
            Event::Unknown {
                message_type: "event".into(),
                event_type: Some("SomethingNew".into())
            }
        );
    }

    /// S3's `SelectObjectContent` is restXml, and its Stats/Progress events carry an XML
    /// structure as the payload rather than JSON.
    #[test]
    fn decodes_an_xml_payload_for_a_rest_xml_stream() {
        let model = Model::from_json(
            br#"{"smithy":"2.0","shapes":{
              "com.x#S":{"type":"service","version":"1","traits":{}},
              "com.x#Num":{"type":"long"},
              "com.x#Stats":{"type":"structure","members":{
                "BytesScanned":{"target":"com.x#Num"},
                "BytesProcessed":{"target":"com.x#Num"}}},
              "com.x#StatsEvent":{"type":"structure","members":{
                "Details":{"target":"com.x#Stats",
                           "traits":{"smithy.api#eventPayload":{}}}}},
              "com.x#Stream":{"type":"union","traits":{"smithy.api#streaming":{}},
                "members":{"Stats":{"target":"com.x#StatsEvent"}}}}}"#,
        )
        .expect("fixture model");
        let union = structure(&model, "com.x#Stream");
        let bytes = frame(
            &[
                (":message-type", HeaderValue::String("event".into())),
                (":event-type", HeaderValue::String("Stats".into())),
                (":content-type", HeaderValue::String("text/xml".into())),
            ],
            b"<Stats><BytesScanned>128</BytesScanned><BytesProcessed>64</BytesProcessed></Stats>",
        );
        let event = interpret(&model, Protocol::RestXml, &union, &decode(&bytes).unwrap()).unwrap();
        match event {
            Event::Event { value, .. } => {
                assert_eq!(value["Details"]["BytesScanned"], 128);
                assert_eq!(value["Details"]["BytesProcessed"], 64);
            }
            other => panic!("expected an event, got {other:?}"),
        }
    }

    /// `encode` must agree byte for byte with the hand-rolled builder above, which is an
    /// independent implementation of the same spec. Using the encoder to build the
    /// decoder's fixtures would let a shared mistake pass both ways.
    #[test]
    fn the_encoder_agrees_with_the_hand_built_frames() {
        let headers = [
            (":message-type".to_string(), HeaderValue::String("event".into())),
            ("n".to_string(), HeaderValue::Long(-9)),
            ("flag".to_string(), HeaderValue::Bool(true)),
            ("raw".to_string(), HeaderValue::Bytes(vec![9, 8, 7])),
        ];
        let by_hand = frame(
            &[
                (":message-type", HeaderValue::String("event".into())),
                ("n", HeaderValue::Long(-9)),
                ("flag", HeaderValue::Bool(true)),
                ("raw", HeaderValue::Bytes(vec![9, 8, 7])),
            ],
            b"body",
        );
        assert_eq!(encode(&headers, b"body"), by_hand);
        assert_eq!(encode(&[], b""), frame(&[], b""));
    }

    /// What the CLI prints for an incoming event is what it accepts for an outgoing one.
    #[test]
    fn an_encoded_event_decodes_back_to_the_same_document() {
        let model = json_model();
        let union = structure(&model, "com.x#Stream");
        let value = serde_json::json!({"SequenceNumber": "42", "Count": 7});
        let bytes =
            encode_event(&model, Protocol::AwsJson1_1, &union, "Rec", &value).unwrap();
        let event =
            interpret(&model, Protocol::AwsJson1_1, &union, &decode(&bytes).unwrap()).unwrap();
        assert_eq!(event, Event::Event { name: "Rec".into(), value });
    }

    /// The header/payload split has to survive the round trip too, or an audio chunk
    /// ends up inside the JSON body where the service will not look for it.
    #[test]
    fn an_encoded_event_keeps_the_header_and_payload_split() {
        let model = json_model();
        let union = structure(&model, "com.x#Stream");
        let value = serde_json::json!({"Kind": "audio", "Bytes": "cmF3"});
        let bytes =
            encode_event(&model, Protocol::AwsJson1_1, &union, "Chunk", &value).unwrap();
        let frame = decode(&bytes).unwrap();
        assert_eq!(frame.header_str("Kind"), Some("audio"));
        assert_eq!(frame.payload, b"raw");
        assert_eq!(frame.header_str(":content-type"), Some("application/octet-stream"));

        let event = interpret(&model, Protocol::AwsJson1_1, &union, &frame).unwrap();
        assert_eq!(event, Event::Event { name: "Chunk".into(), value });
    }

    /// A blob in an event *body* (not the payload) must survive a round trip unchanged.
    /// The generic JSON serializer base64-encodes blob input, which would encode an
    /// already-base64 audio chunk a second time — and the service would decode it to
    /// base64 text rather than to audio.
    #[test]
    fn a_body_blob_is_base64_in_both_directions() {
        let model = Model::from_json(
            br#"{"smithy":"2.0","shapes":{
              "com.x#S":{"type":"service","version":"1","traits":{}},
              "com.x#Bin":{"type":"blob"},
              "com.x#Str":{"type":"string"},
              "com.x#Audio":{"type":"structure","members":{
                "audioChunk":{"target":"com.x#Bin"},
                "label":{"target":"com.x#Str"}}},
              "com.x#Stream":{"type":"union","traits":{"smithy.api#streaming":{}},
                "members":{"Audio":{"target":"com.x#Audio"}}}}}"#,
        )
        .expect("fixture model");
        let union = structure(&model, "com.x#Stream");
        // "aGk=" is base64 for "hi".
        let value = serde_json::json!({"audioChunk": "aGk=", "label": "one"});
        let bytes =
            encode_event(&model, Protocol::RestJson1, &union, "Audio", &value).unwrap();
        let frame = decode(&bytes).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&frame.payload),
            r#"{"audioChunk": "aGk=", "label": "one"}"#
        );

        let event = interpret(&model, Protocol::RestJson1, &union, &frame).unwrap();
        assert_eq!(event, Event::Event { name: "Audio".into(), value });
    }

    #[test]
    fn refuses_an_event_the_stream_does_not_have() {
        let model = json_model();
        let union = structure(&model, "com.x#Stream");
        let error =
            encode_event(&model, Protocol::AwsJson1_1, &union, "Nope", &serde_json::json!({}))
                .unwrap_err()
                .to_string();
        assert!(error.contains("not an event of this stream"), "{error}");
    }

    #[test]
    fn refuses_an_absurd_frame_length() {
        let mut bytes = vec![0u8; PRELUDE];
        bytes[..4].copy_from_slice(&u32::MAX.to_be_bytes());
        let crc = crc32(&bytes[..8]);
        bytes[8..12].copy_from_slice(&crc.to_be_bytes());
        assert!(frame_length(&bytes).is_err());
    }
}
