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

    // Unknown flags are rejected here rather than dropped: silently ignoring a parameter
    // the user supplied would send a request they did not ask for.
    let input = args::build_input(&model, input_shape, &parsed.parameters)
        .map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;

    let wire = dispatch::serialize(
        &model,
        protocol,
        op_id.name(),
        op,
        input_shape,
        input.as_ref(),
    )
    .map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;

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
        let (headers, signature) = http::sign_request(&request, &creds, &timestamp);

        if parsed.debug {
            eprintln!("endpoint: {}", request.endpoint.url);
            eprintln!("body: {}", request.body);
            eprintln!("CanonicalRequest:\n{}", signature.canonical_request);
            eprintln!("StringToSign:\n{}", signature.string_to_sign);
            eprintln!("Signature:\n{}", signature.signature);
        }

        let response =
            http::send(&request, &headers).map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;

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

    match aws_cli_output::render(&value, parsed.output) {
        Ok(Some(text)) => println!("{text}"),
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
