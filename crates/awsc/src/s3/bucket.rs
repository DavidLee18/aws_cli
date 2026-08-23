//! `mb`, `rb`, `presign` and `website`.
//!
//! `mb` and `rb` are unusual in this codebase: they catch every exception themselves and
//! return **1** with an undecorated `make_bucket failed: ...` on stderr, rather than
//! letting the driver print `aws: [ERROR]:` and exit 254. That is reproduced.

use super::{param_error, service_error, uri};
use aws_cli_runtime::http;
use crate::args::Parsed;
use crate::client::{Client, Globals};
use crate::exit;
use crate::Failure;
use std::process::ExitCode;

/// The `s3://` prefix is mandatory for `mb`/`rb`, unlike the rest of the tree.
fn require_scheme(path: &str, usage: &str) -> Result<(), Failure> {
    if path.starts_with("s3://") {
        return Ok(());
    }
    Err(param_error(format!("{usage}\nError: Invalid argument type")))
}

fn positional(parsed: &Parsed) -> Result<&str, Failure> {
    parsed
        .positionals
        .first()
        .map(String::as_str)
        .ok_or_else(|| param_error("the following arguments are required: paths"))
}

pub fn mb(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let path = positional(parsed)?.to_string();
    require_scheme(&path, "<S3Uri>")?;
    let (bucket, _) = uri::split_bucket_key(&path).map_err(param_error)?;
    // A key part is silently discarded: `mb s3://b/k` creates bucket `b`.

    if bucket.ends_with("--x-s3") {
        return Err(param_error("Cannot use mb command with a directory bucket."));
    }

    let model =
        crate::load_model("s3api").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let client = Client::for_bucket(&model, globals, Some(&bucket))?;
    let region = client.endpoint.signing_region.clone();

    // us-east-1 must NOT carry a LocationConstraint; S3 rejects it there.
    let body = if region == "us-east-1" {
        Vec::new()
    } else {
        format!(
            "<CreateBucketConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
             <LocationConstraint>{region}</LocationConstraint></CreateBucketConfiguration>"
        )
        .into_bytes()
    };

    match client.send_raw("PUT", "/", "", &[], http::Body::from_vec(body)) {
        Ok(response) if response.status < 400 => {
            println!("make_bucket: {bucket}");
            Ok(exit::code(exit::SUCCESS))
        }
        Ok(response) => {
            // Deliberately undecorated, and rc 1 rather than 254.
            eprintln!("make_bucket failed: {path} {}", service_error("CreateBucket", &response).message());
            Ok(exit::code(1))
        }
        Err(failure) => {
            eprintln!("make_bucket failed: {path} {}", failure.message());
            Ok(exit::code(1))
        }
    }
}

pub fn rb(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let path = positional(parsed)?.to_string();
    require_scheme(&path, "<S3Uri>")?;
    let (bucket, key) = uri::split_bucket_key(&path).map_err(param_error)?;
    // Unlike `mb`, a trailing key is rejected rather than ignored.
    if !key.is_empty() {
        return Err(param_error(format!(
            "Please specify a valid bucket name only. E.g. s3://{bucket}"
        )));
    }

    // `--force` empties the bucket first, exactly as the reference does by invoking
    // `rm --recursive` and refusing to continue if anything failed.
    if parsed.extras.iter().any(|f| f == "--force") {
        let mut emptied = parsed.clone();
        emptied.extras.retain(|f| f != "--force");
        emptied.extras.push("--recursive".to_string());
        let code = crate::s3::transfer::rm(&emptied, globals)?;
        if code != exit::code(exit::SUCCESS) {
            return Err(Failure::new(
                exit::GENERAL_ERROR,
                "remove_bucket failed: Unable to delete all objects in the bucket, \
                 bucket will not be deleted.",
            ));
        }
    }

    let model =
        crate::load_model("s3api").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let client = Client::for_bucket(&model, globals, Some(&bucket))?;

    match client.send_raw("DELETE", "/", "", &[], http::Body::Empty) {
        Ok(response) if response.status < 400 => {
            println!("remove_bucket: {bucket}");
            Ok(exit::code(exit::SUCCESS))
        }
        Ok(response) => {
            eprintln!(
                "remove_bucket failed: {path} {}",
                service_error("DeleteBucket", &response).message()
            );
            Ok(exit::code(1))
        }
        Err(failure) => {
            eprintln!("remove_bucket failed: {path} {}", failure.message());
            Ok(exit::code(1))
        }
    }
}

pub fn presign(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let path = positional(parsed)?.to_string();
    let (bucket, key) = uri::split_bucket_key(&path).map_err(param_error)?;
    // The reference validates the GetObject parameters, and Key has a minimum length of
    // one — so presigning a bucket with no key is a parameter-validation failure.
    if key.is_empty() {
        return Err(param_error(
            "Parameter validation failed:\nInvalid length for parameter Key, value: 0, \
             valid min length: 1",
        ));
    }

    // Default 3600. The documented 604800 maximum is help text only — the reference does
    // not validate it, and S3 rejects an over-large value at request time.
    let mut expires: u32 = 3600;
    let tokens = &parsed.extras;
    let mut i = 0;
    while i < tokens.len() {
        let (name, inline) = match tokens[i].split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (tokens[i].as_str(), None),
        };
        if name == "--expires-in" {
            let raw = match inline {
                Some(v) => v,
                None => {
                    i += 1;
                    tokens.get(i).cloned().unwrap_or_default()
                }
            };
            expires = super::parse_int(&raw)?;
        } else {
            return Err(param_error(format!("Unknown options: {name}")));
        }
        i += 1;
    }

    let model =
        crate::load_model("s3api").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let client = Client::for_bucket(&model, globals, Some(&bucket))?;

    let ctx = aws_cli_runtime::sigv4::SigningContext {
        credentials: &client.credentials,
        region: &client.endpoint.signing_region,
        service: &client.endpoint.signing_name,
        timestamp: &aws_cli_runtime::sigv4::format_timestamp(crate::now_unix()),
    };
    // The bucket lives in the host under virtual-host addressing, but in the path when
    // the ruleset falls back to path-style (a bucket name containing a dot).
    let path_part = format!("{}/{}", client.endpoint.path_prefix, super::encode_key(&key));
    let query = aws_cli_runtime::presign::presign(
        &ctx,
        &aws_cli_runtime::presign::PresignRequest {
            method: "GET",
            host: &client.endpoint.host,
            path: &path_part,
            params: Vec::new(),
            extra_signed_headers: Vec::new(),
            expires,
            // S3 presigns with the literal UNSIGNED-PAYLOAD, not the empty-body hash.
            payload: aws_cli_runtime::presign::Payload::Unsigned,
        },
    );

    // Built from the origin plus the full path, since `endpoint.url` already contains
    // the prefix that `path_part` starts with.
    let scheme = client.endpoint.url.split_once("://").map(|(s, _)| s).unwrap_or("https");
    println!("{scheme}://{}{path_part}?{query}", client.endpoint.host);
    Ok(exit::code(exit::SUCCESS))
}

pub fn website(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let path = positional(parsed)?.to_string();
    let bucket = uri::website_bucket_name(&path).map_err(param_error)?;

    let mut index = None;
    let mut error = None;
    let tokens = &parsed.extras;
    let mut i = 0;
    while i < tokens.len() {
        let (name, inline) = match tokens[i].split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (tokens[i].as_str(), None),
        };
        let take = |i: &mut usize| -> Option<String> {
            if let Some(v) = inline.clone() {
                return Some(v);
            }
            *i += 1;
            tokens.get(*i).cloned()
        };
        match name {
            "--index-document" => index = take(&mut i),
            "--error-document" => error = take(&mut i),
            other => return Err(param_error(format!("Unknown options: {other}"))),
        }
        i += 1;
    }

    // With neither flag the reference sends an empty configuration and lets S3 reject it.
    let mut config = String::from(
        "<WebsiteConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">",
    );
    if let Some(suffix) = &index {
        config.push_str(&format!("<IndexDocument><Suffix>{}</Suffix></IndexDocument>", escape(suffix)));
    }
    if let Some(key) = &error {
        config.push_str(&format!("<ErrorDocument><Key>{}</Key></ErrorDocument>", escape(key)));
    }
    config.push_str("</WebsiteConfiguration>");

    let model =
        crate::load_model("s3api").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let client = Client::for_bucket(&model, globals, Some(&bucket))?;
    let response =
        client.send_raw("PUT", "/", "website=", &[], http::Body::from_vec(config.into_bytes()))?;
    if response.status >= 400 {
        return Err(service_error("PutBucketWebsite", &response));
    }
    // Success prints nothing at all.
    Ok(exit::code(exit::SUCCESS))
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_xml_text() {
        assert_eq!(escape("a&b<c>d"), "a&amp;b&lt;c&gt;d");
        assert_eq!(escape("plain"), "plain");
    }
}
