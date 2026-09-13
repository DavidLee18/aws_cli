//! `aws cloudfront sign`: a signed CloudFront URL for private content.
//!
//! A port of `customizations/cloudfront.py:SignCommand` plus the `CloudFrontSigner` it
//! calls (`botocore/signers.py`). Nothing here talks to AWS — the whole command is local
//! arithmetic over a private key, which is why it needs no credentials and no region.
//!
//! Three details decide whether the URL CloudFront receives actually validates:
//!
//! - **The signature is RSA PKCS#1 v1.5 over SHA-1.** Not SHA-256. CloudFront specifies
//!   SHA-1 and rejects anything else, which is also why this is the one place in the
//!   tree that cannot use the aws-lc-rs already linked in under rustls: that library
//!   verifies PKCS#1-SHA1 but its *signing* encodings stop at SHA-256.
//! - **The base64 is CloudFront's own alphabet**, not URL-safe base64 and not standard:
//!   `+` becomes `-`, `=` becomes `_`, and `/` becomes `~`. Standard base64 produces a
//!   URL that looks right and fails to authenticate.
//! - **The policy is compact JSON with a fixed key order** — `DateLessThan`, then
//!   `IpAddress`, then `DateGreaterThan`. botocore builds it from an `OrderedDict` for
//!   exactly this reason; a different order is a different signed document.
//!
//! The canned form (only `--date-less-than`) puts `Expires=` in the URL and signs the
//! policy it did not send; the custom form sends `Policy=` base64-encoded. Both then
//! carry `Signature=` and `Key-Pair-Id=`.

use crate::args::Parsed;
use crate::exit;
use crate::Failure;
use std::process::ExitCode;

/// The flags `sign` accepts, in the reference's order.
pub const FLAGS: &[&str] = &[
    "--url",
    "--key-pair-id",
    "--private-key",
    "--date-less-than",
    "--date-greater-than",
    "--ip-address",
];

const REQUIRED: &[&str] = &["--url", "--key-pair-id", "--private-key", "--date-less-than"];

pub fn sign(parsed: &Parsed) -> Result<ExitCode, Failure> {
    let args = crate::custom::take_args(parsed, FLAGS)?;

    let missing: Vec<&str> =
        REQUIRED.iter().copied().filter(|flag| !args.contains_key(flag)).collect();
    if !missing.is_empty() {
        return Err(crate::custom::missing_required(&missing));
    }
    let value = |flag: &str| args.get(flag).copied().flatten().unwrap_or_default();

    let url = value("--url");
    let key_pair_id = value("--key-pair-id");
    // `--private-key` takes the default paramfile treatment, so `file://key.pem` is the
    // documented spelling; `--url` is declared `no_paramfile` and is never expanded.
    let private_key = crate::args::expand_paramfile(value("--private-key"))
        .map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;

    let date_less_than = parse_date(value("--date-less-than"))?;
    let date_greater_than =
        args.get("--date-greater-than").copied().flatten().map(parse_date).transpose()?;
    let ip_address = args.get("--ip-address").copied().flatten();

    let signer = Signer::new(&private_key)?;
    let signed = signer.presigned_url(
        url,
        key_pair_id,
        date_less_than,
        date_greater_than,
        ip_address,
    )?;
    // `sys.stdout.write`, so no trailing newline: the URL is meant to be captured.
    print!("{signed}");
    Ok(exit::code(exit::SUCCESS))
}

struct Signer {
    key: rsa::RsaPrivateKey,
}

impl Signer {
    fn new(pem: &str) -> Result<Self, Failure> {
        use rsa::pkcs1::DecodeRsaPrivateKey;
        use rsa::pkcs8::DecodePrivateKey;

        // Both spellings are in the wild: `BEGIN RSA PRIVATE KEY` (PKCS#1) is what
        // `openssl genrsa` writes by default, `BEGIN PRIVATE KEY` (PKCS#8) is what
        // newer tooling and the CloudFront console produce.
        let key = rsa::RsaPrivateKey::from_pkcs1_pem(pem)
            .or_else(|_| rsa::RsaPrivateKey::from_pkcs8_pem(pem))
            .map_err(|_| {
                Failure::new(
                    exit::GENERAL_ERROR,
                    "the private key could not be read: expected a PEM-encoded RSA private \
                     key, as written by `openssl genrsa`. Pass it as --private-key \
                     file://path/to/private-key.pem",
                )
            })?;
        Ok(Signer { key })
    }

    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, Failure> {
        use sha1::Digest;

        // CloudFront's algorithm: the SHA-1 digest, PKCS#1 v1.5 padded.
        let digest = sha1::Sha1::digest(message);
        self.key
            .sign(rsa::Pkcs1v15Sign::new::<sha1::Sha1>(), &digest)
            .map_err(|e| Failure::new(exit::GENERAL_ERROR, format!("could not sign the policy: {e}")))
    }

    fn presigned_url(
        &self,
        url: &str,
        key_pair_id: &str,
        date_less_than: i64,
        date_greater_than: Option<i64>,
        ip_address: Option<&str>,
    ) -> Result<String, Failure> {
        // A canned policy is still built and signed when only an expiry is given — it is
        // simply not sent, because CloudFront can reconstruct it from `Expires`.
        let custom = date_greater_than.is_some() || ip_address.is_some();
        let policy = build_policy(url, date_less_than, date_greater_than, ip_address);
        let signature = self.sign(policy.as_bytes())?;

        let mut params = Vec::with_capacity(3);
        if custom {
            params.push(format!("Policy={}", url_b64(policy.as_bytes())));
        } else {
            params.push(format!("Expires={date_less_than}"));
        }
        params.push(format!("Signature={}", url_b64(&signature)));
        params.push(format!("Key-Pair-Id={key_pair_id}"));

        let separator = if url.contains('?') { '&' } else { '?' };
        Ok(format!("{url}{separator}{}", params.join("&")))
    }
}

/// The policy document, compact and in botocore's fixed key order.
fn build_policy(
    resource: &str,
    date_less_than: i64,
    date_greater_than: Option<i64>,
    ip_address: Option<&str>,
) -> String {
    let mut condition = format!(r#""DateLessThan":{{"AWS:EpochTime":{date_less_than}}}"#);
    if let Some(ip) = ip_address.filter(|ip| !ip.is_empty()) {
        // A bare address is widened to a /32 host route, as the reference does — without
        // it CloudFront rejects the policy rather than treating it as a single host.
        let cidr = if ip.contains('/') { ip.to_string() } else { format!("{ip}/32") };
        condition.push_str(&format!(r#","IpAddress":{{"AWS:SourceIp":"{cidr}"}}"#));
    }
    if let Some(moment) = date_greater_than {
        condition.push_str(&format!(r#","DateGreaterThan":{{"AWS:EpochTime":{moment}}}"#));
    }
    // `json.dumps(..., separators=(',', ':'))`: no spaces anywhere.
    format!(
        r#"{{"Statement":[{{"Resource":{},"Condition":{{{condition}}}}}]}}"#,
        serde_json::Value::String(resource.to_string())
    )
}

/// base64 in CloudFront's alphabet.
fn url_b64(bytes: &[u8]) -> String {
    awsc_protocol::shapes::base64_encode(bytes)
        .replace('+', "-")
        .replace('=', "_")
        .replace('/', "~")
}

/// `--date-less-than` / `--date-greater-than`, as unix seconds.
///
/// The reference hands the string to `parse_to_aware_datetime`, which tries `int()`
/// **first** — so `20261231` is epoch time in 1970, not New Year's Eve 2026. The help
/// text warns about it in capitals, and reproducing the surprise is the point: a user
/// who wrote `YYYYMMDD` gets the same URL from both CLIs rather than two different
/// wrong answers.
fn parse_date(raw: &str) -> Result<i64, Failure> {
    let text = raw.trim();
    if let Ok(seconds) = text.parse::<i64>() {
        return Ok(seconds);
    }
    if let Ok(seconds) = text.parse::<f64>() {
        return Ok(seconds as i64);
    }
    parse_iso8601(text).ok_or_else(|| {
        Failure::new(
            exit::GENERAL_ERROR,
            format!(
                "Unknown string format: {text}\n\nSupported formats include: YYYY-MM-DD \
                 (0AM UTC of that day), YYYY-MM-DDThh:mm:ss (UTC unless an offset is \
                 given), YYYY-MM-DDThh:mm:ss+hh:mm, or epoch seconds."
            ),
        )
    })
}

/// A date, or a date and time with an optional offset. No offset means UTC — which is
/// botocore's choice and not the local time a reader might expect.
fn parse_iso8601(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        let text = value.get(range)?;
        text.bytes().all(|b| b.is_ascii_digit()).then(|| text.parse().ok())?
    };
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let days = awsc_protocol::shapes::days_from_civil(year, month, day);
    if bytes.len() == 10 {
        return Some(days * 86_400);
    }
    if !matches!(bytes[10], b'T' | b't' | b' ') || bytes.len() < 19 {
        return None;
    }
    if bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    let rest = &value[19..];
    // Fractional seconds are accepted and dropped: the policy carries whole seconds.
    let rest = match rest.strip_prefix('.') {
        Some(after) => &after[after.bytes().take_while(u8::is_ascii_digit).count()..],
        None => rest,
    };
    let offset = match rest {
        "" | "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let body = &rest[1..];
            let (hours, minutes) = match body.split_once(':') {
                Some((h, m)) => (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?),
                None if body.len() == 4 => {
                    (body[0..2].parse::<i64>().ok()?, body[2..4].parse::<i64>().ok()?)
                }
                None if body.len() == 2 => (body.parse::<i64>().ok()?, 0),
                _ => return None,
            };
            sign * (hours * 3600 + minutes * 60)
        }
    };
    Some(days * 86_400 + hour * 3600 + minute * 60 + second - offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_date_alone_is_midnight_utc() {
        assert_eq!(parse_iso8601("2020-01-01"), Some(1_577_836_800));
    }

    #[test]
    fn a_time_without_an_offset_is_utc() {
        assert_eq!(parse_iso8601("2020-01-01T10:00:00"), Some(1_577_872_800));
        assert_eq!(parse_iso8601("2020-01-01T10:00:00Z"), Some(1_577_872_800));
        assert_eq!(parse_iso8601("2020-01-01 10:00:00"), Some(1_577_872_800));
    }

    #[test]
    fn an_offset_moves_the_instant() {
        assert_eq!(parse_iso8601("2020-01-01T10:00:00+02:00"), Some(1_577_865_600));
        assert_eq!(parse_iso8601("2020-01-01T10:00:00-0200"), Some(1_577_880_000));
    }

    #[test]
    fn fractional_seconds_are_dropped() {
        assert_eq!(parse_iso8601("2020-01-01T10:00:00.123456Z"), Some(1_577_872_800));
    }

    /// The documented trap: `int()` is tried first, so a compact date is epoch seconds.
    #[test]
    fn a_compact_date_is_read_as_epoch_time() {
        assert_eq!(parse_date("20261231").expect("parses"), 20_261_231);
        assert_eq!(parse_date("0").expect("parses"), 0);
    }

    #[test]
    fn a_canned_policy_has_only_the_expiry() {
        let policy = build_policy("https://example.com/f.txt", 1_577_836_800, None, None);
        assert_eq!(
            policy,
            r#"{"Statement":[{"Resource":"https://example.com/f.txt","Condition":{"DateLessThan":{"AWS:EpochTime":1577836800}}}]}"#
        );
    }

    /// The order is `DateLessThan`, `IpAddress`, `DateGreaterThan` — botocore's, not the
    /// order the arguments are declared in. A different order signs a different document.
    #[test]
    fn a_custom_policy_keeps_botocores_key_order() {
        let policy =
            build_policy("https://e.com/f", 200, Some(100), Some("192.0.2.1"));
        assert_eq!(
            policy,
            r#"{"Statement":[{"Resource":"https://e.com/f","Condition":{"DateLessThan":{"AWS:EpochTime":200},"IpAddress":{"AWS:SourceIp":"192.0.2.1/32"},"DateGreaterThan":{"AWS:EpochTime":100}}}]}"#
        );
    }

    #[test]
    fn a_cidr_is_left_alone() {
        let policy = build_policy("u", 1, None, Some("10.0.0.0/24"));
        assert!(policy.contains(r#""AWS:SourceIp":"10.0.0.0/24""#));
    }

    /// A resource containing a quote or a backslash has to stay valid JSON.
    #[test]
    fn the_resource_is_json_escaped() {
        let policy = build_policy(r#"https://e.com/a"b"#, 1, None, None);
        assert!(policy.contains(r#""Resource":"https://e.com/a\"b""#));
        serde_json::from_str::<serde_json::Value>(&policy).expect("valid JSON");
    }

    /// CloudFront's alphabet, which is neither standard nor URL-safe base64.
    #[test]
    fn base64_uses_cloudfronts_substitutions() {
        // 0xfb 0xff 0xfe is `+//+` in standard base64: `+` becomes `-` and `/` becomes
        // `~`. A single byte pads with `=`, which becomes `_`.
        assert_eq!(url_b64(&[0xfb, 0xff, 0xfe]), "-~~-");
        assert_eq!(url_b64(&[0x00]), "AA__");
        assert!(!url_b64(&[0xff, 0xff, 0xff]).contains(['+', '/', '=']));
    }
}
