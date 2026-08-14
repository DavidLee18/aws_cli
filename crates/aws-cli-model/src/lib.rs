//! Loader and index for AWS Smithy JSON AST service models.
//!
//! The CLI is model-driven: `aws sts get-caller-identity` is not hand-written code but a
//! lookup into a service model, so this crate is the foundation everything else builds
//! on. Models are the Smithy 2.0 JSON AST published in `awslabs/aws-sdk-rust`, which
//! carries pagination, waiters, and endpoint rulesets as traits.

pub mod custom_surface;
pub mod close_matches;
pub mod command_table;
pub mod customizations;
pub mod model;
pub mod naming;
pub mod paginators;
pub mod protocol_metadata;
pub mod service_names;
pub mod surface_overlays;
pub mod shape;
pub mod shape_id;

pub use model::{Model, ModelError, Protocol};
pub use shape::{Member, Shape, Traits};
pub use shape_id::ShapeId;
