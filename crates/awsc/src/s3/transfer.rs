//! `cp`, `mv` and `rm`.
//!
//! Three deliberate departures from the reference, all of them UI:
//!
//! - The source is **scanned in full before any transfer starts**, so the progress totals
//!   are exact rather than the reference's `~`-prefixed running estimates.
//! - **Parts of a single large object transfer concurrently**, not just whole files, so
//!   one big file saturates the connection instead of one part at a time.
//! - The progress line is **clamped to the terminal width** (see [`super::progress`]).
//!
//! Everything else follows the reference: the 8 MiB multipart threshold and chunk size,
//! ten concurrent requests, the `upload:`/`download:`/`copy:`/`move:`/`delete:` result
//! lines, and the exit-code rule (1 if anything failed, 2 if only warnings).

pub use super::conn::Conn;
use awsc_runtime::http;
use super::delete;
use super::pool::Pool;
use super::progress::{Counter, Progress};
use super::{param_error, uri::Location, xml};
use crate::args::Parsed;
use crate::client::{Client, Globals};
use crate::exit;
use crate::Failure;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::process::ExitCode;

/// Reference defaults from `customizations/s3/transferconfig.py`.
const MULTIPART_THRESHOLD: u64 = 8 * 1024 * 1024;
const MULTIPART_CHUNKSIZE: u64 = 8 * 1024 * 1024;
const MAX_CONCURRENT_REQUESTS: usize = 10;
/// S3's own limits, which clamp the chunk size for very large objects.
const MAX_PARTS: u64 = 10_000;
const MIN_CHUNKSIZE: u64 = 5 * 1024 * 1024;

pub struct Options {
    pub recursive: bool,
    pub dryrun: bool,
    pub quiet: bool,
    pub only_show_errors: bool,
    pub progress: bool,
    /// `None` means adapt at runtime; `Some(n)` pins the worker count.
    pub concurrency: Option<usize>,
    pub storage_class: Option<String>,
    pub acl: Option<String>,
    pub sse: Option<String>,
    pub content_type: Option<String>,
    pub cache_control: Option<String>,
    pub content_disposition: Option<String>,
    pub content_encoding: Option<String>,
    pub content_language: Option<String>,
    pub expires: Option<String>,
    pub website_redirect: Option<String>,
    /// `--metadata`, user metadata sent as `x-amz-meta-*`.
    pub metadata: Vec<(String, String)>,
    /// `--metadata-directive`, COPY or REPLACE. Copies only.
    pub metadata_directive: Option<String>,
    /// `--grants`, each `Permission=Grantee_Type=Grantee_ID`.
    pub grants: Vec<String>,
    pub sse_kms_key_id: Option<String>,
    /// `--sse-c` and `--sse-c-key`: customer-provided encryption.
    pub sse_c: Option<String>,
    pub sse_c_key: Option<String>,
    /// `--follow-symlinks` is the default; `--no-follow-symlinks` turns it off.
    pub follow_symlinks: bool,
    pub excludes: Vec<(bool, String)>,
    /// `--delete`, sync only.
    pub delete: bool,
    /// How entries present on both sides are compared, sync only.
    pub strategy: super::sync::Strategy,
    /// `--multipart-threshold`: at or above this size an object is transferred in parts.
    pub multipart_threshold: u64,
    /// `--multipart-chunksize`: the part size, before the `MAX_PARTS` clamp.
    pub multipart_chunksize: u64,
}

impl Options {
    fn parse(parsed: &Parsed) -> Result<(Options, Vec<String>), Failure> {
        let mut options = Options {
            recursive: false,
            dryrun: false,
            quiet: false,
            only_show_errors: false,
            progress: true,
            concurrency: None,
            storage_class: None,
            acl: None,
            sse: None,
            content_type: None,
            cache_control: None,
            content_disposition: None,
            content_encoding: None,
            content_language: None,
            expires: None,
            website_redirect: None,
            metadata: Vec::new(),
            metadata_directive: None,
            grants: Vec::new(),
            sse_kms_key_id: None,
            sse_c: None,
            sse_c_key: None,
            follow_symlinks: true,
            excludes: Vec::new(),
            delete: false,
            strategy: super::sync::Strategy::SizeAndTime,
            multipart_threshold: MULTIPART_THRESHOLD,
            multipart_chunksize: MULTIPART_CHUNKSIZE,
        };
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
                let next = tokens.get(*i + 1)?;
                if next.starts_with("--") {
                    return None;
                }
                *i += 1;
                Some(next.clone())
            };
            match name {
                "--recursive" => options.recursive = true,
                "--dryrun" | "--dry-run" => options.dryrun = true,
                "--quiet" => options.quiet = true,
                "--only-show-errors" => options.only_show_errors = true,
                "--no-progress" => options.progress = false,
                // Pins the pool; without it the worker count adapts to measured
                // throughput. The reference's fixed default is the fallback when the
                // value cannot be read.
                "--concurrency" | "--max-concurrent-requests" => {
                    options.concurrency = Some(
                        take(&mut i)
                            .and_then(|v| v.parse().ok())
                            .filter(|n| *n > 0)
                            .unwrap_or(MAX_CONCURRENT_REQUESTS),
                    )
                }
                // Part sizing. The defaults suit a link of a few MB/s, where an 8 MiB
                // part is a couple of seconds of transfer; a fat pipe wants larger parts,
                // because the per-part round trip stops being negligible. Unreadable or
                // zero values keep the default rather than failing the transfer.
                "--multipart-threshold" => {
                    if let Some(n) = take(&mut i).as_deref().and_then(parse_size) {
                        options.multipart_threshold = n;
                    }
                }
                "--multipart-chunksize" => {
                    if let Some(n) = take(&mut i).as_deref().and_then(parse_size) {
                        options.multipart_chunksize = n;
                    }
                }
                "--storage-class" => options.storage_class = take(&mut i),
                "--acl" => options.acl = take(&mut i),
                "--sse" => options.sse = Some(take(&mut i).unwrap_or_else(|| "AES256".into())),
                "--content-type" => options.content_type = take(&mut i),
                "--cache-control" => options.cache_control = take(&mut i),
                "--content-disposition" => options.content_disposition = take(&mut i),
                "--content-encoding" => options.content_encoding = take(&mut i),
                "--content-language" => options.content_language = take(&mut i),
                "--expires" => options.expires = take(&mut i),
                "--website-redirect" => options.website_redirect = take(&mut i),
                "--metadata-directive" => options.metadata_directive = take(&mut i),
                "--sse-kms-key-id" => options.sse_kms_key_id = take(&mut i),
                "--sse-c" => {
                    options.sse_c = Some(take(&mut i).unwrap_or_else(|| "AES256".into()))
                }
                "--sse-c-key" => options.sse_c_key = take(&mut i),
                "--follow-symlinks" => options.follow_symlinks = true,
                "--no-follow-symlinks" => options.follow_symlinks = false,
                // `--grants a=b=c d=e=f` takes every following non-flag token.
                "--grants" => {
                    if let Some(first) = inline.clone() {
                        options.grants.push(first);
                    }
                    while let Some(next) = tokens.get(i + 1) {
                        if next.starts_with("--") {
                            break;
                        }
                        options.grants.push(next.clone());
                        i += 1;
                    }
                }
                // `--metadata` is a map: either `KeyName1=string,KeyName2=string`
                // shorthand or a JSON object.
                "--metadata" => {
                    let raw = take(&mut i).unwrap_or_default();
                    options.metadata = parse_metadata(&raw)?;
                }
                // The last matching rule wins, so order is preserved.
                "--exclude" => options.excludes.push((false, take(&mut i).unwrap_or_default())),
                "--include" => options.excludes.push((true, take(&mut i).unwrap_or_default())),
                // sync-only flags. `--size-only` and `--exact-timestamps` both claim the
                // same slot; the reference lets the later registration win, which makes
                // --exact-timestamps beat --size-only when both are given.
                "--delete" => options.delete = true,
                "--size-only" => {
                    if options.strategy != super::sync::Strategy::ExactTimestamps {
                        options.strategy = super::sync::Strategy::SizeOnly;
                    }
                }
                "--exact-timestamps" => {
                    options.strategy = super::sync::Strategy::ExactTimestamps
                }
                other => return Err(param_error(format!("Unknown options: {other}"))),
            }
            i += 1;
        }
        Ok((options, parsed.positionals.clone()))
    }

    /// Sync accepts the same flags; the distinction exists so `cp` keeps rejecting
    /// `--delete` and friends as unknown options, exactly as the reference does.
    pub fn parse_for_sync(parsed: &Parsed) -> Result<(Options, Vec<String>), Failure> {
        Options::parse(parsed)
    }
}

/// `--metadata` takes either `Key=value,Key2=value2` shorthand or a JSON object.
fn parse_metadata(raw: &str) -> Result<Vec<(String, String)>, Failure> {
    let trimmed = raw.trim();
    if trimmed.starts_with('{') {
        let parsed: serde_json::Map<String, serde_json::Value> = serde_json::from_str(trimmed)
            .map_err(|_| param_error("Error parsing parameter '--metadata': Invalid JSON received."))?;
        return Ok(parsed
            .into_iter()
            .map(|(k, v)| (k, v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())))
            .collect());
    }
    let mut out = Vec::new();
    for pair in trimmed.split(',').filter(|p| !p.is_empty()) {
        let Some((key, value)) = pair.split_once('=') else {
            // The shorthand parser reports the position it gave up at, with a caret under
            // the offending input on the following lines.
            return Err(param_error(format!(
                "Error parsing parameter '--metadata': Expected: '=', received: 'EOF' \
                 for input:\n {pair}\n{}^",
                " ".repeat(pair.len())
            )));
        };
        out.push((key.to_string(), value.to_string()));
    }
    Ok(out)
}

/// The SSE-C headers: the key travels base64-encoded with its MD5 alongside, which S3
/// uses to detect a key mangled in transit.
fn sse_c_headers(options: &Options) -> Vec<(String, String)> {
    let (Some(algorithm), Some(key)) = (&options.sse_c, &options.sse_c_key) else {
        return Vec::new();
    };
    use base64ct::{Base64, Encoding};
    use md5::{Digest, Md5};
    let raw = key.as_bytes();
    vec![
        ("x-amz-server-side-encryption-customer-algorithm".into(), algorithm.clone()),
        ("x-amz-server-side-encryption-customer-key".into(), Base64::encode_string(raw)),
        (
            "x-amz-server-side-encryption-customer-key-md5".into(),
            Base64::encode_string(&Md5::digest(raw)),
        ),
    ]
}

/// Per-object metadata headers, shared by uploads and copies.
fn object_headers(options: &Options, key: &str) -> Vec<(String, String)> {
    let mut headers = Vec::new();
    if let Some(class) = &options.storage_class {
        headers.push(("x-amz-storage-class".into(), class.clone()));
    }
    if let Some(acl) = &options.acl {
        headers.push(("x-amz-acl".into(), acl.clone()));
    }
    if let Some(sse) = &options.sse {
        headers.push(("x-amz-server-side-encryption".into(), sse.clone()));
    }
    if let Some(kms) = &options.sse_kms_key_id {
        headers.push(("x-amz-server-side-encryption-aws-kms-key-id".into(), kms.clone()));
    }
    headers.extend(sse_c_headers(options));
    for (header, value) in [
        ("cache-control", &options.cache_control),
        ("content-disposition", &options.content_disposition),
        ("content-encoding", &options.content_encoding),
        ("content-language", &options.content_language),
        ("expires", &options.expires),
        ("x-amz-website-redirect-location", &options.website_redirect),
        ("x-amz-metadata-directive", &options.metadata_directive),
    ] {
        if let Some(value) = value {
            headers.push((header.to_string(), value.clone()));
        }
    }
    for (name, value) in &options.metadata {
        headers.push((format!("x-amz-meta-{name}"), value.clone()));
    }
    // `Permission=Grantee_Type=Grantee_ID`, one header per permission.
    for grant in &options.grants {
        let mut parts = grant.splitn(3, '=');
        let (Some(permission), Some(kind), Some(id)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let header = match permission {
            "read" => "x-amz-grant-read",
            "readacl" => "x-amz-grant-read-acp",
            "writeacl" => "x-amz-grant-write-acp",
            "full" => "x-amz-grant-full-control",
            _ => continue,
        };
        headers.push((header.to_string(), format!("{kind}={id}")));
    }
    let content_type = options.content_type.clone().or_else(|| guess_content_type(key));
    if let Some(ct) = content_type {
        headers.push(("content-type".into(), ct));
    }
    headers
}

/// A small extension table, standing in for Python's `mimetypes`.
fn guess_content_type(key: &str) -> Option<String> {
    let extension = key.rsplit('.').next()?.to_ascii_lowercase();
    let mime = match extension.as_str() {
        "txt" => "text/plain",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" => "text/javascript",
        "json" => "application/json",
        "xml" => "application/xml",
        "csv" => "text/csv",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        "mp4" => "video/mp4",
        "mp3" => "audio/mpeg",
        "wasm" => "application/wasm",
        _ => return None,
    };
    Some(mime.to_string())
}

/// One object to move.
pub struct Item {
    /// Local path or S3 key on the source side.
    pub source: String,
    /// S3 key or local path on the destination side.
    pub dest: String,
    pub size: u64,
    /// Last modification, as seconds since the epoch.
    ///
    /// Sub-second precision is kept for local files and lost for S3 objects, whose
    /// `LastModified` is whole seconds. `sync` compares these directly with no tolerance,
    /// exactly as the reference does.
    pub modified: f64,
}

/// The chunk size for an object, doubled until it fits within S3's 10,000-part limit.
/// A byte count, plain or suffixed: `8388608`, `8MB`, `8MiB`, `1GB`.
///
/// `MB` and `MiB` both mean 2^20 — S3 part sizes are powers of two everywhere in the
/// service's own documentation, and a decimal reading would silently produce parts that
/// are not, which is the opposite of what anyone tuning this wants. Anything
/// unparseable, or zero, yields `None`, and the caller keeps its default.
fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let digits = text.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let suffix = text[digits.len()..].to_ascii_uppercase();
    let scale = match suffix.as_str() {
        "" | "B" => 1u64,
        "K" | "KB" | "KIB" => 1024,
        "M" | "MB" | "MIB" => 1024 * 1024,
        "G" | "GB" | "GIB" => 1024 * 1024 * 1024,
        _ => return None,
    };
    digits.trim().parse::<u64>().ok().filter(|n| *n > 0)?.checked_mul(scale)
}

fn chunk_size_for(total: u64, base: u64) -> u64 {
    let mut chunk = base.max(MIN_CHUNKSIZE);
    while total.div_ceil(chunk) > MAX_PARTS {
        chunk *= 2;
    }
    chunk.max(MIN_CHUNKSIZE)
}

pub fn cp(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    run(parsed, globals, Verb::Copy)
}

pub fn mv(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    run(parsed, globals, Verb::Move)
}

pub fn rm(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    run(parsed, globals, Verb::Remove)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Copy,
    Move,
    Remove,
    Sync,
}

fn run(parsed: &Parsed, globals: &Globals, verb: Verb) -> Result<ExitCode, Failure> {
    let (options, paths) = Options::parse(parsed)?;
    // Only sync understands these.
    for flag in ["--delete", "--size-only", "--exact-timestamps"] {
        if parsed.extras.iter().any(|t| t == flag || t.starts_with(&format!("{flag}="))) {
            return Err(param_error(format!("Unknown options: {flag}")));
        }
    }

    let expected = if verb == Verb::Remove { 1 } else { 2 };
    // Too few positionals is argparse's "arguments are required" with a usage block; too
    // many are reported as unknown options, since argparse never binds them.
    if paths.len() < expected {
        return Err(Failure::new(
            exit::PARAM_VALIDATION,
            format!(
                "{}\n\n{}",
                awsc_runtime::RuntimeError::ParamValidation(
                    "the following arguments are required: paths".to_string()
                ),
                crate::USAGE_HINT
            ),
        ));
    }
    if paths.len() > expected {
        return Err(param_error(format!("Unknown options: {}", paths[expected..].join(","))));
    }

    let source = Location::parse(&paths[0]).map_err(param_error)?;
    let dest = if expected == 2 {
        Location::parse(&paths[1]).map_err(param_error)?
    } else {
        Location::Local(String::new())
    };

    // A missing local source is validated up front, before the transfer starts, so it is
    // reported as a decorated general error at 255 rather than a `fatal error:` line.
    if let Location::Local(path) = &source {
        if !std::path::Path::new(path).exists() {
            return Err(Failure::new(
                exit::GENERAL_ERROR,
                format!("The user-provided path {path} does not exist."),
            ));
        }
    }

    let model =
        crate::load_model("s3api").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;

    let outcome = (|| -> Result<ExitCode, Failure> {
    match (&source, &dest, verb) {
        // Streaming forms: `cp - s3://...` reads stdin, `cp s3://... -` writes stdout.
        // Both suppress the result line, as the reference does by forcing
        // `only_show_errors` — stdout belongs to the object, not to progress reporting.
        (Location::Stream, Location::S3 { bucket, key }, _) => {
            if options.recursive {
                return Err(param_error(
                    "Streaming currently is only compatible with non-recursive cp commands",
                ));
            }
            let client = Client::for_bucket(&model, globals, Some(bucket))?;
            let conn = Conn::from_client(&client, globals);
            let mut body = Vec::new();
            std::io::Read::read_to_end(&mut std::io::stdin(), &mut body)
                .map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;
            let headers = object_headers(&options, key);
            conn.send_checked("PutObject", "PUT", &conn.object_path(key), "", &headers, http::Body::from_vec(body))?;
            Ok(exit::code(exit::SUCCESS))
        }
        (Location::S3 { bucket, key }, Location::Stream, _) => {
            if options.recursive {
                return Err(param_error(
                    "Streaming currently is only compatible with non-recursive cp commands",
                ));
            }
            let _ = bucket;
            let client = Client::for_bucket(&model, globals, Some(bucket))?;
            let conn = Conn::from_client(&client, globals);
            let response = conn
                .send_checked(
                    "GetObject",
                    "GET",
                    &conn.object_path(key),
                    "",
                    &sse_c_headers(&options),
                    http::Body::Empty,
                )
                .map_err(|e| missing_key_message(e, key))?;
            std::io::Write::write_all(&mut std::io::stdout(), response.bytes())
                .map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;
            Ok(exit::code(exit::SUCCESS))
        }
        (Location::S3 { bucket, key }, _, Verb::Remove) => {
            let client = Client::for_bucket(&model, globals, Some(bucket))?;
            let conn = Conn::from_client(&client, globals);
            remove(&conn, key, &options)
        }
        (Location::Local(path), Location::S3 { bucket, key }, _) => {
            let client = Client::for_bucket(&model, globals, Some(bucket))?;
            let conn = Conn::from_client(&client, globals);
            upload(&conn, path, key, &options, verb)
        }
        (Location::S3 { bucket, key }, Location::Local(path), _) => {
            let client = Client::for_bucket(&model, globals, Some(bucket))?;
            let conn = Conn::from_client(&client, globals);
            download(&conn, key, path, &options, verb, bucket)
        }
        (Location::S3 { bucket: sb, key: sk }, Location::S3 { bucket: db, key: dk }, _) => {
            let client = Client::for_bucket(&model, globals, Some(db))?;
            let conn = Conn::from_client(&client, globals);
            let source_client = Client::for_bucket(&model, globals, Some(sb))?;
            let source_conn = Conn::from_client(&source_client, globals);
            copy(&conn, &source_conn, sb, sk, dk, &options, verb)
        }
        _ => Err(param_error(
            "usage: aws s3 cp <LocalPath> <S3Uri> or <S3Uri> <LocalPath> or <S3Uri> <S3Uri>\n\
             Error: Invalid argument type",
        )),
    }
    })();

    match outcome {
        Ok(code) => Ok(code),
        // Parameter problems keep their own exit code and decoration; anything that fails
        // once the transfer is under way is reported by the result recorder as a bare
        // `fatal error:` line at rc 1.
        Err(failure) if failure.exit_code() == exit::PARAM_VALIDATION => Err(failure),
        Err(failure) => {
            if !options.quiet {
                eprintln!(
                    "{}",
                    crate::color::error(&format!("fatal error: {}", failure.message()))
                );
            }
            Ok(exit::code(1))
        }
    }
}

/// Glob matching with the reference's semantics: `*` and `?` cross `/`, and the *last*
/// matching rule decides.
///
/// Patterns are anchored to the **source root**, not matched against the relative key —
/// the reference prefixes each pattern with the root and matches the full path. So
/// `--exclude "sub/*"` only excludes `sub/` directly under the source, not a `sub/`
/// nested deeper.
pub fn included(path: &str, root: &str, rules: &[(bool, String)]) -> bool {
    let mut include = true;
    for (is_include, pattern) in rules {
        if glob_match(&anchor(root, pattern), path) {
            include = *is_include;
        }
    }
    include
}

/// `os.path.join(root, pattern)` — note an absolute pattern discards the root entirely.
fn anchor(root: &str, pattern: &str) -> String {
    if pattern.starts_with('/') {
        return pattern.to_string();
    }
    if root.is_empty() {
        return pattern.to_string();
    }
    format!("{}/{}", root.trim_end_matches('/'), pattern)
}

/// `fnmatch` semantics: `*` matches anything including separators, `?` matches one
/// character, `[...]` a set.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while ti < t.len() {
        // A `[...]` that PARSES is authoritative, whether or not it matched. Falling
        // through to the literal comparison below on a non-match made `[` match a literal
        // `[` in the text, so `[Erai-raws] One Piece -*.mkv` matched a file actually named
        // `[Erai-raws] One Piece - 1001.mkv` -- where fnmatch reads the brackets as a
        // character class and matches nothing. Only an *unterminated* `[` is a literal.
        if pi < p.len() && p[pi] == '[' {
            if let Some((matched, next)) = match_class(&p, pi, t[ti]) {
                if matched {
                    pi = next;
                    ti += 1;
                } else if star != usize::MAX {
                    pi = star + 1;
                    mark += 1;
                    ti = mark;
                } else {
                    return false;
                }
                continue;
            }
        }
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// `[abc]` / `[!abc]` / `[a-z]`; returns whether it matched and where the class ends.
fn match_class(pattern: &[char], start: usize, candidate: char) -> Option<(bool, usize)> {
    let mut i = start + 1;
    let negated = pattern.get(i) == Some(&'!');
    if negated {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while i < pattern.len() && (pattern[i] != ']' || first) {
        first = false;
        if i + 2 < pattern.len() && pattern[i + 1] == '-' && pattern[i + 2] != ']' {
            if pattern[i] <= candidate && candidate <= pattern[i + 2] {
                matched = true;
            }
            i += 3;
        } else {
            if pattern[i] == candidate {
                matched = true;
            }
            i += 1;
        }
    }
    if i >= pattern.len() {
        // Unterminated class: `[` is a literal, as fnmatch treats it.
        return None;
    }
    Some((matched != negated, i + 1))
}


/// Result accounting, which drives the exit code: 1 if anything failed, 2 if only
/// warnings, 0 otherwise.
#[derive(Default)]
pub struct Outcome {
    pub failed: AtomicU64,
    pub warned: AtomicU64,
}

impl Outcome {
    pub fn code(&self) -> ExitCode {
        if self.failed.load(Ordering::Relaxed) > 0 {
            exit::code(1)
        } else if self.warned.load(Ordering::Relaxed) > 0 {
            exit::code(2)
        } else {
            exit::code(exit::SUCCESS)
        }
    }
}

/// One unit of transferable work.
///
/// Small objects and individual parts of large ones sit in the **same** queue, so a single
/// large file saturates the pool on its own and a mix of one big file and many small ones
/// keeps every worker busy. Splitting them into separate phases, as an earlier version
/// did, left the pool idle whenever the current phase was thin.
enum Job {
    /// A whole object, below the multipart threshold.
    Whole { item: usize },
    /// One part of a multipart upload.
    Part { item: usize, part: u64, upload: usize },
    /// One byte range of a download.
    Range { item: usize, index: u64 },
}

/// Print a result line unless the mode suppresses it.
fn report(progress: &Progress, options: &Options, line: &str) {
    if options.quiet || options.only_show_errors {
        return;
    }
    progress.println(line);
}

fn report_failure(progress: &Progress, options: &Options, line: &str) {
    if options.quiet {
        return;
    }
    // Through the bar rather than around it: `eprintln!` on its own would be written over
    // the frame still on screen, and clearing the bar for good would mean one failed file
    // among a thousand left the rest of the transfer with no display at all.
    progress.eprintln(&crate::color::error(line));
}

pub fn verb_word(verb: Verb, uploading: bool) -> &'static str {
    match verb {
        Verb::Move => "move",
        Verb::Remove => "delete",
        // `sync` reports the underlying operation, not the word `sync`.
        Verb::Copy | Verb::Sync if uploading => "upload",
        Verb::Copy | Verb::Sync => "download",
    }
}

/// The reference's warning for anything it cannot read, on stderr, naming the absolute
/// path. It is a *warning*, not an error: the walk continues and the command exits 2.
fn warn_unreadable(path: &std::path::Path, quiet: bool) {
    if !quiet {
        warn(&format!(
            "warning: Skipping file {}. File/Directory is not readable.",
            abspath(&path.to_string_lossy())
        ));
    }
}

/// Special files have no contents to upload, and the reference says so distinctly.
fn warn_special(path: &std::path::Path, quiet: bool) {
    if !quiet {
        warn(&format!(
            "warning: Skipping file {}. File is character special device, block special \
             device, FIFO, or socket.",
            abspath(&path.to_string_lossy())
        ));
    }
}

/// Print a warning in yellow, on stderr.
///
/// The leading erase matters: warnings come out of the scan, which is drawing its own
/// counter on that row, and out of the transfer, which is drawing the bar there. Without
/// it the warning lands on top of a line that is still on screen.
fn warn(text: &str) {
    eprintln!("\r\x1b[2K{}", crate::color::warning(text));
}

/// Can this file actually be opened?
///
/// `metadata` succeeding does not mean the bytes are readable -- a mode-000 file stats
/// fine and fails to open. The reference opens every candidate for exactly this reason,
/// and skipping here is much better than discovering it mid-transfer, where it would abort
/// everything already in flight.
fn is_readable_file(path: &std::path::Path) -> bool {
    std::fs::File::open(path).is_ok()
}

/// Walk a local directory, or yield the single file.
///
/// Returns the items and the number of warnings emitted. Anything unreadable is **skipped
/// with a warning**, never fatal: one protected directory somewhere under the source used
/// to abort the entire transfer with `fatal error: Operation not permitted (os error 1)`,
/// which on macOS is what a TCC-protected folder returns for `read_dir`. The reference
/// walks past it and uploads everything else.
pub fn scan_local(
    root: &str,
    recursive: bool,
    follow_symlinks: bool,
    quiet: bool,
    counting: bool,
) -> Result<(Vec<Item>, u64), Failure> {
    let path = std::path::Path::new(root);
    // The walk happens before the first byte moves, and on a large tree it is a pause
    // with nothing on screen. Reporting the running count is the difference between a
    // slow start and an apparent hang.
    let counter = Counter::new(counting, "files");
    let io = |e: std::io::Error| Failure::new(exit::GENERAL_ERROR, e);
    let mut warnings = 0u64;
    if !recursive {
        let meta = std::fs::metadata(path).map_err(io)?;
        if is_special(&meta) {
            warn_special(path, quiet);
            return Ok((Vec::new(), 1));
        }
        if !is_readable_file(path) {
            warn_unreadable(path, quiet);
            return Ok((Vec::new(), 1));
        }
        return Ok((
            vec![Item {
                source: root.to_string(),
                dest: String::new(),
                size: meta.len(),
                modified: mtime_seconds(&meta),
            }],
            0,
        ));
    }
    let mut out = Vec::new();
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        // A directory we cannot list is the case that used to kill the whole walk.
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => {
                warn_unreadable(&dir, quiet);
                warnings += 1;
                continue;
            }
        };
        for entry in entries {
            let Ok(entry) = entry else {
                warn_unreadable(&dir, quiet);
                warnings += 1;
                continue;
            };
            let entry_path = entry.path();
            // `DirEntry::metadata` does NOT traverse symlinks, unlike `fs::metadata` —
            // following is the default, so the link has to be resolved explicitly.
            // A broken link is skipped rather than failing the whole walk.
            let Ok(link_meta) = std::fs::symlink_metadata(&entry_path) else {
                warn_unreadable(&entry_path, quiet);
                warnings += 1;
                continue;
            };
            if link_meta.file_type().is_symlink() && !follow_symlinks {
                continue;
            }
            let Ok(meta) = std::fs::metadata(&entry_path) else { continue };
            if meta.is_dir() {
                stack.push(entry_path);
            } else if is_special(&meta) {
                warn_special(&entry_path, quiet);
                warnings += 1;
            } else if meta.is_file() && !is_readable_file(&entry_path) {
                warn_unreadable(&entry_path, quiet);
                warnings += 1;
            } else if meta.is_file() {
                let relative = entry_path
                    .strip_prefix(path)
                    .unwrap_or(&entry_path)
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/");
                out.push(Item {
                    source: entry_path.to_string_lossy().into_owned(),
                    dest: relative,
                    size: meta.len(),
                    modified: mtime_seconds(&meta),
                });
                counter.add(1);
            }
        }
    }
    counter.clear();
    // Byte order, so a listing matches S3's own collation.
    out.sort_by(|a, b| a.dest.cmp(&b.dest));
    Ok((out, warnings))
}

/// A device, FIFO or socket: nothing to upload, and reading one may block forever.
fn is_special(meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        let t = meta.file_type();
        return t.is_block_device() || t.is_char_device() || t.is_fifo() || t.is_socket();
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
    }
}

/// Scan a local tree that may not exist yet — a sync destination often does not.
pub fn scan_local_if_present(root: &str) -> Result<Vec<Item>, Failure> {
    if !std::path::Path::new(root).exists() {
        return Ok(Vec::new());
    }
    // The destination side of a sync: warnings about it are not the user's concern here,
    // since nothing is being read from it.
    Ok(scan_local(root, true, true, true, false)?.0)
}

/// Execute a sync plan whose transfers are uploads.
pub fn sync_upload(
    conn: &Conn,
    plan: Vec<super::sync::Action>,
    key: &str,
    options: &Options,
    bucket: &str,
    scan_warnings: u64,
) -> Result<ExitCode, Failure> {
    let mut uploads = Vec::new();
    let mut deletes = Vec::new();
    for action in plan {
        if action.delete {
            deletes.push(action.item);
        } else {
            let mut item = action.item;
            item.dest = join_key(key, &item.dest);
            uploads.push(item);
        }
    }

    let total: u64 = uploads.iter().map(|i| i.size).sum();
    // Anything the scan could not read is already reported; it still has to reach the
    // exit code, which is 2 when warnings were the only problem.
    let count = (uploads.len() + deletes.len()) as u64;
    let progress = Progress::new(count, total, options.progress && !options.quiet);
    let outcome = Outcome::default();
    // Unreadable paths the scan skipped count toward the exit code: 2 when they were the
    // only problem, which is what the reference returns.
    outcome.warned.fetch_add(scan_warnings, Ordering::Relaxed);

    // A dry run still reports warnings in its exit code: the reference returns 2 when it
    // skipped something unreadable, whether or not it went on to transfer anything.
    if options.dryrun {
        for item in &uploads {
            println!(
                "(dryrun) upload: {} to s3://{}",
                display_local(&item.source),
                display_key(conn, &item.dest)
            );
        }
        for item in &deletes {
            println!("(dryrun) delete: s3://{bucket}/{}", item.source);
        }
        return Ok(outcome.code());
    }

    execute_uploads(conn, &uploads, options, "upload", &progress, &outcome)?;
    execute_deletes(conn, &deletes, options, &progress, &outcome);
    progress.clear();
    Ok(outcome.code())
}

/// Execute a sync plan whose transfers are downloads.
pub fn sync_download(
    conn: &Conn,
    plan: Vec<super::sync::Action>,
    local: &str,
    options: &Options,
    bucket: &str,
) -> Result<ExitCode, Failure> {
    let mut downloads = Vec::new();
    let mut deletes = Vec::new();
    for action in plan {
        if action.delete {
            deletes.push(action.item);
        } else {
            let mut item = action.item;
            item.dest = format!("{}/{}", local.trim_end_matches('/'), item.dest);
            downloads.push(item);
        }
    }

    let total: u64 = downloads.iter().map(|i| i.size).sum();
    let count = (downloads.len() + deletes.len()) as u64;
    let progress = Progress::new(count, total, options.progress && !options.quiet);
    let outcome = Outcome::default();

    if options.dryrun {
        for item in &downloads {
            println!(
                "(dryrun) download: s3://{bucket}/{} to {}",
                item.source,
                display_local(&item.dest)
            );
        }
        for item in &deletes {
            println!("(dryrun) delete: {}", display_local(&item.source));
        }
        return Ok(outcome.code());
    }

    let pool = Pool::new(options.concurrency);
    pool.run(&downloads, options.concurrency.is_none(), |item| {
        let result = get_object(conn, item, options, &progress, &pool).and_then(|_| {
            // Stamp the local mtime to the object's LastModified. Without this a clean
            // download leaves the local file newer than the object, and the next sync
            // would download it all over again.
            set_mtime(&item.dest, item.modified);
            Ok(())
        });
        finish(
            &progress,
            &outcome,
            options,
            "download",
            result,
            &item.source,
            &format!("s3://{bucket}/{}", item.source),
            &display_local(&item.dest),
        );
    });

    // `sync --delete` on a download removes local files, not objects.
    for item in &deletes {
        let result = std::fs::remove_file(&item.source)
            .map_err(|e| Failure::new(exit::GENERAL_ERROR, e));
        match result {
            Ok(()) => {
                if !options.quiet && !options.only_show_errors {
                    progress.println(&format!("delete: {}", display_local(&item.source)));
                }
                progress.finish_file(&item.source);
            }
            Err(e) => {
                outcome.failed.fetch_add(1, Ordering::Relaxed);
                if !options.quiet {
                    progress.eprintln(&crate::color::error(&format!(
                        "delete failed: {} {}",
                        display_local(&item.source),
                        e.message()
                    )));
                }
                progress.finish_file(&item.source);
            }
        }
    }

    progress.clear();
    Ok(outcome.code())
}

/// Execute a sync plan whose transfers are server-side copies.
pub fn sync_copy(
    conn: &Conn,
    source_conn: &Conn,
    plan: Vec<super::sync::Action>,
    key: &str,
    options: &Options,
    source_bucket: &str,
) -> Result<ExitCode, Failure> {
    let mut copies = Vec::new();
    let mut deletes = Vec::new();
    for action in plan {
        if action.delete {
            deletes.push(action.item);
        } else {
            let mut item = action.item;
            item.dest = join_key(key, &item.dest);
            copies.push(item);
        }
    }

    let total: u64 = copies.iter().map(|i| i.size).sum();
    let count = (copies.len() + deletes.len()) as u64;
    let progress = Progress::new(count, total, options.progress && !options.quiet);
    let outcome = Outcome::default();

    if options.dryrun {
        for item in &copies {
            println!(
                "(dryrun) copy: s3://{source_bucket}/{} to s3://{}",
                item.source,
                display_key(conn, &item.dest)
            );
        }
        for item in &deletes {
            println!("(dryrun) delete: s3://{}", display_key(conn, &item.source));
        }
        return Ok(outcome.code());
    }

    let pool = Pool::new(options.concurrency);
    pool.run(&copies, options.concurrency.is_none(), |item| {
        progress.begin_file(&item.source, &item.source, item.size);
        let result = copy_object(conn, source_bucket, item, options).inspect(|_| {
            pool.record_bytes(item.size);
            // The bytes move inside S3, so there is no stream to watch: one object, one
            // step.
            progress.add_file_bytes(&item.source, item.size);
        });
        finish(
            &progress,
            &outcome,
            options,
            "copy",
            result,
            &item.source,
            &format!("s3://{source_bucket}/{}", item.source),
            &format!("s3://{}", display_key(conn, &item.dest)),
        );
    });
    let _ = source_conn;
    execute_deletes(conn, &deletes, options, &progress, &outcome);
    progress.clear();
    Ok(outcome.code())
}

/// Delete a set of objects.
fn execute_deletes(
    conn: &Conn,
    items: &[Item],
    options: &Options,
    progress: &Progress,
    outcome: &Outcome,
) {
    let keys: Vec<String> = items.iter().map(|i| i.source.clone()).collect();
    delete::batched(conn, &keys, options.concurrency, |key, result| {
        let target = format!("s3://{}", display_key(conn, key));
        match result {
            Ok(()) => {
                if !options.quiet && !options.only_show_errors {
                    progress.println(&format!("delete: {target}"));
                }
            }
            Err(message) => {
                outcome.failed.fetch_add(1, Ordering::Relaxed);
                if !options.quiet {
                    progress.eprintln(&crate::color::error(&format!(
                        "delete failed: {target} {message}"
                    )));
                }
            }
        }
        progress.finish_file(key);
    });
}

/// Copy one large object part by part.
///
/// Three details that a single `CopyObject` handles for free and this must do explicitly:
///
/// - **The source is pinned.** Every `UploadPartCopy` carries
///   `x-amz-copy-source-if-match` with the source's ETag, so a source replaced mid-copy
///   fails the transfer rather than silently stitching together two different objects.
/// - **Nothing is inherited.** A server-side multipart copy does not carry the source's
///   metadata across, so the properties are read from the source and set on
///   `CreateMultipartUpload` — this is what `--copy-props` governs in the reference, whose
///   default is to preserve them.
/// - **`CreateMultipartUpload` rejects the copy-source conditionals**, so they are only
///   attached to the part requests.
fn multipart_copy(
    conn: &Conn,
    source_conn: &Conn,
    source_bucket: &str,
    item: &Item,
    options: &Options,
    pool: &Pool,
    progress: &Progress,
) -> Result<(), Failure> {
    let head = source_conn.send_checked(
        "HeadObject",
        "HEAD",
        &source_conn.object_path(&item.source),
        "",
        &sse_c_headers(options),
        http::Body::Empty,
    )?;
    let source_etag = head.header("etag").unwrap_or_default();

    let mut headers = object_headers(options, &item.dest);
    // `--metadata-directive` is meaningless here — there is no directive on a multipart
    // create — and inherited properties are supplied explicitly instead.
    headers.retain(|(name, _)| name != "x-amz-metadata-directive");
    if options.metadata.is_empty() {
        for header in ["cache-control", "content-disposition", "content-encoding", "content-language", "expires"] {
            if !headers.iter().any(|(n, _)| n == header) {
                if let Some(value) = head.header(header) {
                    headers.push((header.to_string(), value));
                }
            }
        }
        for (name, value) in head.headers() {
            if let Some(suffix) = name.to_ascii_lowercase().strip_prefix("x-amz-meta-") {
                headers.push((format!("x-amz-meta-{suffix}"), value.clone()));
            }
        }
    }
    if options.content_type.is_none() {
        if let Some(ct) = head.header("content-type") {
            headers.retain(|(n, _)| n != "content-type");
            headers.push(("content-type".to_string(), ct));
        }
    }

    let path = conn.object_path(&item.dest);
    let created =
        conn.send_checked("CreateMultipartUpload", "POST", &path, "uploads=", &headers, http::Body::Empty)?;
    let upload_id = xml::parse(&created.text())
        .map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?
        .get("UploadId")
        .to_string();

    let chunk = chunk_size_for(item.size, options.multipart_chunksize);
    let parts: Vec<u64> = (1..=item.size.div_ceil(chunk)).collect();
    let etags: Mutex<Vec<(u64, String)>> = Mutex::new(Vec::new());
    let failure: Mutex<Option<Failure>> = Mutex::new(None);
    let copy_source = format!("/{source_bucket}/{}", super::encode_key(&item.source));

    pool.run(&parts, options.concurrency.is_none(), |part| {
        if failure.lock().expect("mutex").is_some() {
            return;
        }
        let start = (part - 1) * chunk;
        let end = (start + chunk).min(item.size) - 1;
        let result = (|| -> Result<(), Failure> {
            let mut part_headers = sse_c_headers(options);
            part_headers.push(("x-amz-copy-source".to_string(), copy_source.clone()));
            part_headers
                .push(("x-amz-copy-source-range".to_string(), format!("bytes={start}-{end}")));
            if !source_etag.is_empty() {
                part_headers
                    .push(("x-amz-copy-source-if-match".to_string(), source_etag.clone()));
            }
            let response = conn.send_checked(
                "UploadPartCopy",
                "PUT",
                &path,
                &format!("partNumber={part}&uploadId={}", super::encode_query(&upload_id)),
                &part_headers,
                http::Body::Empty,
            )?;
            // The part ETag is in the body here, not a header.
            let etag = xml::parse(&response.text())
                .map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?
                .get("ETag")
                .to_string();
            etags.lock().expect("mutex").push((*part, etag));
            pool.record_bytes(end - start + 1);
            progress.add_file_bytes(&item.source, end - start + 1);
            Ok(())
        })();
        if let Err(e) = result {
            if e.service_error_code.as_deref() == Some("SlowDown") {
                pool.note_throttle();
            }
            *failure.lock().expect("mutex") = Some(e);
        }
    });

    if let Some(e) = failure.into_inner().expect("mutex") {
        let _ = conn.send(
            "DELETE",
            &path,
            &format!("uploadId={}", super::encode_query(&upload_id)),
            &[],
            http::Body::Empty,
        );
        // A source that changed underneath us is reported as such rather than as a bare
        // precondition failure.
        if e.service_error_code.as_deref() == Some("PreconditionFailed") {
            return Err(Failure::new(
                exit::CLIENT_ERROR,
                format!(
                    "Contents of stored object \"{}\" in bucket \"{source_bucket}\" did not \
                     match expected ETag.",
                    item.source
                ),
            ));
        }
        return Err(e);
    }

    let mut collected = etags.into_inner().expect("mutex");
    collected.sort_by_key(|(n, _)| *n);
    let mut body = String::from("<CompleteMultipartUpload>");
    for (number, etag) in &collected {
        body.push_str(&format!("<Part><PartNumber>{number}</PartNumber><ETag>{etag}</ETag></Part>"));
    }
    body.push_str("</CompleteMultipartUpload>");
    conn.send_checked(
        "CompleteMultipartUpload",
        "POST",
        &path,
        &format!("uploadId={}", super::encode_query(&upload_id)),
        &sse_c_headers(options),
        http::Body::from_vec(body.into_bytes()),
    )?;
    Ok(())
}

/// One server-side copy.
fn copy_object(
    conn: &Conn,
    source_bucket: &str,
    item: &Item,
    options: &Options,
) -> Result<(), Failure> {
    let mut headers = object_headers(options, &item.dest);
    headers.push((
        "x-amz-copy-source".to_string(),
        format!("/{source_bucket}/{}", super::encode_key(&item.source)),
    ));
    conn.send_checked("CopyObject", "PUT", &conn.object_path(&item.dest), "", &headers, http::Body::Empty)?;
    Ok(())
}

/// Set a file's modification time, best effort.
fn set_mtime(path: &str, seconds: f64) {
    #[cfg(unix)]
    {
        let Ok(c_path) = std::ffi::CString::new(path) else { return };
        let times = [
            libc::timeval { tv_sec: seconds as libc::time_t, tv_usec: 0 },
            libc::timeval { tv_sec: seconds as libc::time_t, tv_usec: 0 },
        ];
        // SAFETY: `c_path` is NUL-terminated and `times` is a two-element array, which is
        // what `utimes` expects. A failure only means the timestamp is left alone.
        unsafe {
            libc::utimes(c_path.as_ptr(), times.as_ptr());
        }
    }
    #[cfg(not(unix))]
    let _ = (path, seconds);
}

/// A file's modification time in seconds since the epoch.
///
/// An unreadable time becomes the epoch, matching the reference, which substitutes
/// `EPOCH_TIME` and warns rather than skipping the file.
fn mtime_seconds(meta: &std::fs::Metadata) -> f64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// A recursive S3 source is a *directory* prefix, so it gains a trailing slash.
///
/// Without it, `s3://bucket/mut` also matches `mut2/...`: ListObjectsV2 takes a raw string
/// prefix, not a path. The reference appends the separator for every `dir_op` command,
/// which is `cp --recursive`, `rm --recursive` and all of `sync`.
pub fn dir_prefix(key: &str) -> String {
    if key.is_empty() || key.ends_with('/') {
        key.to_string()
    } else {
        format!("{key}/")
    }
}

/// List every object under a prefix.
///
/// Fans out over sub-prefixes where the keyspace has them: the continuation chain of a
/// single prefix is strictly sequential, so a deep listing is round-trip bound rather than
/// bandwidth bound. See [`super::list`].
pub fn scan_s3(conn: &Conn, prefix: &str, counting: bool) -> Result<Vec<Item>, Failure> {
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) * 2;
    let counter = Counter::new(counting, "objects");
    let entries = super::list::deep(conn, prefix, workers, &counter)?;
    counter.clear();
    Ok(entries
        .into_iter()
        .filter(|entry| {
            // Zero-byte pseudo-folders are skipped for transfers.
            !(entry.key.ends_with('/') && entry.size == 0)
        })
        .map(|entry| {
            let relative =
                entry.key.strip_prefix(prefix).unwrap_or(&entry.key).trim_start_matches('/');
            Item {
                dest: relative.to_string(),
                size: entry.size,
                modified: super::ls::parse_iso8601(&entry.last_modified)
                    .map(|s| s as f64)
                    .unwrap_or(0.0),
                source: entry.key.clone(),
            }
        })
        .collect())
}

/// A 404 from `HeadObject` has an empty body, so the reference supplies the wording.
pub fn missing_key_message(failure: Failure, key: &str) -> Failure {
    if failure.service_error_code.as_deref() != Some("404") {
        return failure;
    }
    let mut replaced = Failure::new(
        exit::CLIENT_ERROR,
        format!(
            "An error occurred (404) when calling the HeadObject operation: \
             Key \"{key}\" does not exist"
        ),
    );
    replaced.service_error_code = Some("404".to_string());
    replaced
}

/// `os.path.abspath`: prepend the working directory and fold away `.`/`..`, without
/// resolving symlinks.
pub fn abspath(path: &str) -> String {
    let raw = std::path::Path::new(path);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(raw)
    };
    let mut parts: Vec<String> = Vec::new();
    for component in joined.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                parts.pop();
            }
            other => parts.push(other.as_os_str().to_string_lossy().into_owned()),
        }
    }
    // The root component already carries its separator.
    let mut out = parts.join("/");
    if out.starts_with("//") {
        out.remove(0);
    }
    out.trim_end_matches('/').to_string()
}

/// A local path as the reference displays it: relative to the working directory.
///
/// `os.path.join(os.path.relpath(dirname, '.'), basename)`, so a file in the current
/// directory shows as `./name` and one elsewhere climbs with `../`.
pub fn display_local(path: &str) -> String {
    let absolute = std::path::Path::new(path);
    let absolute = if absolute.is_absolute() {
        absolute.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(absolute)
    };
    let Some(parent) = absolute.parent() else { return path.to_string() };
    let base = absolute.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let cwd = std::env::current_dir().unwrap_or_default();

    let from: Vec<_> = cwd.components().collect();
    let to: Vec<_> = parent.components().collect();
    let shared = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = std::iter::repeat_n("..".to_string(), from.len() - shared)
        .chain(to[shared..].iter().map(|c| c.as_os_str().to_string_lossy().into_owned()))
        .collect();
    if parts.is_empty() {
        parts.push(".".to_string());
    }
    parts.push(base);
    parts.join("/")
}

/// Join a destination prefix and a relative path, tolerating a missing separator.
pub fn join_key(prefix: &str, relative: &str) -> String {
    if relative.is_empty() {
        return prefix.to_string();
    }
    if prefix.is_empty() {
        return relative.to_string();
    }
    format!("{}/{}", prefix.trim_end_matches('/'), relative)
}

fn upload(
    conn: &Conn,
    local: &str,
    key: &str,
    options: &Options,
    verb: Verb,
) -> Result<ExitCode, Failure> {
    let (mut items, scan_warnings) =
        scan_local(
            local,
            options.recursive,
            options.follow_symlinks,
            options.quiet,
            options.progress && !options.quiet,
        )?;
    if !options.recursive {
        let base = std::path::Path::new(local)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let target = if key.is_empty() || key.ends_with('/') {
            join_key(key, &base)
        } else {
            key.to_string()
        };
        items[0].dest = target;
    } else {
        // Local root, absolutised, with no trailing separator — the form the reference
        // builds its full-path patterns from. `abspath`, NOT `canonicalize`: resolving
        // symlinks would rewrite `/tmp` to `/private/tmp` on macOS and stop every pattern
        // matching, since the scanned paths keep the name the user typed.
        let root = abspath(local);
        for item in &mut items {
            item.dest = join_key(key, &item.dest);
        }
        // Patterns are anchored to the ABSOLUTISED root, so the paths matched against them
        // must be absolute too. The scan yields paths in the form the user typed, so a
        // relative source like `.` produced `./x.mkv` against a root of `/abs/dir` and
        // matched NOTHING -- every --exclude and --include was silently ignored, and
        // `--exclude "*" --include "..."` uploaded the entire tree.
        items.retain(|i| included(&abspath(&i.source), &root, &options.excludes));
    }

    let total_bytes: u64 = items.iter().map(|i| i.size).sum();
    let progress =
        Progress::new(items.len() as u64, total_bytes, options.progress && !options.quiet);
    let outcome = Outcome::default();
    // Unreadable paths the scan skipped count toward the exit code: 2 when they were the
    // only problem, which is what the reference returns.
    outcome.warned.fetch_add(scan_warnings, Ordering::Relaxed);
    let word = verb_word(verb, true);

    if options.dryrun {
        for item in &items {
            println!(
                "(dryrun) {word}: {} to s3://{}",
                display_local(&item.source),
                display_key(conn, &item.dest)
            );
        }
        return Ok(outcome.code());
    }

    execute_uploads(conn, &items, options, word, &progress, &outcome)?;
    progress.clear();
    Ok(outcome.code())
}

/// Upload a planned set of items. Shared by `cp`/`mv` and `sync`.
pub fn execute_uploads(
    conn: &Conn,
    items: &[Item],
    options: &Options,
    word: &str,
    progress: &Progress,
    outcome: &Outcome,
) -> Result<(), Failure> {
    // Open a multipart upload for each large object, then queue every part alongside the
    // small objects so one pool serves them all.
    let large: Vec<usize> = (0..items.len())
        .filter(|i| items[*i].size >= options.multipart_threshold)
        .collect();
    let uploads: Vec<Upload> = large
        .iter()
        .map(|index| begin_upload(conn, *index, &items[*index], options))
        .collect::<Result<Vec<_>, _>>()?;

    let mut jobs: Vec<Job> = (0..items.len())
        .filter(|i| items[*i].size < options.multipart_threshold)
        .map(|item| Job::Whole { item })
        .collect();
    for (upload_index, upload) in uploads.iter().enumerate() {
        let item = &items[upload.item];
        let parts = item.size.div_ceil(upload.chunk);
        for part in 1..=parts {
            jobs.push(Job::Part { item: upload.item, part, upload: upload_index });
        }
    }

    let pool = Pool::new(options.concurrency);
    let failures: Mutex<Vec<(usize, Failure)>> = Mutex::new(Vec::new());
    let etags: Mutex<Vec<(usize, u64, String)>> = Mutex::new(Vec::new());

    pool.run(&jobs, options.concurrency.is_none(), |job| {
        match job {
            Job::Whole { item } => {
                let item_ref = &items[*item];
                progress.begin_file(&item_ref.source, &display_local(&item_ref.source), item_ref.size);
                let result = put_object(conn, item_ref, options, progress);
                match result {
                    Ok(()) => {
                        pool.record_bytes(items[*item].size);
                        report_item(progress, outcome, options, word, Ok(()), &items[*item], conn);
                    }
                    Err(e) => report_item(
                        progress, outcome, options, word, Err(e), &items[*item], conn,
                    ),
                }
            }
            Job::Part { item, part, upload } => {
                let upload = &uploads[*upload];
                if failures.lock().expect("mutex").iter().any(|(i, _)| i == item) {
                    return;
                }
                let offset = (part - 1) * upload.chunk;
                let length = upload.chunk.min(items[*item].size - offset);
                // Every part of one object reports into one row, whichever part gets
                // here first.
                progress.begin_file(
                    &items[*item].source,
                    &display_local(&items[*item].source),
                    items[*item].size,
                );
                match upload_part(conn, &items[*item], upload, *part, offset, length, progress) {
                    Ok(etag) => {
                        etags.lock().expect("mutex").push((*item, *part, etag));
                        pool.record_bytes(length);
                    }
                    Err(e) => {
                        if e.service_error_code.as_deref() == Some("SlowDown") {
                            pool.note_throttle();
                        }
                        failures.lock().expect("mutex").push((*item, e));
                    }
                }
            }
            Job::Range { .. } => unreachable!("uploads never queue ranges"),
        }
    });

    // Finish or abort each multipart upload now every part has been attempted.
    let failed = failures.into_inner().expect("mutex");
    let collected = etags.into_inner().expect("mutex");
    for upload in &uploads {
        let item = &items[upload.item];
        let result = match failed.iter().find(|(i, _)| *i == upload.item) {
            Some((_, _)) => {
                abort_upload(conn, item, upload);
                Err(failed
                    .iter()
                    .find(|(i, _)| *i == upload.item)
                    .map(|(_, e)| Failure::new(e.exit_code(), e.message()))
                    .expect("failure present"))
            }
            None => {
                let mut parts: Vec<(u64, String)> = collected
                    .iter()
                    .filter(|(i, _, _)| *i == upload.item)
                    .map(|(_, n, tag)| (*n, tag.clone()))
                    .collect();
                parts.sort_by_key(|(n, _)| *n);
                complete_upload(conn, item, upload, &parts)
            }
        };
        report_item(progress, outcome, options, word, result, item, conn);
    }
    Ok(())
}

/// An in-flight multipart upload.
struct Upload {
    item: usize,
    id: String,
    chunk: u64,
    path: String,
}

fn begin_upload(
    conn: &Conn,
    index: usize,
    item: &Item,
    options: &Options,
) -> Result<Upload, Failure> {
    let path = conn.object_path(&item.dest);
    let headers = object_headers(options, &item.dest);
    let created = conn.send_checked(
        "CreateMultipartUpload",
        "POST",
        &path,
        "uploads=",
        &headers,
        http::Body::Empty,
    )?;
    let id = xml::parse(&created.text())
        .map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?
        .get("UploadId")
        .to_string();
    Ok(Upload { item: index, id, chunk: chunk_size_for(item.size, options.multipart_chunksize), path })
}

#[allow(clippy::too_many_arguments)]
fn upload_part(
    conn: &Conn,
    item: &Item,
    upload: &Upload,
    part: u64,
    offset: u64,
    length: u64,
    progress: &Progress,
) -> Result<String, Failure> {
    // Described, not read: the part is streamed off disk while the request is in flight,
    // so a worker costs a chunk buffer rather than a whole part. With ten workers and
    // 64 MiB parts that is the difference between a few hundred kilobytes and 640 MiB.
    let body = http::Body::FileRange { path: item.source.clone().into(), offset, len: length };
    let response = conn.send_checked_watched(
        "UploadPart",
        "PUT",
        &upload.path,
        &format!("partNumber={part}&uploadId={}", super::encode_query(&upload.id)),
        &[],
        body,
        Some(progress.watch(&item.source)),
    )?;
    Ok(response.header("etag").unwrap_or_default())
}

fn complete_upload(
    conn: &Conn,
    _item: &Item,
    upload: &Upload,
    parts: &[(u64, String)],
) -> Result<(), Failure> {
    let mut body = String::from("<CompleteMultipartUpload>");
    for (number, etag) in parts {
        body.push_str(&format!(
            "<Part><PartNumber>{number}</PartNumber><ETag>{etag}</ETag></Part>"
        ));
    }
    body.push_str("</CompleteMultipartUpload>");
    conn.send_checked(
        "CompleteMultipartUpload",
        "POST",
        &upload.path,
        &format!("uploadId={}", super::encode_query(&upload.id)),
        &[],
        http::Body::from_vec(body.into_bytes()),
    )?;
    Ok(())
}

/// Abort so a failed upload leaves no parts behind accruing storage charges.
fn abort_upload(conn: &Conn, _item: &Item, upload: &Upload) {
    let _ = conn.send(
        "DELETE",
        &upload.path,
        &format!("uploadId={}", super::encode_query(&upload.id)),
        &[],
        http::Body::Empty,
    );
}

fn report_item(
    progress: &Progress,
    outcome: &Outcome,
    options: &Options,
    word: &str,
    result: Result<(), Failure>,
    item: &Item,
    conn: &Conn,
) {
    finish(
        progress,
        outcome,
        options,
        word,
        result,
        &item.source,
        &display_local(&item.source),
        &format!("s3://{}", display_key(conn, &item.dest)),
    );
}

pub fn display_key(conn: &Conn, key: &str) -> String {
    // The bucket is in the host under virtual-host addressing; recover it for display.
    let host = &conn.endpoint.host;
    let bucket = if conn.endpoint.path_prefix.is_empty() {
        host.split(".s3").next().unwrap_or(host).to_string()
    } else {
        conn.endpoint.path_prefix.trim_start_matches('/').to_string()
    };
    format!("{bucket}/{key}")
}

/// `key` identifies the file in the in-flight list, which is not the same string as the
/// `from` shown to the user: uploads list a local path and report it relative, downloads
/// list a key and report it as a URL.
#[allow(clippy::too_many_arguments)]
fn finish(
    progress: &Progress,
    outcome: &Outcome,
    options: &Options,
    word: &str,
    result: Result<(), Failure>,
    key: &str,
    from: &str,
    to: &str,
) {
    match result {
        Ok(()) => {
            report(progress, options, &format!("{word}: {from} to {to}"));
            progress.finish_file(key);
        }
        Err(failure) => {
            outcome.failed.fetch_add(1, Ordering::Relaxed);
            report_failure(progress, options, &format!("{word} failed: {from} to {to} {}", failure.message()));
            progress.finish_file(key);
        }
    }
}

fn put_object(
    conn: &Conn,
    item: &Item,
    options: &Options,
    progress: &Progress,
) -> Result<(), Failure> {
    // Streamed off disk rather than read up front, so a whole-file upload costs a chunk
    // buffer no matter how large the file is. `item.size` comes from the scan that
    // planned this transfer.
    let body = http::Body::FileRange {
        path: item.source.clone().into(),
        offset: 0,
        len: item.size,
    };
    let headers = object_headers(options, &item.dest);
    conn.send_checked_watched(
        "PutObject",
        "PUT",
        &conn.object_path(&item.dest),
        "",
        &headers,
        body,
        // Watched, so the bar moves while a single large object is going up rather than
        // once it has gone.
        Some(progress.watch(&item.source)),
    )?;
    Ok(())
}

fn write_all_at(file: &std::fs::File, buffer: &[u8], offset: u64) -> Result<(), Failure> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.write_all_at(buffer, offset).map_err(|e| Failure::new(exit::GENERAL_ERROR, e))
    }
    #[cfg(not(unix))]
    {
        use std::io::{Seek, Write};
        let mut file = file.try_clone().map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;
        file.seek(std::io::SeekFrom::Start(offset))
            .and_then(|_| file.write_all(buffer))
            .map_err(|e| Failure::new(exit::GENERAL_ERROR, e))
    }
}

fn download(
    conn: &Conn,
    key: &str,
    local: &str,
    options: &Options,
    verb: Verb,
    bucket: &str,
) -> Result<ExitCode, Failure> {
    let mut items = if options.recursive {
        let root = format!("{bucket}/{}", key.trim_end_matches('/'));
        let mut found = scan_s3(conn, &dir_prefix(key), options.progress && !options.quiet)?;
        found.retain(|i| included(&format!("{bucket}/{}", i.source), &root, &options.excludes));
        found
    } else {
        let head = conn
            .send_checked(
                "HeadObject",
                "HEAD",
                &conn.object_path(key),
                "",
                &sse_c_headers(options),
                http::Body::Empty,
            )
            .map_err(|e| missing_key_message(e, key))?;
        let size =
            head.header("content-length").and_then(|v| v.parse().ok()).unwrap_or_default();
        vec![Item { source: key.to_string(), dest: String::new(), size, modified: 0.0 }]
    };

    if options.recursive {
        for item in &mut items {
            item.dest = format!("{}/{}", local.trim_end_matches('/'), item.dest);
        }
    } else {
        let target = std::path::Path::new(local);
        let is_dir = target.is_dir() || local.ends_with('/');
        items[0].dest = if is_dir {
            let base = key.rsplit('/').next().unwrap_or_default();
            format!("{}/{base}", local.trim_end_matches('/'))
        } else {
            local.to_string()
        };
    }

    let total_bytes: u64 = items.iter().map(|i| i.size).sum();
    let progress =
        Progress::new(items.len() as u64, total_bytes, options.progress && !options.quiet);
    let outcome = Outcome::default();
    let word = verb_word(verb, false);

    if options.dryrun {
        for item in &items {
            println!(
                "(dryrun) {word}: s3://{bucket}/{} to {}",
                item.source,
                display_local(&item.dest)
            );
        }
        return Ok(outcome.code());
    }

    // Preallocate the large files, then queue every range next to the small files so one
    // pool serves both — a single large object gets the whole pool to itself.
    let mut jobs: Vec<Job> = Vec::new();
    let mut handles: Vec<Option<std::fs::File>> = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        if item.size < options.multipart_threshold {
            handles.push(None);
            jobs.push(Job::Whole { item: index });
            continue;
        }
        create_parent(&item.dest)?;
        let file =
            std::fs::File::create(&item.dest).map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;
        file.set_len(item.size).map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;
        handles.push(Some(file));
        let chunk = chunk_size_for(item.size, options.multipart_chunksize);
        for range in 0..item.size.div_ceil(chunk) {
            jobs.push(Job::Range { item: index, index: range });
        }
    }

    let pool = Pool::new(options.concurrency);
    let failures: Mutex<Vec<(usize, Failure)>> = Mutex::new(Vec::new());

    pool.run(&jobs, options.concurrency.is_none(), |job| match job {
        Job::Whole { item } => {
            let result = get_object(conn, &items[*item], options, &progress, &pool);
            finish(
                &progress,
                &outcome,
                options,
                word,
                result,
                &items[*item].source,
                &format!("s3://{bucket}/{}", items[*item].source),
                &display_local(&items[*item].dest),
            );
        }
        Job::Range { item, index } => {
            if failures.lock().expect("mutex").iter().any(|(i, _)| i == item) {
                return;
            }
            let chunk = chunk_size_for(items[*item].size, options.multipart_chunksize);
            let start = index * chunk;
            let end = (start + chunk).min(items[*item].size) - 1;
            let file = handles[*item].as_ref().expect("large items have a handle");
            progress.begin_file(
                &items[*item].source,
                &items[*item].source,
                items[*item].size,
            );
            let result = (|| -> Result<(), Failure> {
                let mut headers = sse_c_headers(options);
                headers.push(("range".to_string(), format!("bytes={start}-{end}")));
                let response = conn.send_checked_watched(
                    "GetObject",
                    "GET",
                    &conn.object_path(&items[*item].source),
                    "",
                    &headers,
                    http::Body::Empty,
                    Some(progress.watch(&items[*item].source)),
                )?;
                write_all_at(file, response.bytes(), start)?;
                pool.record_bytes(response.bytes().len() as u64);
                Ok(())
            })();
            if let Err(e) = result {
                if e.service_error_code.as_deref() == Some("SlowDown") {
                    pool.note_throttle();
                }
                failures.lock().expect("mutex").push((*item, e));
            }
        }
        Job::Part { .. } => unreachable!("downloads never queue upload parts"),
    });

    // Report each large file once every one of its ranges has been attempted.
    let failed = failures.into_inner().expect("mutex");
    for (index, item) in items.iter().enumerate() {
        if item.size < options.multipart_threshold {
            continue;
        }
        let result = match failed.iter().find(|(i, _)| *i == index) {
            Some((_, e)) => {
                // The file was preallocated at full size before the first range was
                // fetched, so a failure would otherwise leave a large sparse file of
                // zeroes that looks like a successful download.
                drop(handles[index].take());
                let _ = std::fs::remove_file(&item.dest);
                Err(Failure::new(e.exit_code(), e.message()))
            }
            None => Ok(()),
        };
        finish(
            &progress,
            &outcome,
            options,
            word,
            result,
            &item.source,
            &format!("s3://{bucket}/{}", item.source),
            &display_local(&item.dest),
        );
    }

    progress.clear();
    Ok(outcome.code())
}

pub fn create_parent(path: &str) -> Result<(), Failure> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;
        }
    }
    Ok(())
}

fn get_object(
    conn: &Conn,
    item: &Item,
    options: &Options,
    progress: &Progress,
    pool: &Pool,
) -> Result<(), Failure> {
    progress.begin_file(&item.source, &item.source, item.size);
    let response = conn.send_checked_watched(
        "GetObject",
        "GET",
        &conn.object_path(&item.source),
        "",
        &sse_c_headers(options),
        http::Body::Empty,
        // The response body is what moves here, and the watcher counts it as it arrives.
        Some(progress.watch(&item.source)),
    )?;
    create_parent(&item.dest)?;
    std::fs::write(&item.dest, response.bytes())
        .map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;
    pool.record_bytes(response.bytes().len() as u64);
    Ok(())
}

fn copy(
    conn: &Conn,
    source_conn: &Conn,
    source_bucket: &str,
    source_key: &str,
    dest_key: &str,
    options: &Options,
    verb: Verb,
) -> Result<ExitCode, Failure> {
    let mut items = if options.recursive {
        let root = format!("{source_bucket}/{}", source_key.trim_end_matches('/'));
        let mut found = scan_s3(source_conn, &dir_prefix(source_key), options.progress && !options.quiet)?;
        found.retain(|i| {
            included(&format!("{source_bucket}/{}", i.source), &root, &options.excludes)
        });
        for item in &mut found {
            item.dest = join_key(dest_key, &item.dest);
        }
        found
    } else {
        let head = source_conn
            .send_checked(
                "HeadObject",
                "HEAD",
                &source_conn.object_path(source_key),
                "",
                &[],
                http::Body::Empty,
            )
            .map_err(|e| missing_key_message(e, source_key))?;
        let size = head.header("content-length").and_then(|v| v.parse().ok()).unwrap_or_default();
        vec![Item {
            source: source_key.to_string(),
            dest: dest_key.to_string(),
            size,
            modified: 0.0,
        }]
    };
    items.retain(|i| !i.source.ends_with('/'));

    let total_bytes: u64 = items.iter().map(|i| i.size).sum();
    let progress = Progress::new(items.len() as u64, total_bytes, options.progress && !options.quiet);
    let outcome = Outcome::default();
    let word = if verb == Verb::Move { "move" } else { "copy" };

    if options.dryrun {
        for item in &items {
            println!("(dryrun) {word}: s3://{source_bucket}/{} to s3://{}", item.source, display_key(conn, &item.dest));
        }
        return Ok(outcome.code());
    }

    // A `CopyObject` is capped at 5 GiB, and a single request for a large object ties up
    // one worker for the whole transfer. Anything at or above the multipart threshold is
    // copied part by part with `UploadPartCopy`, with the parts sharing the pool.
    let (large, small): (Vec<&Item>, Vec<&Item>) =
        items.iter().partition(|i| i.size >= options.multipart_threshold);

    let pool = Pool::new(options.concurrency);
    // For a move, the sources of the copies that succeeded, deleted together once the
    // copying is done. Deleting per object would be one request per key; more
    // importantly, the old code discarded the delete's result entirely, so a move that
    // copied and then failed to remove the source still printed `move:` and exited 0.
    let moved: Mutex<Vec<String>> = Mutex::new(Vec::new());

    for item in large {
        progress.begin_file(&item.source, &item.source, item.size);
        let result = multipart_copy(conn, source_conn, source_bucket, item, options, &pool, &progress);
        if verb == Verb::Move && result.is_ok() {
            moved.lock().expect("moved keys mutex").push(item.source.clone());
        }
        finish(
            &progress,
            &outcome,
            options,
            word,
            result,
            &item.source,
            &format!("s3://{source_bucket}/{}", item.source),
            &format!("s3://{}", display_key(conn, &item.dest)),
        );
    }

    pool.run(&small, options.concurrency.is_none(), |item| {
        let result = (|| -> Result<(), Failure> {
            let mut headers = object_headers(options, &item.dest);
            headers.push((
                "x-amz-copy-source".to_string(),
                format!("/{source_bucket}/{}", super::encode_key(&item.source)),
            ));
            conn.send_checked("CopyObject", "PUT", &conn.object_path(&item.dest), "", &headers, http::Body::Empty)?;
            progress.add_file_bytes(&item.source, item.size);
            Ok(())
        })();
        if verb == Verb::Move && result.is_ok() {
            moved.lock().expect("moved keys mutex").push(item.source.clone());
        }
        finish(&progress, &outcome, options, word, result, &item.source,
            &format!("s3://{source_bucket}/{}", item.source),
            &format!("s3://{}", display_key(conn, &item.dest)));
    });

    let mut moved = moved.into_inner().expect("moved keys mutex");
    moved.sort();
    delete::batched(source_conn, &moved, options.concurrency, |key, result| {
        if let Err(message) = result {
            outcome.failed.fetch_add(1, Ordering::Relaxed);
            report_failure(
                &progress,
                options,
                &format!("delete failed: s3://{source_bucket}/{key} {message}"),
            );
        }
    });

    progress.clear();
    Ok(outcome.code())
}

fn remove(conn: &Conn, key: &str, options: &Options) -> Result<ExitCode, Failure> {
    let items = if options.recursive {
        let bucket = display_key(conn, "");
        let root = format!("{}{}", bucket, key.trim_end_matches('/'));
        let mut found = scan_s3(conn, &dir_prefix(key), options.progress && !options.quiet)?;
        found.retain(|i| included(&format!("{bucket}{}", i.source), &root, &options.excludes));
        found
    } else {
        vec![Item { source: key.to_string(), dest: String::new(), size: 0, modified: 0.0 }]
    };

    let progress = Progress::new(items.len() as u64, 0, options.progress && !options.quiet);
    let outcome = Outcome::default();

    if options.dryrun {
        for item in &items {
            println!("(dryrun) delete: s3://{}", display_key(conn, &item.source));
        }
        return Ok(outcome.code());
    }

    let keys: Vec<String> = items.iter().map(|i| i.source.clone()).collect();
    delete::batched(conn, &keys, options.concurrency, |key, result| {
        let target = format!("s3://{}", display_key(conn, key));
        match result {
            Ok(()) => {
                // `delete:` lines carry no destination.
                report(&progress, options, &format!("delete: {target}"));
            }
            Err(message) => {
                outcome.failed.fetch_add(1, Ordering::Relaxed);
                report_failure(&progress, options, &format!("delete failed: {target} {message}"));
            }
        }
        progress.finish_file(key);
    });

    progress.clear();
    Ok(outcome.code())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `*` crosses `/`, which is why `--exclude "*"` is recursive.
    #[test]
    fn star_crosses_path_separators() {
        assert!(glob_match("/a/*", "/a/b/c.txt"));
        assert!(glob_match("*", "any/deep/path"));
        assert!(glob_match("*.txt", "sub/dir/x.txt"));
        assert!(glob_match("/a?b", "/a/b"));
    }

    #[test]
    fn matches_literals_and_classes() {
        assert!(glob_match("abc", "abc"));
        assert!(!glob_match("abc", "abd"));
        assert!(glob_match("a[bc]d", "abd"));
        assert!(glob_match("a[b-d]e", "ace"));
        assert!(!glob_match("a[!bc]d", "abd"));
        assert!(glob_match("a[!bc]d", "aed"));
    }

    /// The decisive property: the LAST matching rule wins, so order is everything.
    #[test]
    fn last_matching_rule_wins() {
        let exclude_then_include =
            [(false, "*".to_string()), (true, "*.txt".to_string())];
        assert!(included("/root/a.txt", "/root", &exclude_then_include));
        assert!(included("/root/sub/b.txt", "/root", &exclude_then_include));
        assert!(!included("/root/c.log", "/root", &exclude_then_include));

        // Reversed, everything is excluded.
        let include_then_exclude =
            [(true, "*.txt".to_string()), (false, "*".to_string())];
        assert!(!included("/root/a.txt", "/root", &include_then_exclude));
        assert!(!included("/root/c.log", "/root", &include_then_exclude));
    }

    #[test]
    fn no_rules_includes_everything() {
        assert!(included("anything", "/root", &[]));
    }

    /// Patterns are anchored to the source root, so `sub/*` excludes only the `sub`
    /// directly beneath it. Verified against the reference.
    #[test]
    fn anchors_patterns_to_the_source_root() {
        let rules = [(false, "sub/*".to_string())];
        assert!(!included("/root/sub/b.log", "/root", &rules));
        assert!(included("/root/a.txt", "/root", &rules));
        // A deeper `sub/` is NOT matched, because the pattern is rooted.
        assert!(included("/root/deep/sub/c.log", "/root", &rules));
    }

    /// An absolute pattern discards the root, which is `os.path.join`'s behaviour.
    #[test]
    fn absolute_patterns_discard_the_root() {
        assert_eq!(anchor("/root", "/etc/x"), "/etc/x");
        assert_eq!(anchor("/root", "sub/*"), "/root/sub/*");
        assert_eq!(anchor("bucket/pre", "*.txt"), "bucket/pre/*.txt");
    }

    /// Doubled until the object fits in 10,000 parts, and never below S3's 5 MiB minimum.
    #[test]
    fn chooses_a_legal_chunk_size() {
        // A small object keeps the default chunk size; the adjuster only ever grows it.
        assert_eq!(chunk_size_for(1024, MULTIPART_CHUNKSIZE), MULTIPART_CHUNKSIZE);
        assert_eq!(chunk_size_for(100 * 1024 * 1024, MULTIPART_CHUNKSIZE), MULTIPART_CHUNKSIZE);
        let huge = 5 * 1024_u64.pow(4); // 5 TiB, S3's per-object maximum
        let chunk = chunk_size_for(huge, MULTIPART_CHUNKSIZE);
        assert!(huge.div_ceil(chunk) <= MAX_PARTS, "{chunk} leaves too many parts");
        assert!(chunk >= MIN_CHUNKSIZE);
    }

    #[test]
    fn reads_sizes_with_and_without_suffixes() {
        assert_eq!(parse_size("8388608"), Some(8 * 1024 * 1024));
        assert_eq!(parse_size("8MB"), Some(8 * 1024 * 1024));
        assert_eq!(parse_size("8MiB"), Some(8 * 1024 * 1024));
        assert_eq!(parse_size("8mb"), Some(8 * 1024 * 1024));
        assert_eq!(parse_size("1GB"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_size("512KB"), Some(512 * 1024));
        // Rejected, so the caller keeps its default rather than transferring in
        // zero-sized or nonsensical parts.
        assert_eq!(parse_size("0"), None);
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("8TB"), None);
        assert_eq!(parse_size("lots"), None);
        // A space between the number and its unit is accepted; it is unambiguous.
        assert_eq!(parse_size("8 MB"), Some(8 * 1024 * 1024));
    }

    /// A chunk size below S3's 5 MiB minimum is raised, not obeyed: parts smaller than
    /// that are rejected for every part but the last.
    #[test]
    fn a_requested_chunk_size_is_clamped_to_the_service_minimum() {
        assert_eq!(chunk_size_for(100 * 1024 * 1024, 1024), MIN_CHUNKSIZE);
        assert_eq!(chunk_size_for(100 * 1024 * 1024, 64 * 1024 * 1024), 64 * 1024 * 1024);
    }

    #[test]
    fn guesses_common_content_types() {
        assert_eq!(guess_content_type("a/b.json").as_deref(), Some("application/json"));
        assert_eq!(guess_content_type("IMG.JPG").as_deref(), Some("image/jpeg"));
        assert_eq!(guess_content_type("noextension"), None);
    }
}

#[cfg(test)]
mod scan_regression_tests {
    use super::*;

    /// `[...]` is a character class, exactly as Python's `fnmatch` reads it. A class that
    /// parses but does not match is a MISMATCH, not a literal `[` -- getting that wrong
    /// made `--include "[Erai-raws] One Piece -*.mkv"` select files literally named
    /// `[Erai-raws] One Piece - ....mkv`, which the reference does not select at all.
    /// Every case here was checked against the reference binary.
    #[test]
    fn character_classes_match_fnmatch() {
        let file = "[Erai-raws] One Piece - 1001 [1080p].mkv";
        assert!(!glob_match("[Erai-raws] One Piece -*.mkv", file), "brackets are a class");
        assert!(glob_match("[[]Erai-raws] One Piece -*.mkv", file), "[[] is a literal bracket");
        assert!(glob_match("*One Piece*.mkv", file));

        assert!(glob_match("[a-z]*", "notes.txt"));
        assert!(!glob_match("[a-z]*", "Notes.txt"));
        assert!(glob_match("[!SE]*", "notes.txt"));
        assert!(!glob_match("[!n]*", "notes.txt"));
        assert!(glob_match("notes.tx[t]", "notes.txt"));

        // An unterminated class has no closing bracket, so `[` really is a literal.
        assert!(glob_match("unterminated[abc", "unterminated[abc"));
        assert!(!glob_match("unterminated[abc", "unterminated"));

        // A class that fails to match must still allow an earlier `*` to backtrack.
        assert!(glob_match("*[0-9].mkv", "One Piece - 1001.mkv"));
        assert!(!glob_match("*[0-9].mkv", "One Piece - x.mkv"));
    }

    /// Patterns are anchored to the absolutised root, so the paths matched against them
    /// have to be absolute too. `scan_local` yields paths in the form the user typed, so
    /// with a relative source every pattern was compared against `./x.mkv` while anchored
    /// at `/abs/dir/` -- nothing ever matched, and **every `--exclude` and `--include` was
    /// silently ignored**. `--exclude "*" --include "only-these"` uploaded the whole tree.
    #[test]
    fn filters_apply_to_a_relative_source() {
        let root = abspath(".");
        let rules = vec![(false, "*".to_string()), (true, "*.mkv".to_string())];

        // The form the scan produces for a relative source, absolutised as the filter
        // site now does.
        assert!(included(&abspath("./keep.mkv"), &root, &rules), "an included file was dropped");
        assert!(!included(&abspath("./drop.txt"), &root, &rules), "--exclude did not exclude");

        // The raw scan form is exactly what used to be passed, and it matches nothing --
        // which is why the bug was invisible rather than noisy.
        assert!(
            included("./drop.txt", &root, &rules),
            "the unabsolutised path should match no pattern; if this fails the anchoring \
             changed and this test no longer guards the bug it was written for"
        );
    }

    /// A directory the walk cannot read is skipped with a warning, not fatal. One
    /// TCC-protected folder anywhere under the source used to abort the whole transfer
    /// with `fatal error: Operation not permitted (os error 1)`.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_directory_is_skipped_not_fatal() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicU32, Ordering as O};
        static N: AtomicU32 = AtomicU32::new(0);

        let base = std::env::temp_dir()
            .join(format!("awsc-scan-{}-{}", std::process::id(), N.fetch_add(1, O::Relaxed)));
        let locked = base.join("locked");
        std::fs::create_dir_all(&locked).expect("temp tree");
        std::fs::write(base.join("readable.txt"), b"x").expect("write");
        std::fs::write(locked.join("inner.txt"), b"x").expect("write");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");

        let scanned = scan_local(&base.to_string_lossy(), true, true, true, false);

        // Restore before asserting, so a failure cannot leave an unremovable directory.
        let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::remove_dir_all(&base);

        let (items, warnings) = scanned.expect("an unreadable directory must not be fatal");
        assert_eq!(warnings, 1, "the skipped directory should have warned exactly once");
        let sources: Vec<&String> = items.iter().map(|i| &i.source).collect();
        assert_eq!(items.len(), 1, "the readable file should still be found: {sources:?}");
        assert!(items[0].source.ends_with("readable.txt"), "{:?}", items[0].source);
    }

    /// A file that stats fine but cannot be opened is skipped too. Discovering it
    /// mid-transfer would abort everything already in flight.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_is_skipped_not_fatal() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicU32, Ordering as O};
        static N: AtomicU32 = AtomicU32::new(0);

        let base = std::env::temp_dir()
            .join(format!("awsc-scanf-{}-{}", std::process::id(), N.fetch_add(1, O::Relaxed)));
        std::fs::create_dir_all(&base).expect("temp tree");
        std::fs::write(base.join("open.txt"), b"x").expect("write");
        let shut = base.join("shut.txt");
        std::fs::write(&shut, b"x").expect("write");
        std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o000)).expect("chmod");

        let scanned = scan_local(&base.to_string_lossy(), true, true, true, false);
        let _ = std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o644));
        let _ = std::fs::remove_dir_all(&base);

        let (items, warnings) = scanned.expect("an unreadable file must not be fatal");
        assert_eq!(warnings, 1, "the unopenable file should have warned");
        let sources: Vec<&String> = items.iter().map(|i| &i.source).collect();
        assert_eq!(items.len(), 1, "only the readable file should be listed: {sources:?}");
        assert!(items[0].source.ends_with("open.txt"), "{:?}", items[0].source);
    }
}
