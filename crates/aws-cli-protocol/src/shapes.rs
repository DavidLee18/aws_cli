//! Cross-protocol value handling: timestamps, blobs, and member naming.
//!
//! These differ per protocol *and* per location — a timestamp in a restJson1 body is a
//! unix epoch number, but the same value in a header is RFC 822 — so the rules live here
//! rather than being scattered through each serializer.

use aws_cli_model::shape::Member;

/// The timestamp encodings AWS protocols use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimestampFormat {
    /// `2026-08-13T21:48:16Z`
    DateTime,
    /// `Wed, 13 Aug 2026 21:48:16 GMT`
    HttpDate,
    /// Seconds since the epoch, as a number.
    EpochSeconds,
}

/// Where a value sits in the request, which can change its encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    Body,
    Header,
    Query,
    Label,
}

impl TimestampFormat {
    /// The default for a protocol and location, before any `timestampFormat` override.
    ///
    /// The location matters: HTTP headers always carry `http-date` regardless of the
    /// protocol's body default, and query/label positions always use `date-time`.
    pub fn default_for(protocol: Protocol, location: Location) -> TimestampFormat {
        match location {
            Location::Header => TimestampFormat::HttpDate,
            Location::Query | Location::Label => TimestampFormat::DateTime,
            Location::Body => match protocol {
                Protocol::AwsJson1_0 | Protocol::AwsJson1_1 | Protocol::RestJson1 => {
                    TimestampFormat::EpochSeconds
                }
                // The XML/query family, plus anything unrecognised: `date-time` is the
                // Smithy-wide default, so it is the right fallback for a protocol we do
                // not implement rather than a panic.
                Protocol::RestXml
                | Protocol::AwsQuery
                | Protocol::Ec2Query
                | Protocol::Rpcv2Cbor
                | Protocol::Unknown => TimestampFormat::DateTime,
            },
        }
    }

    /// Apply a member's `smithy.api#timestampFormat` override, if it carries one.
    pub fn resolve(protocol: Protocol, location: Location, member: &Member) -> TimestampFormat {
        member
            .traits
            .get("smithy.api#timestampFormat")
            .and_then(|v| v.as_str())
            .and_then(Self::from_trait)
            .unwrap_or_else(|| Self::default_for(protocol, location))
    }

    pub fn from_trait(name: &str) -> Option<TimestampFormat> {
        Some(match name {
            "date-time" => TimestampFormat::DateTime,
            "http-date" => TimestampFormat::HttpDate,
            "epoch-seconds" => TimestampFormat::EpochSeconds,
            _ => return None,
        })
    }

    /// Render a unix timestamp.
    pub fn format(self, unix_seconds: i64) -> String {
        match self {
            TimestampFormat::EpochSeconds => unix_seconds.to_string(),
            TimestampFormat::DateTime => {
                let (y, mo, d, h, mi, s) = civil(unix_seconds);
                format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
            }
            TimestampFormat::HttpDate => {
                const DAYS: [&str; 7] =
                    ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
                const MONTHS: [&str; 12] = [
                    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct",
                    "Nov", "Dec",
                ];
                let (y, mo, d, h, mi, s) = civil(unix_seconds);
                // 1970-01-01 was a Thursday, which is index 3 in a Monday-first table.
                let dow = (unix_seconds.div_euclid(86_400) + 3).rem_euclid(7) as usize;
                format!(
                    "{}, {d:02} {} {y:04} {h:02}:{mi:02}:{s:02} GMT",
                    DAYS[dow],
                    MONTHS[(mo - 1) as usize]
                )
            }
        }
    }
}

/// The wire protocols, re-exported here so this module does not depend on the model
/// crate's enum ordering.
pub use aws_cli_model::Protocol;

/// Render a timestamp the way the CLI *prints* it, which is not any of the wire formats.
///
/// The reference parses every timestamp into a timezone-aware `datetime` and prints
/// `.isoformat()`, giving a `+00:00` offset rather than a `Z` suffix — so S3's
/// `2026-07-29T05:24:54.000Z` on the wire is printed as `2026-07-29T05:24:54+00:00`.
/// (`cli_timestamp_format = iso8601` is the default; `wire` would print the raw value.)
pub fn format_cli_output(unix_seconds: i64) -> String {
    let (y, mo, d, h, mi, s) = civil(unix_seconds);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}+00:00")
}

/// Split a unix timestamp into civil date-time components.
fn civil(unix: i64) -> (i64, i64, i64, i64, i64, i64) {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
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
    (y, m, d, secs / 3600, (secs % 3600) / 60, secs % 60)
}

/// Parse the timestamp forms AWS returns, into unix seconds.
pub fn parse_timestamp(raw: &str) -> Option<i64> {
    let trimmed = raw.trim();
    // Epoch seconds, possibly fractional.
    if let Ok(n) = trimmed.parse::<f64>() {
        return Some(n as i64);
    }
    // `date-time` (ISO 8601).
    if trimmed.len() >= 19 && trimmed.as_bytes()[4] == b'-' {
        let num = |r: std::ops::Range<usize>| trimmed.get(r)?.parse::<i64>().ok();
        let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
        let (h, mi, s) = (num(11..13)?, num(14..16)?, num(17..19)?);
        return Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + s);
    }
    None
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Standard base64, for blob members.
pub fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

pub fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut buffer = 0u32;
    let mut bits = 0u32;
    let mut out = Vec::new();
    for c in input.bytes().filter(|b| !b.is_ascii_whitespace()) {
        if c == b'=' {
            break;
        }
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

/// The name a member takes on the wire in a JSON protocol.
pub fn json_name<'a>(member_name: &'a str, member: &'a Member) -> &'a str {
    member.traits.get("smithy.api#jsonName").and_then(|v| v.as_str()).unwrap_or(member_name)
}

/// The name a member takes in XML.
pub fn xml_name<'a>(member_name: &'a str, member: &'a Member) -> &'a str {
    member.traits.get("smithy.api#xmlName").and_then(|v| v.as_str()).unwrap_or(member_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: i64 = 1_786_657_696; // 2026-08-13T21:48:16Z, a Thursday

    #[test]
    fn formats_each_timestamp_style() {
        assert_eq!(TimestampFormat::EpochSeconds.format(T), "1786657696");
        assert_eq!(TimestampFormat::DateTime.format(T), "2026-08-13T21:48:16Z");
        assert_eq!(TimestampFormat::HttpDate.format(T), "Thu, 13 Aug 2026 21:48:16 GMT");
    }

    #[test]
    fn http_date_weekday_is_correct_across_the_week() {
        // 1970-01-01 was a Thursday; check the next few days line up.
        assert!(TimestampFormat::HttpDate.format(0).starts_with("Thu, 01 Jan 1970"));
        assert!(TimestampFormat::HttpDate.format(86_400).starts_with("Fri, 02 Jan 1970"));
        assert!(TimestampFormat::HttpDate.format(4 * 86_400).starts_with("Mon, 05 Jan 1970"));
    }

    /// The location matters as much as the protocol: a restJson1 body timestamp is an
    /// epoch number, but the same value in a header is an HTTP date.
    #[test]
    fn defaults_depend_on_protocol_and_location() {
        use Location::*;
        use Protocol::*;
        assert_eq!(
            TimestampFormat::default_for(RestJson1, Body),
            TimestampFormat::EpochSeconds
        );
        assert_eq!(TimestampFormat::default_for(RestJson1, Header), TimestampFormat::HttpDate);
        assert_eq!(TimestampFormat::default_for(RestJson1, Query), TimestampFormat::DateTime);
        assert_eq!(TimestampFormat::default_for(RestXml, Body), TimestampFormat::DateTime);
        assert_eq!(TimestampFormat::default_for(AwsJson1_1, Body), TimestampFormat::EpochSeconds);
        assert_eq!(TimestampFormat::default_for(AwsQuery, Body), TimestampFormat::DateTime);
    }

    #[test]
    fn parses_the_forms_aws_returns() {
        assert_eq!(parse_timestamp("1786657696"), Some(T));
        assert_eq!(parse_timestamp("1786657696.25"), Some(T));
        assert_eq!(parse_timestamp("2026-08-13T21:48:16Z"), Some(T));
        assert_eq!(parse_timestamp("2026-08-13T21:48:16.123Z"), Some(T));
        assert_eq!(parse_timestamp("nonsense"), None);
    }

    #[test]
    fn base64_round_trips_including_padding() {
        for input in ["", "f", "fo", "foo", "foob", "fooba", "foobar"] {
            let encoded = base64_encode(input.as_bytes());
            assert_eq!(base64_decode(&encoded).unwrap(), input.as_bytes(), "{input}");
        }
        // Known vectors from RFC 4648.
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_rejects_invalid_characters() {
        assert!(base64_decode("!!!!").is_none());
    }
}
