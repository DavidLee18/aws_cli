//! Client-side parameter validation, before any request is sent.
//!
//! A port of `awscli/botocore/validate.py`. Two things about it are surprising enough to
//! state plainly:
//!
//! - **Only `min` is ever checked, never `max`.** `range_check` tests the lower bound and
//!   returns; a value over the maximum is sent to the service and rejected there.
//! - **All errors are collected**, not just the first, and reported as one newline-joined
//!   block.
//!
//! Names are built as `input` at the root, then `.member`, `[index]`, and `.key` for map
//! values, with a leading `.` stripped.

use aws_cli_model::shape::StructureShape;
use aws_cli_model::{Model, Shape, ShapeId};
use serde_json::Value;

/// The collected report, formatted the way the reference prints it.
#[derive(Debug, Default)]
pub struct ValidationErrors {
    messages: Vec<String>,
}

impl ValidationErrors {
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// `Parameter validation failed:\n<one message per line>`
    pub fn report(&self) -> String {
        format!("Parameter validation failed:\n{}", self.messages.join("\n"))
    }
}

/// Validate `params` against an operation's input shape.
pub fn validate(
    model: &Model,
    shape: Option<&StructureShape>,
    params: Option<&Value>,
) -> ValidationErrors {
    let mut errors = ValidationErrors::default();
    let Some(shape) = shape else { return errors };
    let empty = Value::Object(Default::default());
    let params = params.unwrap_or(&empty);
    validate_structure(model, shape, params, "", &mut errors);
    errors
}

fn display_name(name: &str) -> String {
    if name.is_empty() {
        "input".to_string()
    } else {
        name.strip_prefix('.').unwrap_or(name).to_string()
    }
}

fn validate_structure(
    model: &Model,
    shape: &StructureShape,
    params: &Value,
    name: &str,
    errors: &mut ValidationErrors,
) {
    let Some(map) = params.as_object() else {
        errors.messages.push(format!(
            "Invalid type for parameter {}, value: {}, type: {}, valid types: <class 'dict'>",
            display_name(name),
            render(params),
            python_type(params)
        ));
        return;
    };

    // Required members first.
    for (member_name, member) in &shape.members {
        if member.traits.is_required() && !map.contains_key(member_name) {
            errors.messages.push(format!(
                "Missing required parameter in {}: \"{member_name}\"",
                display_name(name)
            ));
        }
    }

    // Then unknown members, listing the valid ones.
    let valid: Vec<&str> = shape.members.keys().map(|k| k.as_str()).collect();
    for key in map.keys() {
        if !shape.members.contains_key(key) {
            errors.messages.push(format!(
                "Unknown parameter in {}: \"{key}\", must be one of: {}",
                display_name(name),
                valid.join(", ")
            ));
        }
    }

    // Then recurse into the members we do know.
    for (key, value) in map {
        let Some(member) = shape.members.get(key) else { continue };
        validate_value(model, &member.target, value, &format!("{name}.{key}"), errors);
    }
}

fn validate_value(
    model: &Model,
    target: &ShapeId,
    value: &Value,
    name: &str,
    errors: &mut ValidationErrors,
) {
    let Some(shape) = model.shape(target) else { return };

    match shape {
        Shape::Structure(s) | Shape::Union(s) => {
            validate_structure(model, s, value, name, errors)
        }

        Shape::List(list) | Shape::Set(list) => {
            let Some(items) = value.as_array() else {
                return type_error(value, name, "<class 'list'>, <class 'tuple'>", errors);
            };
            // For a list, the constraint is on the item count.
            min_check(items.len() as f64, shape, name, Constraint::Length, value, errors);
            for (i, item) in items.iter().enumerate() {
                validate_value(model, &list.member.target, item, &format!("{name}[{i}]"), errors);
            }
        }

        Shape::Map(map_shape) => {
            let Some(entries) = value.as_object() else {
                return type_error(value, name, "<class 'dict'>", errors);
            };
            for (key, entry) in entries {
                validate_value(
                    model,
                    &map_shape.key.target,
                    &Value::String(key.clone()),
                    &format!("{name} (key: {key})"),
                    errors,
                );
                validate_value(model, &map_shape.value.target, entry, &format!("{name}.{key}"), errors);
            }
        }

        Shape::String(_) | Shape::Enum(_) => {
            let Some(s) = value.as_str() else {
                return type_error(value, name, "<class 'str'>", errors);
            };
            // For a string the constraint is on its length.
            min_check(s.chars().count() as f64, shape, name, Constraint::Length, value, errors);
        }

        Shape::Integer(_) | Shape::Long(_) | Shape::Short(_) | Shape::Byte(_) => {
            let Some(n) = value.as_i64() else {
                return type_error(value, name, "<class 'int'>", errors);
            };
            min_check(n as f64, shape, name, Constraint::Range, value, errors);
        }

        Shape::Float(_) | Shape::Double(_) => {
            let Some(n) = value.as_f64() else {
                return type_error(value, name, "<class 'float'>, <class 'int'>", errors);
            };
            min_check(n, shape, name, Constraint::Range, value, errors);
        }

        Shape::Boolean(_) => {
            if !value.is_boolean() {
                type_error(value, name, "<class 'bool'>", errors);
            }
        }

        // Blobs accept text, and timestamps accept a string or a number; neither is
        // range-checked.
        Shape::Blob(_) | Shape::Timestamp(_) | Shape::Document(_) => {}

        _ => {}
    }
}

enum Constraint {
    Length,
    Range,
}

/// The reference checks ONLY the minimum. A value above `max` is left to the service.
fn min_check(
    measured: f64,
    shape: &Shape,
    name: &str,
    constraint: Constraint,
    original: &Value,
    errors: &mut ValidationErrors,
) {
    let trait_id = match constraint {
        Constraint::Length => "smithy.api#length",
        Constraint::Range => "smithy.api#range",
    };
    let Some(min) = shape.traits().get(trait_id).and_then(|v| v.get("min")).and_then(|v| v.as_f64())
    else {
        return;
    };
    if measured >= min {
        return;
    }
    let min_text = if min.fract() == 0.0 { format!("{}", min as i64) } else { min.to_string() };
    let message = match constraint {
        Constraint::Length => format!(
            "Invalid length for parameter {}, value: {}, valid min length: {min_text}",
            display_name(name),
            measured as i64
        ),
        Constraint::Range => format!(
            "Invalid value for parameter {}, value: {}, valid min value: {min_text}",
            display_name(name),
            render(original)
        ),
    };
    errors.messages.push(message);
}

fn type_error(value: &Value, name: &str, valid_types: &str, errors: &mut ValidationErrors) {
    errors.messages.push(format!(
        "Invalid type for parameter {}, value: {}, type: {}, valid types: {valid_types}",
        display_name(name),
        render(value),
        python_type(value)
    ));
}

/// Python's `str()` of the value, as it appears in the message.
fn render(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        other => other.to_string(),
    }
}

fn python_type(value: &Value) -> &'static str {
    match value {
        Value::String(_) => "<class 'str'>",
        Value::Number(n) if n.is_i64() || n.is_u64() => "<class 'int'>",
        Value::Number(_) => "<class 'float'>",
        Value::Bool(_) => "<class 'bool'>",
        Value::Array(_) => "<class 'list'>",
        Value::Object(_) => "<class 'dict'>",
        Value::Null => "<class 'NoneType'>",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn model() -> Model {
        Model::from_json(
            br#"{"smithy":"2.0","shapes":{
                "com.x#S":{"type":"service","version":"1","traits":{}},
                "com.x#Arn":{"type":"string","traits":{"smithy.api#length":{"min":20,"max":2048}}},
                "com.x#Count":{"type":"integer","traits":{"smithy.api#range":{"min":1,"max":10}}},
                "com.x#Names":{"type":"list","member":{"target":"smithy.api#String"},
                               "traits":{"smithy.api#length":{"min":2}}},
                "com.x#In":{"type":"structure","members":{
                    "RoleArn":{"target":"com.x#Arn","traits":{"smithy.api#required":{}}},
                    "Count":{"target":"com.x#Count"},
                    "Names":{"target":"com.x#Names"},
                    "Flag":{"target":"smithy.api#Boolean"}}}}}"#,
        )
        .unwrap()
    }

    fn input(m: &Model) -> StructureShape {
        match m.shape(&ShapeId::parse("com.x#In").unwrap()) {
            Some(Shape::Structure(s)) => s.clone(),
            _ => panic!("input shape"),
        }
    }

    #[test]
    fn reports_missing_required_members() {
        let m = model();
        let errors = validate(&m, Some(&input(&m)), Some(&json!({})));
        assert_eq!(
            errors.report(),
            "Parameter validation failed:\nMissing required parameter in input: \"RoleArn\""
        );
    }

    #[test]
    fn reports_unknown_members_with_the_valid_list() {
        let m = model();
        let errors = validate(&m, Some(&input(&m)), Some(&json!({"RoleArn": "a".repeat(20), "Nope": 1})));
        assert!(errors.report().contains("Unknown parameter in input: \"Nope\", must be one of:"));
    }

    /// The exact wording the reference emits, verified live.
    #[test]
    fn reports_string_length_below_minimum() {
        let m = model();
        let errors = validate(&m, Some(&input(&m)), Some(&json!({"RoleArn": "abc"})));
        assert_eq!(
            errors.report(),
            "Parameter validation failed:\n\
             Invalid length for parameter RoleArn, value: 3, valid min length: 20"
        );
    }

    /// botocore checks ONLY the minimum — an over-long value is the service's problem.
    #[test]
    fn does_not_check_maximums() {
        let m = model();
        let errors = validate(
            &m,
            Some(&input(&m)),
            Some(&json!({"RoleArn": "a".repeat(5000), "Count": 999})),
        );
        assert!(errors.is_empty(), "got: {}", errors.report());
    }

    #[test]
    fn reports_numeric_range_and_list_length() {
        let m = model();
        let errors =
            validate(&m, Some(&input(&m)), Some(&json!({"RoleArn": "a".repeat(20), "Count": 0})));
        assert!(errors.report().contains("Invalid value for parameter Count, value: 0, valid min value: 1"));

        let errors =
            validate(&m, Some(&input(&m)), Some(&json!({"RoleArn": "a".repeat(20), "Names": ["x"]})));
        assert!(errors.report().contains("Invalid length for parameter Names, value: 1, valid min length: 2"));
    }

    #[test]
    fn reports_type_mismatches() {
        let m = model();
        let errors =
            validate(&m, Some(&input(&m)), Some(&json!({"RoleArn": "a".repeat(20), "Flag": "yes"})));
        assert!(
            errors.report().contains("Invalid type for parameter Flag, value: yes"),
            "got: {}",
            errors.report()
        );
    }

    /// Every error is collected, not just the first.
    #[test]
    fn collects_all_errors() {
        let m = model();
        let errors = validate(&m, Some(&input(&m)), Some(&json!({"Count": 0, "Nope": 1})));
        assert_eq!(errors.report().lines().count(), 4, "header plus three errors");
    }

    #[test]
    fn valid_input_produces_nothing() {
        let m = model();
        let errors = validate(
            &m,
            Some(&input(&m)),
            Some(&json!({"RoleArn": "a".repeat(20), "Count": 5, "Names": ["a", "b"]})),
        );
        assert!(errors.is_empty(), "got: {}", errors.report());
    }
}
