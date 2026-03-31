use anyhow::{Context, Result};
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::types::{BucketCannedAcl, ObjectCannedAcl, WebsiteConfiguration};
use aws_sdk_s3::Client;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

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

/// Move/rename a local file or S3 object.
pub async fn cmd_mv(client: &Client, src: &str, dst: &str) -> Result<()> {
    let src_is_s3 = src.starts_with("s3://");
    let dst_is_s3 = dst.starts_with("s3://");

    match (src_is_s3, dst_is_s3) {
        (false, false) => {
            std::fs::rename(src, dst).with_context(|| format!("Failed to move {src} to {dst}"))?;
            println!("move: {src} -> {dst}");
            Ok(())
        }
        (false, true) => {
            upload_file(client, src, dst).await?;
            std::fs::remove_file(src)
                .with_context(|| format!("Uploaded but failed to remove source file {src}"))?;
            println!("move: {src} -> {dst}");
            Ok(())
        }
        (true, false) => {
            download_file(client, src, dst).await?;
            cmd_rm(client, src).await?;
            println!("move: {src} -> {dst}");
            Ok(())
        }
        (true, true) => {
            copy_object(client, src, dst).await?;
            cmd_rm(client, src).await?;
            println!("move: {src} -> {dst}");
            Ok(())
        }
    }
}

/// Sync a local directory to/from S3.
pub async fn cmd_sync(client: &Client, src: &str, dst: &str) -> Result<()> {
    let src_is_s3 = src.starts_with("s3://");
    let dst_is_s3 = dst.starts_with("s3://");

    match (src_is_s3, dst_is_s3) {
        (false, true) => sync_local_to_s3(client, src, dst).await,
        (true, false) => sync_s3_to_local(client, src, dst).await,
        _ => anyhow::bail!("sync currently supports local→S3 or S3→local"),
    }
}

/// Generate a presigned URL for an S3 object.
pub async fn cmd_presign(client: &Client, uri: &str, expires_in: u64) -> Result<()> {
    let (bucket, key_opt) = parse_s3_uri(uri)?;
    let key = key_opt
        .filter(|k| !k.is_empty())
        .context("s3 presign requires an object key")?;

    let config = PresigningConfig::expires_in(Duration::from_secs(expires_in))
        .context("Invalid expiration for presign")?;

    let presigned = client
        .get_object()
        .bucket(&bucket)
        .key(&key)
        .presigned(config)
        .await
        .with_context(|| format!("Failed to presign s3://{bucket}/{key}"))?;

    println!("{}", presigned.uri());
    Ok(())
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

/// Configure (or disable) static website hosting on a bucket.
pub async fn cmd_website(
    client: &Client,
    uri: &str,
    index_document: &Option<String>,
    error_document: &Option<String>,
    disable: bool,
) -> Result<()> {
    let (bucket, _) = parse_s3_uri(uri)?;
    if disable {
        client
            .delete_bucket_website()
            .bucket(&bucket)
            .send()
            .await
            .with_context(|| format!("Failed to delete website config for {bucket}"))?;
        println!("website disabled: s3://{bucket}");
        return Ok(());
    }

    let index = index_document
        .as_deref()
        .context("index-document is required unless --disable is used")?;
    let mut cfg = WebsiteConfiguration::builder().index_document(
        aws_sdk_s3::types::IndexDocument::builder()
            .suffix(index)
            .build()?,
    );
    if let Some(err) = error_document {
        cfg = cfg.error_document(
            aws_sdk_s3::types::ErrorDocument::builder()
                .key(err)
                .build()?,
        );
    }
    client
        .put_bucket_website()
        .bucket(&bucket)
        .website_configuration(cfg.build())
        .send()
        .await
        .with_context(|| format!("Failed to set website config for {bucket}"))?;
    println!("website enabled: s3://{bucket} (index: {index})");
    Ok(())
}

/// Get the ACL for a bucket or object.
pub async fn cmd_get_acl(client: &Client, uri: &str) -> Result<()> {
    let (bucket, key_opt) = parse_s3_uri(uri)?;
    if let Some(key) = key_opt.filter(|k| !k.is_empty()) {
        let resp = client
            .get_object_acl()
            .bucket(&bucket)
            .key(&key)
            .send()
            .await
            .with_context(|| format!("Failed to get ACL for s3://{bucket}/{key}"))?;
        print_acl(resp.owner(), Some(resp.grants()), Some(&key));
    } else {
        let resp = client
            .get_bucket_acl()
            .bucket(&bucket)
            .send()
            .await
            .with_context(|| format!("Failed to get ACL for bucket {bucket}"))?;
        print_acl(resp.owner(), Some(resp.grants()), None);
    }
    Ok(())
}

/// Set a canned ACL for a bucket or object.
pub async fn cmd_put_acl(client: &Client, uri: &str, acl: &str) -> Result<()> {
    let (bucket, key_opt) = parse_s3_uri(uri)?;
    if let Some(key) = key_opt.filter(|k| !k.is_empty()) {
        let canned = ObjectCannedAcl::from(acl);
        client
            .put_object_acl()
            .bucket(&bucket)
            .key(&key)
            .acl(canned)
            .send()
            .await
            .with_context(|| format!("Failed to set ACL for s3://{bucket}/{key}"))?;
        println!("acl set: s3://{bucket}/{key} ({acl})");
    } else {
        let canned = BucketCannedAcl::from(acl);
        client
            .put_bucket_acl()
            .bucket(&bucket)
            .acl(canned)
            .send()
            .await
            .with_context(|| format!("Failed to set ACL for bucket {bucket}"))?;
        println!("acl set: s3://{bucket} ({acl})");
    }
    Ok(())
}

/// Get the bucket policy JSON for a bucket.
pub async fn cmd_get_bucket_policy(client: &Client, uri: &str) -> Result<()> {
    let (bucket, _) = parse_s3_uri(uri)?;
    let resp = client
        .get_bucket_policy()
        .bucket(&bucket)
        .send()
        .await
        .with_context(|| format!("Failed to get bucket policy for {bucket}"))?;
    if let Some(pol) = resp.policy() {
        println!("{pol}");
    } else {
        println!("(no policy)");
    }
    Ok(())
}

/// Set or replace the bucket policy for a bucket.
pub async fn cmd_put_bucket_policy(client: &Client, uri: &str, policy: &str) -> Result<()> {
    let (bucket, _) = parse_s3_uri(uri)?;
    let policy_str = if Path::new(policy).exists() {
        fs::read_to_string(policy)
            .with_context(|| format!("Failed to read policy file {policy}"))?
    } else {
        policy.to_string()
    };
    client
        .put_bucket_policy()
        .bucket(&bucket)
        .policy(policy_str)
        .send()
        .await
        .with_context(|| format!("Failed to put bucket policy for {bucket}"))?;
    println!("bucket policy applied: s3://{bucket}");
    Ok(())
}

/// Delete the bucket policy for a bucket.
pub async fn cmd_delete_bucket_policy(client: &Client, uri: &str) -> Result<()> {
    let (bucket, _) = parse_s3_uri(uri)?;
    client
        .delete_bucket_policy()
        .bucket(&bucket)
        .send()
        .await
        .with_context(|| format!("Failed to delete bucket policy for {bucket}"))?;
    println!("bucket policy deleted: s3://{bucket}");
    Ok(())
}

/// List object versions for a bucket/prefix.
pub async fn cmd_list_object_versions(client: &Client, uri: &str) -> Result<()> {
    let (bucket, prefix) = parse_s3_uri(uri)?;
    let mut req = client.list_object_versions().bucket(&bucket);
    if let Some(p) = prefix {
        if !p.is_empty() {
            req = req.prefix(p);
        }
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("Failed to list object versions for {bucket}"))?;

    for ver in resp.versions() {
        let key = ver.key().unwrap_or("<unknown>");
        let vid = ver.version_id().unwrap_or("<null>");
        let is_latest = ver.is_latest().unwrap_or(false);
        let size = ver.size().unwrap_or(0);
        println!(
            "{:<8} {:<36} {:>10} {}",
            if is_latest { "LATEST" } else { "" },
            vid,
            size,
            key
        );
    }
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

async fn sync_local_to_s3(client: &Client, local_dir: &str, s3_uri: &str) -> Result<()> {
    let base = Path::new(local_dir);
    if !base.is_dir() {
        anyhow::bail!("Local path for sync must be a directory");
    }
    let files = collect_local_files(base)?;
    let (bucket, prefix_opt) = parse_s3_uri(s3_uri)?;
    let base_prefix = prefix_opt.unwrap_or_default();
    let base_prefix = if base_prefix.is_empty() || base_prefix.ends_with('/') {
        base_prefix
    } else {
        format!("{base_prefix}/")
    };

    for file in files {
        let rel = file
            .strip_prefix(base)
            .context("Failed to compute relative path during sync")?;
        let rel_str = normalize_path_for_s3(rel);
        let key = format!("{base_prefix}{rel_str}");

        let body = aws_sdk_s3::primitives::ByteStream::from_path(&file)
            .await
            .with_context(|| format!("Cannot read file {}", file.display()))?;

        client
            .put_object()
            .bucket(&bucket)
            .key(&key)
            .body(body)
            .send()
            .await
            .with_context(|| {
                format!("Failed to upload {} to s3://{bucket}/{key}", file.display())
            })?;

        println!("upload: {} -> s3://{bucket}/{key}", file.display());
    }

    Ok(())
}

async fn sync_s3_to_local(client: &Client, s3_uri: &str, local_dir: &str) -> Result<()> {
    let (bucket, prefix_opt) = parse_s3_uri(s3_uri)?;
    let prefix = prefix_opt.unwrap_or_default();
    let dest_root = Path::new(local_dir);
    std::fs::create_dir_all(dest_root).with_context(|| {
        format!(
            "Failed to create destination directory {}",
            dest_root.display()
        )
    })?;

    let mut continuation: Option<String> = None;
    loop {
        let mut req = client.list_objects_v2().bucket(&bucket);
        if !prefix.is_empty() {
            req = req.prefix(&prefix);
        }
        if let Some(ref tok) = continuation {
            req = req.continuation_token(tok);
        }

        let resp = req
            .send()
            .await
            .with_context(|| format!("Failed to list objects in {bucket}"))?;

        for obj in resp.contents() {
            if let Some(key) = obj.key() {
                let rel = if prefix.is_empty() {
                    key.to_string()
                } else if let Some(stripped) = key.strip_prefix(&prefix) {
                    stripped.to_string()
                } else {
                    anyhow::bail!(
                        "S3 sync encountered key {key} that does not match expected prefix {prefix}"
                    );
                };
                let dest_path = dest_root.join(&rel);
                if let Some(parent) = dest_path.parent() {
                    std::fs::create_dir_all(parent).with_context(|| {
                        format!("Failed to create directory {}", parent.display())
                    })?;
                }

                let data = client
                    .get_object()
                    .bucket(&bucket)
                    .key(key)
                    .send()
                    .await
                    .with_context(|| format!("Failed to download s3://{bucket}/{key}"))?
                    .body
                    .collect()
                    .await
                    .context("Failed to read response body")?;

                std::fs::write(&dest_path, data.into_bytes())
                    .with_context(|| format!("Cannot write {}", dest_path.display()))?;
                println!("download: s3://{bucket}/{key} -> {}", dest_path.display());
            }
        }

        match resp.next_continuation_token() {
            Some(tok) => continuation = Some(tok.to_string()),
            None => break,
        }
    }

    Ok(())
}

fn collect_local_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut stack = vec![root.to_path_buf()];
    let mut files = Vec::new();

    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)
            .with_context(|| format!("Failed to read directory {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }

    Ok(files)
}

fn normalize_path_for_s3(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn print_acl(
    owner: Option<&aws_sdk_s3::types::Owner>,
    grants: Option<&[aws_sdk_s3::types::Grant]>,
    key: Option<&str>,
) {
    if let Some(o) = owner {
        let id = o.id().unwrap_or("");
        let display_name = o.display_name().unwrap_or("");
        if let Some(k) = key {
            println!("Owner (object): {display_name} {id}  key={k}");
        } else {
            println!("Owner (bucket): {display_name} {id}");
        }
    }
    if let Some(gs) = grants {
        for g in gs {
            let grantee = g.grantee();
            let name = grantee.and_then(|gr| gr.display_name()).unwrap_or("");
            let typ = grantee.map(|gr| gr.r#type().as_str()).unwrap_or("");
            let perm = g
                .permission()
                .map(|p| p.as_str().to_string())
                .unwrap_or_else(|| "unknown".to_string());
            println!("Grant: {:<18} {:<10} {}", perm, typ, name);
        }
    }
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
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    struct TempDirGuard {
        path: PathBuf,
    }

    impl TempDirGuard {
        fn new(path: PathBuf) -> Self {
            Self { path }
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

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

    #[test]
    fn test_collect_local_files_nested() {
        let seed = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("aws_cli_sync_test_{seed}"));
        let _guard = TempDirGuard::new(root.clone());

        std::fs::create_dir_all(root.join("nested/inner")).unwrap();
        std::fs::write(root.join("file1.txt"), "one").unwrap();
        std::fs::write(root.join("nested/inner/file2.txt"), "two").unwrap();

        let files = collect_local_files(&root).unwrap();
        let mut rels: Vec<String> = files
            .iter()
            .map(|p| {
                let rel = p
                    .strip_prefix(&root)
                    .expect("collected file should reside under the root directory");
                normalize_path_for_s3(rel)
            })
            .collect();
        rels.sort();

        assert_eq!(rels, vec!["file1.txt", "nested/inner/file2.txt"]);
    }

    #[test]
    fn test_normalize_path_for_s3_backslashes() {
        let path = std::path::Path::new("dir\\sub\\file.txt");
        assert_eq!(normalize_path_for_s3(path), "dir/sub/file.txt");
    }

    #[test]
    fn test_normalize_path_for_s3_mixed() {
        let mut path = std::path::PathBuf::new();
        path.push("dir");
        path.push("sub");
        path.push("file.txt");
        assert_eq!(normalize_path_for_s3(&path), "dir/sub/file.txt");
    }
}
