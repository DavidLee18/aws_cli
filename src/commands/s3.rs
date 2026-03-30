use anyhow::{Context, Result};
use aws_sdk_s3::Client;

/// List S3 buckets, or objects within a bucket.
///
/// * `uri` – `None` → list all buckets; `Some("s3://bucket[/prefix]")` → list
///   objects.
pub async fn cmd_ls(client: &Client, uri: Option<&str>, recursive: bool) -> Result<()> {
    match uri {
        None => list_buckets(client).await,
        Some(u) => {
            let (bucket, prefix) = parse_s3_uri(u)?;
            list_objects(client, &bucket, prefix.as_deref(), recursive).await
        }
    }
}

/// Copy a local file to S3, S3 to local, or S3 to S3.
pub async fn cmd_cp(client: &Client, src: &str, dst: &str) -> Result<()> {
    let src_is_s3 = src.starts_with("s3://");
    let dst_is_s3 = dst.starts_with("s3://");

    match (src_is_s3, dst_is_s3) {
        (false, true) => upload_file(client, src, dst).await,
        (true, false) => download_file(client, src, dst).await,
        (true, true) => copy_object(client, src, dst).await,
        (false, false) => anyhow::bail!("At least one argument must be an S3 URI (s3://)"),
    }
}

/// Remove an object from S3.
pub async fn cmd_rm(client: &Client, uri: &str) -> Result<()> {
    let (bucket, key_opt) = parse_s3_uri(uri)?;
    let key = key_opt
        .filter(|k| !k.is_empty())
        .context("s3 rm requires a full object key, not just a bucket name")?;
    client
        .delete_object()
        .bucket(&bucket)
        .key(&key)
        .send()
        .await
        .with_context(|| format!("Failed to delete s3://{bucket}/{key}"))?;
    println!("delete: s3://{bucket}/{key}");
    Ok(())
}

/// Create a new S3 bucket.
pub async fn cmd_mb(client: &Client, uri: &str, region: &str) -> Result<()> {
    let (bucket, _) = parse_s3_uri(uri)?;
    if region == "us-east-1" {
        client
            .create_bucket()
            .bucket(&bucket)
            .send()
            .await
            .with_context(|| format!("Failed to create bucket {bucket}"))?;
    } else {
        let constraint = aws_sdk_s3::types::BucketLocationConstraint::from(region);
        let cfg = aws_sdk_s3::types::CreateBucketConfiguration::builder()
            .location_constraint(constraint)
            .build();
        client
            .create_bucket()
            .bucket(&bucket)
            .create_bucket_configuration(cfg)
            .send()
            .await
            .with_context(|| format!("Failed to create bucket {bucket}"))?;
    }
    println!("make_bucket: s3://{bucket}");
    Ok(())
}

/// Remove an empty S3 bucket.
pub async fn cmd_rb(client: &Client, uri: &str, force: bool) -> Result<()> {
    let (bucket, _) = parse_s3_uri(uri)?;
    if force {
        // Delete all objects first.
        let mut continuation: Option<String> = None;
        loop {
            let mut req = client.list_objects_v2().bucket(&bucket);
            if let Some(ref tok) = continuation {
                req = req.continuation_token(tok);
            }
            let resp = req
                .send()
                .await
                .with_context(|| format!("Failed to list objects in {bucket}"))?;

            let objects = resp.contents();
            if objects.is_empty() {
                break;
            }
            for obj in objects {
                if let Some(key) = obj.key() {
                    client
                        .delete_object()
                        .bucket(&bucket)
                        .key(key)
                        .send()
                        .await
                        .with_context(|| format!("Failed to delete {key}"))?;
                }
            }
            match resp.next_continuation_token() {
                Some(tok) => continuation = Some(tok.to_string()),
                None => break,
            }
        }
    }
    client
        .delete_bucket()
        .bucket(&bucket)
        .send()
        .await
        .with_context(|| format!("Failed to remove bucket {bucket}"))?;
    println!("remove_bucket: s3://{bucket}");
    Ok(())
}

// ── Internals ────────────────────────────────────────────────────────────────

async fn list_buckets(client: &Client) -> Result<()> {
    let resp = client
        .list_buckets()
        .send()
        .await
        .context("Failed to list S3 buckets")?;
    for bucket in resp.buckets() {
        let name = bucket.name().unwrap_or("<unknown>");
        let created = bucket
            .creation_date()
            .map(|d| d.to_string())
            .unwrap_or_default();
        println!("{created:24} s3://{name}");
    }
    Ok(())
}

async fn list_objects(
    client: &Client,
    bucket: &str,
    prefix: Option<&str>,
    recursive: bool,
) -> Result<()> {
    let mut continuation: Option<String> = None;
    let delimiter = if recursive { None } else { Some("/") };

    loop {
        let mut req = client.list_objects_v2().bucket(bucket);
        if let Some(p) = prefix {
            req = req.prefix(p);
        }
        if let Some(d) = delimiter {
            req = req.delimiter(d);
        }
        if let Some(ref tok) = continuation {
            req = req.continuation_token(tok);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("Failed to list objects in {bucket}"))?;

        // Common prefixes (directories)
        for cp in resp.common_prefixes() {
            if let Some(p) = cp.prefix() {
                println!("                           PRE {p}");
            }
        }
        // Objects
        for obj in resp.contents() {
            let key = obj.key().unwrap_or("<unknown>");
            let size = obj.size().unwrap_or(0);
            let modified = obj
                .last_modified()
                .map(|d| d.to_string())
                .unwrap_or_default();
            println!("{modified:24} {size:>10} {key}");
        }

        match resp.next_continuation_token() {
            Some(tok) => continuation = Some(tok.to_string()),
            None => break,
        }
    }
    Ok(())
}

async fn upload_file(client: &Client, local_path: &str, s3_uri: &str) -> Result<()> {
    let (bucket, key_opt) = parse_s3_uri(s3_uri)?;
    let mut key = key_opt.unwrap_or_default();
    if key.is_empty() || key.ends_with('/') {
        // Use the filename as the key.
        let filename = std::path::Path::new(local_path)
            .file_name()
            .and_then(|n| n.to_str())
            .context("Cannot determine filename from local path")?;
        key.push_str(filename);
    }

    let body = aws_sdk_s3::primitives::ByteStream::from_path(local_path)
        .await
        .with_context(|| format!("Cannot read file {local_path}"))?;

    client
        .put_object()
        .bucket(&bucket)
        .key(&key)
        .body(body)
        .send()
        .await
        .with_context(|| format!("Failed to upload {local_path} to s3://{bucket}/{key}"))?;

    println!("upload: {local_path} to s3://{bucket}/{key}");
    Ok(())
}

async fn download_file(client: &Client, s3_uri: &str, local_path: &str) -> Result<()> {
    let (bucket, key_opt) = parse_s3_uri(s3_uri)?;
    let key = key_opt
        .filter(|k| !k.is_empty())
        .context("s3 cp source URI must include an object key")?;

    let resp = client
        .get_object()
        .bucket(&bucket)
        .key(&key)
        .send()
        .await
        .with_context(|| format!("Failed to download s3://{bucket}/{key}"))?;

    let mut dest_path = std::path::PathBuf::from(local_path);
    if dest_path.is_dir() {
        let filename = std::path::Path::new(&key)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&key);
        dest_path.push(filename);
    }

    let data = resp
        .body
        .collect()
        .await
        .context("Failed to read response body")?;
    std::fs::write(&dest_path, data.into_bytes())
        .with_context(|| format!("Cannot write {}", dest_path.display()))?;

    println!("download: s3://{bucket}/{key} to {}", dest_path.display());
    Ok(())
}

async fn copy_object(client: &Client, src_uri: &str, dst_uri: &str) -> Result<()> {
    let (src_bucket, src_key_opt) = parse_s3_uri(src_uri)?;
    let src_key = src_key_opt
        .filter(|k| !k.is_empty())
        .context("s3 cp source URI must include an object key")?;

    let (dst_bucket, dst_key_opt) = parse_s3_uri(dst_uri)?;
    let dst_key = dst_key_opt.unwrap_or_else(|| src_key.clone());

    let copy_source = format!("{src_bucket}/{src_key}");
    client
        .copy_object()
        .bucket(&dst_bucket)
        .key(&dst_key)
        .copy_source(&copy_source)
        .send()
        .await
        .with_context(|| {
            format!("Failed to copy s3://{src_bucket}/{src_key} to s3://{dst_bucket}/{dst_key}")
        })?;

    println!("copy: s3://{src_bucket}/{src_key} to s3://{dst_bucket}/{dst_key}");
    Ok(())
}

/// Parse `s3://bucket/optional/key` into `(bucket, Option<key>)`.
pub fn parse_s3_uri(uri: &str) -> Result<(String, Option<String>)> {
    let without_scheme = uri
        .strip_prefix("s3://")
        .with_context(|| format!("Invalid S3 URI (must start with s3://): {uri}"))?;

    match without_scheme.split_once('/') {
        Some((bucket, key)) => Ok((bucket.to_string(), Some(key.to_string()))),
        None => Ok((without_scheme.to_string(), None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_s3_uri_bucket_only() {
        let (bucket, key) = parse_s3_uri("s3://my-bucket").unwrap();
        assert_eq!(bucket, "my-bucket");
        assert_eq!(key, None);
    }

    #[test]
    fn test_parse_s3_uri_with_key() {
        let (bucket, key) = parse_s3_uri("s3://my-bucket/path/to/file.txt").unwrap();
        assert_eq!(bucket, "my-bucket");
        assert_eq!(key, Some("path/to/file.txt".to_string()));
    }

    #[test]
    fn test_parse_s3_uri_invalid() {
        assert!(parse_s3_uri("https://my-bucket/key").is_err());
    }
}
