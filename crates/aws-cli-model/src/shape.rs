//! Smithy shape definitions as they appear in the JSON AST.

use crate::shape_id::ShapeId;
use indexmap::IndexMap;
use serde::Deserialize;
use std::collections::BTreeMap;

/// Structure members keep their MODEL order, not alphabetical order: the CLI prints
/// response fields in the order the model declares them, so a sorted map would emit
/// `Account, Arn, UserId` where the reference emits `UserId, Account, Arn`.
pub type Members = IndexMap<String, Member>;

/// Trait values are kept as raw JSON. Traits are an open set — AWS ships dozens of
/// protocol- and endpoint-specific ones — so decoding them eagerly into typed structs
/// would mean churning this crate every time a new one appears. Consumers reach for the
/// handful they care about via [`Traits::get`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(transparent)]
pub struct Traits(pub BTreeMap<String, serde_json::Value>);

impl Traits {
    pub fn get(&self, id: &str) -> Option<&serde_json::Value> {
        self.0.get(id)
    }

    pub fn has(&self, id: &str) -> bool {
        self.0.contains_key(id)
    }

    /// `smithy.api#documentation`, which AWS ships as HTML.
    pub fn documentation(&self) -> Option<&str> {
        self.get("smithy.api#documentation")?.as_str()
    }

    pub fn is_required(&self) -> bool {
        self.has("smithy.api#required")
    }

    pub fn is_deprecated(&self) -> bool {
        self.has("smithy.api#deprecated")
    }

    /// `smithy.api#enum` (Smithy 1.0) — modern models use a dedicated `enum` shape
    /// instead, so this only fires on older vendored models.
    pub fn enum_values(&self) -> Option<Vec<&str>> {
        let entries = self.get("smithy.api#enum")?.as_array()?;
        Some(entries.iter().filter_map(|e| e.get("value")?.as_str()).collect())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Member {
    pub target: ShapeId,
    #[serde(default)]
    pub traits: Traits,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Reference {
    pub target: ShapeId,
}

/// A Smithy shape. `type` discriminates the variant.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Shape {
    // --- simple shapes ---
    Blob(SimpleShape),
    Boolean(SimpleShape),
    String(SimpleShape),
    Byte(SimpleShape),
    Short(SimpleShape),
    Integer(SimpleShape),
    Long(SimpleShape),
    Float(SimpleShape),
    Double(SimpleShape),
    BigInteger(SimpleShape),
    BigDecimal(SimpleShape),
    Timestamp(SimpleShape),
    Document(SimpleShape),

    // --- enums ---
    Enum(EnumShape),
    IntEnum(EnumShape),

    // --- aggregate shapes ---
    List(ListShape),
    /// Deprecated in Smithy 2.0 in favour of `list` with `@uniqueItems`, but still
    /// present in some vendored models.
    Set(ListShape),
    Map(MapShape),
    Structure(StructureShape),
    Union(StructureShape),

    // --- service shapes ---
    Service(ServiceShape),
    Operation(OperationShape),
    Resource(ResourceShape),
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SimpleShape {
    #[serde(default)]
    pub traits: Traits,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EnumShape {
    #[serde(default)]
    pub members: Members,
    #[serde(default)]
    pub traits: Traits,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListShape {
    pub member: Member,
    #[serde(default)]
    pub traits: Traits,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MapShape {
    pub key: Member,
    pub value: Member,
    #[serde(default)]
    pub traits: Traits,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StructureShape {
    #[serde(default)]
    pub members: Members,
    #[serde(default)]
    pub traits: Traits,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceShape {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub operations: Vec<Reference>,
    #[serde(default)]
    pub resources: Vec<Reference>,
    #[serde(default)]
    pub errors: Vec<Reference>,
    #[serde(default)]
    pub traits: Traits,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationShape {
    pub input: Option<Reference>,
    pub output: Option<Reference>,
    #[serde(default)]
    pub errors: Vec<Reference>,
    #[serde(default)]
    pub traits: Traits,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceShape {
    #[serde(default)]
    pub operations: Vec<Reference>,
    #[serde(default)]
    pub collection_operations: Vec<Reference>,
    #[serde(default)]
    pub resources: Vec<Reference>,
    // Lifecycle bindings: each is a normal operation, attached via a dedicated slot
    // instead of `operations`. Over 1,000 operations across the catalogue are reachable
    // *only* through these, so a resource walk that skips them silently loses commands.
    pub create: Option<Reference>,
    pub put: Option<Reference>,
    pub read: Option<Reference>,
    pub update: Option<Reference>,
    pub delete: Option<Reference>,
    pub list: Option<Reference>,
    #[serde(default)]
    pub traits: Traits,
}

impl ResourceShape {
    /// All operations bound to this resource, lifecycle slots included.
    pub fn all_operations(&self) -> impl Iterator<Item = &Reference> {
        self.operations
            .iter()
            .chain(self.collection_operations.iter())
            .chain(
                [&self.create, &self.put, &self.read, &self.update, &self.delete, &self.list]
                    .into_iter()
                    .filter_map(|o| o.as_ref()),
            )
    }
}

impl Shape {
    pub fn traits(&self) -> &Traits {
        match self {
            Shape::Blob(s)
            | Shape::Boolean(s)
            | Shape::String(s)
            | Shape::Byte(s)
            | Shape::Short(s)
            | Shape::Integer(s)
            | Shape::Long(s)
            | Shape::Float(s)
            | Shape::Double(s)
            | Shape::BigInteger(s)
            | Shape::BigDecimal(s)
            | Shape::Timestamp(s)
            | Shape::Document(s) => &s.traits,
            Shape::Enum(s) | Shape::IntEnum(s) => &s.traits,
            Shape::List(s) | Shape::Set(s) => &s.traits,
            Shape::Map(s) => &s.traits,
            Shape::Structure(s) | Shape::Union(s) => &s.traits,
            Shape::Service(s) => &s.traits,
            Shape::Operation(s) => &s.traits,
            Shape::Resource(s) => &s.traits,
        }
    }

    /// Short type name, matching the `type` discriminator in the AST.
    pub fn type_name(&self) -> &'static str {
        match self {
            Shape::Blob(_) => "blob",
            Shape::Boolean(_) => "boolean",
            Shape::String(_) => "string",
            Shape::Byte(_) => "byte",
            Shape::Short(_) => "short",
            Shape::Integer(_) => "integer",
            Shape::Long(_) => "long",
            Shape::Float(_) => "float",
            Shape::Double(_) => "double",
            Shape::BigInteger(_) => "bigInteger",
            Shape::BigDecimal(_) => "bigDecimal",
            Shape::Timestamp(_) => "timestamp",
            Shape::Document(_) => "document",
            Shape::Enum(_) => "enum",
            Shape::IntEnum(_) => "intEnum",
            Shape::List(_) => "list",
            Shape::Set(_) => "set",
            Shape::Map(_) => "map",
            Shape::Structure(_) => "structure",
            Shape::Union(_) => "union",
            Shape::Service(_) => "service",
            Shape::Operation(_) => "operation",
            Shape::Resource(_) => "resource",
        }
    }

    /// True for shapes that carry no nested members — the leaves of a shape graph walk.
    pub fn is_scalar(&self) -> bool {
        matches!(
            self,
            Shape::Blob(_)
                | Shape::Boolean(_)
                | Shape::String(_)
                | Shape::Byte(_)
                | Shape::Short(_)
                | Shape::Integer(_)
                | Shape::Long(_)
                | Shape::Float(_)
                | Shape::Double(_)
                | Shape::BigInteger(_)
                | Shape::BigDecimal(_)
                | Shape::Timestamp(_)
                | Shape::Document(_)
                | Shape::Enum(_)
                | Shape::IntEnum(_)
        )
    }
}
