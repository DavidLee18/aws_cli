//! Smithy shape identifiers: `namespace#Name$member`.

use std::fmt;

/// An absolute Smithy shape id, stored as the full string with cached split points.
///
/// Smithy ids look like `com.amazonaws.sts#GetCallerIdentity` or, for members,
/// `com.amazonaws.sts#Credentials$AccessKeyId`.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShapeId {
    full: String,
    /// Byte index of `#`.
    hash: usize,
    /// Byte index of `$`, if this id names a member.
    dollar: Option<usize>,
}

impl ShapeId {
    pub fn parse(s: &str) -> Result<Self, InvalidShapeId> {
        let hash = s.find('#').ok_or_else(|| InvalidShapeId(s.to_string()))?;
        let dollar = s[hash..].find('$').map(|i| i + hash);
        if hash == 0 || hash + 1 == s.len() {
            return Err(InvalidShapeId(s.to_string()));
        }
        Ok(Self { full: s.to_string(), hash, dollar })
    }

    pub fn as_str(&self) -> &str {
        &self.full
    }

    pub fn namespace(&self) -> &str {
        &self.full[..self.hash]
    }

    /// The shape name, excluding any `$member` suffix.
    pub fn name(&self) -> &str {
        let end = self.dollar.unwrap_or(self.full.len());
        &self.full[self.hash + 1..end]
    }

    /// The `$member` suffix, if present.
    pub fn member(&self) -> Option<&str> {
        self.dollar.map(|d| &self.full[d + 1..])
    }

    /// The id with any `$member` suffix stripped.
    pub fn root(&self) -> ShapeId {
        match self.dollar {
            None => self.clone(),
            Some(d) => ShapeId {
                full: self.full[..d].to_string(),
                hash: self.hash,
                dollar: None,
            },
        }
    }
}

impl fmt::Display for ShapeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.full)
    }
}

impl fmt::Debug for ShapeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ShapeId({})", self.full)
    }
}

impl<'de> serde::Deserialize<'de> for ShapeId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        ShapeId::parse(&s).map_err(serde::de::Error::custom)
    }
}

impl serde::Serialize for ShapeId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.full)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid Smithy shape id: {0}")]
pub struct InvalidShapeId(pub String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_id() {
        let id = ShapeId::parse("com.amazonaws.sts#GetCallerIdentity").unwrap();
        assert_eq!(id.namespace(), "com.amazonaws.sts");
        assert_eq!(id.name(), "GetCallerIdentity");
        assert_eq!(id.member(), None);
    }

    #[test]
    fn parses_member_id() {
        let id = ShapeId::parse("com.amazonaws.sts#Credentials$AccessKeyId").unwrap();
        assert_eq!(id.name(), "Credentials");
        assert_eq!(id.member(), Some("AccessKeyId"));
        assert_eq!(id.root().as_str(), "com.amazonaws.sts#Credentials");
    }

    #[test]
    fn rejects_unqualified() {
        assert!(ShapeId::parse("GetCallerIdentity").is_err());
        assert!(ShapeId::parse("#Foo").is_err());
        assert!(ShapeId::parse("foo#").is_err());
    }
}
