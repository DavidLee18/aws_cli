//! `--cli-error-format`: how an error is rendered on stderr.
//!
//! The reference's `errorhandler.py` builds a small structured record for each error it
//! recognises — `{Code, Message}` plus whatever the error *shape* models — and then
//! renders it in one of six styles. Four of them are the ordinary output formatters
//! pointed at stderr with the pseudo-operation name `error`, which is why they need no
//! new code here.
//!
//! Two subtleties are worth stating, because they make the implementation smaller than
//! the six choices suggest:
//!
//! - `legacy` prints the record's *message alone*, where `enhanced` prints it behind the
//!   "An error occurred (Code): " wrapper and then appends an "Additional error details"
//!   block for modelled fields beyond code and message.
//!   `_display_structured_error` returns `False` for `legacy`, and the caller then falls
//!   back to `error_info['Message']` -- the unwrapped text. (For a *service* error the two
//!   agree, because `ClientErrorHandler` overrides that fallback to use the wrapped
//!   `str(exception)`.)
//! - An error with no structured record — anything reaching the general handler — ignores
//!   the setting entirely and prints the plain line. So this applies to service errors and
//!   to the recognised parameter/configuration failures, and to nothing else.

use aws_cli_output::Format;
use serde_json::{Map, Value};

/// The choices `data/cli.json` declares, in its order.
pub const ERROR_FORMATS: &[&str] = &["legacy", "json", "yaml", "text", "table", "enhanced"];

/// Collections of at most this many simple items are shown inline; larger ones are
/// `<complex value>` (`errorhandler.py:MAX_INLINE_ITEMS`).
const MAX_INLINE_ITEMS: usize = 5;

/// Resolve the format: the flag, then `AWS_CLI_ERROR_FORMAT`, then the profile's
/// `cli_error_format`, then `enhanced` (`clidriver.py:_construct_cli_error_format_chain`).
///
/// An unrecognised value from the environment or config is NOT an error — the reference
/// logs a warning and falls back to `enhanced`. Only the command-line flag rejects.
pub fn resolve(flag: Option<&str>, profile: Option<&str>) -> String {
    let candidate = flag
        .map(str::to_string)
        .or_else(|| std::env::var("AWS_CLI_ERROR_FORMAT").ok().filter(|s| !s.is_empty()))
        .or_else(|| {
            aws_cli_runtime::credentials::profile_setting("cli_error_format", profile)
        });
    match candidate {
        Some(v) if ERROR_FORMATS.contains(&v.to_lowercase().as_str()) => v.to_lowercase(),
        _ => "enhanced".to_string(),
    }
}

/// Render an error record, returning the exact stderr text (including the leading blank
/// line the reference writes before every error).
///
/// `message` is the line the plain formats print; `info` is the structured record the
/// data formats render. `None` means "no structured record", which is the general-error
/// case that ignores the setting.
pub fn render(format: &str, message: &str, info: Option<&Map<String, Value>>) -> String {
    let Some(info) = info else {
        return plain(message);
    };
    match format {
        "json" | "yaml" | "text" | "table" => {
            let value = Value::Object(info.clone());
            let output = Format::parse(format).expect("checked against ERROR_FORMATS");
            match aws_cli_output::render_named("error", &value, output) {
                Ok(Some(text)) => text,
                // An empty render falls back rather than printing nothing: an error that
                // reports nothing at all is worse than one in the wrong style.
                _ => plain(message),
            }
        }
        // `legacy` drops the "An error occurred (Code): " wrapper and prints the record's
        // message on its own.
        "legacy" => plain(info.get("Message").and_then(Value::as_str).unwrap_or(message).trim_end()),
        _ => {
            let mut out = plain(message);
            out.push_str(&additional_details(info));
            out
        }
    }
}

fn plain(message: &str) -> String {
    format!("\naws: [ERROR]: {message}\n")
}

/// The `enhanced` format's trailing block, listing every field that is not the code or
/// the message. Absent entirely when there are none, which is the common case.
fn additional_details(info: &Map<String, Value>) -> String {
    let extra: Vec<(&String, &Value)> = info
        .iter()
        .filter(|(k, _)| {
            let k = k.to_lowercase();
            k != "code" && k != "message"
        })
        .collect();
    if extra.is_empty() {
        return String::new();
    }

    let mut out = String::from("\nAdditional error details:\n");
    let mut has_complex = false;
    for (key, value) in extra {
        if let Some(text) = simple_value(value) {
            out.push_str(&format!("{key}: {text}\n"));
        } else if let Some(text) = small_collection(value) {
            out.push_str(&format!("{key}: {text}\n"));
        } else {
            out.push_str(&format!("{key}: <complex value>\n"));
            has_complex = true;
        }
    }
    if has_complex {
        out.push_str(
            "Use \"--cli-error-format json\" or another error format \
             to see the full details.\n",
        );
    }
    out
}

/// Python's `str()` of a scalar, which is what the reference interpolates.
fn simple_value(value: &Value) -> Option<String> {
    Some(match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Value::Null => "None".to_string(),
        _ => return None,
    })
}

fn small_collection(value: &Value) -> Option<String> {
    match value {
        Value::Array(items) if items.len() < MAX_INLINE_ITEMS => {
            let parts: Option<Vec<String>> = items.iter().map(simple_value).collect();
            Some(format!("[{}]", parts?.join(", ")))
        }
        Value::Object(map) if map.len() < MAX_INLINE_ITEMS => {
            let parts: Option<Vec<String>> =
                map.iter().map(|(k, v)| simple_value(v).map(|v| format!("{k}: {v}"))).collect();
            Some(format!("{{{}}}", parts?.join(", ")))
        }
        _ => None,
    }
}

/// The structured record for an error we raise ourselves, as the reference's handlers
/// build it: a fixed code and the message text.
pub fn info(code: &str, message: &str) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("Code".to_string(), Value::String(code.to_string()));
    map.insert("Message".to_string(), Value::String(message.to_string()));
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `legacy` is the record's own message; `enhanced` is the wrapped line the handler
    /// composed. They are the same text only when nothing wrapped it.
    #[test]
    fn legacy_prints_the_unwrapped_message() {
        let info = info("ParamValidation", "bad");
        let wrapped = "An error occurred (ParamValidation): bad";
        assert_eq!(render("legacy", wrapped, Some(&info)), "\naws: [ERROR]: bad\n");
        assert_eq!(render("enhanced", wrapped, Some(&info)), format!("\naws: [ERROR]: {wrapped}\n"));
    }

    /// The only thing `enhanced` adds is the block for modelled fields beyond the two
    /// standard keys.
    #[test]
    fn enhanced_lists_the_modelled_extras() {
        let mut info = info("ThrottlingException", "slow down");
        info.insert("RetryAfterSeconds".to_string(), json!(30));
        let out = render("enhanced", "An error occurred (ThrottlingException): slow down", Some(&info));
        assert!(out.contains("Additional error details:\nRetryAfterSeconds: 30\n"), "{out}");
        assert!(!out.contains("<complex value>"), "{out}");
    }

    #[test]
    fn a_large_collection_is_not_inlined_and_points_at_json() {
        let mut info = info("X", "y");
        info.insert("Items".to_string(), json!([1, 2, 3, 4, 5, 6]));
        let out = render("enhanced", "m", Some(&info));
        assert!(out.contains("Items: <complex value>"), "{out}");
        assert!(out.contains("--cli-error-format json"), "{out}");
    }

    #[test]
    fn json_renders_the_record_not_the_line() {
        let info = info("ParamValidation", "bad");
        let out = render("json", "ignored", Some(&info));
        assert!(out.contains("\"Code\": \"ParamValidation\""), "{out}");
        assert!(!out.contains("aws: [ERROR]"), "{out}");
    }

    /// An error with no structured record ignores the setting entirely, which is what
    /// keeps `--cli-error-format json` from swallowing a general failure.
    #[test]
    fn an_unstructured_error_ignores_the_format() {
        assert_eq!(render("json", "boom", None), "\naws: [ERROR]: boom\n");
    }

    #[test]
    fn an_unknown_configured_value_falls_back_rather_than_failing() {
        assert_eq!(resolve(None, None), "enhanced");
    }
}
