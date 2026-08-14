//! `awsc` — the CLI entry point.
//!
//! Dispatch is model-driven: `awsc <service> <operation> [--flags]` loads the service
//! model, binds the operation's arguments from its input shape, serializes, signs, sends
//! and formats. Nothing here is specific to any one service.
//!
//! All six AWS wire protocols are dispatched from `dispatch.rs`; `rpcv2Cbor` is
//! recognised and refused explicitly rather than mis-serialized.

use aws_cli_model::Model;
use aws_cli_runtime::{credentials, endpoint, http, sigv4};
use std::process::ExitCode;

mod args;
mod dispatch;
mod paginate;
mod exit;

const USAGE: &str = "\
usage: awsc <command> <subcommand> [parameters]

A Rust port of the AWS CLI. Options:
  --region <region>        region to call
  --profile <name>         credentials profile
  --output <format>        json (default)
  --endpoint-url <url>     override the resolved endpoint
  --debug                  print the signed request to stderr
  --version                print version
";

/// The block the reference prints after an argument-parsing failure.
const USAGE_HINT: &str = "\
usage: aws [options] <command> <subcommand> [<subcommand> ...] [parameters]
To see help text, you can run:

  aws help
  aws <command> help
  aws <command> <subcommand> help";

/// An error paired with the exit code the reference would use for it.
struct Failure {
    message: String,
    code: u8,
}

impl Failure {
    fn new(code: u8, message: impl std::fmt::Display) -> Self {
        Failure { message: message.to_string(), code }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(f) => {
            // The reference prefixes every error with a blank line — verified across
            // service errors, unknown profiles and SSO failures. Cosmetic, but it is a
            // byte difference in stderr and this is a drop-in replacement.
            eprintln!("\naws: [ERROR]: {}", f.message);
            exit::code(f.code)
        }
    }
}

fn run() -> Result<ExitCode, Failure> {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    let parsed = match args::parse(&argv) {
        Ok(args::Outcome::Run(p)) => p,
        // `help`/`--version` succeed; bare `awsc` prints usage but is a usage error, at
        // 252, matching the reference.
        Ok(args::Outcome::Help) => {
            print!("{USAGE}");
            return Ok(exit::code(exit::SUCCESS));
        }
        Ok(args::Outcome::Usage) => {
            eprint!("{USAGE}");
            return Ok(exit::code(exit::PARAM_VALIDATION));
        }
        Err(e) => return Err(Failure::new(exit::PARAM_VALIDATION, e)),
    };

    let model = load_model(&parsed.service)
        .map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;

    let protocol = model.protocol().map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;
    // The paginator overlay is keyed by CLI service name, which is not always the
    // name the user typed (aliases) nor the model filename.
    let cli_service =
        model.cli_service_name().map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;

    let (op_id, op) = model
        .operation(&parsed.operation)
        .map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let input_shape =
        model.operation_input(op).map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;
    let output_shape =
        model.operation_output(op).map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;

    // --generate-cli-skeleton short-circuits the API call entirely, exit 0.
    if let Some(mode) = &parsed.generate_skeleton {
        match mode.as_str() {
            "input" => {}
            // `yaml-input` annotates every member with `# [REQUIRED] <docs>`, and
            // `output` runs full parameter validation before stubbing the response.
            // Neither is implemented, and emitting a plausible-but-different document is
            // worse than refusing.
            "yaml-input" => {
                return Err(Failure::new(
                    exit::GENERAL_ERROR,
                    "--generate-cli-skeleton yaml-input is not implemented yet \
                     (it annotates each member with its documentation)",
                ))
            }
            // `output` still validates the real parameters before printing the
            // stubbed response shape, which is what makes it a checking mode.
            "output" => {}
            other => {
                return Err(Failure::new(
                    exit::PARAM_VALIDATION,
                    format!("invalid --generate-cli-skeleton value `{other}`"),
                ))
            }
        }
        if mode == "output" {
            let built = args::build_input(&model, input_shape, &parsed.parameters)
                .map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
            let validation =
                aws_cli_protocol::validate::validate(&model, input_shape, built.as_ref());
            if !validation.is_empty() {
                return Err(Failure::new(
                    exit::PARAM_VALIDATION,
                    aws_cli_runtime::RuntimeError::ParamValidation(validation.report()),
                ));
            }
        }
        let skeleton = match mode.as_str() {
            "output" => args::generate_skeleton(&model, output_shape, true),
            _ => args::generate_skeleton(&model, input_shape, false),
        };
        // The reference stubs this skeleton as the response, and the stubber validates
        // it against the output shape — so a placeholder that violates the shape's own
        // constraints fails. `sts get-caller-identity` is exactly that case: the
        // generated `Arn: "Arn"` is 3 characters against a minimum of 20.
        if mode == "output" {
            let validation =
                aws_cli_protocol::validate::validate(&model, output_shape, Some(&skeleton));
            if !validation.is_empty() {
                return Err(Failure::new(
                    exit::PARAM_VALIDATION,
                    aws_cli_runtime::RuntimeError::ParamValidation(validation.report()),
                ));
            }
        }
        match aws_cli_output::render_named(op_id.name(), &skeleton, parsed.output) {
            Ok(Some(text)) => print!("{text}"),
            Ok(None) => {}
            Err(e) => return Err(Failure::new(exit::GENERAL_ERROR, e)),
        }
        return Ok(exit::code(exit::SUCCESS));
    }

    // Required flags are enforced here, before model validation, because the reference
    // reports them with argparse's wording and a usage block rather than as a parameter
    // validation failure.
    let missing = args::missing_required_flags(input_shape, &parsed);
    if !missing.is_empty() {
        return Err(Failure::new(
            exit::PARAM_VALIDATION,
            format!(
                "{}\n\n{USAGE_HINT}",
                aws_cli_runtime::RuntimeError::ParamValidation(format!(
                    "the following arguments are required: {}",
                    missing.join(", ")
                ))
            ),
        ));
    }

    // Unknown flags are rejected here rather than dropped: silently ignoring a parameter
    // the user supplied would send a request they did not ask for.
    let mut input = args::build_input(&model, input_shape, &parsed.parameters)
        .map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;

    // --cli-input-json/yaml fills in top-level keys the command line did not set. The
    // command line wins, and the fill is shallow: a key set by an argument discards the
    // document's value for it wholesale.
    if let Some(raw) = &parsed.cli_input {
        let text = args::expand_paramfile(raw)
            .map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
        let document: serde_json::Value = serde_json::from_str(&text)
            .map_err(|_| Failure::new(exit::PARAM_VALIDATION, "Invalid JSON received."))?;
        let mut built = input.take().unwrap_or_else(|| serde_json::Value::Object(Default::default()));
        args::merge_cli_input(&mut built, &document)
            .map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
        input = Some(built);
    }

    // The ruleset decides the endpoint, and may supply a signing region that differs
    // from the client region. A region is still required: services without a global
    // endpoint produce a rule error otherwise, which is reported as configuration (253)
    // to match the reference.
    let region = endpoint::resolve_region(parsed.region.as_deref(), None);
    let ep_params = endpoint::EndpointParams {
        region,
        endpoint_url: parsed.endpoint_url.clone(),
        ..Default::default()
    };
    let ep = endpoint::resolve(&model, &ep_params).map_err(|e| match e {
        // A ruleset that rejects the inputs, or a missing region, is a configuration
        // problem (253). The ruleset's own wording beats anything we would substitute.
        endpoint::EndpointError::Rules(_) | endpoint::EndpointError::NoRegion => {
            Failure::new(exit::CONFIGURATION, e)
        }
        other => Failure::new(exit::GENERAL_ERROR, other),
    })?;
    let creds = credentials::resolve(parsed.profile.as_deref(), Some(&ep.signing_region))
        .map_err(|e| {
        // Only "no credentials found at all" is a configuration error; an unknown
        // profile or an expired SSO token is general (255). Matches the reference.
            let code = if e.is_configuration_error() {
                exit::CONFIGURATION
            } else if e.is_client_error() {
                exit::CLIENT_ERROR
            } else {
                exit::GENERAL_ERROR
            };
            Failure::new(code, e)
        })?;

    // Client-side validation runs before any network work, exactly as the reference
    // does — the error text and exit code both differ from letting the service reject.
    let validation = aws_cli_protocol::validate::validate(&model, input_shape, input.as_ref());
    if !validation.is_empty() {
        return Err(Failure::new(
            exit::PARAM_VALIDATION,
            aws_cli_runtime::RuntimeError::ParamValidation(validation.report()),
        ));
    }

    let transport = http::Transport {
        verify_ssl: parsed.verify_ssl,
        ca_bundle: parsed.ca_bundle.clone(),
        read_timeout: parsed.read_timeout,
        connect_timeout: parsed.connect_timeout,
    };

    // One round trip. Kept as a closure so pagination can issue it repeatedly with a
    // different token injected, without re-resolving credentials or the endpoint.
    let issue = |input: Option<&serde_json::Value>| -> Result<serde_json::Value, Failure> {
        let wire = dispatch::serialize(&model, protocol, op_id.name(), op, input_shape, input)
            .map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;

        let request = http::PreparedRequest {
            method: wire.method,
            endpoint: ep.clone(),
            path: wire.path,
            query: wire.query,
            content_type: wire.content_type,
            extra_headers: wire.headers,
            body: wire.body,
        };

        let timestamp = sigv4::format_timestamp(now_unix());
        let (headers, signature) = if parsed.no_sign_request {
            (http::unsigned_headers(&request), None)
        } else {
            let (h, s) = http::sign_request(&request, &creds, &timestamp);
            (h, Some(s))
        };

        if parsed.debug {
            eprintln!("endpoint: {}", request.endpoint.url);
            eprintln!("body: {}", request.body);
            if let Some(signature) = &signature {
                eprintln!("CanonicalRequest:\n{}", signature.canonical_request);
                eprintln!("StringToSign:\n{}", signature.string_to_sign);
                eprintln!("Signature:\n{}", signature.signature);
            }
        }

        let response = http::send(&request, &headers, &transport)
            .map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;

        if response.status >= 400 {
            let (code, message) = dispatch::parse_error(
                protocol,
                &response.body,
                response.header("x-amzn-errortype").as_deref(),
            );
            return Err(Failure::new(
                exit::CLIENT_ERROR,
                aws_cli_runtime::RuntimeError::Service {
                    code,
                    message,
                    operation: op_id.name().to_string(),
                },
            ));
        }

        dispatch::parse_response(&model, protocol, op_id.name(), output_shape, &response.body)
            .map_err(|e| Failure::new(exit::GENERAL_ERROR, e))
    };

    let value = paginate::run(&paginate::Settings {
        service: &cli_service,
        operation: &parsed.operation,
        input: input.clone(),
        no_paginate: parsed.no_paginate,
        max_items: parsed.max_items,
        page_size: parsed.page_size,
        starting_token: parsed.starting_token.clone(),
    }, issue)?;

    // --query runs after the pagination merge and after ResponseMetadata removal, so an
    // expression can never see either. Matches the reference's ordering.
    let value = match &parsed.query {
        Some(expression) => aws_cli_output::query::apply(&value, expression)
            .map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?,
        None => value,
    };

    // The table titles itself with the API operation name (`GetCallerIdentity`), not the
    // CLI spelling (`get-caller-identity`).
    match aws_cli_output::render_named(op_id.name(), &value, parsed.output) {
        Ok(Some(text)) => print!("{text}"),
        Ok(None) => {}
        Err(e) => return Err(Failure::new(exit::GENERAL_ERROR, e)),
    }
    Ok(exit::code(exit::SUCCESS))
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is after 1970")
        .as_secs() as i64
}

/// Locate and load a service model by CLI service name.
///
/// `models/` is named by aws-sdk-rust's conventions, not the CLI's — `logs` lives in
/// `cloudwatch-logs.json` — so the fast path tries the obvious filename and the fallback
/// resolves each candidate's own CLI name rather than guessing.
fn load_model(cli_service: &str) -> Result<Model, String> {
    let dir = models_dir();
    let direct = dir.join(format!("{cli_service}.json"));
    if let Ok(bytes) = std::fs::read(&direct) {
        if let Ok(model) = Model::from_json(&bytes) {
            if model.cli_service_name().is_ok_and(|n| n == cli_service) {
                return Ok(model);
            }
        }
    }

    let entries = std::fs::read_dir(&dir).map_err(|e| {
        format!(
            "cannot read models directory {} ({e}); run scripts/fetch-models.sh",
            dir.display()
        )
    })?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let Ok(model) = Model::from_json(&bytes) else { continue };
        if model.cli_service_name().is_ok_and(|n| n == cli_service) {
            return Ok(model);
        }
    }
    Err(format!("unknown service `{cli_service}`"))
}

fn models_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("AWSC_MODELS_DIR") {
        return dir.into();
    }
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models")
}
