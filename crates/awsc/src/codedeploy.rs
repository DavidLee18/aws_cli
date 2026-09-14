//! `aws deploy register` and `deregister`: on-premises instances in CodeDeploy.
//!
//! Ports of `customizations/codedeploy/`. Registering an on-premises instance means
//! creating an IAM user for it, giving that user an access key, writing the key into a
//! config file the agent reads, and telling CodeDeploy the instance exists.
//! Deregistering undoes all of it.
//!
//! Two things shape the code more than the API calls do:
//!
//! - **`register` prints a long-term secret access key to stdout**, and writes it to a
//!   file. That is the reference's behaviour and the whole point of the command — the key
//!   has to reach the instance somehow — but it means the config file is created `0600`
//!   and `chmod`ed, and it means a reader should not paste this command's output into a
//!   ticket.
//! - **Both commands are step-by-step and report as they go** (`Creating the IAM user...
//!   DONE`), because each step is a separate API call and a failure halfway leaves real
//!   resources behind. On failure they print what was done, then tell the reader to
//!   finish by hand — and exit **255**.

use crate::args::Parsed;
use crate::client::{Client, Globals};
use crate::exit;
use crate::Failure;
use serde_json::{json, Value};
use std::io::Write;
use std::process::ExitCode;

const DEFAULT_CONFIG_FILE: &str = "codedeploy.onpremises.yml";
const MAX_INSTANCE_NAME_LENGTH: usize = 100;
const MAX_TAGS_PER_INSTANCE: usize = 10;
const MAX_TAG_KEY_LENGTH: usize = 128;
const MAX_TAG_VALUE_LENGTH: usize = 256;

/// The policy the agent needs: read-only S3, so it can fetch revisions.
const AGENT_POLICY: &str = "{\n    \"Version\": \"2012-10-17\",\n    \"Statement\": [ {\n        \"Action\": [ \"s3:Get*\", \"s3:List*\" ],\n        \"Effect\": \"Allow\",\n        \"Resource\": \"*\"\n    } ]\n}";

pub fn dispatch(parsed: &Parsed, globals: &Globals) -> Result<Option<ExitCode>, Failure> {
    match parsed.operation.as_str() {
        "register" => Ok(Some(guarded(register(parsed, globals), "Register"))),
        "deregister" => Ok(Some(guarded(deregister(parsed, globals), "Deregister"))),
        "push" => push(parsed, globals).map(Some),
        _ => Ok(None),
    }
}

/// Both commands catch everything and report the same way: the error, then how to finish
/// the job by hand, then exit 255. A half-registered instance is a real state, and a bare
/// stack trace would not say which half.
fn guarded(outcome: Result<ExitCode, Failure>, verb: &str) -> ExitCode {
    match outcome {
        Ok(code) => code,
        Err(failure) => {
            let _ = std::io::stdout().flush();
            eprintln!(
                "ERROR\n{}\n{verb} the on-premises instance by following the instructions \
                 in \"Configure Existing On-Premises Instances by Using AWS CodeDeploy\" \
                 in the AWS CodeDeploy User Guide.",
                failure.message()
            );
            exit::code(exit::GENERAL_ERROR)
        }
    }
}

fn register(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let args =
        crate::custom::take_args(parsed, &["--instance-name", "--tags", "--iam-user-arn"])?;
    let Some(Some(instance_name)) = args.get("--instance-name").copied() else {
        return Err(crate::custom::missing_required(&["--instance-name"]));
    };
    validate_instance_name(instance_name)?;

    let tag_tokens = crate::custom::take_list(parsed, "--tags");
    let tags: Vec<Value> = tag_tokens
        .iter()
        .map(|token| crate::custom::parse_shorthand_token(token, "--tags"))
        .collect::<Result<_, _>>()?;
    validate_tags(&tags)?;

    let given_arn = args.get("--iam-user-arn").copied().flatten();
    if let Some(arn) = given_arn {
        validate_iam_user_arn(arn)?;
    }

    let region = crate::custom::resolve_region(globals)
        .ok_or_else(|| Failure::new(exit::CONFIGURATION, awsc_runtime::RuntimeError::NoRegion))?;
    let cd_globals = Globals { region: Some(region.clone()), ..globals.clone() };
    let cd_model =
        crate::load_model("deploy").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let codedeploy = Client::new(&cd_model, &cd_globals)?;
    let iam_globals = Globals { region: Some(region.clone()), ..globals.for_service("iam") };
    let iam_model = crate::load_model("iam").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let iam = Client::new(&iam_model, &iam_globals)?;

    // An ARN supplied by the caller means the user already exists: no user, key, policy or
    // config file is created, and the instance is simply registered against it.
    let iam_user_arn = match given_arn {
        Some(arn) => arn.to_string(),
        None => {
            let user_name = instance_name;
            print!("Creating the IAM user... ");
            let _ = std::io::stdout().flush();
            let created = iam.call(
                "create-user",
                Some(&json!({ "Path": "/AWS/CodeDeploy/", "UserName": user_name })),
            )?;
            let arn = created
                .get("User")
                .and_then(|user| user.get("Arn"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            println!("DONE\nIamUserArn: {arn}");

            print!("Creating the IAM user access key... ");
            let _ = std::io::stdout().flush();
            let key = iam.call("create-access-key", Some(&json!({ "UserName": user_name })))?;
            let access_key_id = key
                .get("AccessKey")
                .and_then(|k| k.get("AccessKeyId"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let secret = key
                .get("AccessKey")
                .and_then(|k| k.get("SecretAccessKey"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            // The secret is printed. It is the only time it exists outside IAM, and the
            // instance needs it; the reference does the same.
            println!("DONE\nAccessKeyId: {access_key_id}\nSecretAccessKey: {secret}");

            print!("Creating the IAM user policy... ");
            let _ = std::io::stdout().flush();
            iam.call(
                "put-user-policy",
                Some(&json!({
                    "UserName": user_name,
                    "PolicyName": "codedeploy-agent",
                    "PolicyDocument": AGENT_POLICY,
                })),
            )?;
            println!("DONE\nPolicyName: codedeploy-agent\nPolicyDocument: {AGENT_POLICY}");

            print!(
                "Creating the on-premises instance configuration file named \
                 {DEFAULT_CONFIG_FILE}..."
            );
            let _ = std::io::stdout().flush();
            write_config(&region, &arn, &access_key_id, &secret)?;
            println!("DONE");
            arn
        }
    };

    print!("Registering the on-premises instance... ");
    let _ = std::io::stdout().flush();
    codedeploy.call(
        "register-on-premises-instance",
        Some(&json!({ "instanceName": instance_name, "iamUserArn": iam_user_arn })),
    )?;
    println!("DONE");

    if !tags.is_empty() {
        print!("Adding tags to the on-premises instance... ");
        let _ = std::io::stdout().flush();
        codedeploy.call(
            "add-tags-to-on-premises-instances",
            Some(&json!({ "tags": tags, "instanceNames": [instance_name] })),
        )?;
        println!("DONE");
    }

    println!(
        "Copy the on-premises configuration file named {DEFAULT_CONFIG_FILE} to the \
         on-premises instance, and run the following command on the on-premises instance \
         to install and configure the AWS CodeDeploy Agent:\naws deploy install \
         --config-file {DEFAULT_CONFIG_FILE}"
    );
    Ok(exit::code(exit::SUCCESS))
}

fn deregister(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let args =
        crate::custom::take_args(parsed, &["--instance-name", "--no-delete-iam-user"])?;
    let Some(Some(instance_name)) = args.get("--instance-name").copied() else {
        return Err(crate::custom::missing_required(&["--instance-name"]));
    };
    validate_instance_name(instance_name)?;
    let delete_user = !args.contains_key("--no-delete-iam-user");

    let region = crate::custom::resolve_region(globals)
        .ok_or_else(|| Failure::new(exit::CONFIGURATION, awsc_runtime::RuntimeError::NoRegion))?;
    let cd_globals = Globals { region: Some(region.clone()), ..globals.clone() };
    let cd_model =
        crate::load_model("deploy").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let codedeploy = Client::new(&cd_model, &cd_globals)?;
    let iam_globals = Globals { region: Some(region), ..globals.for_service("iam") };
    let iam_model = crate::load_model("iam").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let iam = Client::new(&iam_model, &iam_globals)?;

    print!("Retrieving on-premises instance information... ");
    let _ = std::io::stdout().flush();
    let info = codedeploy
        .call("get-on-premises-instance", Some(&json!({ "instanceName": instance_name })))?;
    let instance_info = info.get("instanceInfo").cloned().unwrap_or(Value::Null);
    let iam_user_arn = instance_info
        .get("iamUserArn")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    // The user name is the ARN's last path segment, which is not always the instance name.
    let user_name = iam_user_arn.rsplit('/').next().unwrap_or_default().to_string();
    let tags = instance_info.get("tags").and_then(Value::as_array).cloned().unwrap_or_default();
    println!("DONE\nIamUserArn: {iam_user_arn}");
    if !tags.is_empty() {
        print!("Tags:");
        for tag in &tags {
            print!(
                " Key={},Value={}",
                tag.get("Key").and_then(Value::as_str).unwrap_or_default(),
                tag.get("Value").and_then(Value::as_str).unwrap_or_default()
            );
        }
        println!();
    }

    if !tags.is_empty() {
        print!("Removing tags from the on-premises instance... ");
        let _ = std::io::stdout().flush();
        codedeploy.call(
            "remove-tags-from-on-premises-instances",
            Some(&json!({ "tags": tags, "instanceNames": [instance_name] })),
        )?;
        println!("DONE");
    }

    print!("Deregistering the on-premises instance... ");
    let _ = std::io::stdout().flush();
    codedeploy.call(
        "deregister-on-premises-instance",
        Some(&json!({ "instanceName": instance_name })),
    )?;
    println!("DONE");

    if delete_user {
        // Each deletion tolerates `NoSuchEntity`, so a partially cleaned-up user finishes
        // cleaning up rather than failing on the first thing that is already gone.
        print!("Deleting the IAM user policies... ");
        let _ = std::io::stdout().flush();
        if let Some(listed) =
            absent_ok(iam.call("list-user-policies", Some(&json!({ "UserName": user_name }))))?
        {
            for policy in listed.get("PolicyNames").and_then(Value::as_array).cloned().unwrap_or_default() {
                absent_ok(iam.call(
                    "delete-user-policy",
                    Some(&json!({ "UserName": user_name, "PolicyName": policy })),
                ))?;
            }
        }
        println!("DONE");

        print!("Deleting the IAM user access keys... ");
        let _ = std::io::stdout().flush();
        if let Some(listed) =
            absent_ok(iam.call("list-access-keys", Some(&json!({ "UserName": user_name }))))?
        {
            for key in listed
                .get("AccessKeyMetadata")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
            {
                let id = key.get("AccessKeyId").cloned().unwrap_or(Value::Null);
                absent_ok(iam.call(
                    "delete-access-key",
                    Some(&json!({ "UserName": user_name, "AccessKeyId": id })),
                ))?;
            }
        }
        println!("DONE");

        print!("Deleting the IAM user ({user_name})... ");
        let _ = std::io::stdout().flush();
        absent_ok(iam.call("delete-user", Some(&json!({ "UserName": user_name }))))?;
        println!("DONE");
    }

    println!(
        "Run the following command on the on-premises instance to uninstall the \
         codedeploy-agent:\naws deploy uninstall"
    );
    Ok(exit::code(exit::SUCCESS))
}

/// `aws deploy push`: zip a source tree, upload it, register it as a revision.
///
/// Two rules decide what ends up in the bundle, and both are easy to get wrong:
/// **paths inside the archive are relative to `--source`**, so the bundle has no leading
/// directory; and **`appspec.yml` must be at the root of it**, which is checked while
/// walking rather than after uploading — the reference fails before the upload too, and a
/// bundle without an appspec is one CodeDeploy will reject much later.
fn push(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let args = crate::custom::take_args(
        parsed,
        &[
            "--application-name",
            "--s3-location",
            "--ignore-hidden-files",
            "--no-ignore-hidden-files",
            "--source",
            "--description",
        ],
    )?;
    let value = |flag: &str| args.get(flag).copied().flatten();
    let missing: Vec<&str> = ["--application-name", "--s3-location"]
        .into_iter()
        .filter(|flag| value(flag).is_none())
        .collect();
    if !missing.is_empty() {
        return Err(crate::custom::missing_required(&missing));
    }
    if args.contains_key("--ignore-hidden-files") && args.contains_key("--no-ignore-hidden-files")
    {
        return Err(param_error(
            "You cannot specify both --ignore-hidden-files and --no-ignore-hidden-files.",
        ));
    }
    let ignore_hidden = args.contains_key("--ignore-hidden-files");
    let application_name = value("--application-name").unwrap_or_default();
    let (bucket, key) = parse_s3_location(value("--s3-location").unwrap_or_default())?;
    // `.` when not given, which means "bundle the directory I am standing in".
    let source = value("--source").unwrap_or(".");
    let description = match value("--description") {
        Some(text) => text.to_string(),
        None => format!(
            "Uploaded by AWS CLI {} UTC",
            awsc_protocol::shapes::format_cli_output(crate::now_unix()).replace("+00:00", "")
        ),
    };

    let bundle = compress(source, ignore_hidden)?;

    let region = crate::custom::resolve_region(globals)
        .ok_or_else(|| Failure::new(exit::CONFIGURATION, awsc_runtime::RuntimeError::NoRegion))?;
    let s3_globals = Globals { region: Some(region.clone()), ..globals.for_service("s3") };
    let s3_model = crate::load_model("s3api").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let s3 = Client::new(&s3_model, &s3_globals)?;

    let uploaded = crate::custom::upload_to_s3(&s3, &bucket, &key, &bundle).map_err(|e| {
        Failure::new(
            exit::GENERAL_ERROR,
            format!(
                "Failed to upload '{source}' to 's3://{bucket}/{key}': {}",
                e.message()
            ),
        )
    })?;
    // The quotes around an ETag are part of the header, not part of the value.
    let etag = uploaded
        .get("ETag")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .replace('"', "");
    let version = uploaded.get("VersionId").and_then(Value::as_str).map(str::to_string);

    let mut s3_location = json!({
        "bucket": bucket,
        "key": key,
        "bundleType": "zip",
        "eTag": etag,
    });
    if let Some(version) = &version {
        s3_location["version"] = Value::String(version.clone());
    }
    let cd_globals = Globals { region: Some(region), ..globals.clone() };
    let cd_model =
        crate::load_model("deploy").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let codedeploy = Client::new(&cd_model, &cd_globals)?;
    codedeploy.call(
        "register-application-revision",
        Some(&json!({
            "applicationName": application_name,
            "revision": { "revisionType": "S3", "s3Location": s3_location },
            "description": description,
        })),
    )?;

    let version_string = match &version {
        Some(version) => format!(",version={version}"),
        None => String::new(),
    };
    // Assembled in pieces rather than as one continued literal: a `\`-continued string in
    // Rust keeps the indentation of the line that follows it, which turned this into a
    // command with runs of spaces in the middle — and the whole point of the line is that
    // it can be pasted.
    let s3_location_string = format!(
        "--s3-location bucket={bucket},key={key},bundleType=zip,eTag={etag}{version_string}"
    );
    let command = format!(
        "aws deploy create-deployment --application-name {application_name} \
{s3_location_string} --deployment-group-name <deployment-group-name> \
--deployment-config-name <deployment-config-name> --description <description>"
    );
    println!("To deploy with this revision, run:\n{command}");
    Ok(exit::code(exit::SUCCESS))
}

/// `s3://bucket/key`, which is the only form this flag takes.
fn parse_s3_location(location: &str) -> Result<(String, String), Failure> {
    let rest = location.strip_prefix("s3://").ok_or_else(|| {
        param_error("--s3-location must specify the format: s3://<bucket>/<key>")
    })?;
    match rest.split_once('/') {
        Some((bucket, key)) if !bucket.is_empty() && !key.is_empty() => {
            Ok((bucket.to_string(), key.to_string()))
        }
        _ => Err(param_error("--s3-location must specify the format: s3://<bucket>/<key>")),
    }
}

/// Walk the source tree into a zip. Paths inside are relative to the source root.
fn compress(source: &str, ignore_hidden: bool) -> Result<Vec<u8>, Failure> {
    let root = std::path::Path::new(source).canonicalize().map_err(|e| {
        Failure::new(exit::GENERAL_ERROR, format!("{source}: {e}"))
    })?;
    let mut archive = crate::zip::Archive::new();
    let mut contains_appspec = false;
    let mut stack = vec![root.clone()];
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    while let Some(directory) = stack.pop() {
        let entries = std::fs::read_dir(&directory)
            .map_err(|e| Failure::new(exit::GENERAL_ERROR, format!("{}: {e}", directory.display())))?;
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            // `--ignore-hidden-files` prunes hidden *directories* as well as files, so a
            // `.git` tree is skipped whole rather than walked and discarded.
            if ignore_hidden && name.starts_with('.') {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    // Sorted, so a bundle of the same tree is the same bundle twice running — the walk
    // order of a directory is not guaranteed.
    files.sort();

    for path in files {
        let arcname = path
            .strip_prefix(&root)
            .map(|relative| relative.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string_lossy().into_owned());
        if arcname == "appspec.yml" {
            contains_appspec = true;
        }
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

    if !contains_appspec {
        return Err(Failure::new(
            exit::GENERAL_ERROR,
            format!("{} was not found", root.join("appspec.yml").display()),
        ));
    }
    archive.finish().map_err(|e| Failure::new(exit::GENERAL_ERROR, e.to_string()))
}

/// `NoSuchEntity` is not a failure while cleaning up: it means the thing is already gone.
fn absent_ok(outcome: Result<Value, Failure>) -> Result<Option<Value>, Failure> {
    match outcome {
        Ok(value) => Ok(Some(value)),
        Err(failure) if failure.service_error_code.as_deref() == Some("NoSuchEntity") => Ok(None),
        Err(failure) => Err(failure),
    }
}

/// The agent's config file: YAML, in the working directory, mode `0600`.
fn write_config(
    region: &str,
    iam_user_arn: &str,
    access_key_id: &str,
    secret_access_key: &str,
) -> Result<(), Failure> {
    let contents = format!(
        "---\nregion: {region}\niam_user_arn: {iam_user_arn}\n\
         aws_access_key_id: {access_key_id}\naws_secret_access_key: {secret_access_key}\n"
    );
    std::fs::write(DEFAULT_CONFIG_FILE, contents).map_err(|e| {
        Failure::new(
            exit::GENERAL_ERROR,
            format!("Failed to create config file {DEFAULT_CONFIG_FILE}: {e}"),
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // `chmod` on top of the create, so a file that already existed with looser
        // permissions is tightened rather than left as it was.
        let _ = std::fs::set_permissions(
            DEFAULT_CONFIG_FILE,
            std::fs::Permissions::from_mode(0o600),
        );
    }
    Ok(())
}

fn validate_instance_name(name: &str) -> Result<(), Failure> {
    let allowed = |c: char| c.is_ascii_alphanumeric() || "+=,.@_-".contains(c);
    if name.is_empty() || !name.chars().all(allowed) {
        return Err(param_error("Instance name contains invalid characters."));
    }
    // `i-` is how EC2 names an instance, and an on-premises one must not look like one.
    if name.starts_with("i-") {
        return Err(param_error("Instance name cannot start with 'i-'."));
    }
    if name.len() > MAX_INSTANCE_NAME_LENGTH {
        return Err(param_error(&format!(
            "Instance name cannot be longer than {MAX_INSTANCE_NAME_LENGTH} characters."
        )));
    }
    Ok(())
}

fn validate_tags(tags: &[Value]) -> Result<(), Failure> {
    if tags.len() > MAX_TAGS_PER_INSTANCE {
        return Err(param_error(&format!(
            "Instances can only have a maximum of {MAX_TAGS_PER_INSTANCE} tags."
        )));
    }
    for tag in tags {
        let key = tag.get("Key").and_then(Value::as_str).unwrap_or_default();
        let value = tag.get("Value").and_then(Value::as_str).unwrap_or_default();
        if key.len() > MAX_TAG_KEY_LENGTH {
            return Err(param_error(&format!(
                "Tag Key cannot be longer than {MAX_TAG_KEY_LENGTH} characters."
            )));
        }
        if value.len() > MAX_TAG_VALUE_LENGTH {
            return Err(param_error(&format!(
                "Tag Value cannot be longer than {MAX_TAG_VALUE_LENGTH} characters."
            )));
        }
    }
    Ok(())
}

fn validate_iam_user_arn(arn: &str) -> Result<(), Failure> {
    // `arn:aws:iam::<12 digits>:user/<name>`
    let valid = arn.strip_prefix("arn:aws:iam::").and_then(|rest| {
        let (account, tail) = rest.split_once(':')?;
        let name = tail.strip_prefix("user/")?;
        let account_ok = account.len() == 12 && account.chars().all(|c| c.is_ascii_digit());
        let name_ok = !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || "/+=,.@_-".contains(c));
        (account_ok && name_ok).then_some(())
    });
    match valid {
        Some(()) => Ok(()),
        None => Err(param_error("Invalid IAM user ARN.")),
    }
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

    /// An on-premises instance must not be named like an EC2 one.
    #[test]
    fn an_instance_name_cannot_look_like_an_ec2_id() {
        assert!(validate_instance_name("i-0123456789abcdef0").is_err());
        assert!(validate_instance_name("ip-10-0-0-1").is_ok());
    }

    #[test]
    fn instance_names_are_checked_for_characters_and_length() {
        assert!(validate_instance_name("web-01.prod@example").is_ok());
        assert!(validate_instance_name("has spaces").is_err());
        assert!(validate_instance_name("has/slash").is_err());
        assert!(validate_instance_name("").is_err());
        assert!(validate_instance_name(&"x".repeat(101)).is_err());
        assert!(validate_instance_name(&"x".repeat(100)).is_ok());
    }

    #[test]
    fn an_iam_user_arn_needs_a_twelve_digit_account_and_a_user_path() {
        assert!(validate_iam_user_arn("arn:aws:iam::123456789012:user/bob").is_ok());
        // A role is not a user.
        assert!(validate_iam_user_arn("arn:aws:iam::123456789012:role/bob").is_err());
        assert!(validate_iam_user_arn("arn:aws:iam::12345:user/bob").is_err());
        assert!(validate_iam_user_arn("arn:aws:iam::123456789012:user/").is_err());
        assert!(validate_iam_user_arn("nonsense").is_err());
    }

    #[test]
    fn tags_are_limited_in_number_and_length() {
        let one = json!({"Key": "k", "Value": "v"});
        assert!(validate_tags(&vec![one.clone(); 10]).is_ok());
        assert!(validate_tags(&vec![one; 11]).is_err());
        assert!(validate_tags(&[json!({"Key": "x".repeat(129), "Value": "v"})]).is_err());
        assert!(validate_tags(&[json!({"Key": "k", "Value": "x".repeat(257)})]).is_err());
    }

    #[test]
    fn an_s3_location_must_name_a_bucket_and_a_key() {
        assert_eq!(
            parse_s3_location("s3://my-bucket/app/rev.zip").expect("parses"),
            ("my-bucket".to_string(), "app/rev.zip".to_string())
        );
        assert!(parse_s3_location("s3://my-bucket").is_err());
        assert!(parse_s3_location("s3://my-bucket/").is_err());
        assert!(parse_s3_location("my-bucket/key").is_err());
    }

    /// The user name is the ARN's last segment, which need not be the instance name — a
    /// user created under a path, or an ARN supplied by the caller.
    #[test]
    fn the_user_name_is_the_arns_last_segment() {
        let arn = "arn:aws:iam::123456789012:user/AWS/CodeDeploy/my-instance";
        assert_eq!(arn.rsplit('/').next(), Some("my-instance"));
    }
}
