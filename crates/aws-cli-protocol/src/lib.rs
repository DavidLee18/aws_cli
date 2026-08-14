//! Wire-protocol serialization and parsing.
//!
//! One module per AWS protocol, plus shared layers: `shapes` (timestamps, blobs,
//! member naming), `http_binding` (the `rest*` URI/query/header/payload rules) and
//! `json` (the body encoding shared by all three JSON protocols).
//!
//! Implemented: `awsQuery`, `ec2Query`, `awsJson1_0`, `awsJson1_1`, `restJson1`,
//! `restXml`. Not implemented: `rpcv2Cbor`.

pub mod aws_json;
pub mod ec2_query;
pub mod http_binding;
pub mod json;
pub mod pagination;
pub mod query;
pub mod response_fixups;
pub mod shapes;
pub mod shorthand;
pub mod validate;
pub mod xml;

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("shape not found in model: {0}")]
    UnknownShape(String),
    #[error("value at {path} is not a(n) {expected}")]
    TypeMismatch { path: String, expected: &'static str },
    #[error("malformed XML response: {0}")]
    Xml(String),
    #[error("{0} is not implemented yet")]
    Unsupported(String),
}
