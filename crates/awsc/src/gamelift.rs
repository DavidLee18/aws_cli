//! `aws gamelift get-game-session-log` and `upload-build`.
//!
//! Ports of `customizations/gamelift/`. Both commands are here because both step outside
//! the service model: one downloads from a presigned URL, and the other uploads to S3
//! with credentials GameLift hands back rather than the caller's own.

use crate::args::Parsed;
use crate::client::{Client, Globals};
use crate::custom::{missing_required, resolve_region, take_args, take_list};
use crate::exit;
use crate::Failure;
use serde_json::{json, Value};
use std::io::Write;
use std::process::ExitCode;

pub fn dispatch(parsed: &Parsed, globals: &Globals) -> Result<Option<ExitCode>, Failure> {
    match parsed.operation.as_str() {
        "get-game-session-log" => get_game_session_log(parsed, globals).map(Some),
        "upload-build" => upload_build(parsed, globals).map(Some),
        _ => Ok(None),
    }
}

/// `aws gamelift get-game-session-log`.
///
/// Asks GameLift for a presigned URL and downloads it to `--save-as`. The download is a
/// plain unsigned GET: the URL already carries its own signature, so sending credentials
/// with it would be both pointless and a leak.
fn get_game_session_log(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let args = take_args(parsed, &["--game-session-id", "--save-as"])?;
    let missing: Vec<&str> = ["--game-session-id", "--save-as"]
        .into_iter()
        .filter(|flag| !args.contains_key(flag))
        .collect();
    if !missing.is_empty() {
        return Err(missing_required(&missing));
    }
    let value = |flag: &str| args.get(flag).copied().flatten().unwrap_or_default();
    let session_id = value("--game-session-id");
    let save_as = value("--save-as");

    let (model, gamelift_globals) = gamelift_client_model(globals)?;
    let client = Client::new(&model, &gamelift_globals)?;
    let response = client.call(
        "get-game-session-log-url",
        Some(&json!({ "GameSessionId": session_id })),
    )?;
    let url = response
        .get("PreSignedUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| Failure::new(exit::GENERAL_ERROR, "'PreSignedUrl'"))?;

    // `\r`, not `\n`: the success line that follows overwrites it.
    print!("Downloading log archive for game session {session_id}...\r");
    let _ = std::io::stdout().flush();

    let response = ureq::get(url)
        .call()
        .map_err(|e| Failure::new(exit::GENERAL_ERROR, format!("<urlopen error {e}>")))?;
    let mut body = response.into_reader();
    let mut file = std::fs::File::create(save_as)
        .map_err(|e| Failure::new(exit::GENERAL_ERROR, format!("{save_as}: {e}")))?;
    std::io::copy(&mut body, &mut file)
        .map_err(|e| Failure::new(exit::GENERAL_ERROR, format!("{save_as}: {e}")))?;

    println!("Successfully downloaded log archive for game session {session_id} to {save_as}");
    Ok(exit::code(exit::SUCCESS))
}

/// `aws gamelift upload-build`: zip a build directory and hand it to GameLift's own bucket.
///
/// The shape of this one is the interesting part. It is three calls, not one:
///
/// 1. `create-build` reserves a build id.
/// 2. `request-upload-credentials` returns **temporary credentials and a bucket and key
///    that belong to GameLift**, not to the caller. The upload is signed with those, so
///    the S3 client is built and then has its credentials replaced — the caller's own
///    credentials have no access to that bucket at all.
/// 3. The zip goes to that location.
///
/// A build id exists from step 1 onward, so a failure during the upload leaves an
/// `INITIALIZED` build behind in the account. The reference leaves it too; it is not
/// deleted here, because deleting it would throw away the id a retry can reuse.
fn upload_build(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let args = take_args(
        parsed,
        &[
            "--name",
            "--build-version",
            "--build-root",
            "--server-sdk-version",
            "--operating-system",
            "--tags",
        ],
    )?;
    let value = |flag: &str| args.get(flag).copied().flatten();
    let missing: Vec<&str> = ["--name", "--build-version", "--build-root"]
        .into_iter()
        .filter(|flag| value(flag).is_none())
        .collect();
    if !missing.is_empty() {
        return Err(missing_required(&missing));
    }
    let name = value("--name").unwrap_or_default();
    let build_version = value("--build-version").unwrap_or_default();
    let build_root = value("--build-root").unwrap_or_default();

    // Checked before anything is created: an empty or missing directory would otherwise
    // leave a build id behind with nothing uploaded against it.
    if !has_any_file(build_root) {
        eprintln!(
            "Fail to upload {build_root}. \
             The build root directory is empty or does not exist."
        );
        return Ok(exit::code(exit::GENERAL_ERROR));
    }

    let mut input = json!({ "Name": name, "Version": build_version });
    if let Some(operating_system) = value("--operating-system") {
        input["OperatingSystem"] = Value::String(operating_system.to_string());
    }
    if let Some(server_sdk_version) = value("--server-sdk-version") {
        input["ServerSdkVersion"] = Value::String(server_sdk_version.to_string());
    }
    let tags = parse_tags(&take_list(parsed, "--tags"));
    if !tags.is_empty() {
        input["Tags"] = Value::Array(tags);
    }

    let (model, gamelift_globals) = gamelift_client_model(globals)?;
    let gamelift = Client::new(&model, &gamelift_globals)?;
    let created = gamelift.call("create-build", Some(&input))?;
    let build_id = created
        .get("Build")
        .and_then(|build| build.get("BuildId"))
        .and_then(Value::as_str)
        .ok_or_else(|| Failure::new(exit::GENERAL_ERROR, "'Build'"))?
        .to_string();

    let granted =
        gamelift.call("request-upload-credentials", Some(&json!({ "BuildId": build_id })))?;
    let field = |group: &str, name: &str| {
        granted
            .get(group)
            .and_then(|value| value.get(name))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let bucket = field("StorageLocation", "Bucket")
        .ok_or_else(|| Failure::new(exit::GENERAL_ERROR, "'StorageLocation'"))?;
    let key = field("StorageLocation", "Key")
        .ok_or_else(|| Failure::new(exit::GENERAL_ERROR, "'StorageLocation'"))?;
    let upload_credentials = awsc_runtime::credentials::Credentials {
        access_key_id: field("UploadCredentials", "AccessKeyId")
            .ok_or_else(|| Failure::new(exit::GENERAL_ERROR, "'UploadCredentials'"))?,
        secret_access_key: field("UploadCredentials", "SecretAccessKey")
            .ok_or_else(|| Failure::new(exit::GENERAL_ERROR, "'UploadCredentials'"))?,
        session_token: field("UploadCredentials", "SessionToken"),
        expires_at: None,
        method: "gamelift-upload",
    };

    let bundle = compress(build_root)?;

    // `for_service("s3")`, so a `--endpoint-url` meant for GameLift does not redirect the
    // upload: the reference passes `endpoint_url` to the GameLift client only.
    let s3_globals =
        Globals { region: gamelift_globals.region.clone(), ..globals.for_service("s3") };
    let s3_model =
        crate::load_model("s3api").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let mut s3 = Client::new(&s3_model, &s3_globals)?;
    s3.credentials = upload_credentials;

    let size = crate::s3::human_readable_size(bundle.len() as u64);
    crate::custom::upload_to_s3(&s3, &bucket, &key, &bundle)?;
    // The reference reports progress as the upload proceeds and never ends the line, so
    // the success message below lands on the same one. We have no per-chunk callback, so
    // this is the finished line only — see `docs/divergences.md`.
    print!("\rUploading {build_root}:  {size} / {size}  (100.00%)");
    let _ = std::io::stdout().flush();

    println!("Successfully uploaded {build_root} to AWS GameLift\nBuild ID: {build_id}");
    Ok(exit::code(exit::SUCCESS))
}

/// The model and globals for the GameLift client.
///
/// `globals` itself, not `for_service`: gamelift IS the service the user named, so a
/// `--endpoint-url` they passed is meant for this call.
fn gamelift_client_model(
    globals: &Globals,
) -> Result<(awsc_model::model::Model, Globals), Failure> {
    let region = resolve_region(globals)
        .ok_or_else(|| Failure::new(exit::CONFIGURATION, awsc_runtime::RuntimeError::NoRegion))?;
    let model = crate::load_model("gamelift").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    Ok((model, Globals { region: Some(region), ..globals.clone() }))
}

/// `Key=Value`, with everything after the first `=` kept as the value, and a bare word
/// meaning an empty value rather than being an error.
fn parse_tags(raw: &[&str]) -> Vec<Value> {
    raw.iter()
        .map(|tag| match tag.split_once('=') {
            Some((key, value)) => json!({ "Key": key, "Value": value }),
            None => json!({ "Key": tag, "Value": "" }),
        })
        .collect()
}

/// Is there at least one file anywhere under the root?
///
/// A directory of empty directories is as useless as a missing one, which is why the
/// check walks rather than just testing that the path exists.
fn has_any_file(root: &str) -> bool {
    if root.is_empty() {
        return false;
    }
    let mut stack = vec![std::path::PathBuf::from(root)];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                return true;
            }
        }
    }
    false
}

/// Zip the build root. Paths inside are relative to it, and nothing is excluded — a
/// build directory is uploaded whole, hidden files included.
fn compress(root: &str) -> Result<Vec<u8>, Failure> {
    let root = std::path::Path::new(root)
        .canonicalize()
        .map_err(|e| Failure::new(exit::GENERAL_ERROR, format!("{root}: {e}")))?;
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(directory) = stack.pop() {
        let entries = std::fs::read_dir(&directory).map_err(|e| {
            Failure::new(exit::GENERAL_ERROR, format!("{}: {e}", directory.display()))
        })?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    // Sorted, so the same tree bundles identically twice running.
    files.sort();

    let mut archive = crate::zip::Archive::new();
    for path in files {
        let arcname = path
            .strip_prefix(&root)
            .map(|relative| relative.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string_lossy().into_owned());
        let contents = std::fs::read(&path)
            .map_err(|e| Failure::new(exit::GENERAL_ERROR, format!("{}: {e}", path.display())))?;
        let modified = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or_else(crate::now_unix);
        archive
            .add(&arcname, &contents, modified)
            .map_err(|e| Failure::new(exit::GENERAL_ERROR, e.to_string()))?;
    }
    archive.finish().map_err(|e| Failure::new(exit::GENERAL_ERROR, e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Key=Value`, and a value that itself contains `=` keeps it.
    #[test]
    fn tags_split_on_the_first_equals_only() {
        let tags = parse_tags(&["env=prod", "expr=a=b", "bare"]);
        assert_eq!(
            tags,
            vec![
                json!({ "Key": "env", "Value": "prod" }),
                json!({ "Key": "expr", "Value": "a=b" }),
                json!({ "Key": "bare", "Value": "" }),
            ]
        );
    }

    #[test]
    fn an_empty_or_missing_build_root_has_no_files() {
        assert!(!has_any_file(""));
        assert!(!has_any_file("/nonexistent-build-root-for-tests"));
        let empty = std::env::temp_dir().join(format!("awsc-empty-{}", std::process::id()));
        std::fs::create_dir_all(empty.join("nested")).expect("creates");
        assert!(!has_any_file(&empty.to_string_lossy()));
        std::fs::write(empty.join("nested/server"), b"x").expect("writes");
        assert!(has_any_file(&empty.to_string_lossy()));
        let _ = std::fs::remove_dir_all(&empty);
    }
}
