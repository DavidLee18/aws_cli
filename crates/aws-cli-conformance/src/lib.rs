//! Differential conformance testing against the reference AWS CLI v2.
//!
//! The reference CLI is treated as an executable specification. This crate extracts what
//! we would expose from the Smithy models, compares it to what the Python CLI actually
//! exposes, and reports every divergence.
//!
//! Two halves:
//!
//! - **Surface conformance** (this crate today) — do we offer the same services,
//!   operations and flags? Offline, needs no credentials, and covers the whole catalogue.
//! - **Behavioural conformance** (once `awsc` exists) — given identical argv, do we emit
//!   an identical signed request, and identical stdout/stderr/exit code?

pub mod corpus;
pub mod diff;
pub mod surface;

pub use corpus::Corpus;
pub use diff::Report;
pub use surface::Surface;
