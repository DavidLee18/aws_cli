//! The high-level `aws s3` command tree.
//!
//! Unrelated to `s3api`, which exposes the modelled S3 operations one-to-one. This tree
//! has no model of its own: each subcommand is hand-written, builds its own requests, and
//! writes plain text to stdout rather than going through `--output`.

pub mod bucket;
pub mod conn;
pub mod delete;
pub mod pool;
pub mod progress;
pub mod sync;
pub mod transfer;
pub mod list;
pub mod ls;
pub mod uri;
pub mod xml;

use crate::args::Parsed;
use crate::client::Globals;
use crate::exit;
use crate::Failure;

/// How many values an `aws s3` flag takes.
///
/// The s3 tree parses its own flags out of `Parsed::extras`, but the generic splitter has
/// to know where a flag's values stop and the command's positionals start — without it
/// `s3 cp --recursive SRC DST` hands `SRC` to `--recursive` and then rejects it as an
/// unknown option, so the flag only worked when written last.
///
/// Kept beside the parser that consumes these flags, so the two cannot drift apart
/// silently; `flag_arity_covers_every_flag` checks that they agree.
pub fn flag_arity(flag: &str) -> crate::args::Arity {
    use crate::args::Arity;
    match flag {
        "--recursive"
        | "--dryrun"
        | "--dry-run"
        | "--quiet"
        | "--only-show-errors"
        | "--no-progress"
        | "--human-readable"
        | "--summarize"
        | "--follow-symlinks"
        | "--no-follow-symlinks"
        | "--delete"
        | "--size-only"
        | "--exact-timestamps" => Arity::None,
        // `--grants a=b=c d=e=f` takes every following token.
        "--grants" => Arity::Many,
        _ => Arity::One,
    }
}

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

/// Percent-decode a value returned under `encoding-type=url`.
///
/// The listing is requested url-encoded so a key containing characters XML cannot carry
/// survives the round trip; every key and prefix read out of the response has to come
/// back through here. A malformed escape is left as written.
pub fn decode_listed(value: &str) -> String {
    if !value.contains('%') && !value.contains('+') {
        return value.to_string();
    }
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        // S3 encodes a space as `+` under `encoding-type=url`; a literal `+` in the key
        // arrives as `%2B`, so this substitution cannot lose one.
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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
        None => (
            response.status.to_string(),
            aws_cli_runtime::http::reason_phrase(response.status).to_string(),
        ),
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
    /// The arity table is a second copy of knowledge that lives in `transfer.rs`'s match
    /// arms, and a flag missing from it silently gets `Arity::One` — which swallows a
    /// positional and produces "Unknown options: <path>". This checks the two agree by
    /// reading the parser's own source.
    #[test]
    fn flag_arity_covers_every_flag() {
        use crate::args::Arity;
        let sources = [
            include_str!("transfer.rs"),
            include_str!("ls.rs"),
            include_str!("bucket.rs"),
            include_str!("sync.rs"),
        ];

        for source in sources {
            let lines: Vec<&str> = source.lines().collect();
            for (index, line) in lines.iter().enumerate() {
                let line = line.trim();
                // Match-arm lines of the form `"--flag" | "--other" => ...`.
                let Some((patterns, rest)) = line.split_once("=>") else { continue };
                if !patterns.trim_start().starts_with('"') {
                    continue;
                }
                // A braced arm continues over several lines, and that is where
                // `--concurrency` reads its value — stopping at the first line would
                // call it a boolean.
                let mut body = rest.to_string();
                if rest.trim_end().ends_with('{') {
                    let mut depth = 1i32;
                    for next in lines.iter().skip(index + 1) {
                        depth += next.matches('{').count() as i32;
                        depth -= next.matches('}').count() as i32;
                        body.push_str(next);
                        if depth <= 0 {
                            break;
                        }
                    }
                }
                let body = body.as_str();
                let flags: Vec<&str> = patterns
                    .split('|')
                    .map(|p| p.trim().trim_matches('"'))
                    .filter(|p| p.starts_with("--"))
                    .collect();
                if flags.is_empty() {
                    continue;
                }
                // An arm that reads a value calls `take` or `value`; one that does not is
                // a boolean. `--grants` consumes its own tokens in a loop.
                let reads_value = body.contains("take(") || body.contains("value(");
                for flag in flags {
                    if flag == "--grants" {
                        assert_eq!(flag_arity(flag), Arity::Many, "{flag}");
                        continue;
                    }
                    let expected = if reads_value { Arity::One } else { Arity::None };
                    assert_eq!(
                        flag_arity(flag),
                        expected,
                        "{flag}: the table and the parser disagree"
                    );
                }
            }
        }
    }

    /// The flags whose position used to matter, spelled out.
    #[test]
    fn boolean_flags_take_no_value() {
        use crate::args::Arity;
        for flag in ["--recursive", "--dryrun", "--delete", "--size-only", "--human-readable"] {
            assert_eq!(flag_arity(flag), Arity::None, "{flag}");
        }
        for flag in ["--exclude", "--include", "--storage-class", "--acl"] {
            assert_eq!(flag_arity(flag), Arity::One, "{flag}");
        }
    }

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

    /// Round trip: what we send url-encoded must come back as it started.
    #[test]
    fn decodes_listed_keys() {
        for original in ["a b/café.txt", "plain.txt", "with+plus", "a/b/c"] {
            assert_eq!(decode_listed(&encode_query(original)), original, "{original}");
        }
        // A malformed escape is left as written rather than dropped.
        assert_eq!(decode_listed("100%"), "100%");
        assert_eq!(decode_listed("a%zzb"), "a%zzb");
        // S3 sends a space as `+`; a literal `+` arrives as `%2B` and survives.
        assert_eq!(decode_listed("a+b.txt"), "a b.txt");
        assert_eq!(decode_listed("a%2Bb.txt"), "a+b.txt");
    }

    #[test]
    fn encodes_keys_leaving_separators_alone() {
        assert_eq!(encode_key("a/b/c.txt"), "a/b/c.txt");
        assert_eq!(encode_key("with space"), "with%20space");
        assert_eq!(encode_key("plus+sign"), "plus%2Bsign");
        assert_eq!(encode_key("caf\u{e9}"), "caf%C3%A9");
    }
}
