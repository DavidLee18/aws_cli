//! The endpoint-rules standard library.
//!
//! Every function is total: invalid input yields [`Value::None`] rather than an error,
//! because the rules language uses "not set" as its failure signal and conditions simply
//! fall through to the next rule.

use super::partitions::Partitions;
use super::value::Value;

/// Evaluate a function by name. Unknown names return `None`, which makes the enclosing
/// condition fail rather than aborting the whole evaluation — the same as the reference
/// treating an unsupported rule branch as non-matching.
pub fn call(name: &str, args: &[Value], partitions: &Partitions) -> Value {
    match name {
        "isSet" => Value::Bool(arg(args, 0).is_set()),
        "not" => match arg(args, 0) {
            Value::None => Value::Bool(true),
            v => Value::Bool(!v.is_truthy()),
        },
        "booleanEquals" => match (arg(args, 0).as_bool(), arg(args, 1).as_bool()) {
            (Some(a), Some(b)) => Value::Bool(a == b),
            _ => Value::Bool(false),
        },
        "stringEquals" => match (arg(args, 0), arg(args, 1)) {
            (Value::String(a), Value::String(b)) => Value::Bool(a == b),
            _ => Value::Bool(false),
        },
        "getAttr" => match (arg(args, 0), arg(args, 1)) {
            (v, Value::String(path)) => v.get_path(&path),
            _ => Value::None,
        },
        "substring" => substring(args),
        "uriEncode" => match arg(args, 0) {
            Value::String(s) => Value::String(uri_encode(&s)),
            _ => Value::None,
        },
        "parseURL" => parse_url(&arg(args, 0)),
        "isValidHostLabel" => is_valid_host_label(args),
        "aws.partition" => match arg(args, 0) {
            Value::String(region) => partitions.resolve(&region),
            _ => Value::None,
        },
        "aws.parseArn" => parse_arn(&arg(args, 0)),
        "aws.isVirtualHostableS3Bucket" => is_virtual_hostable_s3_bucket(args),
        _ => Value::None,
    }
}

fn arg(args: &[Value], i: usize) -> Value {
    args.get(i).cloned().unwrap_or(Value::None)
}

/// `substring(string, start, stop, reverse)`.
///
/// Defined over ASCII only: any non-ASCII input yields `None`, matching the spec (index
/// arithmetic would otherwise be ambiguous over UTF-8).
fn substring(args: &[Value]) -> Value {
    let Value::String(s) = arg(args, 0) else { return Value::None };
    let (Value::Int(start), Value::Int(stop)) = (arg(args, 1), arg(args, 2)) else {
        return Value::None;
    };
    let reverse = arg(args, 3).as_bool().unwrap_or(false);

    if !s.is_ascii() {
        return Value::None;
    }
    let (start, stop) = (start as usize, stop as usize);
    if start >= stop || s.len() < stop {
        return Value::None;
    }
    let bytes = s.as_bytes();
    let slice = if reverse {
        let end = bytes.len() - start;
        let begin = bytes.len() - stop;
        &bytes[begin..end]
    } else {
        &bytes[start..stop]
    };
    Value::String(String::from_utf8_lossy(slice).into_owned())
}

/// RFC 3986 percent-encoding of everything outside the unreserved set.
fn uri_encode(s: &str) -> String {
    const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.~";
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        if UNRESERVED.contains(b) {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `parseURL(url)` -> record with scheme, authority, path, normalizedPath, isIp.
fn parse_url(value: &Value) -> Value {
    let Some(url) = value.as_str() else { return Value::None };

    let Some((scheme, rest)) = url.split_once("://") else { return Value::None };
    if scheme != "http" && scheme != "https" {
        return Value::None;
    }
    // A query or fragment makes the URL invalid for endpoint purposes.
    if rest.contains('?') || rest.contains('#') {
        return Value::None;
    }

    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return Value::None;
    }

    // `isIp` covers bracketed IPv6 and dotted-quad IPv4.
    let host = authority.split(':').next().unwrap_or(authority);
    let is_ip = (authority.starts_with('[') && authority.contains(']'))
        || (host.split('.').count() == 4
            && host.split('.').all(|o| !o.is_empty() && o.bytes().all(|b| b.is_ascii_digit())));

    let normalized = if path.is_empty() {
        "/".to_string()
    } else if path.ends_with('/') {
        path.to_string()
    } else {
        format!("{path}/")
    };

    let mut record = std::collections::BTreeMap::new();
    record.insert("scheme".into(), Value::String(scheme.to_string()));
    record.insert("authority".into(), Value::String(authority.to_string()));
    record.insert("path".into(), Value::String(path.to_string()));
    record.insert("normalizedPath".into(), Value::String(normalized));
    record.insert("isIp".into(), Value::Bool(is_ip));
    Value::Record(record)
}

/// `isValidHostLabel(label, allowSubDomains)` — RFC 1123 host labels.
fn is_valid_host_label(args: &[Value]) -> Value {
    let Some(label) = arg(args, 0).as_str().map(str::to_string) else {
        return Value::Bool(false);
    };
    let allow_subdomains = arg(args, 1).as_bool().unwrap_or(false);

    if allow_subdomains {
        if label.is_empty() {
            return Value::Bool(false);
        }
        return Value::Bool(label.split('.').all(|part| valid_label(part)));
    }
    Value::Bool(valid_label(&label))
}

fn valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 63
        && label.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && label.bytes().last().is_some_and(|b| b.is_ascii_alphanumeric())
        && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// `aws.parseArn(arn)` -> record with partition, service, region, accountId, resourceId.
///
/// `resourceId` is an array: the remainder split on `:` and `/`, which is how rulesets
/// index into it (`{Arn#resourceId[1]}`).
fn parse_arn(value: &Value) -> Value {
    let Some(arn) = value.as_str() else { return Value::None };

    let parts: Vec<&str> = arn.splitn(6, ':').collect();
    if parts.len() < 6 || parts[0] != "arn" {
        return Value::None;
    }
    // Partition and service must be present; region and account may legitimately be empty.
    if parts[1].is_empty() || parts[2].is_empty() || parts[5].is_empty() {
        return Value::None;
    }

    let resource: Vec<Value> = parts[5]
        .split([':', '/'])
        .map(|s| Value::String(s.to_string()))
        .collect();

    let mut record = std::collections::BTreeMap::new();
    record.insert("partition".into(), Value::String(parts[1].to_string()));
    record.insert("service".into(), Value::String(parts[2].to_string()));
    record.insert("region".into(), Value::String(parts[3].to_string()));
    record.insert("accountId".into(), Value::String(parts[4].to_string()));
    record.insert("resourceId".into(), Value::Array(resource));
    Value::Record(record)
}

/// `aws.isVirtualHostableS3Bucket(bucket, allowSubDomains)`.
fn is_virtual_hostable_s3_bucket(args: &[Value]) -> Value {
    let Some(bucket) = arg(args, 0).as_str().map(str::to_string) else {
        return Value::Bool(false);
    };
    let allow_subdomains = arg(args, 1).as_bool().unwrap_or(false);

    // S3 bucket labels are 3-63 characters; the lower bound is real and load-bearing
    // (a 2-character bucket must fall back to a path-style URL).
    let hostable = |label: &str| {
        label.len() >= 3
            && label.len() <= 63
            && label.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && label.bytes().next().is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            && label.bytes().last().is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            // A dotted-quad bucket name is not virtual-hostable.
            && !(label.split('.').count() == 4
                && label.split('.').all(|o| !o.is_empty() && o.bytes().all(|b| b.is_ascii_digit())))
    };

    if allow_subdomains {
        if bucket.len() < 3 || bucket.len() > 63 {
            return Value::Bool(false);
        }
        return Value::Bool(bucket.split('.').all(hostable));
    }
    Value::Bool(hostable(&bucket))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Value {
        Value::String(v.to_string())
    }

    fn parts() -> Partitions {
        Partitions::embedded()
    }

    #[test]
    fn not_treats_unset_as_true() {
        // `not(isSet(X))` is the commonest idiom in the corpus.
        assert_eq!(call("not", &[Value::None], &parts()), Value::Bool(true));
        assert_eq!(call("not", &[Value::Bool(true)], &parts()), Value::Bool(false));
    }

    #[test]
    fn equality_is_type_strict() {
        assert_eq!(call("stringEquals", &[s("a"), s("a")], &parts()), Value::Bool(true));
        assert_eq!(call("stringEquals", &[s("a"), Value::Bool(true)], &parts()), Value::Bool(false));
        assert_eq!(
            call("booleanEquals", &[Value::Bool(true), Value::Bool(true)], &parts()),
            Value::Bool(true)
        );
    }

    #[test]
    fn substring_handles_reverse_and_bounds() {
        let args = |a: &str, b: i64, c: i64, r: bool| {
            vec![s(a), Value::Int(b), Value::Int(c), Value::Bool(r)]
        };
        assert_eq!(call("substring", &args("abcde", 0, 3, false), &parts()), s("abc"));
        assert_eq!(call("substring", &args("abcde", 0, 3, true), &parts()), s("cde"));
        // Out of range and inverted ranges are absent, not errors.
        assert!(!call("substring", &args("ab", 0, 5, false), &parts()).is_set());
        assert!(!call("substring", &args("abcde", 3, 1, false), &parts()).is_set());
        // Non-ASCII is explicitly unsupported.
        assert!(!call("substring", &args("héllo", 0, 2, false), &parts()).is_set());
    }

    #[test]
    fn parses_urls() {
        let v = call("parseURL", &[s("https://example.com/path")], &parts());
        assert_eq!(v.get_path("scheme").as_str(), Some("https"));
        assert_eq!(v.get_path("authority").as_str(), Some("example.com"));
        assert_eq!(v.get_path("path").as_str(), Some("/path"));
        assert_eq!(v.get_path("normalizedPath").as_str(), Some("/path/"));
        assert_eq!(v.get_path("isIp").as_bool(), Some(false));

        // No path normalizes to "/".
        let bare = call("parseURL", &[s("https://example.com")], &parts());
        assert_eq!(bare.get_path("normalizedPath").as_str(), Some("/"));

        // IPs, including a port.
        assert_eq!(
            call("parseURL", &[s("http://127.0.0.1:8080")], &parts()).get_path("isIp").as_bool(),
            Some(true)
        );
        // Queries and non-http schemes are rejected.
        assert!(!call("parseURL", &[s("https://x.com/a?b=1")], &parts()).is_set());
        assert!(!call("parseURL", &[s("ftp://x.com")], &parts()).is_set());
    }

    #[test]
    fn validates_host_labels() {
        let f = |l: &str, sub: bool| {
            call("isValidHostLabel", &[s(l), Value::Bool(sub)], &parts()).as_bool().unwrap()
        };
        assert!(f("us-east-1", false));
        assert!(!f("us.east.1", false));
        assert!(f("us.east.1", true));
        assert!(!f("-bad", false));
        assert!(!f("bad-", false));
        assert!(!f("", false));
        assert!(!f(&"a".repeat(64), false));
    }

    #[test]
    fn parses_arns() {
        let v = call("aws.parseArn", &[s("arn:aws:s3:::bucket/key")], &parts());
        assert_eq!(v.get_path("partition").as_str(), Some("aws"));
        assert_eq!(v.get_path("service").as_str(), Some("s3"));
        // Region and account are legitimately empty for S3.
        assert_eq!(v.get_path("region").as_str(), Some(""));
        assert_eq!(v.get_path("resourceId[0]").as_str(), Some("bucket"));
        assert_eq!(v.get_path("resourceId[1]").as_str(), Some("key"));

        assert!(!call("aws.parseArn", &[s("not-an-arn")], &parts()).is_set());
        assert!(!call("aws.parseArn", &[s("arn:aws:s3")], &parts()).is_set());
    }

    #[test]
    fn checks_virtual_hostable_buckets() {
        let f = |b: &str| {
            call("aws.isVirtualHostableS3Bucket", &[s(b), Value::Bool(false)], &parts())
                .as_bool()
                .unwrap()
        };
        assert!(f("my-bucket"));
        assert!(!f("My-Bucket"), "uppercase is not hostable");
        assert!(!f("my_bucket"));
        assert!(!f("192.168.1.1"), "dotted quad is not hostable");
        assert!(!f("bucket."), "dots need allowSubDomains");
        // The 3-character minimum: `aa` must fall back to a path-style URL.
        assert!(!f("aa"));
        assert!(f("aaa"));
    }

    #[test]
    fn unknown_function_is_absent_not_panic() {
        assert!(!call("no.such.function", &[s("x")], &parts()).is_set());
    }
}
