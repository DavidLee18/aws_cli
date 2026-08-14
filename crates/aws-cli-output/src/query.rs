//! `--query`: JMESPath filtering of the response before formatting.
//!
//! JMESPath is a published specification with an official compliance suite, so this
//! delegates to the `jmespath` crate rather than hand-rolling a subset — unlike sigv4 or
//! the endpoint rules, there are no botocore-specific quirks to match here, only the
//! standard.
//!
//! Applied *after* `ResponseMetadata` is stripped and after pagination merges pages, so a
//! query can never see either.

use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error("Invalid JMESPath expression: {0}")]
    Invalid(String),
    #[error("Error evaluating JMESPath expression: {0}")]
    Evaluation(String),
}

/// Compile an expression, so an invalid `--query` fails before any API call is made —
/// the reference validates it on `top-level-args-parsed`.
pub fn validate(expression: &str) -> Result<(), QueryError> {
    jmespath::compile(&relax_literals(expression))
        .map(|_| ())
        .map_err(|e| QueryError::Invalid(e.to_string()))
}

/// Make backtick literals lenient, the way Python's `jmespath` is.
///
/// A JMESPath literal is `` `json` ``, so a string literal is strictly `` `"us-east-1"` ``.
/// Python's implementation also accepts the unquoted `` `us-east-1` `` — a legacy
/// behaviour that a great many published AWS CLI examples rely on
/// (`--query "Regions[?RegionName==`us-east-1`]"`). The Rust crate is strict, so any
/// literal whose contents are not valid JSON is quoted here before compiling.
///
/// Only the inside of backtick pairs is touched; `\``-escapes are preserved.
fn relax_literals(expression: &str) -> String {
    let mut out = String::with_capacity(expression.len());
    let mut chars = expression.char_indices().peekable();

    while let Some((_, c)) = chars.next() {
        if c != '`' {
            out.push(c);
            continue;
        }
        // Collect to the closing backtick, honouring backslash escapes.
        let mut literal = String::new();
        let mut closed = false;
        while let Some((_, c)) = chars.next() {
            if c == '\\' {
                if let Some((_, escaped)) = chars.next() {
                    literal.push('\\');
                    literal.push(escaped);
                }
                continue;
            }
            if c == '`' {
                closed = true;
                break;
            }
            literal.push(c);
        }
        if !closed {
            // Unterminated: hand it to the parser unchanged so the error is the
            // parser's, not ours.
            out.push('`');
            out.push_str(&literal);
            break;
        }
        let is_json = serde_json::from_str::<serde_json::Value>(literal.trim()).is_ok();
        out.push('`');
        if is_json {
            out.push_str(&literal);
        } else {
            out.push_str(&serde_json::Value::String(literal).to_string());
        }
        out.push('`');
    }
    out
}

/// Evaluate `expression` against `value`.
pub fn apply(value: &Value, expression: &str) -> Result<Value, QueryError> {
    let compiled = jmespath::compile(&relax_literals(expression))
        .map_err(|e| QueryError::Invalid(e.to_string()))?;
    // The crate takes anything Serialize; going through its own Variable keeps the
    // conversion in one place.
    let data = jmespath::Variable::try_from(value.clone())
        .map_err(|e| QueryError::Evaluation(e.to_string()))?;
    let result =
        compiled.search(data).map_err(|e| QueryError::Evaluation(e.to_string()))?;

    // The crate's Variable serialises straight back into serde_json.
    serde_json::to_value(&*result).map_err(|e| QueryError::Evaluation(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn data() -> Value {
        json!({
            "Roles": [
                {"RoleName": "alpha", "Id": 1, "Tags": ["a", "b"]},
                {"RoleName": "beta", "Id": 2, "Tags": []}
            ],
            "IsTruncated": false
        })
    }

    #[test]
    fn projects_fields_from_a_list() {
        assert_eq!(apply(&data(), "Roles[].RoleName").unwrap(), json!(["alpha", "beta"]));
    }

    #[test]
    fn indexes_filters_and_multiselects() {
        assert_eq!(apply(&data(), "Roles[0].RoleName").unwrap(), json!("alpha"));
        assert_eq!(apply(&data(), "Roles[?Id > `1`].RoleName").unwrap(), json!(["beta"]));
        assert_eq!(
            apply(&data(), "Roles[].{Name: RoleName, N: Id}").unwrap(),
            json!([{"Name": "alpha", "N": 1}, {"Name": "beta", "N": 2}])
        );
    }

    /// A query can legitimately produce a scalar rather than an object — the formatters
    /// have to cope with that.
    #[test]
    fn can_produce_scalars_and_null() {
        assert_eq!(apply(&data(), "IsTruncated").unwrap(), json!(false));
        assert_eq!(apply(&data(), "length(Roles)").unwrap(), json!(2));
        // A path that matches nothing yields null, not an error.
        assert_eq!(apply(&data(), "Nope").unwrap(), Value::Null);
    }

    /// The unquoted-literal form appears in most published AWS CLI examples, and the
    /// reference accepts it even though it is not strictly valid JMESPath.
    #[test]
    fn accepts_unquoted_backtick_literals() {
        let d = data();
        assert_eq!(
            apply(&d, "Roles[?RoleName==`alpha`].Id").unwrap(),
            json!([1]),
            "unquoted literal, as the reference accepts"
        );
        assert_eq!(
            apply(&d, "Roles[?RoleName==`\"alpha\"`].Id").unwrap(),
            json!([1]),
            "properly quoted literal still works"
        );
        // Genuine JSON literals must keep their meaning, not become strings.
        assert_eq!(apply(&d, "Roles[?Id==`2`].RoleName").unwrap(), json!(["beta"]));
        assert_eq!(apply(&d, "Roles[?Id > `1`].RoleName").unwrap(), json!(["beta"]));
    }

    #[test]
    fn relaxing_leaves_valid_json_literals_alone() {
        assert_eq!(relax_literals("a==`2`"), "a==`2`");
        assert_eq!(relax_literals("a==`true`"), "a==`true`");
        assert_eq!(relax_literals(r#"a==`"s"`"#), r#"a==`"s"`"#);
        // Only non-JSON contents get quoted.
        assert_eq!(relax_literals("a==`us-east-1`"), r#"a==`"us-east-1"`"#);
        assert_eq!(relax_literals("no literals here"), "no literals here");
    }

    #[test]
    fn invalid_expressions_are_rejected_before_use() {
        assert!(validate("Roles[].RoleName").is_ok());
        assert!(validate("Roles[").is_err());
        assert!(apply(&data(), "Roles[").is_err());
    }
}
