//! S3 URI parsing, as the high-level `s3` tree does it.
//!
//! Deliberately not a URL parser: the reference strips a literal five-character `s3://`
//! prefix and splits the remainder on the first `/`. There is no bucket-name validation
//! anywhere in this layer, and the scheme is optional for `ls`, `presign` and `website`
//! (but mandatory for `mb`/`rb`, which check it themselves).

/// A parsed `s3://bucket/key`, or a local path.
///
/// Used by the transfer commands (`cp`/`mv`/`sync`) to classify each side of a transfer.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    S3 { bucket: String, key: String },
    Local(String),
    /// A literal `-`, meaning stdin or stdout.
    Stream,
}

#[allow(dead_code)]
impl Location {
    pub fn parse(raw: &str) -> Result<Location, String> {
        if raw == "-" {
            return Ok(Location::Stream);
        }
        if !raw.starts_with("s3://") {
            return Ok(Location::Local(raw.to_string()));
        }
        let (bucket, key) = split_bucket_key(raw)?;
        Ok(Location::S3 { bucket, key })
    }

    pub fn is_s3(&self) -> bool {
        matches!(self, Location::S3 { .. })
    }
}

/// `s3://bucket/key` -> `(bucket, key)`. The scheme is optional.
pub fn split_bucket_key(path: &str) -> Result<(String, String), String> {
    let rest = path.strip_prefix("s3://").unwrap_or(path);
    block_unsupported_resources(rest)?;
    match rest.split_once('/') {
        Some((bucket, key)) => Ok((bucket.to_string(), key.to_string())),
        None => Ok((rest.to_string(), String::new())),
    }
}

/// Two ARN shapes the `s3` tree refuses outright, pointing at the right command instead.
pub fn block_unsupported_resources(path: &str) -> Result<(), String> {
    if path.starts_with("arn:") && path.contains(":s3-object-lambda:") {
        return Err("s3 commands do not support S3 Object Lambda resources. \
                    Use s3api commands instead."
            .to_string());
    }
    // An Outpost *bucket* ARN, as opposed to an Outpost access point, which is supported.
    if path.starts_with("arn:") && path.contains(":s3-outposts:") && path.contains("/bucket/") {
        return Err("s3 commands do not support Outpost Bucket ARNs. \
                    Use s3control commands instead."
            .to_string());
    }
    Ok(())
}

/// The bucket name for `website`, which does its own trimming rather than splitting.
///
/// It strips the scheme and exactly one trailing slash and stops — so anything after a
/// further slash stays in the "bucket", and the request fails server-side. Reproduced
/// rather than corrected.
pub fn website_bucket_name(path: &str) -> Result<String, String> {
    let mut name = path.strip_prefix("s3://").unwrap_or(path);
    if let Some(trimmed) = name.strip_suffix('/') {
        name = trimmed;
    }
    block_unsupported_resources(name)?;
    Ok(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(p: &str) -> (String, String) {
        split_bucket_key(p).unwrap()
    }

    /// The full table from the reference's own behaviour, including the cases where a
    /// URL parser would disagree.
    #[test]
    fn splits_bucket_and_key() {
        assert_eq!(split("s3://"), (String::new(), String::new()));
        assert_eq!(split("s3://b"), ("b".into(), String::new()));
        // A trailing slash yields an EMPTY key, not a key of "/".
        assert_eq!(split("s3://b/"), ("b".into(), String::new()));
        assert_eq!(split("s3://b/k"), ("b".into(), "k".into()));
        assert_eq!(split("s3://b/p/"), ("b".into(), "p/".into()));
        // Only the FIRST slash separates; the rest is all key.
        assert_eq!(split("s3://b/a/b/c"), ("b".into(), "a/b/c".into()));
        // The scheme is optional at this layer.
        assert_eq!(split("b/k"), ("b".into(), "k".into()));
    }

    #[test]
    fn recognises_locations() {
        assert_eq!(Location::parse("-").unwrap(), Location::Stream);
        assert_eq!(Location::parse("./x").unwrap(), Location::Local("./x".into()));
        assert!(Location::parse("s3://b/k").unwrap().is_s3());
        // A local path that merely mentions s3 is still local.
        assert_eq!(Location::parse("s3-backup").unwrap(), Location::Local("s3-backup".into()));
    }

    #[test]
    fn refuses_the_two_unsupported_arn_shapes() {
        let lambda = "arn:aws:s3-object-lambda:us-west-2:123456789012:accesspoint/ap";
        assert!(split_bucket_key(lambda).unwrap_err().contains("s3api commands instead"));
        let outpost = "arn:aws:s3-outposts:us-west-2:123456789012:outpost/op-1/bucket/b";
        assert!(split_bucket_key(outpost).unwrap_err().contains("s3control commands instead"));
    }

    /// `website` strips one trailing slash and nothing more.
    #[test]
    fn derives_the_website_bucket_name() {
        assert_eq!(website_bucket_name("s3://b").unwrap(), "b");
        assert_eq!(website_bucket_name("s3://b/").unwrap(), "b");
        assert_eq!(website_bucket_name("b").unwrap(), "b");
        // Not split on the slash: this really is sent as the bucket.
        assert_eq!(website_bucket_name("s3://b/k").unwrap(), "b/k");
    }
}
