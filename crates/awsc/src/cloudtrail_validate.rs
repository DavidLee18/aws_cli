//! `aws cloudtrail validate-logs`.
//!
//! A port of `customizations/cloudtrail/validation.py`. CloudTrail writes a *digest file*
//! every hour listing the log files it delivered, each with its SHA-256; every digest is
//! RSA-signed, and each one names the previous digest and includes that digest's
//! signature in what it signs. So the digests form a **backwards-linked chain**, and the
//! command walks it from the newest in range to the oldest, verifying each link.
//!
//! That chain is the whole point: it is what makes deleting a log file detectable. Three
//! things follow from it and shape the code:
//!
//! - **Traversal is backwards**, newest first, following `previousDigestS3Object`.
//! - **A broken link is not the end.** When a digest has no previous — the trail was off
//!   for a while — the walk falls back to the next-oldest digest that S3 actually has,
//!   and reports the gap.
//! - **The signature covers the link.** The signed string ends with the *previous*
//!   digest's signature, so re-signing one digest in the middle cannot be done without
//!   the private key for every digest after it.

use crate::args::Parsed;
use crate::client::{Client, Globals};
use crate::exit;
use crate::Failure;
use serde_json::{json, Value};
use std::io::Write;
use std::process::ExitCode;

/// `%Y%m%dT%H%M%SZ`, the form that appears in a digest's S3 key.
fn format_date(unix: i64) -> String {
    let (year, month, day, hour, minute, second) = civil(unix);
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

/// `%Y-%m-%dT%H:%M:%SZ`, the form the command prints.
fn format_display_date(unix: i64) -> String {
    let (year, month, day, hour, minute, second) = civil(unix);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// The date forms that actually occur: an ISO-8601 timestamp as the user types it, and
/// the compact form inside a digest key.
///
/// `dateutil.parser.parse` accepts far more than this. Anything it would accept and this
/// does not is reported rather than guessed at, which is the safer direction for a
/// command whose answer is "these logs were not tampered with".
fn parse_date(text: &str) -> Option<i64> {
    let digits: Vec<i64> = {
        let cleaned: String = text.chars().filter(|c| c.is_ascii_digit()).collect();
        if !(cleaned.len() == 8 || cleaned.len() == 14) {
            return None;
        }
        let n = |range: std::ops::Range<usize>| cleaned.get(range)?.parse::<i64>().ok();
        let mut parts = vec![n(0..4)?, n(4..6)?, n(6..8)?];
        if cleaned.len() == 14 {
            parts.push(n(8..10)?);
            parts.push(n(10..12)?);
            parts.push(n(12..14)?);
        } else {
            parts.extend([0, 0, 0]);
        }
        parts
    };
    let [year, month, day, hour, minute, second] = digits[..] else { return None };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59
        || second > 60
    {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = if month <= 2 { month + 12 } else { month };
    let doy = (153 * (m - 3) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil(unix: i64) -> (i64, i64, i64, i64, i64, i64) {
    let days = unix.div_euclid(86_400);
    let seconds = unix.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, seconds / 3600, (seconds % 3600) / 60, seconds % 60)
}

/// A backfill digest is written when CloudTrail catches up on a period it missed. Its key
/// carries a `_backfill` suffix, which moves where the date sits.
fn is_backfill_key(key: &str) -> bool {
    key.ends_with("_backfill.json.gz")
}

/// The timestamp inside a digest's key, by position — the reference slices rather than
/// parses, and the two suffix lengths are why backfill keys need their own offsets.
fn extract_key_date(key: &str) -> &str {
    let bytes = key.len();
    let (from, to) = if is_backfill_key(key) {
        (bytes.saturating_sub(33), bytes.saturating_sub(17))
    } else {
        (bytes.saturating_sub(24), bytes.saturating_sub(8))
    };
    key.get(from..to).unwrap_or("")
}

/// `arn:aws:cloudtrail:us-east-1:123456789012:trail/name`.
fn arn_is_valid(arn: &str) -> bool {
    let parts: Vec<&str> = arn.splitn(6, ':').collect();
    if parts.len() != 6 {
        return false;
    }
    parts[0] == "arn"
        && !parts[1].is_empty()
        && parts[2] == "cloudtrail"
        && !parts[3].is_empty()
        && parts[4].len() == 12
        && parts[4].chars().all(|c| c.is_ascii_digit())
        && parts[5].starts_with("trail/")
        && parts[5].len() > "trail/".len()
}

/// Everything needed to name a digest in S3.
struct KeyNaming {
    account_id: String,
    trail_name: String,
    home_region: String,
    source_region: String,
    organization_id: Option<String>,
}

impl KeyNaming {
    /// The key a digest written at `unix` would have. Used only as a list marker, so it
    /// does not have to exist — and one minute is subtracted so the range stays inclusive.
    fn marker(&self, unix: i64, prefix: Option<&str>) -> String {
        let date = unix - 60;
        let (year, month, day, ..) = civil(date);
        let ymd = format!("{year:04}/{month:02}/{day:02}");
        let stamp = format_date(date);
        let key = match &self.organization_id {
            Some(organization_id) => format!(
                "AWSLogs/{organization_id}/{}/CloudTrail-Digest/{}/{ymd}/{}_CloudTrail-Digest_{}_{}_{}_{stamp}.json.gz",
                self.account_id, self.source_region, self.account_id, self.source_region,
                self.trail_name, self.home_region
            ),
            None => format!(
                "AWSLogs/{}/CloudTrail-Digest/{}/{ymd}/{}_CloudTrail-Digest_{}_{}_{}_{stamp}.json.gz",
                self.account_id, self.source_region, self.account_id, self.source_region,
                self.trail_name, self.home_region
            ),
        };
        with_prefix(prefix, &key)
    }

    /// The listing prefix, which scopes the scan to this trail's region.
    fn prefix(&self, prefix: Option<&str>) -> String {
        let key = match &self.organization_id {
            Some(organization_id) => format!(
                "AWSLogs/{organization_id}/{}/CloudTrail-Digest/{}",
                self.account_id, self.source_region
            ),
            None => format!(
                "AWSLogs/{}/CloudTrail-Digest/{}",
                self.account_id, self.source_region
            ),
        };
        with_prefix(prefix, &key)
    }

    /// Does this key belong to this trail?
    ///
    /// The reference builds a regex; this matches the same shape by structure, which
    /// avoids having to escape a trail name that contains regex metacharacters — a real
    /// possibility, since trail names allow `.` and `-`.
    fn matches(&self, key: &str, prefix: Option<&str>) -> bool {
        let Some(rest) = strip_prefix(prefix, key) else { return false };
        let head = match &self.organization_id {
            Some(organization_id) => format!(
                "AWSLogs/{organization_id}/{}/CloudTrail-Digest/{}/",
                self.account_id, self.source_region
            ),
            None => format!(
                "AWSLogs/{}/CloudTrail-Digest/{}/",
                self.account_id, self.source_region
            ),
        };
        let Some(rest) = rest.strip_prefix(&head) else { return false };
        // `\d+/\d+/\d+/` — the year, month and day directories.
        let mut parts = rest.splitn(4, '/');
        let (Some(y), Some(m), Some(d), Some(file)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return false;
        };
        if ![y, m, d].iter().all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
        {
            return false;
        }
        let expected = format!(
            "{}_CloudTrail-Digest_{}_{}_{}_",
            self.account_id, self.source_region, self.trail_name, self.home_region
        );
        // `.+(?:_backfill)?\.json\.gz` — a non-empty tail, then the extension.
        file.strip_prefix(&expected)
            .and_then(|tail| tail.strip_suffix(".json.gz"))
            .is_some_and(|tail| !tail.is_empty())
    }
}

fn with_prefix(prefix: Option<&str>, key: &str) -> String {
    match prefix.filter(|p| !p.is_empty()) {
        Some(prefix) => format!("{prefix}/{key}"),
        None => key.to_string(),
    }
}

fn strip_prefix<'a>(prefix: Option<&str>, key: &'a str) -> Option<&'a str> {
    match prefix.filter(|p| !p.is_empty()) {
        Some(prefix) => key.strip_prefix(&format!("{prefix}/")),
        None => Some(key),
    }
}

/// The string a digest's signature covers.
///
/// The last line is the **previous** digest's signature, which is what links the chain:
/// changing any digest invalidates every digest after it. A first digest has no previous,
/// and the literal `null` is used — matching the Java implementation that writes them.
fn string_to_sign(digest: &Value, inflated: &[u8]) -> String {
    use sha2::Digest as _;
    let text = |key: &str| digest.get(key).and_then(Value::as_str).unwrap_or_default();
    let previous = match digest.get("previousDigestSignature") {
        Some(Value::String(signature)) => signature.clone(),
        _ => "null".to_string(),
    };
    format!(
        "{}\n{}/{}\n{}\n{previous}",
        text("digestEndTime"),
        text("digestS3Bucket"),
        text("digestS3Object"),
        hex(&sha2::Sha256::digest(inflated))
    )
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Verify one digest's signature against the public key CloudTrail published.
///
/// The key is a base64 **PKCS#1** `RSAPublicKey`, not the `SubjectPublicKeyInfo` most
/// tools emit — which is why the reference's error says "Unable to load PKCS #1 key".
fn verify_digest(public_key_base64: &str, digest: &Value, inflated: &[u8]) -> Result<(), String> {
    use rsa::pkcs1::DecodeRsaPublicKey;
    use sha2::Digest as _;

    let der = crate::cloudtrail::base64_decode_public(public_key_base64)
        .ok_or_else(|| "key".to_string())?;
    let key = rsa::RsaPublicKey::from_pkcs1_der(&der).map_err(|_| "key".to_string())?;
    let signature = digest
        .get("_signature")
        .and_then(Value::as_str)
        .and_then(hex_decode)
        .ok_or_else(|| "signature".to_string())?;
    let hashed = sha2::Sha256::digest(string_to_sign(digest, inflated).as_bytes());
    key.verify(rsa::Pkcs1v15Sign::new::<sha2::Sha256>(), &hashed, &signature)
        .map_err(|_| "signature".to_string())
}

/// Inflate a gzip member, reporting whether anything followed it.
///
/// Trailing data is reported rather than ignored: a log file with bytes after the end of
/// the compressed stream has been appended to, which is exactly what this command exists
/// to notice. The reference checks `unused_data` for the same reason.
fn gunzip(bytes: &[u8]) -> Option<(Vec<u8>, bool)> {
    use std::io::Read;
    // The `bufread` decoder, not the `read` one: it consumes exactly the gzip member from
    // the underlying `BufRead`, so what is left over is genuinely trailing data rather
    // than the decoder's own read-ahead.
    let mut decoder = flate2::bufread::GzDecoder::new(bytes);
    let mut out = Vec::with_capacity(bytes.len() * 4);
    decoder.read_to_end(&mut out).ok()?;
    let remaining = decoder.into_inner();
    Some((out, !remaining.is_empty()))
}

/// Why a digest could not be used.
enum DigestProblem {
    /// The object is gone from S3. The service's message is not reported — the reference
    /// builds its own "not found" line — but keeping the variant distinct is what decides
    /// which line that is.
    Missing,
    /// It is there and it does not check out.
    Invalid(String),
    /// Something that should stop the command outright.
    Fatal(Failure),
}

/// S3 clients, one per region, resolved per bucket.
///
/// A trail's digests and its logs can live in different buckets in different regions, and
/// an S3 request has to be signed for the bucket's own region — so the location is looked
/// up once per bucket and cached.
struct S3Clients<'a> {
    model: &'a awsc_model::Model,
    globals: Globals,
    location_region: String,
    bucket_regions: std::cell::RefCell<std::collections::BTreeMap<String, String>>,
}

impl<'a> S3Clients<'a> {
    fn client(&self, bucket: &str) -> Result<Client<'a>, Failure> {
        let region = self.region_of(bucket)?;
        Client::new(self.model, &Globals { region: Some(region), ..self.globals.clone() })
    }

    fn region_of(&self, bucket: &str) -> Result<String, Failure> {
        if let Some(region) = self.bucket_regions.borrow().get(bucket) {
            return Ok(region.clone());
        }
        let locator = Client::new(
            self.model,
            &Globals { region: Some(self.location_region.clone()), ..self.globals.clone() },
        )?;
        let response = locator.call("get-bucket-location", Some(&json!({ "Bucket": bucket })))?;
        // An empty constraint means us-east-1, which is the one region S3 does not name.
        let region = response
            .get("LocationConstraint")
            .and_then(Value::as_str)
            .filter(|region| !region.is_empty())
            .unwrap_or("us-east-1")
            .to_string();
        self.bucket_regions.borrow_mut().insert(bucket.to_string(), region.clone());
        Ok(region)
    }

    /// `get-object`, returning the body's bytes and the object's user metadata.
    ///
    /// Raw rather than modelled: the body is gzip, which the modelled path would decode
    /// as UTF-8, and the signature travels in `x-amz-meta-*` headers.
    fn get_object(&self, bucket: &str, key: &str) -> Result<ObjectParts, Failure> {
        let client = self.client(bucket)?;
        let (op_id, op) = client
            .model
            .operation("get-object")
            .map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;
        let input_shape = client
            .model
            .operation_input(op)
            .map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;
        let response = client.call_operation_raw(
            op_id.name(),
            op,
            input_shape,
            Some(&json!({ "Bucket": bucket, "Key": key })),
        )?;
        Ok((response.bytes().to_vec(), response.headers().to_vec()))
    }
}

/// An object's bytes and its response headers.
type ObjectParts = (Vec<u8>, Vec<(String, String)>);

fn metadata<'h>(headers: &'h [(String, String)], name: &str) -> Option<&'h str> {
    let wanted = format!("x-amz-meta-{name}");
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(&wanted))
        .map(|(_, value)| value.as_str())
}

/// Load one digest: fetch, inflate, decode, and attach its signature.
fn fetch_digest(
    s3: &S3Clients<'_>,
    bucket: &str,
    key: &str,
) -> Result<(Value, Vec<u8>), DigestProblem> {
    let invalid_format =
        || DigestProblem::Invalid(format!("Digest file\ts3://{bucket}/{key}\tINVALID: invalid format"));
    let (body, headers) = s3.get_object(bucket, key).map_err(|failure| {
        if failure.service_error_code.as_deref() == Some("NoSuchKey") {
            DigestProblem::Missing
        } else {
            DigestProblem::Fatal(failure)
        }
    })?;
    let (inflated, _trailing) = gunzip(&body).ok_or_else(invalid_format)?;
    let mut digest: Value =
        serde_json::from_slice(&inflated).map_err(|_| invalid_format())?;

    let (Some(signature), Some(algorithm)) =
        (metadata(&headers, "signature"), metadata(&headers, "signature-algorithm"))
    else {
        return Err(DigestProblem::Invalid(format!(
            "Digest file\ts3://{bucket}/{key}\tINVALID: signature verification failed"
        )));
    };
    digest["_signature"] = Value::String(signature.to_string());
    digest["_signature_algorithm"] = Value::String(algorithm.to_string());

    if is_backfill_key(key) {
        match metadata(&headers, "backfill-generation-timestamp") {
            Some(stamp) => {
                digest["_backfill_generation_timestamp"] = Value::String(stamp.to_string())
            }
            None => return Err(invalid_format()),
        }
    }
    Ok((digest, inflated))
}

const REQUIRED_KEYS: [&str; 6] = [
    "digestPublicKeyFingerprint",
    "digestS3Bucket",
    "digestS3Object",
    "previousDigestSignature",
    "digestEndTime",
    "digestStartTime",
];

/// One run of the command: the clients, the counters and the output rules.
struct Validate<'a> {
    s3: S3Clients<'a>,
    cloudtrail: Client<'a>,
    naming: KeyNaming,
    bucket: String,
    prefix: Option<String>,
    start: i64,
    end: i64,
    verbose: bool,
    /// Digest keys found in the bucket, newest last, loaded once per (bucket, kind).
    listings: std::collections::BTreeMap<(String, bool), Vec<String>>,
    public_keys: std::collections::BTreeMap<String, String>,
    valid_digests: u32,
    invalid_digests: u32,
    valid_backfill: u32,
    invalid_backfill: u32,
    valid_logs: u32,
    invalid_logs: u32,
    /// Whether the last thing written already ended in a blank line. The reference tracks
    /// this so an error never runs into the line before it.
    last_was_double_space: bool,
    found_start: Option<i64>,
    found_end: Option<i64>,
}

impl<'a> Validate<'a> {
    fn write_status(&mut self, message: &str, is_error: bool) {
        if is_error {
            let _ = std::io::stdout().flush();
            if self.last_was_double_space {
                eprint!("{message}\n\n");
            } else {
                eprint!("\n{message}\n\n");
            }
            self.last_was_double_space = true;
        } else if self.verbose {
            self.last_was_double_space = false;
            println!("{message}");
        }
    }

    /// Every digest key for this trail in the window, oldest first.
    ///
    /// One listing serves both kinds: the standard and backfill digests are separated as
    /// the keys go by, because a second pass over S3 would cost another set of requests
    /// for data already in hand.
    fn listing(&mut self, bucket: &str, backfill: bool) -> Result<Vec<String>, Failure> {
        let cache_key = (bucket.to_string(), backfill);
        if let Some(keys) = self.listings.get(&cache_key) {
            return Ok(keys.clone());
        }
        let prefix = self.prefix.as_deref();
        let marker = self.naming.marker(self.start, prefix);
        let listing_prefix = self.naming.prefix(prefix);
        // The window is widened by an hour at the end, because a digest written just
        // after it can still cover log files delivered inside it.
        let target_start = format_date(self.start);
        let target_end = format_date(self.end + 3600);

        let client = self.s3.client(bucket)?;
        let mut standard = Vec::new();
        let mut backfills = Vec::new();
        let mut next_marker = marker;
        'listing: loop {
            let page = client.call(
                "list-objects",
                Some(&json!({ "Bucket": bucket, "Marker": next_marker, "Prefix": listing_prefix })),
            )?;
            let contents =
                page.get("Contents").and_then(Value::as_array).cloned().unwrap_or_default();
            if contents.is_empty() {
                break;
            }
            for entry in &contents {
                let Some(key) = entry.get("Key").and_then(Value::as_str) else { continue };
                if !self.naming.matches(key, prefix) {
                    continue;
                }
                // Keys sort lexicographically and so do these timestamps, which is what
                // makes stopping at the first one past the end correct.
                let date = extract_key_date(key);
                if date > target_end.as_str() {
                    break 'listing;
                }
                if date < target_start.as_str() {
                    continue;
                }
                if is_backfill_key(key) {
                    backfills.push(key.to_string());
                } else {
                    standard.push(key.to_string());
                }
            }
            if !page.get("IsTruncated").and_then(Value::as_bool).unwrap_or(false) {
                break;
            }
            next_marker = contents
                .last()
                .and_then(|entry| entry.get("Key"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if next_marker.is_empty() {
                break;
            }
        }
        self.listings.insert((bucket.to_string(), false), standard);
        self.listings.insert((bucket.to_string(), true), backfills);
        Ok(self.listings.get(&cache_key).cloned().unwrap_or_default())
    }

    /// Load the public keys CloudTrail published in a window, by fingerprint.
    fn load_public_keys(&mut self, start: i64, end: i64) -> Result<(), Failure> {
        let response = self.cloudtrail.call(
            "list-public-keys",
            Some(&json!({
                "StartTime": format_display_date(start),
                "EndTime": format_display_date(end),
            })),
        )?;
        let listed =
            response.get("PublicKeyList").and_then(Value::as_array).cloned().unwrap_or_default();
        if listed.is_empty() && self.public_keys.is_empty() {
            return Err(Failure::new(
                exit::GENERAL_ERROR,
                format!(
                    "No public keys found between {} and {}",
                    format_display_date(start),
                    format_display_date(end)
                ),
            ));
        }
        for key in listed {
            let text = |name: &str| key.get(name).and_then(Value::as_str).map(str::to_string);
            if let (Some(fingerprint), Some(value)) = (text("Fingerprint"), text("Value")) {
                self.public_keys.insert(fingerprint, value);
            }
        }
        Ok(())
    }

    /// Check one digest and hand back its parsed form.
    fn load_and_validate(
        &mut self,
        bucket: &str,
        key: &str,
        backfill: bool,
    ) -> Result<Value, DigestProblem> {
        let (digest, inflated) = fetch_digest(&self.s3, bucket, key)?;
        for required in REQUIRED_KEYS {
            if digest.get(required).is_none() {
                return Err(DigestProblem::Invalid(format!(
                    "Digest file\ts3://{bucket}/{key}\tINVALID: invalid format"
                )));
            }
        }
        // A digest names its own location, so a copy of it somewhere else is detectable
        // even though the signature would still verify.
        let text = |name: &str| digest.get(name).and_then(Value::as_str).unwrap_or_default();
        if text("digestS3Bucket") != bucket || text("digestS3Object") != key {
            return Err(DigestProblem::Invalid(format!(
                "Digest file\ts3://{bucket}/{key}\tINVALID: has been moved from its original location"
            )));
        }

        let fingerprint = text("digestPublicKeyFingerprint").to_string();
        if !self.public_keys.contains_key(&fingerprint) && backfill {
            // A backfill digest can be signed by a key from long after the period it
            // covers, so its own generation time decides where to look.
            if let Some(stamp) = digest
                .get("_backfill_generation_timestamp")
                .and_then(Value::as_str)
                .and_then(parse_date)
            {
                self.load_public_keys(stamp - 3600, stamp + 3600)
                    .map_err(DigestProblem::Fatal)?;
            }
        }
        let Some(public_key) = self.public_keys.get(&fingerprint).cloned() else {
            return Err(DigestProblem::Invalid(format!(
                "Digest file\ts3://{bucket}/{key}\tINVALID: public key not found in region {} \
                 for fingerprint {fingerprint}",
                self.naming.home_region
            )));
        };
        match verify_digest(&public_key, &digest, &inflated) {
            Ok(()) => Ok(digest),
            Err(reason) if reason == "key" => Err(DigestProblem::Invalid(format!(
                "Digest file\ts3://{bucket}/{key}\tINVALID: Unable to load PKCS #1 key with \
                 fingerprint {fingerprint}"
            ))),
            Err(_) => Err(DigestProblem::Invalid(format!(
                "Digest file\ts3://{bucket}/{key}\tINVALID: signature verification failed"
            ))),
        }
    }

    /// Walk one chain of digests backwards from the newest in range.
    fn traverse(&mut self, backfill: bool) -> Result<(), Failure> {
        let mut bucket = self.bucket.clone();
        let mut digests = self.listing(&bucket, backfill)?;
        if !backfill {
            // Two hours past the end, because a digest can be signed with a key that was
            // published after the window closed.
            self.load_public_keys(self.start, self.end + 2 * 3600)?;
        }

        let Some((mut key, _)) = pop_latest(&mut digests, None) else { return Ok(()) };
        let mut last_start = parse_date(extract_key_date(&key)).unwrap_or(self.end);

        while self.start <= last_start {
            match self.load_and_validate(&bucket, &key, backfill) {
                Ok(digest) => {
                    self.track_found_times(&digest);
                    if backfill {
                        self.valid_backfill += 1;
                    } else {
                        self.valid_digests += 1;
                    }
                    let label = if backfill { "(backfill) " } else { "" };
                    let text =
                        |name: &str| digest.get(name).and_then(Value::as_str).unwrap_or_default();
                    let message = format!(
                        "{label}Digest file\ts3://{}/{}\tvalid",
                        text("digestS3Bucket"),
                        text("digestS3Object")
                    );
                    self.write_status(&message, false);
                    last_start = digest
                        .get("digestStartTime")
                        .and_then(Value::as_str)
                        .and_then(parse_date)
                        .unwrap_or(last_start);

                    let logs =
                        digest.get("logFiles").and_then(Value::as_array).cloned().unwrap_or_default();
                    for log in &logs {
                        self.check_log(log)?;
                    }

                    let previous_bucket =
                        digest.get("previousDigestS3Bucket").and_then(Value::as_str);
                    let previous_key =
                        digest.get("previousDigestS3Object").and_then(Value::as_str);
                    match (previous_bucket, previous_key) {
                        (Some(previous_bucket), Some(previous_key)) => {
                            let next_key = previous_key.to_string();
                            if previous_bucket != bucket {
                                bucket = previous_bucket.to_string();
                                digests = self.listing(&bucket, backfill)?;
                            }
                            key = next_key;
                            continue;
                        }
                        // No link: the trail was off. Fall back to whatever S3 still has
                        // before this one, and say so — but only if there is one, since
                        // reaching the start of the chain is not a gap.
                        _ => {
                            let found = pop_latest(&mut digests, Some(&key));
                            if let Some((_, next_end)) = &found {
                                let label = if backfill { "(backfill) " } else { "" };
                                let message = format!(
                                    "{label}No log files were delivered by CloudTrail between {} and {}",
                                    format_display_date(*next_end),
                                    format_display_date(last_start)
                                );
                                self.write_status(&message, true);
                            }
                            match found {
                                Some((next_key, next_end)) => {
                                    key = next_key;
                                    last_start = next_end;
                                    continue;
                                }
                                None => return Ok(()),
                            }
                        }
                    }
                }
                Err(DigestProblem::Fatal(failure)) => return Err(failure),
                Err(problem) => {
                    let label = if backfill { "(backfill) " } else { "" };
                    let message = match &problem {
                        DigestProblem::Missing => {
                            format!("{label}Digest file\ts3://{bucket}/{key}\tINVALID: not found")
                        }
                        DigestProblem::Invalid(message) => format!("{label}{message}"),
                        DigestProblem::Fatal(_) => unreachable!("handled above"),
                    };
                    if backfill {
                        self.invalid_backfill += 1;
                    } else {
                        self.invalid_digests += 1;
                    }
                    self.write_status(&message, true);
                    match pop_latest(&mut digests, Some(&key)) {
                        Some((next_key, next_end)) => {
                            key = next_key;
                            last_start = next_end;
                        }
                        None => return Ok(()),
                    }
                }
            }
        }
        Ok(())
    }

    /// Download one log file and compare its SHA-256 with what the digest recorded.
    fn check_log(&mut self, log: &Value) -> Result<(), Failure> {
        use sha2::Digest as _;
        let text = |name: &str| log.get(name).and_then(Value::as_str).unwrap_or_default();
        let (bucket, key) = (text("s3Bucket").to_string(), text("s3Object").to_string());
        let expected = text("hashValue").to_string();

        let body = match self.s3.get_object(&bucket, &key) {
            Ok((body, _)) => body,
            Err(failure) if failure.service_error_code.as_deref() == Some("NoSuchKey") => {
                self.invalid_logs += 1;
                let message = format!("Log file\ts3://{bucket}/{key}\tINVALID: not found");
                self.write_status(&message, true);
                return Ok(());
            }
            Err(failure) => return Err(failure),
        };
        let Some((inflated, trailing)) = gunzip(&body) else {
            self.invalid_logs += 1;
            let message = format!("Log file\ts3://{bucket}/{key}\tINVALID: invalid format");
            self.write_status(&message, true);
            return Ok(());
        };
        if trailing {
            self.invalid_logs += 1;
            let message = format!(
                "Log file\ts3://{bucket}/{key}\tINVALID: unexpected data after end of \
                 compressed stream"
            );
            self.write_status(&message, true);
            return Ok(());
        }
        // The hash is of the *inflated* contents, so re-compressing a log with different
        // settings does not make it look tampered with.
        if hex(&sha2::Sha256::digest(&inflated)) != expected {
            self.invalid_logs += 1;
            let message =
                format!("Log file\ts3://{bucket}/{key}\tINVALID: hash value doesn't match");
            self.write_status(&message, true);
            return Ok(());
        }
        self.valid_logs += 1;
        let message = format!("Log file\ts3://{bucket}/{key}\tvalid");
        self.write_status(&message, false);
        Ok(())
    }

    /// The summary's range is what was actually covered, clamped to what was asked for.
    fn track_found_times(&mut self, digest: &Value) {
        let at = |name: &str| digest.get(name).and_then(Value::as_str).and_then(parse_date);
        if let Some(digest_start) = at("digestStartTime") {
            let earliest = digest_start.max(self.start);
            if self.found_start.is_none_or(|found| earliest < found) {
                self.found_start = Some(earliest);
            }
        }
        if let Some(digest_end) = at("digestEndTime") {
            let latest = digest_end.min(self.end);
            if self.found_end.is_none_or(|found| latest > found) {
                self.found_end = Some(latest);
            }
        }
    }

    fn write_summary(&mut self) {
        if !self.last_was_double_space {
            println!();
        }
        println!(
            "Results requested for {} to {}",
            format_display_date(self.start),
            format_display_date(self.end)
        );
        let valid = self.valid_digests + self.valid_backfill;
        let invalid = self.invalid_digests + self.invalid_backfill;
        if valid == 0 && invalid == 0 {
            println!("No digests found");
            return;
        }
        match (self.found_start, self.found_end) {
            (Some(start), Some(end)) => println!(
                "Results found for {} to {}:",
                format_display_date(start),
                format_display_date(end)
            ),
            _ => println!("No valid digests found in range"),
        }
        write_ratio(self.valid_digests, self.invalid_digests, "digest");
        write_ratio(self.valid_backfill, self.invalid_backfill, "backfill digest");
        write_ratio(self.valid_logs, self.invalid_logs, "log");
        println!();
    }
}

fn write_ratio(valid: u32, invalid: u32, name: &str) {
    let total = valid + invalid;
    if total == 0 {
        return;
    }
    print!("\n{valid}/{total} {name} files valid");
    if invalid > 0 {
        print!(", {invalid}/{total} {name} files INVALID");
    }
}

/// Take the newest digest key, or the newest one older than `before`.
///
/// Keys are consumed as they are visited, so the walk cannot loop: a chain that points
/// back at a digest already seen simply finds nothing left.
fn pop_latest(digests: &mut Vec<String>, before: Option<&str>) -> Option<(String, i64)> {
    match before {
        None => {
            let key = digests.pop()?;
            let date = parse_date(extract_key_date(&key))?;
            Some((key, date))
        }
        Some(before) => {
            let before_date = parse_date(extract_key_date(before))?;
            while let Some(key) = digests.pop() {
                if let Some(date) = parse_date(extract_key_date(&key)) {
                    if date < before_date {
                        return Some((key, date));
                    }
                }
            }
            None
        }
    }
}

pub fn run(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let args = crate::custom::take_args(
        parsed,
        &[
            "--trail-arn",
            "--start-time",
            "--end-time",
            "--s3-bucket",
            "--s3-prefix",
            "--account-id",
            "--verbose",
        ],
    )?;
    let value = |flag: &str| args.get(flag).copied().flatten();
    let missing: Vec<&str> = ["--trail-arn", "--start-time"]
        .into_iter()
        .filter(|flag| value(flag).is_none())
        .collect();
    if !missing.is_empty() {
        return Err(crate::custom::missing_required(&missing));
    }
    let trail_arn = value("--trail-arn").unwrap_or_default().to_string();
    let start = value("--start-time")
        .and_then(parse_date)
        .ok_or_else(|| param_error(&format!(
            "Unable to parse date value: {}",
            value("--start-time").unwrap_or_default()
        )))?;
    let end = match value("--end-time") {
        None => crate::now_unix(),
        Some(text) => parse_date(text)
            .ok_or_else(|| param_error(&format!("Unable to parse date value: {text}")))?,
    };
    if start > end {
        return Err(param_error(
            "Invalid time range specified: start-time must occur before end-time",
        ));
    }
    if !arn_is_valid(&trail_arn) {
        return Err(param_error(&format!("Invalid trail ARN provided: {trail_arn}")));
    }

    let region = crate::custom::resolve_region(globals)
        .ok_or_else(|| Failure::new(exit::CONFIGURATION, awsc_runtime::RuntimeError::NoRegion))?;
    let cloudtrail_model =
        crate::load_model("cloudtrail").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    // `--endpoint-url` reaches CloudTrail, as the reference passes it; below the flag
    // botocore still applies `AWS_ENDPOINT_URL_CLOUDTRAIL`.
    let cloudtrail = Client::new(
        &cloudtrail_model,
        &Globals {
            region: Some(region.clone()),
            endpoint_url: globals
                .endpoint_url
                .clone()
                .or_else(|| Globals::endpoint_from_environment("cloudtrail")),
            ..globals.clone()
        },
    )?;

    // The trail's home region and name come out of the ARN, not out of the API.
    let home_region = trail_arn.split(':').nth(3).unwrap_or_default().to_string();
    let trail_name = trail_arn.rsplit('/').next().unwrap_or_default().to_string();
    let mut account_id = value("--account-id").map(str::to_string);
    let mut organization_id = None;

    let (bucket, prefix) = match value("--s3-bucket") {
        Some(bucket) => (bucket.to_string(), value("--s3-prefix").map(str::to_string)),
        None => {
            // Only looked up when the bucket was not given, which is also the only path
            // that can discover the trail is an organization trail.
            let trails = cloudtrail.call("describe-trails", None)?;
            let trail = trails
                .get("trailList")
                .and_then(Value::as_array)
                .and_then(|list| {
                    list.iter().find(|trail| {
                        trail.get("TrailARN").and_then(Value::as_str) == Some(&trail_arn)
                    })
                })
                .cloned()
                .ok_or_else(|| {
                    Failure::new(
                        exit::GENERAL_ERROR,
                        format!("A trail could not be found for {trail_arn}"),
                    )
                })?;
            let text = |name: &str| trail.get(name).and_then(Value::as_str).map(str::to_string);
            if trail.get("IsOrganizationTrail").and_then(Value::as_bool).unwrap_or(false) {
                if account_id.is_none() {
                    return Err(param_error(
                        "Missing required parameter for organization trail: '--account-id'",
                    ));
                }
                let organizations_model = crate::load_model("organizations")
                    .map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
                let organizations = Client::new(
                    &organizations_model,
                    &Globals { region: Some(region.clone()), ..globals.for_service("organizations") },
                )?;
                let described = organizations.call("describe-organization", None)?;
                organization_id = described
                    .get("Organization")
                    .and_then(|organization| organization.get("Id"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            (
                text("S3BucketName").unwrap_or_default(),
                text("S3KeyPrefix").or_else(|| value("--s3-prefix").map(str::to_string)),
            )
        }
    };
    if account_id.is_none() {
        account_id = trail_arn.split(':').nth(4).map(str::to_string);
    }

    let s3_model =
        crate::load_model("s3api").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let mut validate = Validate {
        s3: S3Clients {
            model: &s3_model,
            globals: globals.for_service("s3"),
            location_region: region.clone(),
            bucket_regions: std::cell::RefCell::new(Default::default()),
        },
        cloudtrail,
        naming: KeyNaming {
            account_id: account_id.unwrap_or_default(),
            trail_name,
            home_region,
            // `--region` names the region whose digests are being validated; without one
            // it is the trail's own.
            source_region: globals
                .region
                .clone()
                .unwrap_or_else(|| trail_arn.split(':').nth(3).unwrap_or_default().to_string()),
            organization_id,
        },
        bucket,
        prefix: prefix.filter(|prefix| !prefix.is_empty()),
        start,
        end,
        verbose: args.contains_key("--verbose"),
        listings: Default::default(),
        public_keys: Default::default(),
        valid_digests: 0,
        invalid_digests: 0,
        valid_backfill: 0,
        invalid_backfill: 0,
        valid_logs: 0,
        invalid_logs: 0,
        last_was_double_space: true,
        found_start: None,
        found_end: None,
    };

    println!(
        "Validating log files for trail {trail_arn} between {} and {}\n",
        format_display_date(start),
        format_display_date(end)
    );
    validate.traverse(false)?;
    validate.traverse(true)?;
    validate.write_summary();

    let invalid = validate.invalid_digests + validate.invalid_backfill + validate.invalid_logs;
    Ok(exit::code(if invalid > 0 { 1 } else { exit::SUCCESS }))
}

fn param_error(message: &str) -> Failure {
    Failure::new(
        exit::PARAM_VALIDATION,
        awsc_runtime::RuntimeError::ParamValidation(message.to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verification against a signature **openssl** produced, not this codebase.
    ///
    /// It is the one test that can tell "our signer and our verifier agree with each
    /// other" from "our verifier agrees with the thing CloudTrail actually does".
    /// `scripts/extract-cloudtrail-digest-fixture.sh` regenerates it.
    #[test]
    fn an_openssl_signature_over_a_real_digest_verifies() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../tests/golden/cloudtrail-digest-signature.json"
        ))
        .expect("the fixture parses");
        let text = |name: &str| fixture[name].as_str().expect(name);
        let inflated = text("inflated_digest").as_bytes();
        let mut digest: Value = serde_json::from_slice(inflated).expect("the digest parses");
        digest["_signature"] = Value::String(text("signature").to_string());

        verify_digest(text("public_key"), &digest, inflated).expect("verifies");

        // One byte of the digest changed, and it no longer does.
        let mut tampered = digest.clone();
        tampered["digestEndTime"] = Value::String("2026-09-01T12:00:01Z".to_string());
        assert!(verify_digest(text("public_key"), &tampered, inflated).is_err());

        // The same digest re-signed under a different previous signature must not verify
        // either — that link is what makes the chain a chain.
        let mut relinked = digest.clone();
        relinked["previousDigestSignature"] = Value::String("000000".to_string());
        assert!(verify_digest(text("public_key"), &relinked, inflated).is_err());

        // A key that is not a PKCS#1 RSAPublicKey is reported as a key problem, not as a
        // bad signature, because the two mean different things to a reader.
        assert_eq!(
            verify_digest("bm90IGEga2V5", &digest, inflated).expect_err("refuses"),
            "key"
        );
    }

    /// Trailing bytes after the gzip member are what an appended log file looks like, so
    /// the decoder has to notice them rather than stopping quietly.
    #[test]
    fn gunzip_reports_data_after_the_stream() {
        use std::io::Write;
        let mut encoder =
            flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(b"log contents").expect("writes");
        let clean = encoder.finish().expect("finishes");

        let (inflated, trailing) = gunzip(&clean).expect("inflates");
        assert_eq!(inflated, b"log contents");
        assert!(!trailing);

        let mut appended = clean.clone();
        appended.extend_from_slice(b"extra");
        let (inflated, trailing) = gunzip(&appended).expect("inflates");
        assert_eq!(inflated, b"log contents", "the member still decodes");
        assert!(trailing, "the appended bytes must be noticed");

        assert!(gunzip(b"not gzip at all").is_none());
    }

    use super::*;

    #[test]
    fn dates_round_trip_through_both_formats() {
        let unix = parse_date("2015-01-08T05:21:42Z").expect("parses");
        assert_eq!(format_date(unix), "20150108T052142Z");
        assert_eq!(format_display_date(unix), "2015-01-08T05:21:42Z");
        // The compact form a digest key carries parses to the same instant.
        assert_eq!(parse_date("20150108T052142Z"), Some(unix));
        // A bare date is midnight.
        assert_eq!(format_display_date(parse_date("2015-01-08").expect("parses")),
                   "2015-01-08T00:00:00Z");
        assert_eq!(parse_date("not a date"), None);
        assert_eq!(parse_date("2015-13-08T05:21:42Z"), None);
    }

    /// The date is sliced out by position, and the two key shapes put it in different
    /// places — an off-by-one here silently compares the wrong substring.
    #[test]
    fn the_date_comes_out_of_both_key_shapes() {
        let standard = "AWSLogs/123456789012/CloudTrail-Digest/us-east-1/2015/01/08/\
                        123456789012_CloudTrail-Digest_us-east-1_my-trail_us-east-1_\
                        20150108T052142Z.json.gz";
        assert_eq!(extract_key_date(standard), "20150108T052142Z");
        let backfill = format!("{}_backfill.json.gz", standard.trim_end_matches(".json.gz"));
        assert!(is_backfill_key(&backfill));
        assert_eq!(extract_key_date(&backfill), "20150108T052142Z");
        assert!(!is_backfill_key(standard));
    }

    #[test]
    fn a_trail_arn_must_have_an_account_and_a_trail_name() {
        assert!(arn_is_valid("arn:aws:cloudtrail:us-east-1:123456789012:trail/my-trail"));
        assert!(arn_is_valid("arn:aws-cn:cloudtrail:cn-north-1:123456789012:trail/t"));
        assert!(!arn_is_valid("arn:aws:cloudtrail:us-east-1:12345:trail/my-trail"));
        assert!(!arn_is_valid("arn:aws:s3:us-east-1:123456789012:trail/my-trail"));
        assert!(!arn_is_valid("arn:aws:cloudtrail:us-east-1:123456789012:trail/"));
        assert!(!arn_is_valid("nonsense"));
    }

    fn naming() -> KeyNaming {
        KeyNaming {
            account_id: "123456789012".to_string(),
            trail_name: "my-trail".to_string(),
            home_region: "us-east-1".to_string(),
            source_region: "us-west-2".to_string(),
            organization_id: None,
        }
    }

    /// The marker is a key that need not exist, one minute before the start so the range
    /// stays inclusive.
    #[test]
    fn the_list_marker_is_a_minute_before_the_start() {
        let start = parse_date("2015-01-08T05:21:42Z").expect("parses");
        assert_eq!(
            naming().marker(start, None),
            "AWSLogs/123456789012/CloudTrail-Digest/us-west-2/2015/01/08/\
             123456789012_CloudTrail-Digest_us-west-2_my-trail_us-east-1_20150108T052042Z.json.gz"
        );
        assert!(naming().marker(start, Some("logs")).starts_with("logs/AWSLogs/"));
    }

    #[test]
    fn the_listing_prefix_scopes_to_the_trails_region() {
        assert_eq!(
            naming().prefix(None),
            "AWSLogs/123456789012/CloudTrail-Digest/us-west-2"
        );
        let mut org = naming();
        org.organization_id = Some("o-abc".to_string());
        assert_eq!(org.prefix(Some("logs")), "logs/AWSLogs/o-abc/123456789012/CloudTrail-Digest/us-west-2");
    }

    /// Keys from another trail, another account or another region must not be walked —
    /// they would fail signature verification and be reported as tampering.
    #[test]
    fn only_this_trails_keys_match() {
        let naming = naming();
        let good = "AWSLogs/123456789012/CloudTrail-Digest/us-west-2/2015/01/08/\
                    123456789012_CloudTrail-Digest_us-west-2_my-trail_us-east-1_20150108T052142Z.json.gz";
        assert!(naming.matches(good, None));
        assert!(naming.matches(&format!("{good}").replace(".json.gz", "_backfill.json.gz"), None));
        // Another trail in the same bucket.
        assert!(!naming.matches(&good.replace("my-trail", "other-trail"), None));
        // Another account.
        assert!(!naming.matches(&good.replace("123456789012", "210987654321"), None));
        // A log file rather than a digest.
        assert!(!naming.matches(
            "AWSLogs/123456789012/CloudTrail/us-west-2/2015/01/08/x.json.gz",
            None
        ));
        // The prefix has to be there when one is configured, and gone when it is not.
        assert!(!naming.matches(good, Some("logs")));
        assert!(naming.matches(&format!("logs/{good}"), Some("logs")));
    }

    /// A trail name containing a regex metacharacter still matches. The reference escapes
    /// it; matching structurally means there is nothing to escape.
    #[test]
    fn a_trail_name_with_metacharacters_matches_literally() {
        let mut naming = naming();
        naming.trail_name = "my.trail+v2".to_string();
        let key = "AWSLogs/123456789012/CloudTrail-Digest/us-west-2/2015/01/08/\
                   123456789012_CloudTrail-Digest_us-west-2_my.trail+v2_us-east-1_20150108T052142Z.json.gz";
        assert!(naming.matches(key, None));
        // `.` must not match an arbitrary character.
        assert!(!naming.matches(&key.replace("my.trail", "myXtrail"), None));
    }

    /// The signed string is four lines, and the last is the *previous* digest's
    /// signature — which is what chains them. A first digest uses the literal `null`.
    #[test]
    fn the_signed_string_ends_with_the_previous_signature() {
        let digest = json!({
            "digestEndTime": "2015-01-08T06:00:00Z",
            "digestS3Bucket": "my-bucket",
            "digestS3Object": "path/to/digest.json.gz",
            "previousDigestSignature": "abc123",
        });
        let signed = string_to_sign(&digest, b"inflated contents");
        let lines: Vec<&str> = signed.split('\n').collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "2015-01-08T06:00:00Z");
        assert_eq!(lines[1], "my-bucket/path/to/digest.json.gz");
        assert_eq!(lines[2].len(), 64, "a hex sha256");
        assert_eq!(lines[3], "abc123");

        let first = json!({
            "digestEndTime": "2015-01-08T06:00:00Z",
            "digestS3Bucket": "my-bucket",
            "digestS3Object": "path/to/digest.json.gz",
            "previousDigestSignature": Value::Null,
        });
        assert!(string_to_sign(&first, b"x").ends_with("\nnull"));
    }
}
