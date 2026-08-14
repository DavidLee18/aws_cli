//! The `yaml` and `yaml-stream` output formats.
//!
//! The reference uses ruamel with `typ='safe'` and `default_flow_style=False`. The
//! settings that matter for byte-parity:
//!
//! - **keys are sorted** (ruamel's `sort_base_mapping_type_on_output`)
//! - map indent 2, and **sequence dashes sit flush with the parent key's indentation**
//!   (`sequence_dash_offset = 0`), not indented under it
//! - no leading `---`
//! - non-ASCII is emitted literally (`allow_unicode = True`)
//! - empty containers use flow style even in block mode: `a: {}`, `a: []`
//! - a **top-level scalar is JSON-dumped instead**, to avoid YAML's `...` document-end
//!   marker — so a bare string prints quoted, unlike every other YAML scalar
//! - long **plain** scalars are folded at `best_width = 80`: the break happens at the
//!   first space PAST column 80, and continuation lines sit at the scalar's own indent

use serde_json::Value;

/// Render as YAML. `None` means print nothing, matching the reference's empty-response
/// rule.
pub fn render(value: &Value) -> Option<String> {
    if value.as_object().is_some_and(|o| o.is_empty()) {
        return None;
    }
    // A top-level scalar goes through JSON, not YAML.
    if !matches!(value, Value::Object(_) | Value::Array(_)) {
        return Some(format!("{value}\n"));
    }

    let mut out = String::new();
    emit(value, 0, &mut out, EmitContext::Root);
    Some(out)
}

/// `yaml-stream`: each page is dumped as its own one-element block sequence, with no
/// separator between pages.
pub fn render_stream_page(value: &Value) -> Option<String> {
    if value.as_object().is_some_and(|o| o.is_empty()) {
        return None;
    }
    let wrapped = Value::Array(vec![value.clone()]);
    let mut out = String::new();
    emit(&wrapped, 0, &mut out, EmitContext::Root);
    Some(out)
}

#[derive(Clone, Copy, PartialEq)]
enum EmitContext {
    Root,
    /// Directly after a `key:` or `-`, so the value starts on the same line.
    Inline,
}

fn emit(value: &Value, indent: usize, out: &mut String, context: EmitContext) {
    match value {
        Value::Object(map) if map.is_empty() => finish_inline("{}", out, context),
        Value::Array(items) if items.is_empty() => finish_inline("[]", out, context),

        Value::Object(map) => {
            if context == EmitContext::Inline {
                out.push('\n');
            }
            // ruamel sorts mapping keys on output.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for key in keys {
                let child = &map[key];
                push_indent(indent, out);
                let rendered_key = scalar(&Value::String(key.clone()), true);
                out.push_str(&rendered_key);
                out.push(':');
                match child {
                    Value::Object(m) if !m.is_empty() => emit(child, indent + 2, out, EmitContext::Inline),
                    Value::Array(a) if !a.is_empty() => {
                        // Dashes sit at the PARENT's indentation, not indented under it.
                        out.push('\n');
                        emit(child, indent, out, EmitContext::Root);
                    }
                    _ => {
                        out.push(' ');
                        // The value starts after `<indent><key>: `.
                        let column = indent + rendered_key.len() + 2;
                        emit_scalar_folded(child, column, indent + 2, out);
                    }
                }
            }
        }

        Value::Array(items) => {
            if context == EmitContext::Inline {
                out.push('\n');
            }
            for item in items {
                push_indent(indent, out);
                out.push_str("- ");
                match item {
                    // A nested container after `- ` continues on the same line, with its
                    // members aligned two past the dash.
                    Value::Object(m) if !m.is_empty() => {
                        emit_map_inline_after_dash(m, indent + 2, out);
                    }
                    Value::Array(a) if !a.is_empty() => {
                        out.push('\n');
                        emit(item, indent + 2, out, EmitContext::Root);
                    }
                    _ => emit(item, indent + 2, out, EmitContext::Inline),
                }
            }
        }

        scalar_value => finish_inline(&scalar(scalar_value, false), out, context),
    }
}

/// `- key: value` — the first key shares the dash's line, the rest align under it.
fn emit_map_inline_after_dash(
    map: &serde_json::Map<String, Value>,
    indent: usize,
    out: &mut String,
) {
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    for (i, key) in keys.iter().enumerate() {
        if i > 0 {
            push_indent(indent, out);
        }
        let rendered_key = scalar(&Value::String((*key).clone()), true);
        out.push_str(&rendered_key);
        out.push(':');
        let child = &map[*key];
        match child {
            Value::Object(m) if !m.is_empty() => emit(child, indent + 2, out, EmitContext::Inline),
            Value::Array(a) if !a.is_empty() => {
                out.push('\n');
                emit(child, indent, out, EmitContext::Root);
            }
            _ => {
                out.push(' ');
                let column = indent + rendered_key.len() + 2;
                emit_scalar_folded(child, column, indent + 2, out);
            }
        }
    }
}

/// Emit a scalar mapping value, folding it if it is a long plain string.
fn emit_scalar_folded(value: &Value, start_column: usize, indent: usize, out: &mut String) {
    let rendered = scalar(value, false);
    let is_plain = !rendered.starts_with('\'') && !rendered.starts_with('"');
    if is_plain {
        out.push_str(&fold_plain(&rendered, start_column, indent));
    } else {
        out.push_str(&rendered);
    }
    out.push('\n');
}

fn finish_inline(text: &str, out: &mut String, context: EmitContext) {
    if context == EmitContext::Inline {
        out.push_str(text);
        out.push('\n');
    } else {
        out.push_str(text);
        out.push('\n');
    }
}

/// ruamel's `best_width`.
const FOLD_WIDTH: usize = 80;

/// Fold a plain scalar the way the YAML emitter does.
///
/// Words are emitted until the column passes `FOLD_WIDTH`; the next space then becomes a
/// line break, with the continuation indented to `indent`. Only plain (unquoted) scalars
/// fold — a quoted scalar keeps its own rules, and a scalar with no spaces cannot be
/// broken at all, which is why ARNs and IDs stay on one line.
fn fold_plain(text: &str, start_column: usize, indent: usize) -> String {
    if text.len() + start_column <= FOLD_WIDTH || !text.contains(' ') {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len() + 8);
    let mut column = start_column;
    for (i, word) in text.split(' ').enumerate() {
        if i == 0 {
            out.push_str(word);
            column += word.len();
            continue;
        }
        if column > FOLD_WIDTH {
            out.push('\n');
            for _ in 0..indent {
                out.push(' ');
            }
            out.push_str(word);
            column = indent + word.len();
        } else {
            out.push(' ');
            out.push_str(word);
            column += 1 + word.len();
        }
    }
    out
}

fn push_indent(indent: usize, out: &mut String) {
    for _ in 0..indent {
        out.push(' ');
    }
}

/// Render a scalar, quoting only when YAML would otherwise misread it.
fn scalar(value: &Value, is_key: bool) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => {
            if needs_quoting(s, is_key) {
                quote(s)
            } else {
                s.clone()
            }
        }
        other => other.to_string(),
    }
}

/// Quote a scalar the way ruamel does: single quotes by default (with `''` for an
/// embedded quote), falling back to double quotes only when the string contains
/// characters single-quoted style cannot carry.
fn quote(s: &str) -> String {
    if s.contains('\n') || s.chars().any(|c| c.is_control()) {
        let escaped = s
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t");
        return format!("\"{escaped}\"");
    }
    format!("'{}'", s.replace('\'', "''"))
}

/// Does this look like a YAML timestamp? Such a string must be quoted or it comes back
/// as a date rather than a string.
fn looks_like_timestamp(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 10
        && b[0..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[7] == b'-'
        && b[8..10].iter().all(u8::is_ascii_digit)
}

/// A plain scalar must not be mistakable for another type or break the syntax.
fn needs_quoting(s: &str, is_key: bool) -> bool {
    if s.is_empty() {
        return true;
    }
    // Values that would parse as something else.
    const RESERVED: &[&str] = &[
        "true", "false", "null", "yes", "no", "on", "off", "~", "True", "False", "Null",
    ];
    if RESERVED.iter().any(|r| s.eq_ignore_ascii_case(r)) {
        return true;
    }
    if s.parse::<f64>().is_ok() {
        return true;
    }
    if looks_like_timestamp(s) {
        return true;
    }
    // Leading indicators and anything that would break block context.
    let first = s.chars().next().unwrap();
    if "-?:,[]{}#&*!|>'\"%@`".contains(first) || first.is_whitespace() {
        return true;
    }
    if s.ends_with(char::is_whitespace) || s.contains('\n') {
        return true;
    }
    // `: ` and ` #` are the sequences that terminate a plain scalar.
    if s.contains(": ") || s.contains(" #") {
        return true;
    }
    // A colon is only ambiguous when followed by a space (already checked above) or at
    // the very end — `SAML:aud` is a perfectly good plain key and ruamel leaves it bare.
    is_key && s.ends_with(':')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sorts_keys_and_uses_two_space_indent() {
        let v = json!({"B": 2, "A": {"D": 4, "C": 3}});
        assert_eq!(render(&v).unwrap(), "A:\n  C: 3\n  D: 4\nB: 2\n");
    }

    /// The dash sits at the PARENT key's indentation, not indented under it.
    #[test]
    fn sequence_dashes_are_flush_with_the_parent_key() {
        let v = json!({"Users": [{"Id": 1, "Name": "a"}]});
        assert_eq!(render(&v).unwrap(), "Users:\n- Id: 1\n  Name: a\n");
    }

    #[test]
    fn empty_containers_use_flow_style() {
        assert_eq!(render(&json!({"a": {}})).unwrap(), "a: {}\n");
        assert_eq!(render(&json!({"a": []})).unwrap(), "a: []\n");
        // An empty top-level object prints nothing at all.
        assert_eq!(render(&json!({})), None);
    }

    /// A top-level scalar is JSON-dumped, so a bare string comes out QUOTED — unlike
    /// every other string in YAML output.
    #[test]
    fn top_level_scalars_go_through_json() {
        assert_eq!(render(&json!("str")).unwrap(), "\"str\"\n");
        assert_eq!(render(&json!(null)).unwrap(), "null\n");
        assert_eq!(render(&json!(true)).unwrap(), "true\n");
        assert_eq!(render(&json!(5)).unwrap(), "5\n");
    }

    #[test]
    fn quotes_only_what_would_be_misread() {
        let v = json!({"a": "plain", "b": "true", "c": "123", "d": "", "e": "has: colon"});
        let out = render(&v).unwrap();
        assert!(out.contains("a: plain\n"));
        assert!(out.contains("b: 'true'\n"), "a string that looks boolean is quoted");
        assert!(out.contains("c: '123'\n"), "a string that looks numeric is quoted");
        assert!(out.contains("d: ''\n"));
        assert!(out.contains("e: 'has: colon'\n"));
        // ...but a bare colon inside a key is fine unquoted.
        let keys = json!({"SAML:aud": "v"});
        assert_eq!(render(&keys).unwrap(), "SAML:aud: v\n");
    }

    /// A timestamp-shaped string must be quoted or YAML reads it back as a date.
    #[test]
    fn quotes_timestamp_shaped_strings() {
        let v = json!({"CreationDate": "2026-07-29T05:24:54+00:00"});
        assert_eq!(render(&v).unwrap(), "CreationDate: '2026-07-29T05:24:54+00:00'\n");
        assert!(looks_like_timestamp("2026-07-29"));
        assert!(!looks_like_timestamp("not-a-date"));
    }

    #[test]
    fn uses_single_quotes_with_doubling() {
        assert_eq!(quote("it's"), "'it''s'");
        // Control characters force the double-quoted form.
        assert!(quote("a\nb").starts_with('"'));
    }

    #[test]
    fn non_ascii_is_literal() {
        assert_eq!(render(&json!({"k": "héllo"})).unwrap(), "k: héllo\n");
    }

    #[test]
    fn nested_lists_and_scalars() {
        let v = json!({"Tags": ["a", "b"], "N": 3});
        assert_eq!(render(&v).unwrap(), "N: 3\nTags:\n- a\n- b\n");
    }

    #[test]
    fn stream_wraps_each_page_in_a_one_element_list() {
        assert_eq!(render_stream_page(&json!({"a": 1})).unwrap(), "- a: 1\n");
    }
}
