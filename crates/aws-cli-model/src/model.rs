//! The loaded model: a shape index plus resolution helpers.

use crate::db::{ModelDb, ServiceView};
use crate::naming;
use crate::shape::{OperationShape, ServiceShape, Shape, StructureShape};
use crate::shape_id::ShapeId;
use elsa::FrozenMap;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("failed to parse Smithy model: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("model contains no service shape")]
    NoService,
    #[error("model contains {0} service shapes; expected exactly one")]
    MultipleServices(usize),
    #[error("dangling shape reference: {0}")]
    Dangling(ShapeId),
    #[error("expected {expected} shape at {id}, found {found}")]
    WrongType { id: ShapeId, expected: &'static str, found: &'static str },
    #[error("unknown operation: {0}")]
    UnknownOperation(String),
}

/// Smithy's prelude "no value" shape, referenced but never defined in model files.
pub const UNIT_SHAPE: &str = "smithy.api#Unit";

/// Add the Smithy prelude's simple shapes to the index.
///
/// Members may target `smithy.api#String`, `smithy.api#Boolean`, etc. directly; the
/// prelude is implicit and never serialized into the model file. Across the vendored
/// catalogue there are ~14,000 such references, so leaving them dangling breaks member
/// resolution wholesale (first seen as ~300 boolean flags losing their `--no-` forms).
///
/// `Primitive*` variants are the Smithy 1.0 non-nullable forms; for CLI purposes they
/// behave like their plain counterparts. `Unit` is deliberately *not* injected: it means
/// "no value", and [`Model::operation_input`]/[`Model::operation_output`] special-case it
/// to `None` — an empty structure here would defeat that.
const PRELUDE: &[(&str, fn() -> Shape)] = &[
        ("smithy.api#Blob", || Shape::Blob(Default::default())),
        ("smithy.api#Boolean", || Shape::Boolean(Default::default())),
        ("smithy.api#PrimitiveBoolean", || Shape::Boolean(Default::default())),
        ("smithy.api#String", || Shape::String(Default::default())),
        ("smithy.api#Byte", || Shape::Byte(Default::default())),
        ("smithy.api#PrimitiveByte", || Shape::Byte(Default::default())),
        ("smithy.api#Short", || Shape::Short(Default::default())),
        ("smithy.api#PrimitiveShort", || Shape::Short(Default::default())),
        ("smithy.api#Integer", || Shape::Integer(Default::default())),
        ("smithy.api#PrimitiveInteger", || Shape::Integer(Default::default())),
        ("smithy.api#Long", || Shape::Long(Default::default())),
        ("smithy.api#PrimitiveLong", || Shape::Long(Default::default())),
        ("smithy.api#Float", || Shape::Float(Default::default())),
        ("smithy.api#PrimitiveFloat", || Shape::Float(Default::default())),
        ("smithy.api#Double", || Shape::Double(Default::default())),
        ("smithy.api#PrimitiveDouble", || Shape::Double(Default::default())),
        ("smithy.api#BigInteger", || Shape::BigInteger(Default::default())),
        ("smithy.api#BigDecimal", || Shape::BigDecimal(Default::default())),
        ("smithy.api#Timestamp", || Shape::Timestamp(Default::default())),
        ("smithy.api#Document", || Shape::Document(Default::default())),
];

fn inject_prelude(shapes: &mut HashMap<ShapeId, Shape>) {
    for (id, make) in PRELUDE {
        let id = ShapeId::parse(id).expect("prelude ids are valid");
        shapes.entry(id).or_insert_with(make);
    }
}

/// The prelude shape for `id`, if it names one.
///
/// The lazy store needs this because prelude shapes are never written into a model file —
/// they are implicit, and ~14,000 members target them directly.
fn prelude_shape(id: &ShapeId) -> Option<Shape> {
    PRELUDE.iter().find(|(name, _)| *name == id.as_str()).map(|(_, make)| make())
}

/// Where a model's shapes come from.
///
/// `Eager` is the whole document parsed up front, which is what tests and the model
/// compiler use. `Lazy` reads a shape's JSON out of the mapped container and decodes it on
/// first use — for a single command that is a couple of hundred shapes instead of ~15,000.
enum Store {
    Eager(HashMap<ShapeId, Shape>),
    Lazy { view: ServiceView<'static>, cache: FrozenMap<ShapeId, Box<Shape>> },
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Store::Eager(shapes) => write!(f, "Eager({} shapes)", shapes.len()),
            Store::Lazy { .. } => write!(f, "Lazy(mapped container)"),
        }
    }
}

impl Store {
    fn get(&self, id: &ShapeId) -> Option<&Shape> {
        match self {
            Store::Eager(shapes) => shapes.get(id),
            Store::Lazy { view, cache } => {
                if let Some(shape) = cache.get(id) {
                    return Some(shape);
                }
                let shape = match view.shape_json(id.as_str()) {
                    Some(json) => serde_json::from_str::<Shape>(json).ok()?,
                    None => prelude_shape(id)?,
                };
                // `FrozenMap` hands back a reference that outlives the insert, which is
                // what lets `Model::shape` keep taking `&self` while decoding on demand.
                Some(cache.insert(id.clone(), Box::new(shape)))
            }
        }
    }
}

/// The container, opened once for the process.
///
/// The path is fixed by the first caller. A CLI invocation talks to one catalogue, and
/// making the map static is what lets a [`ServiceView`] be `'static`.
fn container(path: &Path) -> Option<&'static ModelDb> {
    static DB: OnceLock<Option<ModelDb>> = OnceLock::new();
    DB.get_or_init(|| ModelDb::open(path).ok()).as_ref()
}

#[derive(Debug, Deserialize)]
struct RawModel {
    #[allow(dead_code)]
    #[serde(default)]
    smithy: String,
    shapes: HashMap<ShapeId, Shape>,
}

/// A single AWS service model, loaded from one Smithy JSON AST file.
#[derive(Debug)]
pub struct Model {
    store: Store,
    service_id: ShapeId,
    /// Operation shape id keyed by the CLI-facing kebab-case name
    /// (`get-caller-identity` -> `com.amazonaws.sts#GetCallerIdentity`).
    operations_by_cli_name: HashMap<String, ShapeId>,
}

impl Model {
    pub fn from_json(bytes: &[u8]) -> Result<Self, ModelError> {
        let raw: RawModel = serde_json::from_slice(bytes)?;
        Self::from_shapes(raw.shapes)
    }

    /// Load one service out of the compiled container, decoding shapes on demand.
    ///
    /// Returns `None` when the container is missing or does not carry this service, so
    /// callers can fall back to the JSON models.
    pub fn from_container(models_dir: &Path, cli_service: &str) -> Option<Model> {
        let db = container(&models_dir.join("models.bin"))?;
        let view = db.service(cli_service)?;
        let service_id = ShapeId::parse(view.service_id()?).ok()?;
        let operations_by_cli_name = view
            .operation_names()
            .filter_map(|name| {
                let id = ShapeId::parse(view.operation_shape_id(name)?).ok()?;
                Some((name.to_string(), id))
            })
            .collect();
        Some(Model {
            store: Store::Lazy { view, cache: FrozenMap::new() },
            service_id,
            operations_by_cli_name,
        })
    }

    fn from_shapes(mut shapes: HashMap<ShapeId, Shape>) -> Result<Self, ModelError> {
        inject_prelude(&mut shapes);
        let services: Vec<&ShapeId> = shapes
            .iter()
            .filter(|(_, s)| matches!(s, Shape::Service(_)))
            .map(|(id, _)| id)
            .collect();

        let service_id = match services.len() {
            0 => return Err(ModelError::NoService),
            1 => services[0].clone(),
            n => return Err(ModelError::MultipleServices(n)),
        };

        let mut model =
            Model { store: Store::Eager(shapes), service_id, operations_by_cli_name: HashMap::new() };
        model.operations_by_cli_name = model.build_operation_index()?;
        Ok(model)
    }

    /// Operations reachable from the service: both directly attached and via resources.
    fn build_operation_index(&self) -> Result<HashMap<String, ShapeId>, ModelError> {
        let mut index = HashMap::new();
        let mut queue: Vec<ShapeId> = Vec::new();
        let svc = self.service()?;

        queue.extend(svc.operations.iter().map(|r| r.target.clone()));
        let mut resources: Vec<ShapeId> = svc.resources.iter().map(|r| r.target.clone()).collect();

        // Resources may nest, so walk them breadth-first. `seen` guards against the
        // cycles that a few models contain.
        let mut seen = std::collections::HashSet::new();
        while let Some(rid) = resources.pop() {
            if !seen.insert(rid.clone()) {
                continue;
            }
            let Some(Shape::Resource(res)) = self.store.get(&rid) else { continue };
            queue.extend(res.all_operations().map(|r| r.target.clone()));
            resources.extend(res.resources.iter().map(|r| r.target.clone()));
        }

        for op_id in queue {
            if self.store.get(&op_id).is_none() {
                return Err(ModelError::Dangling(op_id));
            }
            index.insert(naming::to_cli_name(op_id.name()), op_id);
        }
        Ok(index)
    }

    pub fn service_id(&self) -> &ShapeId {
        &self.service_id
    }

    pub fn service(&self) -> Result<&ServiceShape, ModelError> {
        match self.store.get(&self.service_id) {
            Some(Shape::Service(s)) => Ok(s),
            _ => Err(ModelError::NoService),
        }
    }

    /// The name this service is invoked by on the command line (`aws sts ...`).
    pub fn cli_service_name(&self) -> Result<String, ModelError> {
        let svc = self.service()?;
        Ok(naming::cli_service_name(&svc.traits))
    }

    /// The Smithy `aws.api#service.sdkId` ("CloudWatch Logs", "S3", ...).
    pub fn sdk_id(&self) -> Result<Option<&str>, ModelError> {
        let svc = self.service()?;
        Ok(svc
            .traits
            .get("aws.api#service")
            .and_then(|s| s.get("sdkId"))
            .and_then(|v| v.as_str()))
    }

    /// Whether the reference CLI ships this service at all. aws-sdk-rust models a few
    /// services the CLI deliberately lacks (`cloudwatch-events`, `transcribe-streaming`);
    /// their `sdkId` has no entry in the service-names table.
    pub fn is_cli_service(&self) -> Result<bool, ModelError> {
        Ok(self
            .sdk_id()?
            .is_some_and(|id| crate::service_names::lookup(id).is_some()))
    }

    pub fn shape(&self, id: &ShapeId) -> Option<&Shape> {
        self.store.get(id)
    }

    /// Resolve a reference, erroring rather than returning `None` — a dangling target
    /// means the model itself is malformed, which callers can't meaningfully recover from.
    pub fn resolve(&self, id: &ShapeId) -> Result<&Shape, ModelError> {
        self.store.get(id).ok_or_else(|| ModelError::Dangling(id.clone()))
    }

    pub fn operation_names(&self) -> impl Iterator<Item = &str> {
        self.operations_by_cli_name.keys().map(|s| s.as_str())
    }

    /// Look up an operation by its CLI-facing name, e.g. `get-caller-identity`.
    pub fn operation(&self, cli_name: &str) -> Result<(&ShapeId, &OperationShape), ModelError> {
        let id = self
            .operations_by_cli_name
            .get(cli_name)
            .ok_or_else(|| ModelError::UnknownOperation(cli_name.to_string()))?;
        match self.resolve(id)? {
            Shape::Operation(op) => Ok((id, op)),
            other => Err(ModelError::WrongType {
                id: id.clone(),
                expected: "operation",
                found: other.type_name(),
            }),
        }
    }

    /// The input structure of an operation. Operations with no input shape yield `None`
    /// rather than an empty struct, so callers can tell "takes nothing" from "takes an
    /// empty object".
    ///
    /// `smithy.api#Unit` is Smithy's prelude "no value" shape: it is referenced by
    /// operations without meaningful input/output but never defined in the model file,
    /// so it must be recognised by id rather than resolved.
    pub fn operation_input(&self, op: &OperationShape) -> Result<Option<&StructureShape>, ModelError> {
        let Some(r) = &op.input else { return Ok(None) };
        if r.target.as_str() == UNIT_SHAPE {
            return Ok(None);
        }
        match self.resolve(&r.target)? {
            Shape::Structure(s) => Ok(Some(s)),
            other => Err(ModelError::WrongType {
                id: r.target.clone(),
                expected: "structure",
                found: other.type_name(),
            }),
        }
    }

    pub fn operation_output(&self, op: &OperationShape) -> Result<Option<&StructureShape>, ModelError> {
        let Some(r) = &op.output else { return Ok(None) };
        if r.target.as_str() == UNIT_SHAPE {
            return Ok(None);
        }
        match self.resolve(&r.target)? {
            Shape::Structure(s) => Ok(Some(s)),
            other => Err(ModelError::WrongType {
                id: r.target.clone(),
                expected: "structure",
                found: other.type_name(),
            }),
        }
    }

    /// Whether an operation's output is a streaming blob (botocore's
    /// `has_streaming_output`).
    ///
    /// The reference CLI gives such operations a positional `outfile` argument and
    /// suppresses `--cli-input-json` / `--cli-input-yaml` / `--generate-cli-skeleton`.
    ///
    /// botocore's definition is *any blob bound as the HTTP payload* — it never checks a
    /// streaming flag. So the Smithy translation is: a blob member carrying
    /// `smithy.api#httpPayload`, or a blob shape carrying `smithy.api#streaming`
    /// (streaming blobs are payload-bound by protocol rule, but check both directions).
    /// Only blob targets count — a streaming *union* is an event stream, which the CLI
    /// either removes outright (`removals.py`) or handles with a bespoke customization.
    pub fn operation_has_streaming_blob_output(
        &self,
        op: &OperationShape,
    ) -> Result<bool, ModelError> {
        let Some(output) = self.operation_output(op)? else { return Ok(false) };
        for member in output.members.values() {
            if member.target.as_str() == UNIT_SHAPE {
                continue;
            }
            if let Shape::Blob(blob) = self.resolve(&member.target)? {
                if member.traits.has("smithy.api#httpPayload")
                    || blob.traits.has("smithy.api#streaming")
                    || member.traits.has("smithy.api#streaming")
                {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// The service's wire protocol, taken from whichever `aws.protocols#*` trait is present.
    pub fn protocol(&self) -> Result<Protocol, ModelError> {
        let traits = &self.service()?.traits;
        for (id, proto) in Protocol::TRAIT_TABLE {
            if traits.has(id) {
                return Ok(*proto);
            }
        }
        Ok(Protocol::Unknown)
    }
}

/// The AWS wire protocols. Each needs its own serializer and response parser; this enum
/// is what the protocol crate dispatches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    AwsJson1_0,
    AwsJson1_1,
    RestJson1,
    RestXml,
    AwsQuery,
    Ec2Query,
    /// Newer binary protocol used by a small number of services.
    Rpcv2Cbor,
    Unknown,
}

impl Protocol {
    /// Consulted in order, so this is a preference list rather than a lookup: sixteen
    /// services declare more than one protocol and the first match wins.
    ///
    /// `rpcv2Cbor` is first because it is the most efficient of them. CBOR is a binary
    /// encoding — integers are integers rather than decimal text, and member names are
    /// length-prefixed rather than quoted and escaped — so the same request and response
    /// are materially smaller than the JSON or XML the same services also speak, and
    /// neither side has to render or scan decimal digits. The services that offer it
    /// offer it precisely because it is the one they would rather receive.
    const TRAIT_TABLE: &'static [(&'static str, Protocol)] = &[
        ("smithy.protocols#rpcv2Cbor", Protocol::Rpcv2Cbor),
        ("aws.protocols#awsJson1_0", Protocol::AwsJson1_0),
        ("aws.protocols#awsJson1_1", Protocol::AwsJson1_1),
        ("aws.protocols#restJson1", Protocol::RestJson1),
        ("aws.protocols#restXml", Protocol::RestXml),
        ("aws.protocols#awsQuery", Protocol::AwsQuery),
        ("aws.protocols#ec2Query", Protocol::Ec2Query),
    ];
}
