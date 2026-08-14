//! The high-level `aws s3` command tree.
//!
//! Unrelated to `s3api`, which exposes the modelled S3 operations one-to-one. This tree
//! has no model of its own: each subcommand is hand-written, builds its own requests, and
//! writes plain text to stdout rather than going through `--output`.

pub mod bucket;
pub mod conn;
pub mod pool;
pub mod progress;
pub mod sync;
pub mod transfer;
pub mod ls;
pub mod uri;
pub mod xml;

use crate::args::Parsed;
use crate::client::Globals;
use crate::exit;
use crate::Failure;

/// Percent-encode one path segment for an S3 object key.
///
/// `/` is deliberately left alone — it separates key components in the URL — and the
/// unreserved set matches what SigV4 canonicalisation expects, so the signature and the
/// sent URL agree.
pub fn encode_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for byte in key.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Percent-encode a **query parameter** value.
///
/// Stricter than [`encode_key`]: `/` must become `%2F` here. SigV4 canonicalises query
/// parameters with `quote(safe='-._~')`, so a literal `/` in a value — a `prefix` of
/// `logs/`, or `delimiter=/` — is signed as `%2F` by S3 but sent raw by us, and the
/// signature does not match. That failure only appears against the real service.
pub fn encode_query(value: &str) -> String {
    aws_cli_runtime::presign::percent_encode(value)
}

/// Base-2 sizes, as `--human-readable` and the progress bar render them.
///
/// Note the two special cases at the bottom (`1 Byte`, `N Bytes`) and that the threshold
/// test rounds before comparing, which is what stops `1024.0 KiB` appearing.
pub fn human_readable_size(value: u64) -> String {
    const SUFFIXES: [&str; 6] = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let base = 1024.0_f64;
    let bytes = value as f64;
    if value == 1 {
        return "1 Byte".to_string();
    }
    if bytes < base {
        return format!("{value} Bytes");
    }
    for (i, suffix) in SUFFIXES.iter().enumerate() {
        let unit = base.powi(i as i32 + 2);
        if round_half_even((bytes / unit) * base) < base {
            return format!("{:.1} {suffix}", base * bytes / unit);
        }
    }
    // Unreachable for any real object: S3 caps a single object at 5 TiB.
    format!("{:.1} EiB", bytes / base.powi(6))
}

/// Python's `round`, which breaks ties to even rather than away from zero.
fn round_half_even(value: f64) -> f64 {
    let rounded = value.round();
    if (value - value.trunc()).abs() == 0.5 && rounded % 2.0 != 0.0 {
        rounded - value.signum()
    } else {
        rounded
    }
}

/// Parse an integer argument the way the reference does: a bare `int()`, so a bad value
/// escapes as an uncaught `ValueError` at 255 rather than as parameter validation at 252.
pub fn parse_int<T: std::str::FromStr>(raw: &str) -> Result<T, Failure> {
    raw.parse().map_err(|_| {
        Failure::new(
            exit::GENERAL_ERROR,
            format!("invalid literal for int() with base 10: '{raw}'"),
        )
    })
}

/// A parameter-validation failure, which the reference reports at 252.
pub fn param_error(message: impl std::fmt::Display) -> Failure {
    Failure::new(
        exit::PARAM_VALIDATION,
        aws_cli_runtime::RuntimeError::ParamValidation(message.to_string()),
    )
}

/// Turn a non-2xx S3 response into the error the reference would report.
pub fn service_error(operation: &str, response: &aws_cli_runtime::http::Response) -> Failure {
    let text = response.text();
    let (code, message) = match aws_cli_protocol::xml::parse_error(&text) {
        Some(e) => (e.code, e.message),
        // HeadObject and friends answer with an empty body, so the status is all there is.
        None => (response.status.to_string(), String::new()),
    };
    let mut failure = Failure::new(
        exit::CLIENT_ERROR,
        format!("An error occurred ({code}) when calling the {operation} operation: {message}"),
    );
    failure.service_error_code = Some(code);
    failure
}

/// Dispatch an `aws s3 ...` invocation.
pub fn dispatch(parsed: &Parsed, globals: &Globals) -> Result<std::process::ExitCode, Failure> {
    match parsed.operation.as_str() {
        "ls" => ls::run(parsed, globals),
        "cp" => transfer::cp(parsed, globals),
        "mv" => transfer::mv(parsed, globals),
        "rm" => transfer::rm(parsed, globals),
        "sync" => sync::run(parsed, globals),
        "mb" => bucket::mb(parsed, globals),
        "rb" => bucket::rb(parsed, globals),
        "presign" => bucket::presign(parsed, globals),
        "website" => bucket::website(parsed, globals),
        // Matches argparse's wording for an invalid subcommand choice, including the
        // blank line before the usage block.
        other => Err(Failure::new(
            exit::PARAM_VALIDATION,
            format!(
                "{}\n\n\n{}",
                aws_cli_runtime::RuntimeError::ParamValidation(format!(
                    "argument subcommand: Found invalid choice '{other}'"
                )),
                crate::USAGE_HINT
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The distinction that broke signing against real S3: a path keeps its separators,
    /// a query value does not.
    #[test]
    fn encodes_query_values_more_strictly_than_paths() {
        assert_eq!(encode_key("logs/2026/"), "logs/2026/");
        assert_eq!(encode_query("logs/2026/"), "logs%2F2026%2F");
        assert_eq!(encode_query("/"), "%2F");
        assert_eq!(encode_query("a+b"), "a%2Bb");
        assert_eq!(encode_query("plain-9._~"), "plain-9._~");
    }

    /// Values checked against the reference's own `human_readable_size`.
    #[test]
    fn formats_human_readable_sizes() {
        let cases = [
            (0, "0 Bytes"),
            (1, "1 Byte"),
            (2, "2 Bytes"),
            (1023, "1023 Bytes"),
            (1024, "1.0 KiB"),
            (1025, "1.0 KiB"),
            (1536, "1.5 KiB"),
            (1_048_576, "1.0 MiB"),
            (1_073_741_824, "1.0 GiB"),
            (1024_u64.pow(5) * 10, "10.0 PiB"),
            (1024_u64.pow(6), "1.0 EiB"),
        ];
        for (input, want) in cases {
            assert_eq!(human_readable_size(input), want, "size {input}");
        }
    }

    /// Just below the next unit the value must not render as `1024.0 KiB`.
    #[test]
    fn never_renders_a_full_unit_of_the_smaller_suffix() {
        for value in [1024_u64 * 1024 - 1, 1024 * 1024, 1024 * 1024 + 1] {
            let rendered = human_readable_size(value);
            assert!(!rendered.starts_with("1024.0"), "{value} rendered as {rendered}");
        }
    }

    #[test]
    fn encodes_keys_leaving_separators_alone() {
        assert_eq!(encode_key("a/b/c.txt"), "a/b/c.txt");
        assert_eq!(encode_key("with space"), "with%20space");
        assert_eq!(encode_key("plus+sign"), "plus%2Bsign");
        assert_eq!(encode_key("caf\u{e9}"), "caf%C3%A9");
    }
}
