//! The `text` output format.
//!
//! Tab-separated, no keys, no indentation. The rules are unobvious enough to be worth
//! stating (all verified against `awscli/text.py`):
//!
//! - **Keys are never printed.** Only values, joined by tabs.
//! - Scalar members of a dict are emitted **sorted by key name**, not model order.
//! - A nested container is labelled with its own key, **uppercased**, as the first field
//!   — the nesting depth is not encoded at all.
//! - A dict with no scalar members emits no label line; the label is simply dropped.
//! - Python `str()` semantics: `true`/`false`/`null` print as `True`/`False`/`None`.
//! - Empty containers emit nothing whatsoever, not even a newline.

use serde_json::{Map, Value};

/// Render a value as text. Returns `None` when nothing at all should be printed.
pub fn render(value: &Value) -> Option<String> {
    let mut out = String::new();
    format_value(value, None, None, &mut out);
    (!out.is_empty()).then_some(out)
}

fn format_value(value: &Value, identifier: Option<&str>, scalar_keys: Option<&[String]>, out: &mut String) {
    match value {
        Value::Object(map) => format_dict(map, identifier, scalar_keys, out),
        Value::Array(items) => format_list(items, identifier, out),
        scalar => {
            out.push_str(&scalar_to_text(scalar));
            out.push('\n');
        }
    }
}

/// Python's `str()` of the value, which is not JSON's spelling.
fn scalar_to_text(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

fn is_scalar(value: &Value) -> bool {
    !matches!(value, Value::Object(_) | Value::Array(_))
}

fn format_dict(
    map: &Map<String, Value>,
    identifier: Option<&str>,
    scalar_keys: Option<&[String]>,
    out: &mut String,
) {
    // When rendering a list of dicts, every row shares one column set so the output
    // lines up; otherwise the columns are this dict's own scalar keys.
    let owned: Vec<String>;
    let keys: &[String] = match scalar_keys {
        Some(keys) => keys,
        None => {
            owned = sorted_scalar_keys(map);
            &owned
        }
    };

    if !keys.is_empty() {
        let mut fields: Vec<String> = Vec::with_capacity(keys.len() + 1);
        if let Some(label) = identifier {
            fields.push(label.to_uppercase());
        }
        for key in keys {
            // A key missing from this particular row becomes an empty column.
            fields.push(map.get(key).map(scalar_to_text).unwrap_or_default());
        }
        out.push_str(&fields.join("\t"));
        out.push('\n');
    }

    // Then the containers, in sorted-key order, each labelled with its own key.
    let mut nested: Vec<(&String, &Value)> =
        map.iter().filter(|(_, v)| !is_scalar(v)).collect();
    nested.sort_by(|a, b| a.0.cmp(b.0));
    for (key, value) in nested {
        format_value(value, Some(key), None, out);
    }
}

fn sorted_scalar_keys(map: &Map<String, Value>) -> Vec<String> {
    let mut keys: Vec<String> =
        map.iter().filter(|(_, v)| is_scalar(v)).map(|(k, _)| k.clone()).collect();
    keys.sort();
    keys
}

/// The sorted union of scalar keys across every dict in a list, so all rows share
/// columns.
fn union_scalar_keys(items: &[Value]) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for item in items {
        if let Value::Object(map) = item {
            for key in map.iter().filter(|(_, v)| is_scalar(v)).map(|(k, _)| k) {
                if !keys.contains(key) {
                    keys.push(key.clone());
                }
            }
        }
    }
    keys.sort();
    keys
}

fn format_list(items: &[Value], identifier: Option<&str>, out: &mut String) {
    if items.is_empty() {
        return;
    }

    if items.iter().any(|i| matches!(i, Value::Object(_))) {
        let keys = union_scalar_keys(items);
        for item in items {
            format_value(item, identifier, Some(&keys), out);
        }
        return;
    }

    if items.iter().any(|i| matches!(i, Value::Array(_))) {
        let scalars: Vec<&Value> = items.iter().filter(|i| is_scalar(i)).collect();
        if !scalars.is_empty() {
            format_scalar_list(&scalars, identifier, out);
        }
        for item in items.iter().filter(|i| matches!(i, Value::Array(_))) {
            format_value(item, identifier, None, out);
        }
        return;
    }

    let scalars: Vec<&Value> = items.iter().collect();
    format_scalar_list(&scalars, identifier, out);
}

/// A labelled scalar list prints one line per element; an unlabelled one prints a single
/// tab-joined line. That second form is the `--query ... --output text` idiom people
/// pipe into `xargs`.
fn format_scalar_list(items: &[&Value], identifier: Option<&str>, out: &mut String) {
    match identifier {
        Some(label) => {
            for item in items {
                out.push_str(&label.to_uppercase());
                out.push('\t');
                out.push_str(&scalar_to_text(item));
                out.push('\n');
            }
        }
        None => {
            let joined: Vec<String> = items.iter().map(|i| scalar_to_text(i)).collect();
            out.push_str(&joined.join("\t"));
            out.push('\n');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The worked example: keys sorted (Id before Name), key names absent, parent key
    /// uppercased as the first column.
    #[test]
    fn renders_a_list_of_dicts() {
        let v = json!({"Users": [{"Name": "a", "Id": 1}]});
        assert_eq!(render(&v).unwrap(), "USERS\t1\ta\n");
    }

    #[test]
    fn nested_containers_are_labelled_and_flattened() {
        let v = json!({
            "Users": [{"Name": "a", "Id": 1, "Groups": ["g1", "g2"],
                       "Tags": [{"K": "k1", "V": "v1"}]}],
            "Marker": null
        });
        // Top-level scalar first (unlabelled), then the row, then nested by sorted key.
        assert_eq!(
            render(&v).unwrap(),
            "None\nUSERS\t1\ta\nGROUPS\tg1\nGROUPS\tg2\nTAGS\tk1\tv1\n"
        );
    }

    /// A dict with no scalar members emits no label line at all.
    #[test]
    fn dicts_without_scalars_emit_no_label() {
        let v = json!({"Outer": {"Inner": {"Deep": "v", "N": 2}}});
        assert_eq!(render(&v).unwrap(), "INNER\tv\t2\n");
    }

    #[test]
    fn uses_python_scalar_spellings() {
        let v = json!({"A": true, "B": false, "C": null, "D": 1.5, "E": ""});
        assert_eq!(render(&v).unwrap(), "True\tFalse\tNone\t1.5\t\n");
    }

    #[test]
    fn empty_containers_emit_nothing() {
        assert_eq!(render(&json!({})), None);
        assert_eq!(render(&json!([])), None);
        assert_eq!(render(&json!({"F": {}, "G": []})), None);
    }

    /// The `--query`-into-`xargs` idiom: a bare list of scalars is one tab-joined line.
    #[test]
    fn bare_scalar_lists_join_on_one_line() {
        assert_eq!(render(&json!(["a", "bb"])).unwrap(), "a\tbb\n");
        // A list of lists prints one line each.
        assert_eq!(render(&json!([["a", "b"], ["c", "d"]])).unwrap(), "a\tb\nc\td\n");
    }

    #[test]
    fn bare_scalars_print_unquoted() {
        assert_eq!(render(&json!("a")).unwrap(), "a\n");
        assert_eq!(render(&json!(5)).unwrap(), "5\n");
        assert_eq!(render(&json!(null)).unwrap(), "None\n");
    }

    /// Rows share a column set, so a member missing from one row leaves a blank column.
    #[test]
    fn missing_keys_become_empty_columns() {
        let v = json!({"R": [{"A": 1, "B": 2}, {"A": 3}]});
        assert_eq!(render(&v).unwrap(), "R\t1\t2\nR\t3\t\n");
    }
}
