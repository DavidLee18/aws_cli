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
        "install" => install(parsed, globals).map(Some),
        "uninstall" => uninstall(parsed, globals).map(Some),
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
    /// The reference matches `(\w+)[-_](release|version)` against every name in `/etc`.
    #[test]
    fn release_file_names_split_the_way_the_reference_matches_them() {
        assert_eq!(split_release_filename("redhat-release"), Some(("redhat", "release")));
        assert_eq!(split_release_filename("os_version"), Some(("os", "version")));
        // No separator, so no match at all.
        assert_eq!(split_release_filename("hostname"), None);
        // A leading separator leaves an empty name.
        assert_eq!(split_release_filename("-release"), None);
    }

    /// `<name> release <version> (<codename>)` is where "Red Hat Enterprise Linux Server"
    /// comes from — the check the reference makes is against the part before " release ".
    #[test]
    fn the_distribution_name_comes_from_before_the_word_release() {
        assert_eq!(
            parse_release_line("Red Hat Enterprise Linux Server release 7.9 (Maipo)"),
            "Red Hat Enterprise Linux Server"
        );
        assert_eq!(parse_release_line("CentOS Linux release 7 (Core)"), "CentOS Linux");
        // RHEL 8 and later dropped "Server", which is why they are not recognised.
        assert_eq!(
            parse_release_line("Red Hat Enterprise Linux release 9.3 (Plow)"),
            "Red Hat Enterprise Linux"
        );
        assert_eq!(parse_release_line("Slackware 14.2"), "Slackware");
        assert_eq!(parse_release_line(""), "");
    }

    /// Each system keeps its config somewhere different, and the installer is named
    /// differently — getting either wrong installs an agent that cannot find its identity.
    #[test]
    fn each_system_knows_where_its_configuration_lives() {
        assert_eq!(
            System::Ubuntu.config_path(),
            "/etc/codedeploy-agent/conf/codedeploy.onpremises.yml"
        );
        assert_eq!(System::Rhel.config_path(), System::Ubuntu.config_path());
        assert_eq!(
            System::Windows.config_path(),
            r"C:\ProgramData\Amazon\CodeDeploy\conf.onpremises.yml"
        );
        assert_eq!(System::Ubuntu.installer(), "install");
        assert_eq!(System::Windows.installer(), "codedeploy-agent.msi");
        // The two Linux systems differ only in what "not installed" looks like.
        assert_ne!(System::Ubuntu.not_found_message(), System::Rhel.not_found_message());
    }

    #[test]
    fn the_agent_installer_must_be_an_s3_url() {
        assert_eq!(
            parse_installer_location("s3://my-bucket/releases/install-1.2").expect("parses"),
            ("my-bucket".to_string(), "releases/install-1.2".to_string())
        );
        for bad in ["https://example/install", "s3://only-a-bucket", "s3:///key", ""] {
            let failure = parse_installer_location(bad).expect_err("refuses");
            assert!(
                failure.message().contains("s3://<bucket>/<key>"),
                "{bad}: {}",
                failure.message()
            );
        }
    }

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

/// The operating systems the agent can be installed on, and where each keeps its config.
///
/// The list is the reference's and it is short on purpose: these are the systems the
/// CodeDeploy agent ships packages for.
///
/// Constructed only on the platforms that have one, so a macOS build sees every variant
/// as unused — which is correct: `local_system()` there returns `None` and both commands
/// report the unsupported-system error.
#[derive(Clone, Copy, PartialEq, Debug)]
#[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
pub enum System {
    Ubuntu,
    Rhel,
    Windows,
}

const UNSUPPORTED_SYSTEM: &str = "Only Ubuntu Server, Red Hat Enterprise Linux Server and \
                                  Windows Server operating systems are supported.";

impl System {
    fn config_dir(self) -> &'static str {
        match self {
            System::Windows => r"C:\ProgramData\Amazon\CodeDeploy",
            _ => "/etc/codedeploy-agent/conf",
        }
    }

    fn config_path(self) -> String {
        match self {
            System::Windows => format!(r"{}\conf.onpremises.yml", self.config_dir()),
            _ => format!("{}/{DEFAULT_CONFIG_FILE}", self.config_dir()),
        }
    }

    /// The file name the installer is saved as, and the default key under `latest/`.
    fn installer(self) -> &'static str {
        match self {
            System::Windows => "codedeploy-agent.msi",
            _ => "install",
        }
    }

    /// The message `service codedeploy-agent stop` prints when the agent is not installed
    /// at all, which is not a failure — there is simply nothing to stop.
    fn not_found_message(self) -> &'static str {
        match self {
            System::Ubuntu => "codedeploy-agent: unrecognized service",
            System::Rhel => "Redirecting to /bin/systemctl stop  codedeploy-agent.service",
            System::Windows => "Cannot find any service with service name 'codedeployagent'",
        }
    }
}

/// `aws deploy install`: put the agent on the machine this is running on.
///
/// Unlike every other command here it acts on the *local* system: it writes
/// `/etc/codedeploy-agent/conf/codedeploy.onpremises.yml`, installs Ruby through the
/// distribution's package manager, downloads the installer from S3 and runs it. So it
/// requires root, and it **refuses to run on an EC2 instance** — detected by the instance
/// metadata endpoint answering — because an EC2 instance uses the agent differently.
fn install(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let args = crate::custom::take_args(
        parsed,
        &["--config-file", "--override-config", "--no-override-config", "--agent-installer"],
    )?;
    let value = |flag: &str| args.get(flag).copied().flatten();
    let Some(config_file) = value("--config-file") else {
        return Err(crate::custom::missing_required(&["--config-file"]));
    };

    let region = crate::custom::resolve_region(globals)
        .ok_or_else(|| Failure::new(exit::CONFIGURATION, "Region not specified."))?;
    let system = detect_system()?;
    validate_administrator(system)?;

    // Checked before anything is downloaded: overwriting a working instance's config is
    // how an instance loses its identity.
    if std::path::Path::new(&system.config_path()).is_file()
        && !args.contains_key("--override-config")
    {
        return Err(Failure::new(
            exit::GENERAL_ERROR,
            "The on-premises instance configuration file already exists. Specify \
             --override-config to update the existing on-premises instance configuration \
             file.",
        ));
    }

    let (bucket, key) = match value("--agent-installer") {
        None => (
            format!("aws-codedeploy-{region}"),
            format!("latest/{}", system.installer()),
        ),
        Some(location) => parse_installer_location(location)?,
    };
    // The name the downloaded file is saved as comes from the key, not from the system's
    // default: `--agent-installer s3://b/releases/install-1.2` runs `./install-1.2`.
    let installer = key.rsplit('/').next().unwrap_or(system.installer()).to_string();

    let outcome = (|| -> Result<(), Failure> {
        create_config(system, config_file)?;
        print!("Installing the AWS CodeDeploy Agent... ");
        let _ = std::io::stdout().flush();
        install_agent(system, globals, &region, &bucket, &key, &installer)?;
        println!("DONE");
        Ok(())
    })();
    Ok(guarded_local(outcome, "Install"))
}

/// `aws deploy uninstall`: take the agent off this machine and forget its identity.
fn uninstall(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    crate::custom::take_args(parsed, &[])?;
    let _region = crate::custom::resolve_region(globals)
        .ok_or_else(|| Failure::new(exit::CONFIGURATION, "Region not specified."))?;
    let system = detect_system()?;
    validate_administrator(system)?;

    let outcome = (|| -> Result<(), Failure> {
        print!("Uninstalling the AWS CodeDeploy Agent... ");
        let _ = std::io::stdout().flush();
        // Only remove the package if the agent actually stopped: a stop that failed for
        // any reason other than "not installed" means something is still running.
        if stop_agent(system)? {
            remove_agent(system)?;
        }
        println!("DONE");

        print!("Deleting the on-premises instance configuration... ");
        let _ = std::io::stdout().flush();
        match std::fs::remove_file(system.config_path()) {
            Ok(()) => {}
            // Already gone is the goal, not a failure.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(Failure::new(
                    exit::GENERAL_ERROR,
                    format!("{}: {e}", system.config_path()),
                ))
            }
        }
        println!("DONE");
        Ok(())
    })();
    Ok(guarded_local(outcome, "Uninstall"))
}

/// The wrapper both commands put around the part that touches the machine.
///
/// Distinct from [`guarded`], which register/deregister use: the wording differs, and so
/// does the *scope* — validation happens outside this, so a bad argument is reported as a
/// plain CLI error rather than as a half-finished install.
fn guarded_local(outcome: Result<(), Failure>, verb: &str) -> ExitCode {
    match outcome {
        Ok(()) => exit::code(exit::SUCCESS),
        Err(failure) => {
            let _ = std::io::stdout().flush();
            eprintln!(
                "ERROR\n{}\n{verb} the AWS CodeDeploy Agent on the on-premises instance by \
                 following the instructions in \"Configure Existing On-Premises Instances by \
                 Using AWS CodeDeploy\" in the AWS CodeDeploy User Guide.",
                failure.message()
            );
            exit::code(exit::GENERAL_ERROR)
        }
    }
}

fn create_config(system: System, config_file: &str) -> Result<(), Failure> {
    print!("Creating the on-premises instance configuration file... ");
    let _ = std::io::stdout().flush();
    std::fs::create_dir_all(system.config_dir()).map_err(|e| {
        Failure::new(exit::GENERAL_ERROR, format!("{}: {e}", system.config_dir()))
    })?;
    // Copying a file onto itself would truncate it, so the case where the user already
    // put the config where it belongs is skipped rather than handled.
    if config_file != system.config_path() {
        std::fs::copy(config_file, system.config_path()).map_err(|e| {
            Failure::new(exit::GENERAL_ERROR, format!("{config_file}: {e}"))
        })?;
    }
    println!("DONE");
    Ok(())
}

fn install_agent(
    system: System,
    globals: &Globals,
    region: &str,
    bucket: &str,
    key: &str,
    installer: &str,
) -> Result<(), Failure> {
    // Ruby first: the installer is a Ruby program, so a machine without it cannot run
    // what is about to be downloaded.
    match system {
        System::Ubuntu => {
            run(&["apt-get", "-y", "update"])?;
            run(&["apt-get", "-y", "install", "ruby2.0"])?;
        }
        System::Rhel => run(&["yum", "-y", "install", "ruby"])?,
        System::Windows => {}
    }
    stop_agent(system)?;

    let s3_globals =
        Globals { region: Some(region.to_string()), ..globals.for_service("s3") };
    let model = crate::load_model("s3api").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let s3 = Client::new(&model, &s3_globals)?;
    let body = s3.call_bytes("get-object", Some(&json!({ "Bucket": bucket, "Key": key })))?;
    std::fs::write(installer, &body)
        .map_err(|e| Failure::new(exit::GENERAL_ERROR, format!("{installer}: {e}")))?;

    match system {
        System::Windows => {
            run(&[&format!(r".\{installer}"), "/quiet", "/l", r".\codedeploy-agent-install-log.txt"])?;
            run(&["powershell.exe", "-Command", "Restart-Service", "-Name", "codedeployagent"])?;
            let status = capture(&[
                "powershell.exe",
                "-Command",
                "Get-Service",
                "-Name",
                "codedeployagent",
            ])?;
            if !status.0.contains("Running") {
                return Err(Failure::new(
                    exit::GENERAL_ERROR,
                    "The AWS CodeDeploy Agent did not start after installation.",
                ));
            }
        }
        _ => {
            run(&["chmod", "+x", &format!("./{installer}")])?;
            // The installer signs its own AWS calls, so it is handed credentials through
            // the environment rather than expecting a profile to exist for root.
            let credentials = crate::custom::resolve_credentials(globals, region)?;
            let mut command = std::process::Command::new(format!("./{installer}"));
            command.arg("auto");
            command.env("AWS_REGION", region);
            command.env("AWS_ACCESS_KEY_ID", &credentials.access_key_id);
            command.env("AWS_SECRET_ACCESS_KEY", &credentials.secret_access_key);
            if let Some(token) = &credentials.session_token {
                command.env("AWS_SESSION_TOKEN", token);
            }
            let status = command
                .status()
                .map_err(|e| Failure::new(exit::GENERAL_ERROR, format!("./{installer}: {e}")))?;
            if !status.success() {
                return Err(Failure::new(
                    exit::GENERAL_ERROR,
                    format!(
                        "Command '['./{installer}', 'auto']' returned non-zero exit status {}.",
                        status.code().unwrap_or(1)
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Stop the agent. `Ok(true)` means it stopped, `Ok(false)` that it was not installed.
fn stop_agent(system: System) -> Result<bool, Failure> {
    let command: &[&str] = match system {
        System::Windows => {
            &["powershell.exe", "-Command", "Stop-Service", "-Name", "codedeployagent"]
        }
        _ => &["service", "codedeploy-agent", "stop"],
    };
    let (_, stderr, success) = capture_status(command)?;
    if success {
        return Ok(true);
    }
    if stderr.contains(system.not_found_message()) {
        return Ok(false);
    }
    Err(Failure::new(
        exit::GENERAL_ERROR,
        format!("Failed to stop the AWS CodeDeploy Agent:\n{stderr}"),
    ))
}

fn remove_agent(system: System) -> Result<(), Failure> {
    match system {
        System::Ubuntu => run(&["dpkg", "-r", "codedeploy-agent"]),
        System::Rhel => run(&["yum", "-y", "erase", "codedeploy-agent"]),
        System::Windows => {
            let (_, stderr, success) = capture_status(&[
                "wmic",
                "product",
                "where",
                r#"name="CodeDeploy Host Agent""#,
                "call",
                "uninstall",
                "/nointeractive",
            ])?;
            if success {
                Ok(())
            } else {
                Err(Failure::new(
                    exit::GENERAL_ERROR,
                    format!("Failed to uninstall the AWS CodeDeploy Agent:\n{stderr}"),
                ))
            }
        }
    }
}

/// Run a command, failing if it does.
fn run(command: &[&str]) -> Result<(), Failure> {
    let status = std::process::Command::new(command[0])
        .args(&command[1..])
        .status()
        .map_err(|e| Failure::new(exit::GENERAL_ERROR, format!("{}: {e}", command[0])))?;
    if status.success() {
        return Ok(());
    }
    Err(Failure::new(
        exit::GENERAL_ERROR,
        format!(
            "Command '{:?}' returned non-zero exit status {}.",
            command,
            status.code().unwrap_or(1)
        ),
    ))
}

fn capture(command: &[&str]) -> Result<(String, String), Failure> {
    let (stdout, stderr, _) = capture_status(command)?;
    Ok((stdout, stderr))
}

fn capture_status(command: &[&str]) -> Result<(String, String, bool), Failure> {
    let output = std::process::Command::new(command[0])
        .args(&command[1..])
        .output()
        .map_err(|e| Failure::new(exit::GENERAL_ERROR, format!("{}: {e}", command[0])))?;
    Ok((
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.success(),
    ))
}

/// `s3://bucket/key`, with the same message the reference gives.
fn parse_installer_location(location: &str) -> Result<(String, String), Failure> {
    let rest = location.strip_prefix("s3://").unwrap_or("");
    match rest.split_once('/') {
        Some((bucket, key)) if !bucket.is_empty() && !key.is_empty() => {
            Ok((bucket.to_string(), key.to_string()))
        }
        _ => Err(param_error(
            "--agent-installer must specify the Amazon S3 URL format as s3://<bucket>/<key>.",
        )),
    }
}

/// Which system this is, refusing anything the agent has no package for — and refusing
/// EC2, where the agent is installed a different way.
fn detect_system() -> Result<System, Failure> {
    let system = local_system()
        .ok_or_else(|| Failure::new(exit::GENERAL_ERROR, UNSUPPORTED_SYSTEM))?;
    if is_ec2_instance() {
        return Err(Failure::new(
            exit::GENERAL_ERROR,
            "Amazon EC2 instances are not supported.",
        ));
    }
    Ok(system)
}

#[cfg(target_os = "windows")]
fn local_system() -> Option<System> {
    Some(System::Windows)
}

#[cfg(target_os = "linux")]
fn local_system() -> Option<System> {
    let distribution = linux_distribution();
    if distribution.contains("Ubuntu") {
        return Some(System::Ubuntu);
    }
    if distribution.contains("Red Hat Enterprise Linux Server") {
        return Some(System::Rhel);
    }
    None
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn local_system() -> Option<System> {
    None
}

/// The distribution name, the way Python's removed `platform.linux_distribution` found it.
///
/// `/etc/lsb-release` first — on Ubuntu it names `Ubuntu` outright, and checking it first
/// is what stops Ubuntu being identified as Debian. Otherwise the first `<name>-release`
/// or `<name>_version` file in `/etc` whose name is a distribution we know, parsed as
/// `<name> release <version> (<codename>)`.
///
/// Worth knowing: on RHEL 8 and later `/etc/redhat-release` says "Red Hat Enterprise Linux
/// release 9.3", without "Server" — so the reference does not recognise it, and neither
/// does this. See `docs/divergences.md`.
#[cfg(target_os = "linux")]
fn linux_distribution() -> String {
    const SUPPORTED: [&str; 15] = [
        "SuSE", "debian", "fedora", "redhat", "centos", "mandrake", "mandriva", "rocks",
        "slackware", "yellowdog", "gentoo", "UnitedLinux", "turbolinux", "arch", "mageia",
    ];
    if let Ok(text) = std::fs::read_to_string("/etc/lsb-release") {
        let id = text.lines().find_map(|line| {
            let (key, value) = line.split_once('=')?;
            key.trim().eq_ignore_ascii_case("DISTRIB_ID").then(|| value.trim().to_string())
        });
        let release = text.lines().find_map(|line| {
            let (key, value) = line.split_once('=')?;
            key.trim().eq_ignore_ascii_case("DISTRIB_RELEASE").then(|| value.trim().to_string())
        });
        if let (Some(id), Some(release)) = (id, release) {
            if !id.is_empty() && !release.is_empty() {
                return id;
            }
        }
    }

    let Ok(entries) = std::fs::read_dir("/etc") else { return String::new() };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        let Some((distribution, suffix)) = split_release_filename(&name) else { continue };
        if !(suffix == "release" || suffix == "version") || !SUPPORTED.contains(&distribution) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(format!("/etc/{name}")) else { continue };
        return parse_release_line(text.lines().next().unwrap_or_default());
    }
    String::new()
}

/// `(\w+)[-_](release|version)` — the name, then the separator, then the kind.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn split_release_filename(name: &str) -> Option<(&str, &str)> {
    let index = name.find(['-', '_'])?;
    let (distribution, rest) = name.split_at(index);
    if distribution.is_empty() || !distribution.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    Some((distribution, &rest[1..]))
}

/// `<name> release <version> (<codename>)`, falling back to the first word.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_release_line(line: &str) -> String {
    match line.find(" release ") {
        Some(index) => line[..index].to_string(),
        None => line.split_whitespace().next().unwrap_or_default().to_string(),
    }
}

/// Is the instance metadata service answering?
///
/// One second, and *any* failure means "not EC2" — including an HTTP error, which is what
/// an IMDSv2-only instance returns to an unauthenticated GET. The reference catches
/// `URLError`, and `HTTPError` is a subclass of it, so it reaches the same conclusion.
fn is_ec2_instance() -> bool {
    ureq::get("http://169.254.169.254/latest/meta-data/")
        .timeout(std::time::Duration::from_secs(1))
        .call()
        .is_ok()
}

#[cfg(unix)]
fn validate_administrator(_system: System) -> Result<(), Failure> {
    // SAFETY: `geteuid` reads the calling process's effective user id.
    if unsafe { libc::geteuid() } != 0 {
        return Err(Failure::new(exit::GENERAL_ERROR, "You must run this command as sudo."));
    }
    Ok(())
}

/// **Divergence:** the reference asks Windows directly with `IsUserAnAdmin()`. There is no
/// equivalent without a Win32 binding, so this asks `net session`, which only an
/// administrator can run. See `docs/divergences.md`.
#[cfg(not(unix))]
fn validate_administrator(_system: System) -> Result<(), Failure> {
    let elevated = std::process::Command::new("net")
        .arg("session")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    if !elevated {
        return Err(Failure::new(
            exit::GENERAL_ERROR,
            "You must run this command as an Administrator.",
        ));
    }
    Ok(())
}
