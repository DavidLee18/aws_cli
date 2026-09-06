//! Values in the endpoint rules language.
//!
//! The language is dynamically typed over a small set: string, bool, array, object
//! (records returned by `aws.partition` / `aws.parseArn` / `parseURL`), plus an explicit
//! "not set" that is distinct from any value — `isSet` tests exactly that.

use serde_json::Value as Json;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// Absent. Returned by a failed lookup or a partial function; `isSet` is false only
    /// for this.
    None,
    Bool(bool),
    Int(i64),
    String(String),
    Array(Vec<Value>),
    Record(BTreeMap<String, Value>),
}

impl Value {
    pub fn is_set(&self) -> bool {
        !matches!(self, Value::None)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// Truthiness as the rules engine defines it: a condition passes when its result is
    /// set and, if boolean, true. A non-boolean set value (a record from `aws.partition`,
    /// say) passes — that is how `assign`-only conditions work.
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::None => false,
            Value::Bool(b) => *b,
            _ => true,
        }
    }

    /// Attribute/index access for `getAttr` and `{Ref#path}` templates.
    ///
    /// Paths mix dotted names and bracket indices: `Foo.bar[2].baz`.
    pub fn get_path(&self, path: &str) -> Value {
        let mut current = self.clone();
        for segment in split_path(path) {
            current = match segment {
                Segment::Field(name) => match &current {
                    Value::Record(map) => map.get(&name).cloned().unwrap_or(Value::None),
                    _ => return Value::None,
                },
                Segment::Index(i) => match &current {
                    Value::Array(items) => items.get(i).cloned().unwrap_or(Value::None),
                    _ => return Value::None,
                },
            };
        }
        current
    }

    /// How a value renders inside a template string.
    pub fn to_template_string(&self) -> String {
        match self {
            Value::String(s) => s.clone(),
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::None => String::new(),
            other => format!("{other:?}"),
        }
    }
}

enum Segment {
    Field(String),
    Index(usize),
}

fn split_path(path: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    for part in path.split('.') {
        let mut rest = part;
        // A part may be `name`, `name[0]`, or a bare `[0]`.
        if let Some(open) = rest.find('[') {
            let (name, brackets) = rest.split_at(open);
            if !name.is_empty() {
                out.push(Segment::Field(name.to_string()));
            }
            rest = brackets;
            for chunk in rest.split('[').filter(|c| !c.is_empty()) {
                if let Some(idx) = chunk.strip_suffix(']').and_then(|n| n.parse().ok()) {
                    out.push(Segment::Index(idx));
                }
            }
        } else if !rest.is_empty() {
            out.push(Segment::Field(rest.to_string()));
        }
    }
    out
}

impl From<&Json> for Value {
    fn from(json: &Json) -> Self {
        match json {
            Json::Null => Value::None,
            Json::Bool(b) => Value::Bool(*b),
            Json::Number(n) => n.as_i64().map(Value::Int).unwrap_or(Value::None),
            Json::String(s) => Value::String(s.clone()),
            Json::Array(items) => Value::Array(items.iter().map(Value::from).collect()),
            Json::Object(map) => {
                Value::Record(map.iter().map(|(k, v)| (k.clone(), Value::from(v))).collect())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record() -> Value {
        Value::from(&json!({
            "name": "aws",
            "dnsSuffix": "amazonaws.com",
            "supportsFIPS": true,
            "nested": {"deep": ["a", "b"]}
        }))
    }

    #[test]
    fn reads_nested_paths_and_indices() {
        let v = record();
        assert_eq!(v.get_path("dnsSuffix").as_str(), Some("amazonaws.com"));
        assert_eq!(v.get_path("supportsFIPS").as_bool(), Some(true));
        assert_eq!(v.get_path("nested.deep[1]").as_str(), Some("b"));
    }

    #[test]
    fn missing_paths_are_none_not_errors() {
        let v = record();
        assert!(!v.get_path("nope").is_set());
        assert!(!v.get_path("nested.deep[9]").is_set());
        // Traversing through a scalar is also just absent.
        assert!(!v.get_path("dnsSuffix.further").is_set());
    }

    #[test]
    fn truthiness_distinguishes_none_false_and_records() {
        assert!(!Value::None.is_truthy());
        assert!(!Value::Bool(false).is_truthy());
        assert!(Value::Bool(true).is_truthy());
        // A record assigned by aws.partition passes its condition.
        assert!(record().is_truthy());
        assert!(Value::String(String::new()).is_truthy());
    }
}
