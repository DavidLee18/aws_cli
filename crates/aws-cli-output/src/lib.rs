//! Output formatting.
//!
//! The reference supports `json`, `text`, `table`, `yaml`, `yaml-stream` and `off`
//! (`data/cli.json`). `json` and `text` are implemented; the rest are stubbed explicitly
//! so an unimplemented format fails loudly instead of silently printing JSON.

pub mod query;
pub mod text;

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Json,
    Text,
    Table,
    Yaml,
    YamlStream,
    Off,
}

impl Format {
    pub fn parse(s: &str) -> Option<Format> {
        Some(match s {
            "json" => Format::Json,
            "text" => Format::Text,
            "table" => Format::Table,
            "yaml" => Format::Yaml,
            "yaml-stream" => Format::YamlStream,
            "off" => Format::Off,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Format::Json => "json",
            Format::Text => "text",
            Format::Table => "table",
            Format::Yaml => "yaml",
            Format::YamlStream => "yaml-stream",
            Format::Off => "off",
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("output format `{0}` is not implemented yet")]
pub struct UnsupportedFormat(pub &'static str);

/// Render a response.
///
/// Returns the EXACT bytes to write, trailing newline included, so callers use `print!`
/// rather than `println!` — text output already ends in a newline and would otherwise
/// gain a spurious blank line.
///
/// `None` means "print nothing at all", which is what the reference does for an empty
/// result.
pub fn render(value: &Value, format: Format) -> Result<Option<String>, UnsupportedFormat> {
    match format {
        Format::Off => Ok(None),
        Format::Json => {
            if value.as_object().is_some_and(|o| o.is_empty()) {
                return Ok(None);
            }
            let mut buf = Vec::new();
            let indent = b"    ";
            let formatter = serde_json::ser::PrettyFormatter::with_indent(indent);
            let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
            serde::Serialize::serialize(value, &mut ser).expect("JSON value always serializes");
            let mut text = String::from_utf8(buf).expect("serde_json emits UTF-8");
            text.push('\n');
            Ok(Some(text))
        }
        Format::Text => Ok(text::render(value)),
        Format::Table => Err(UnsupportedFormat("table")),
        Format::Yaml => Err(UnsupportedFormat("yaml")),
        Format::YamlStream => Err(UnsupportedFormat("yaml-stream")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matches_reference_json_shape() {
        let v = json!({"UserId": "AIDA", "Account": "123", "Arn": "arn:aws:iam::123:user/x"});
        let out = render(&v, Format::Json).unwrap().unwrap();
        // 4-space indent, keys in insertion order, one trailing newline.
        assert!(out.starts_with("{\n    \"UserId\": \"AIDA\","), "got: {out}");
        assert!(out.ends_with("}\n"));
    }

    #[test]
    fn empty_result_prints_nothing() {
        assert_eq!(render(&json!({}), Format::Json).unwrap(), None);
        assert_eq!(render(&json!({"a": 1}), Format::Off).unwrap(), None);
    }

    #[test]
    fn unimplemented_formats_fail_loudly() {
        assert!(render(&json!({"a": 1}), Format::Table).is_err());
    }
}
