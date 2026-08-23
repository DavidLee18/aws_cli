//! HTTP transport: one pooled client for the process, and bodies that stream.
//!
//! Split from the signing code in [`crate::http`] on purpose. Signing decides *what*
//! bytes go on the wire; this module only moves them, and knows nothing about SigV4.

pub mod body;
pub mod client;

pub use body::Body;
pub use client::{send, send_async, send_to_writer, Request, Response, ResponseHead, Transport};
