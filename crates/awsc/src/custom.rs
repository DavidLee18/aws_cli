//! Custom commands: the CLI surface that is not derived from a service model.
//!
//! The reference builds these as `BasicCommand`s injected into a service's command table
//! on the `building-command-table.<service>` event. They never receive the universal
//! injected flags, so each declares its own complete argument list, and their output is
//! written straight to stdout rather than passing through the `--output` formatter —
//! `--output json` does not turn `ecr get-login-password` into a JSON document.
//!
//! Commands live here only once their behaviour has been read out of the reference
//! source. A command the reference has and we do not is left unhandled, so it falls
//! through to the model lookup and reports an unknown-operation error, rather than being
//! answered with a plausible guess.

use crate::args::Parsed;
use crate::client::{Client, Globals};
use crate::exit;
use crate::Failure;
use serde_json::Value;
use std::collections::BTreeMap;
use std::process::ExitCode;

/// Run a custom command if `parsed` names one. `Ok(None)` means it does not.
pub fn dispatch(parsed: &Parsed) -> Result<Option<ExitCode>, Failure> {
    let globals = Globals::from_parsed(parsed);
    let outcome = match (parsed.service.as_str(), parsed.operation.as_str()) {
        ("ecr", "get-login-password") => get_login_password(parsed, &globals, false)?,
        ("ecr-public", "get-login-password") => get_login_password(parsed, &globals, true)?,
        ("configservice", "get-status") => configservice_get_status(parsed, &globals)?,
        ("rds", "generate-db-auth-token") => generate_db_auth_token(parsed, &globals)?,
        ("codecommit", "credential-helper") => codecommit_credential_helper(parsed, &globals)?,
        ("eks", "get-token") => eks_get_token(parsed, &globals)?,
        ("configservice", "subscribe") => configservice_subscribe(parsed, &globals)?,
        ("logs", "tail") => crate::logs_tail::run(parsed, &globals)?,
        // The whole s3 tree is custom: it has no model of its own.
        ("s3", _) => crate::s3::dispatch(parsed, &globals)?,
        // As is `configure`, which mostly edits the config files rather than calling AWS.
        ("configure", _) => crate::configure::dispatch(parsed)?,
        _ => return Ok(None),
    };
    Ok(Some(outcome))
}

/// `An error occurred (ParamValidation): Unknown options: --a,1,--b`
///
/// The reference joins the *raw argv tokens* with a comma, so `--flag value` contributes
/// two entries and `--flag=value` contributes one.
fn unknown_options(extras: &[String]) -> Failure {
    Failure::new(
        exit::PARAM_VALIDATION,
        aws_cli_runtime::RuntimeError::ParamValidation(format!(
            "Unknown options: {}",
            extras.join(",")
        )),
    )
}

/// The reference's argparse wording for a missing required flag, with the usage block.
fn missing_required(missing: &[&str]) -> Failure {
    Failure::new(
        exit::PARAM_VALIDATION,
        format!(
            "{}\n\n{}",
            aws_cli_runtime::RuntimeError::ParamValidation(format!(
                "the following arguments are required: {}",
                missing.join(", ")
            )),
            crate::USAGE_HINT
        ),
    )
}

/// Pull the declared flags out, and reject anything left over.
///
/// The reference's argparse does both; here it has to be explicit, because silently
/// ignoring a flag would produce a request the user did not ask for.
fn take_args<'a>(
    parsed: &'a Parsed,
    accepted: &[&str],
) -> Result<BTreeMap<&'a str, Option<&'a str>>, Failure> {
    let mut out = BTreeMap::new();
    let mut leftover: Vec<String> = Vec::new();
    let mut skip_next = false;
    for token in &parsed.extras {
        if skip_next {
            skip_next = false;
            continue;
        }
        let (name, inline) = match token.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (token.as_str(), None),
        };
        if !accepted.contains(&name) {
            leftover.push(token.clone());
            continue;
        }
        let value = match inline {
            Some(v) => Some(v),
            // `parameters` already resolved whether a following token was consumed as
            // this flag's value; reuse that decision so the two stay in step.
            None => parsed.parameters.get(name).and_then(|v| v.as_deref()),
        };
        if inline.is_none() && value.is_some() {
            skip_next = true;
        }
        out.insert(name, value);
    }
    if !leftover.is_empty() {
        return Err(unknown_options(&leftover));
    }
    Ok(out)
}

/// `aws ecr get-login-password` / `aws ecr-public get-login-password`.
///
/// Calls `GetAuthorizationToken` with no parameters, base64-decodes the token, and prints
/// the half after the colon. The two differ only in the response shape: ecr models
/// `authorizationData` as a list and the reference takes `[0]`, while ecr-public models it
/// as a single structure and the reference indexes it directly.
///
/// The token is `user:password`, and the reference splits on **every** colon and unpacks
/// into exactly two names — so a token containing more than one colon is an error there,
/// not a password containing a colon. That is reproduced rather than "fixed".
fn get_login_password(
    parsed: &Parsed,
    globals: &Globals,
    public: bool,
) -> Result<ExitCode, Failure> {
    let service = if public { "ecr-public" } else { "ecr" };
    take_args(parsed, &[])?;

    // The endpoint override applies: this is the service the user named, and the
    // reference's `create_client_from_parsed_globals` forwards `endpoint_url` to it.
    let model = crate::load_model(service).map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let client = Client::new(&model, globals)?;
    let response = client.call("get-authorization-token", None)?;

    let auth = if public {
        response.get("authorizationData")
    } else {
        response.get("authorizationData").and_then(|d| d.get(0))
    };
    let encoded = auth
        .and_then(|a| a.get("authorizationToken"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            Failure::new(exit::GENERAL_ERROR, "GetAuthorizationToken returned no authorizationToken")
        })?;

    let decoded = aws_cli_protocol::shapes::base64_decode(encoded)
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .ok_or_else(|| {
            Failure::new(exit::GENERAL_ERROR, "authorizationToken is not valid base64 UTF-8")
        })?;

    let fields: Vec<&str> = decoded.split(':').collect();
    let [_user, password] = fields.as_slice() else {
        return Err(Failure::new(
            exit::GENERAL_ERROR,
            format!(
                "authorization token has {} colon-separated fields, expected 2",
                fields.len()
            ),
        ));
    };

    println!("{password}");
    Ok(exit::code(exit::SUCCESS))
}

/// `aws configservice get-status`.
///
/// Two calls, rendered as plain text. This output does **not** go through `--output`: the
/// reference writes it with `sys.stdout.write`, so `--output json` changes nothing.
fn configservice_get_status(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    take_args(parsed, &[])?;

    let model =
        crate::load_model("configservice").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let client = Client::new(&model, globals)?;

    let mut out = String::new();

    out.push_str("Configuration Recorders:\n\n");
    let recorders = client.call("describe-configuration-recorder-status", None)?;
    for recorder in array(&recorders, "ConfigurationRecordersStatus") {
        out.push_str(&format!("name: {}\n", string(recorder, "name")));
        // The reference maps the boolean through {False: 'OFF', True: 'ON'}.
        let recording = recorder.get("recording").and_then(Value::as_bool).unwrap_or(false);
        out.push_str(&format!("recorder: {}\n", if recording { "ON" } else { "OFF" }));
        if recording {
            last_status(&mut out, recorder, "");
        }
        out.push('\n');
    }

    out.push_str("Delivery Channels:\n\n");
    let channels = client.call("describe-delivery-channel-status", None)?;
    for channel in array(&channels, "DeliveryChannelsStatus") {
        out.push_str(&format!("name: {}\n", string(channel, "name")));
        // Each sub-status is printed only when its object is present and non-empty, and
        // the label carries a trailing space so the recorder case above reads
        // `last status:` with no filler.
        for (key, label) in [
            ("configStreamDeliveryInfo", "stream delivery "),
            ("configHistoryDeliveryInfo", "history delivery "),
            ("configSnapshotDeliveryInfo", "snapshot delivery "),
        ] {
            if let Some(info) = channel.get(key).filter(|v| truthy(v)) {
                last_status(&mut out, info, label);
            }
        }
        out.push('\n');
    }

    print!("{out}");
    Ok(exit::code(exit::SUCCESS))
}

/// `aws rds generate-db-auth-token --hostname H --port P --username U`.
///
/// Entirely local: no request is sent. The token is a presigned URL for a synthetic
/// `connect` action against the database host itself, signed for `rds-db` (not `rds`),
/// with the `https://` prefix stripped off the front.
fn generate_db_auth_token(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let args = take_args(parsed, &["--hostname", "--port", "--username"])?;

    let missing: Vec<&str> = ["--hostname", "--port", "--username"]
        .into_iter()
        .filter(|f| !args.contains_key(f))
        .collect();
    if !missing.is_empty() {
        return Err(missing_required(&missing));
    }
    let value = |flag: &str| args.get(flag).copied().flatten().unwrap_or_default();
    let hostname = value("--hostname");
    let username = value("--username");
    let port_text = value("--port");
    // The reference converts with a bare `int()`, so a non-numeric port escapes as an
    // uncaught ValueError at exit 255 rather than as parameter validation.
    let port: u16 = port_text.parse().map_err(|_| {
        Failure::new(
            exit::GENERAL_ERROR,
            format!("invalid literal for int() with base 10: '{port_text}'"),
        )
    })?;

    let region = resolve_region(globals)
        .ok_or_else(|| Failure::new(exit::CONFIGURATION, aws_cli_runtime::RuntimeError::NoRegion))?;
    let creds = resolve_credentials(globals, &region)?;

    // The signed host omits `:443` and is lowercased, but the emitted URL keeps the port
    // and the hostname's original case.
    let signed_host = if port == 443 {
        hostname.to_ascii_lowercase()
    } else {
        format!("{}:{port}", hostname.to_ascii_lowercase())
    };

    let ctx = aws_cli_runtime::sigv4::SigningContext {
        credentials: &creds,
        region: &region,
        service: "rds-db",
        timestamp: &aws_cli_runtime::sigv4::format_timestamp(crate::now_unix()),
    };
    let query = aws_cli_runtime::presign::presign(
        &ctx,
        &aws_cli_runtime::presign::PresignRequest {
            method: "GET",
            host: &signed_host,
            path: "/",
            params: vec![
                ("Action".into(), "connect".into()),
                ("DBUser".into(), username.into()),
            ],
            extra_signed_headers: Vec::new(),
            expires: 900,
            payload: aws_cli_runtime::presign::Payload::EmptyBody,
        },
    );

    println!("{hostname}:{port}/?{query}");
    Ok(exit::code(exit::SUCCESS))
}

/// `aws codecommit credential-helper get`.
///
/// This is **not** a presigned URL and not standard SigV4. The reference hand-builds a
/// canonical request with the literal method `GIT`, an empty canonical query string, and —
/// critically — an *empty payload-hash field* rather than a SHA-256, and it stamps the
/// time as `%Y%m%dT%H%M%S` with **no trailing `Z`** inside the string-to-sign. The `Z`
/// appears only when the timestamp is concatenated with the signature to form the
/// password. Reproducing those two quirks is the whole job; a "correct" SigV4 signature
/// here would be rejected by CodeCommit.
fn codecommit_credential_helper(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let args = take_args(parsed, &["--ignore-host-check"])?;
    let ignore_host_check = args.contains_key("--ignore-host-check");

    // `store` and `erase` are accepted no-ops; bare `credential-helper` is a usage error.
    let sub = parsed.positionals.first().map(String::as_str).unwrap_or_default();
    match sub {
        "get" => {}
        "store" | "erase" => return Ok(exit::code(exit::SUCCESS)),
        "" => {
            return Err(Failure::new(
                exit::PARAM_VALIDATION,
                format!("usage: aws codecommit credential-helper <get|store|erase>\n\n{}", crate::USAGE_HINT),
            ))
        }
        other => {
            return Err(Failure::new(
                exit::PARAM_VALIDATION,
                format!("Invalid choice: '{other}', maybe you meant:\n\n  * get"),
            ))
        }
    }

    let mut input = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)
        .map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;
    let mut fields: BTreeMap<&str, &str> = BTreeMap::new();
    for line in input.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Split on the FIRST `=`; a line without one is a hard error upstream too.
        let Some((key, value)) = line.split_once('=') else {
            return Err(Failure::new(
                exit::GENERAL_ERROR,
                "not enough values to unpack (expected 2, got 1)",
            ));
        };
        fields.insert(key, value);
    }

    let get = |key: &str| -> Result<&str, Failure> {
        fields
            .get(key)
            .copied()
            .ok_or_else(|| Failure::new(exit::GENERAL_ERROR, format!("'{key}'")))
    };
    let host = get("host")?;

    // A plain substring test, exactly as the reference writes it. A host that fails the
    // check produces no output and still exits 0 — git then falls back to another helper.
    if !(host.contains("amazon.com") || host.contains("amazonaws.com") || ignore_host_check) {
        return Ok(exit::code(exit::SUCCESS));
    }

    let protocol = get("protocol")?;
    let path = get("path")?;
    let url = format!("{protocol}://{host}/{path}");

    let region = codecommit_region(host)
        .map(str::to_string)
        .or_else(|| resolve_region(globals))
        .ok_or_else(|| Failure::new(exit::CONFIGURATION, aws_cli_runtime::RuntimeError::NoRegion))?;
    let creds = resolve_credentials(globals, &region)?;

    // The port is stripped from the signed host but its case is preserved — this uses
    // `netloc.split(':')[0]`, not the lowercasing `_host_from_url` used elsewhere.
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(&url);
    let netloc = after_scheme.split('/').next().unwrap_or_default();
    let signed_host = netloc.split(':').next().unwrap_or_default();
    let url_path = &after_scheme[netloc.len()..];

    let canonical_request = format!("GIT\n{url_path}\n\nhost:{signed_host}\n\nhost\n");

    let stamp = aws_cli_runtime::sigv4::format_timestamp(crate::now_unix());
    // `%Y%m%dT%H%M%S` — the reference's timestamp has no `Z` inside the string-to-sign.
    let unzoned = stamp.trim_end_matches('Z');
    let ctx = aws_cli_runtime::sigv4::SigningContext {
        credentials: &creds,
        region: &region,
        service: "codecommit",
        timestamp: unzoned,
    };
    let (_, signature) =
        aws_cli_runtime::sigv4::sign_canonical_request(&ctx, &canonical_request);

    let mut username = creds.access_key_id.clone();
    if let Some(token) = &creds.session_token {
        // Appended raw after a literal `%`, deliberately not URL-encoded.
        username.push('%');
        username.push_str(token);
    }
    println!("username={username}");
    println!("password={unzoned}Z{signature}");
    Ok(exit::code(exit::SUCCESS))
}

/// `aws eks get-token --cluster-name N | --cluster-id N [--role-arn R]`.
///
/// The token is a presigned STS `GetCallerIdentity` URL, 60-second expiry, with the
/// cluster name bound in as the signed header `x-k8s-aws-id` — the header never appears
/// as a query parameter, only inside `X-Amz-SignedHeaders`. The URL is then base64url
/// encoded with the padding stripped and prefixed with `k8s-aws-v1.`.
///
/// Two output quirks are reproduced: the document goes through the normal `--output`
/// formatter (so `--output text` really does emit tab-separated rows), and the command
/// writes an *extra* newline after it, so JSON output ends `}\n\n`.
fn eks_get_token(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let args = take_args(parsed, &["--cluster-name", "--cluster-id", "--role-arn"])?;
    let cluster_name = args.get("--cluster-name").copied().flatten();
    let cluster_id = args.get("--cluster-id").copied().flatten();

    if cluster_name.is_some() && cluster_id.is_some() {
        return Err(Failure::new(
            exit::PARAM_VALIDATION,
            aws_cli_runtime::RuntimeError::ParamValidation(
                "The key \"cluster_id\" cannot be specified when one of the following \
                 keys are also specified: cluster_name"
                    .to_string(),
            ),
        ));
    }
    // `cluster_id` wins when only one is set. The reference *returns* the ValueError
    // rather than raising it, so this case exits 1 — not one of the usual codes.
    let Some(identifier) = cluster_id.or(cluster_name) else {
        return Err(Failure::bare(
            1,
            "Either parameter --cluster-name or --cluster-id must be specified.",
        ));
    };

    let region = resolve_region(globals)
        .ok_or_else(|| Failure::new(exit::CONFIGURATION, aws_cli_runtime::RuntimeError::NoRegion))?;

    // `--endpoint-url` is not forwarded here: the reference builds the STS client without
    // it, so an override aimed at EKS must not redirect the token's STS endpoint.
    let sts_globals = Globals { region: Some(region.clone()), ..globals.for_other_service() };
    let sts_model =
        crate::load_model("sts").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let sts = Client::new(&sts_model, &sts_globals)?;

    // With --role-arn, a first STS call swaps in temporary credentials, and those sign
    // the token.
    let credentials = match args.get("--role-arn").copied().flatten() {
        None => sts.credentials.clone(),
        Some(role_arn) => {
            let assumed = sts.call(
                "assume-role",
                Some(&serde_json::json!({
                    "RoleArn": role_arn,
                    "RoleSessionName": "EKSGetTokenAuth",
                })),
            )?;
            let creds = assumed.get("Credentials").ok_or_else(|| {
                Failure::new(exit::GENERAL_ERROR, "AssumeRole returned no Credentials")
            })?;
            aws_cli_runtime::credentials::Credentials {
                access_key_id: string(creds, "AccessKeyId").to_string(),
                secret_access_key: string(creds, "SecretAccessKey").to_string(),
                session_token: Some(string(creds, "SessionToken").to_string()),
                expires_at: None,
                method: "assume-role",
            }
        }
    };

    let now = crate::now_unix();
    let ctx = aws_cli_runtime::sigv4::SigningContext {
        credentials: &credentials,
        region: &sts.endpoint.signing_region,
        service: &sts.endpoint.signing_name,
        timestamp: &aws_cli_runtime::sigv4::format_timestamp(now),
    };
    let query = aws_cli_runtime::presign::presign(
        &ctx,
        &aws_cli_runtime::presign::PresignRequest {
            method: "GET",
            host: &sts.endpoint.host,
            path: "/",
            params: vec![
                ("Action".into(), "GetCallerIdentity".into()),
                ("Version".into(), "2011-06-15".into()),
            ],
            extra_signed_headers: vec![("x-k8s-aws-id".into(), identifier.to_string())],
            expires: 60,
            payload: aws_cli_runtime::presign::Payload::EmptyBody,
        },
    );
    let url = format!("{}/?{query}", sts.endpoint.url.trim_end_matches('/'));
    let token = format!("k8s-aws-v1.{}", base64url_unpadded(url.as_bytes()));

    // 14 minutes, stamped after the token is built, so it can land a second later than
    // `X-Amz-Date` + 14min.
    let expiration = format_rfc3339(crate::now_unix() + 14 * 60);

    let document = serde_json::json!({
        "kind": "ExecCredential",
        "apiVersion": discover_api_version(),
        "spec": {},
        "status": { "expirationTimestamp": expiration, "token": token },
    });

    let document = match &parsed.query {
        Some(expression) => aws_cli_output::query::apply(&document, expression)
            .map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?,
        None => document,
    };
    match aws_cli_output::render_named("get-token", &document, parsed.output) {
        Ok(Some(text)) => print!("{text}"),
        Ok(None) => {}
        Err(e) => return Err(Failure::new(exit::GENERAL_ERROR, e)),
    }
    // The command's own trailing newline, on top of the formatter's.
    println!();
    Ok(exit::code(exit::SUCCESS))
}

const DEFAULT_EXEC_API_VERSION: &str = "client.authentication.k8s.io/v1beta1";

/// Read `KUBERNETES_EXEC_INFO` for the `apiVersion` to echo back.
///
/// Every failure mode falls back to v1beta1 with its own warning on stderr; `v1alpha1` is
/// passed through *with* a warning, which is the one case where the warning does not mean
/// the value was discarded.
fn discover_api_version() -> String {
    let Ok(raw) = std::env::var("KUBERNETES_EXEC_INFO") else {
        return DEFAULT_EXEC_API_VERSION.to_string();
    };
    if raw.is_empty() {
        return DEFAULT_EXEC_API_VERSION.to_string();
    }
    let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
        eprintln!(
            "Error parsing KUBERNETES_EXEC_INFO, defaulting to {DEFAULT_EXEC_API_VERSION}. \
             This is likely a bug in your Kubernetes client. Please update your Kubernetes client."
        );
        return DEFAULT_EXEC_API_VERSION.to_string();
    };
    match parsed.get("apiVersion").and_then(Value::as_str) {
        Some("client.authentication.k8s.io/v1") => "client.authentication.k8s.io/v1".to_string(),
        Some(DEFAULT_EXEC_API_VERSION) => DEFAULT_EXEC_API_VERSION.to_string(),
        Some("client.authentication.k8s.io/v1alpha1") => {
            eprintln!(
                "Kubeconfig user entry is using deprecated API version \
                 client.authentication.k8s.io/v1alpha1. Run 'aws eks update-kubeconfig' to update."
            );
            "client.authentication.k8s.io/v1alpha1".to_string()
        }
        _ => {
            eprintln!(
                "Unrecognized API version in KUBERNETES_EXEC_INFO, defaulting to \
                 {DEFAULT_EXEC_API_VERSION}. This is likely due to an outdated AWS CLI. \
                 Please update your AWS CLI."
            );
            DEFAULT_EXEC_API_VERSION.to_string()
        }
    }
}

/// `base64.urlsafe_b64encode(...).rstrip('=')`.
fn base64url_unpadded(bytes: &[u8]) -> String {
    aws_cli_protocol::shapes::base64_encode(bytes)
        .trim_end_matches('=')
        .replace('+', "-")
        .replace('/', "_")
}

/// `%Y-%m-%dT%H:%M:%SZ`, built from the sigv4 formatter so there is one date routine.
fn format_rfc3339(unix_seconds: i64) -> String {
    let compact = aws_cli_runtime::sigv4::format_timestamp(unix_seconds);
    format!(
        "{}-{}-{}T{}:{}:{}Z",
        &compact[0..4],
        &compact[4..6],
        &compact[6..8],
        &compact[9..11],
        &compact[11..13],
        &compact[13..15]
    )
}

/// The region embedded in a CodeCommit host, which wins over `--region`.
///
/// `re.match` anchors at the start only, so anything may follow the matched prefix.
fn codecommit_region(host: &str) -> Option<&str> {
    static PATTERN: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"^(vpce-.+\.)?git-codecommit(-fips)?\.([^.]+)\.(vpce\.)?amazonaws\.com",
        )
        .expect("codecommit host pattern is valid")
    });
    PATTERN.captures(host).and_then(|c| c.get(3)).map(|m| m.as_str())
}

/// The region, honouring the profile's `region` key as botocore's precedence does.
fn resolve_region(globals: &Globals) -> Option<String> {
    let profile_region =
        aws_cli_runtime::credentials::profile::profile_region(globals.profile.as_deref());
    aws_cli_runtime::endpoint::resolve_region(globals.region.as_deref(), profile_region.as_deref())
}

fn resolve_credentials(
    globals: &Globals,
    region: &str,
) -> Result<aws_cli_runtime::credentials::Credentials, Failure> {
    aws_cli_runtime::credentials::resolve(globals.profile.as_deref(), Some(region)).map_err(|e| {
        let code = if e.is_configuration_error() {
            exit::CONFIGURATION
        } else if e.is_client_error() {
            exit::CLIENT_ERROR
        } else {
            exit::GENERAL_ERROR
        };
        Failure::new(code, e)
    })
}

/// `aws configservice subscribe --s3-bucket B[/prefix] --sns-topic T --iam-role R`.
///
/// Orchestrates three services: it ensures the S3 bucket and SNS topic exist, then points
/// a Config delivery channel at them and starts the recorder.
///
/// `--endpoint-url` applies **only** to the Config calls. The reference is explicit about
/// this ("Use the specified endpoint only for config related commands"), and it matters:
/// an override aimed at Config must not redirect the bucket check to a Config endpoint.
fn configservice_subscribe(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let args = take_args(parsed, &["--s3-bucket", "--sns-topic", "--iam-role"])?;
    let missing: Vec<&str> = ["--s3-bucket", "--sns-topic", "--iam-role"]
        .into_iter()
        .filter(|f| !args.contains_key(f))
        .collect();
    if !missing.is_empty() {
        return Err(missing_required(&missing));
    }
    let value = |flag: &str| args.get(flag).copied().flatten().unwrap_or_default();
    let s3_bucket = value("--s3-bucket");
    let sns_topic = value("--sns-topic");
    let iam_role = value("--iam-role");

    // `bucket/prefix`, prefix optional — split on the FIRST slash.
    let (bucket, prefix) = match s3_bucket.split_once('/') {
        Some((b, p)) => (b, p),
        None => (s3_bucket, ""),
    };

    let region = resolve_region(globals)
        .ok_or_else(|| Failure::new(exit::CONFIGURATION, aws_cli_runtime::RuntimeError::NoRegion))?;
    let other = Globals { region: Some(region.clone()), ..globals.for_other_service() };

    // "s3api" is the CLI's name for the modelled S3 service; plain "s3" is the separate
    // high-level command tree and resolves to no model.
    let s3_model =
        crate::load_model("s3api").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let s3 = Client::new(&s3_model, &other)?;

    // The reference converts the error code with a bare `int()`, so only a numeric code
    // is understood: 404 means "absent", any other number means "present". A non-numeric
    // code such as AccessDenied escapes as a ValueError rather than being handled.
    let exists = match s3.call("head-bucket", Some(&serde_json::json!({ "Bucket": bucket }))) {
        Ok(_) => true,
        Err(failure) => {
            let code = failure.service_error_code.clone().unwrap_or_default();
            match code.parse::<i64>() {
                Ok(404) => false,
                Ok(_) => true,
                Err(_) => {
                    return Err(Failure::new(
                        exit::GENERAL_ERROR,
                        format!("invalid literal for int() with base 10: '{code}'"),
                    ))
                }
            }
        }
    };

    if exists {
        print!("Using existing S3 bucket: {bucket}\n");
    } else {
        let mut input = serde_json::json!({ "Bucket": bucket });
        // us-east-1 must NOT carry a LocationConstraint; S3 rejects it there.
        if region != "us-east-1" {
            input["CreateBucketConfiguration"] =
                serde_json::json!({ "LocationConstraint": region });
        }
        s3.call("create-bucket", Some(&input))?;
        print!("Using new S3 bucket: {bucket}\n");
    }

    // An ARN is detected by nothing more than containing a colon.
    let topic_arn = if sns_topic.contains(':') {
        print!("Using existing SNS topic: {sns_topic}\n");
        sns_topic.to_string()
    } else {
        let sns_model =
            crate::load_model("sns").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
        let sns = Client::new(&sns_model, &other)?;
        let created = sns.call("create-topic", Some(&serde_json::json!({ "Name": sns_topic })))?;
        let arn = string(&created, "TopicArn").to_string();
        print!("Using new SNS topic: {arn}\n");
        arn
    };

    let config_model =
        crate::load_model("configservice").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let config = Client::new(&config_model, globals)?;

    config.call(
        "put-configuration-recorder",
        Some(&serde_json::json!({
            "ConfigurationRecorder": { "name": "default", "roleARN": iam_role }
        })),
    )?;

    let mut channel = serde_json::json!({
        "name": "default",
        "s3BucketName": bucket,
        "snsTopicARN": topic_arn,
    });
    if !prefix.is_empty() {
        channel["s3KeyPrefix"] = Value::String(prefix.to_string());
    }
    config.call("put-delivery-channel", Some(&serde_json::json!({ "DeliveryChannel": channel })))?;

    config.call(
        "start-configuration-recorder",
        Some(&serde_json::json!({ "ConfigurationRecorderName": "default" })),
    )?;

    print!("Subscribe succeeded:\n\n");
    print!("Configuration Recorders: ");
    let recorders = config.call("describe-configuration-recorders", None)?;
    print!("{}", python_json(recorders.get("ConfigurationRecorders").unwrap_or(&Value::Null)));
    print!("\n\n");

    print!("Delivery Channels: ");
    let channels = config.call("describe-delivery-channels", None)?;
    print!("{}", python_json(channels.get("DeliveryChannels").unwrap_or(&Value::Null)));
    println!();

    Ok(exit::code(exit::SUCCESS))
}

/// `json.dumps(value, indent=4)`: four-space indent, and with an indent Python's
/// separators become `(',', ': ')` — so no space before the comma.
pub fn python_json(value: &Value) -> String {
    let mut buffer = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"    ");
    let mut serializer = serde_json::Serializer::with_formatter(&mut buffer, formatter);
    serde::Serialize::serialize(value, &mut serializer).expect("serializing a Value cannot fail");
    String::from_utf8(buffer).expect("serde_json emits UTF-8")
}

/// `last <name>status: <s>`, plus the error pair when the status is exactly `FAILURE`.
fn last_status(out: &mut String, status: &Value, name: &str) {
    let last = string(status, "lastStatus");
    out.push_str(&format!("last {name}status: {last}\n"));
    if last == "FAILURE" {
        out.push_str(&format!("error code: {}\n", string(status, "lastErrorCode")));
        out.push_str(&format!("message: {}\n", string(status, "lastErrorMessage")));
    }
}

/// Python truthiness for the values that reach here: a present, non-empty object.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Object(map) => !map.is_empty(),
        Value::Array(items) => !items.is_empty(),
        _ => true,
    }
}

fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value.get(key).and_then(Value::as_array).map(|v| v.as_slice()).unwrap_or(&[])
}

fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Ground truth is the reference's format strings, quoted in
    /// `awscli/customizations/configservice/getstatus.py`. A recorder that is OFF prints
    /// no status line, and each block is followed by a blank line.
    #[test]
    fn renders_a_failed_delivery_channel() {
        let mut out = String::new();
        let info = json!({
            "lastStatus": "FAILURE",
            "lastErrorCode": "NoSuchBucket",
            "lastErrorMessage": "The bucket does not exist"
        });
        last_status(&mut out, &info, "stream delivery ");
        assert_eq!(
            out,
            "last stream delivery status: FAILURE\n\
             error code: NoSuchBucket\n\
             message: The bucket does not exist\n"
        );
    }

    /// The recorder case passes an empty name, giving `last status:` with no filler.
    #[test]
    fn recorder_status_has_no_label_filler() {
        let mut out = String::new();
        last_status(&mut out, &json!({"lastStatus": "SUCCESS"}), "");
        assert_eq!(out, "last status: SUCCESS\n");
    }

    /// The host-embedded region wins over `--region`, and the pattern is anchored only at
    /// the start, so a suffix after `.amazonaws.com` still matches.
    #[test]
    fn extracts_the_region_from_codecommit_hosts() {
        assert_eq!(codecommit_region("git-codecommit.us-east-1.amazonaws.com"), Some("us-east-1"));
        assert_eq!(
            codecommit_region("git-codecommit-fips.eu-west-1.amazonaws.com"),
            Some("eu-west-1")
        );
        assert_eq!(
            codecommit_region("vpce-0a1b.git-codecommit.ap-south-1.vpce.amazonaws.com"),
            Some("ap-south-1")
        );
        assert_eq!(codecommit_region("github.com"), None);
    }

    /// base64url with the padding stripped, which is what `k8s-aws-v1.` tokens use.
    #[test]
    fn encodes_base64url_without_padding() {
        // Bytes chosen to force both `+`/`-` and `/`/`_` substitutions.
        assert_eq!(base64url_unpadded(&[0xfb, 0xff, 0xbe]), "-_--");
        assert_eq!(base64url_unpadded(b"a"), "YQ");
        assert_eq!(base64url_unpadded(b""), "");
    }

    #[test]
    fn formats_the_expiration_timestamp() {
        assert_eq!(format_rfc3339(1_786_657_696), "2026-08-13T21:48:16Z");
    }

    /// `json.dumps(..., indent=4)`: four spaces, and no space before a comma.
    #[test]
    fn matches_python_json_dumps_with_indent() {
        let value = json!([{"name": "default", "recordingGroup": {"allSupported": true}}]);
        assert_eq!(
            python_json(&value),
            "[\n    {\n        \"name\": \"default\",\n        \
             \"recordingGroup\": {\n            \"allSupported\": true\n        }\n    }\n]"
        );
        assert_eq!(python_json(&json!([])), "[]");
    }

    /// An empty sub-status object is falsy in Python and prints nothing.
    #[test]
    fn empty_delivery_info_is_falsy() {
        assert!(!truthy(&json!({})));
        assert!(!truthy(&Value::Null));
        assert!(truthy(&json!({"lastStatus": "SUCCESS"})));
    }
}
