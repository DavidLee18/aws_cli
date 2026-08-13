//! Wire-protocol serialization and parsing.
//!
//! One module per AWS protocol; the model's `Protocol` enum selects which. Only
//! `awsQuery` (plus its XML responses) is implemented so far — enough for the STS
//! vertical slice. The remaining five follow the same shape-driven pattern.

pub mod query;
pub mod xml;

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("shape not found in model: {0}")]
    UnknownShape(String),
    #[error("value at {path} is not a(n) {expected}")]
    TypeMismatch { path: String, expected: &'static str },
    #[error("malformed XML response: {0}")]
    Xml(String),
}
