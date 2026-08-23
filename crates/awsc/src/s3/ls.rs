//! `aws s3 ls`.
//!
//! Three shapes: no path lists buckets, a bucket or prefix lists one level with
//! `Delimiter=/`, and `--recursive` drops the delimiter and prints full keys.
//!
//! Output goes straight to stdout; `--output`, `--query` and `--no-paginate` are ignored,
//! which the reference states in its own description.

use aws_cli_runtime::http;
use super::{human_readable_size, param_error, service_error, uri, xml};
use crate::args::Parsed;
use crate::client::{Client, Globals};
use crate::exit;
use crate::Failure;
use std::io::Write;
use std::process::ExitCode;

pub fn run(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let options = Options::parse(parsed)?;

    let (bucket, key) = uri::split_bucket_key(&options.path).map_err(param_error)?;

    let model =
        crate::load_model("s3api").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    // The bucket selects the host, so it must be known before the endpoint resolves.
    let client =
        Client::for_bucket(&model, globals, Some(bucket.as_str()).filter(|b| !b.is_empty()))?;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut totals = Totals::default();

    if bucket.is_empty() {
        list_buckets(&client, &options, &mut out)?;
    } else if options.recursive {
        list_objects(&client, &options, &key, None, &mut totals, &mut out)?;
    } else {
        list_objects(&client, &options, &key, Some("/"), &mut totals, &mut out)?;
    }

    if options.summarize {
        // The reference's `rjust(15)` on "\nTotal Objects: " is a no-op — that string is
        // already 16 characters including the newline — so only Total Size is padded.
        let _ = write!(out, "\nTotal Objects: {}\n", totals.objects);
        let size = if options.human_readable {
            human_readable_size(totals.bytes)
        } else {
            totals.bytes.to_string()
        };
        let _ = write!(out, "   Total Size: {size}\n");
    }

    // A prefix that matched nothing on the first page exits 1, silently. With no key at
    // all — a bare bucket, or no path — an empty result is still 0.
    if !key.is_empty() && totals.empty_first_page {
        return Ok(exit::code(1));
    }
    Ok(exit::code(exit::SUCCESS))
}

#[derive(Default)]
struct Totals {
    objects: u64,
    bytes: u64,
    empty_first_page: bool,
}

struct Options {
    path: String,
    recursive: bool,
    human_readable: bool,
    summarize: bool,
    page_size: Option<u32>,
    request_payer: Option<String>,
    bucket_name_prefix: Option<String>,
    bucket_region: Option<String>,
}

impl Options {
    fn parse(parsed: &Parsed) -> Result<Options, Failure> {
        let mut options = Options {
            // The positional defaults to `s3://`, i.e. "list every bucket".
            path: parsed.positionals.first().cloned().unwrap_or_else(|| "s3://".to_string()),
            recursive: false,
            human_readable: false,
            summarize: false,
            page_size: None,
            request_payer: None,
            bucket_name_prefix: None,
            bucket_region: None,
        };
        if parsed.positionals.len() > 1 {
            return Err(param_error(format!(
                "Unknown options: {}",
                parsed.positionals[1..].join(",")
            )));
        }

        let tokens = &parsed.extras;
        let mut leftover = Vec::new();
        let mut i = 0;
        while i < tokens.len() {
            let (name, inline) = match tokens[i].split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (tokens[i].as_str(), None),
            };
            let value = |i: &mut usize| -> Option<String> {
                if let Some(v) = inline.clone() {
                    return Some(v);
                }
                let next = tokens.get(*i + 1)?;
                if next.starts_with("--") {
                    return None;
                }
                *i += 1;
                Some(next.clone())
            };
            match name {
                "--recursive" => options.recursive = true,
                "--human-readable" => options.human_readable = true,
                "--summarize" => options.summarize = true,
                "--page-size" => {
                    let raw = value(&mut i).unwrap_or_default();
                    // A bare `int()` upstream, so a bad value is an uncaught ValueError
                    // at 255 rather than parameter validation at 252.
                    options.page_size = Some(super::parse_int(&raw)?);
                }
                // `nargs='?'` with a const: the bare flag means `requester`.
                "--request-payer" => {
                    options.request_payer = Some(value(&mut i).unwrap_or_else(|| "requester".into()))
                }
                "--bucket-name-prefix" => options.bucket_name_prefix = value(&mut i),
                "--bucket-region" => options.bucket_region = value(&mut i),
                other => leftover.push(other.to_string()),
            }
            i += 1;
        }
        if !leftover.is_empty() {
            return Err(param_error(format!("Unknown options: {}", leftover.join(","))));
        }
        Ok(options)
    }
}

fn list_buckets(
    client: &Client<'_>,
    options: &Options,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let mut token: Option<String> = None;
    loop {
        let mut query = Vec::new();
        if let Some(size) = options.page_size {
            query.push(format!("max-buckets={size}"));
        }
        if let Some(prefix) = &options.bucket_name_prefix {
            query.push(format!("prefix={}", super::encode_query(prefix)));
        }
        if let Some(region) = &options.bucket_region {
            query.push(format!("bucket-region={}", super::encode_query(region)));
        }
        if let Some(t) = &token {
            query.push(format!("continuation-token={}", super::encode_query(t)));
        }

        let response = client.send_raw("GET", "/", &query.join("&"), &[], http::Body::Empty)?;
        if response.status >= 400 {
            return Err(service_error("ListBuckets", &response));
        }
        let root = xml::parse(&response.text())
            .map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;

        if let Some(buckets) = root.child("Buckets") {
            for bucket in buckets.all("Bucket") {
                let _ = write!(
                    out,
                    "{} {}\n",
                    last_modified(bucket.get("CreationDate")),
                    bucket.get("Name")
                );
            }
        }

        match root.get("ContinuationToken") {
            "" => break,
            next => token = Some(next.to_string()),
        }
    }
    Ok(())
}

fn list_objects(
    client: &Client<'_>,
    options: &Options,
    prefix: &str,
    delimiter: Option<&str>,
    totals: &mut Totals,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let mut token: Option<String> = None;
    let mut first_page = true;

    loop {
        // `Prefix` is always sent, even when empty.
        // `encoding-type=url` so a key containing characters XML cannot carry survives the
        // listing; the fields are decoded again as they are read.
        let mut query = vec![
            "list-type=2".to_string(),
            "encoding-type=url".to_string(),
            format!("prefix={}", super::encode_query(prefix)),
        ];
        if let Some(d) = delimiter {
            query.push(format!("delimiter={}", super::encode_query(d)));
        }
        if let Some(size) = options.page_size {
            query.push(format!("max-keys={size}"));
        }
        if let Some(t) = &token {
            query.push(format!("continuation-token={}", super::encode_query(t)));
        }
        let mut headers = Vec::new();
        if let Some(payer) = &options.request_payer {
            headers.push(("x-amz-request-payer".to_string(), payer.clone()));
        }

        // Virtual-host addressing puts the bucket in the host, so the path is just `/`.
        let response = client.send_raw("GET", "/", &query.join("&"), &headers, http::Body::Empty)?;
        if response.status >= 400 {
            return Err(service_error("ListObjectsV2", &response));
        }
        let root = xml::parse(&response.text())
            .map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;

        let prefixes: Vec<&xml::Element> = root.all("CommonPrefixes").collect();
        let contents: Vec<&xml::Element> = root.all("Contents").collect();
        if first_page && prefixes.is_empty() && contents.is_empty() {
            totals.empty_first_page = true;
        }
        first_page = false;

        for common in prefixes {
            // Only the LAST component is shown, not the whole prefix: `a/b/` prints `b/`.
            let full = &super::decode_listed(common.get("Prefix"));
            let name = full.trim_end_matches('/').rsplit('/').next().unwrap_or_default();
            let _ = write!(out, "{:>30} {name}/\n", "PRE");
        }

        for content in contents {
            let size: u64 = content.get("Size").parse().unwrap_or_default();
            totals.objects += 1;
            totals.bytes += size;
            let size_text = if options.human_readable {
                human_readable_size(size)
            } else {
                size.to_string()
            };
            // Under --recursive the full key is printed; otherwise just the basename.
            let key = &super::decode_listed(content.get("Key"));
            let name = if delimiter.is_some() {
                key.rsplit('/').next().unwrap_or_default()
            } else {
                key
            };
            let _ = write!(
                out,
                "{} {:>10} {name}\n",
                last_modified(content.get("LastModified")),
                size_text
            );
        }

        match root.get("NextContinuationToken") {
            "" => break,
            next => token = Some(next.to_string()),
        }
    }
    Ok(())
}

/// `YYYY-MM-DD HH:MM:SS` in the machine's **local** timezone, as the reference prints it.
fn last_modified(iso: &str) -> String {
    let Some(unix) = parse_iso8601(iso) else {
        return " ".repeat(19);
    };
    let local = unix + aws_cli_runtime::localtime::offset_seconds(unix);
    let compact = aws_cli_runtime::sigv4::format_timestamp(local);
    format!(
        "{}-{}-{} {}:{}:{}",
        &compact[0..4],
        &compact[4..6],
        &compact[6..8],
        &compact[9..11],
        &compact[11..13],
        &compact[13..15]
    )
}

/// S3 returns `2026-08-13T21:48:16.000Z`; a fractional part and the `Z` are both optional.
pub fn parse_iso8601(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let number = |r: std::ops::Range<usize>| -> Option<i64> { value.get(r)?.parse().ok() };
    let (y, m, d) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hh, mm, ss) = (number(11..13)?, number(14..16)?, number(17..19)?);
    Some(days_from_civil(y, m, d) * 86_400 + hh * 3600 + mm * 60 + ss)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_s3_timestamps() {
        assert_eq!(parse_iso8601("2026-08-13T21:48:16.000Z"), Some(1_786_657_696));
        assert_eq!(parse_iso8601("2026-08-13T21:48:16Z"), Some(1_786_657_696));
        assert_eq!(parse_iso8601("nonsense"), None);
    }

    /// A timestamp that cannot be parsed still occupies the full 19-character column, so
    /// the size and name stay aligned.
    #[test]
    fn keeps_the_timestamp_column_width() {
        assert_eq!(last_modified("nonsense").len(), 19);
        assert_eq!(last_modified("2026-08-13T21:48:16.000Z").len(), 19);
    }

    /// The object line is `<19 char ts> <size rjust 10> <name>`, so the name begins at
    /// column 32 — exactly where `PRE` lines put theirs.
    #[test]
    fn aligns_object_and_prefix_columns() {
        let object = format!("{} {:>10} {}\n", last_modified("2026-08-13T21:48:16Z"), "12", "a.txt");
        let prefix = format!("{:>30} {}/\n", "PRE", "sub");
        assert_eq!(object.find("a.txt"), Some(31));
        assert_eq!(prefix.find("sub/"), Some(31));
        assert_eq!(prefix, "                           PRE sub/\n");
    }

    /// Sizes wider than the column are padded, never truncated.
    #[test]
    fn never_truncates_a_large_size() {
        let line = format!("{:>10}", "123456789012");
        assert_eq!(line, "123456789012");
    }
}
