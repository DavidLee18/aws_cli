//! `awsc` — the CLI entry point.
//!
//! Dispatch is model-driven: `awsc <service> <operation> [--flags]` loads the service
//! model, binds the operation's arguments from its input shape, serializes, signs, sends
//! and formats. Nothing here is specific to any one service.
//!
//! All six AWS wire protocols are dispatched from `dispatch.rs`; `rpcv2Cbor` is
//! recognised and refused explicitly rather than mis-serialized.

use aws_cli_model::Model;
use std::process::ExitCode;

mod args;
mod client;
mod custom;
mod dispatch;
mod logs_tail;
mod paginate;
mod s3;
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
#[derive(Debug)]
pub struct Failure {
    message: String,
    code: u8,
    /// Print the message alone, with none of the usual decoration.
    ///
    /// A handful of custom commands *return* an exception object instead of raising it,
    /// so Python's `sys.exit` prints `str(obj)` bare and exits 1 — no leading blank line
    /// and no `aws: [ERROR]:` prefix. `eks get-token` with neither cluster flag is one.
    raw: bool,
    /// Printed before the error line, without decoration.
    ///
    /// argparse writes its usage block *first* and the message after it, which is the
    /// opposite order from every other error the CLI reports.
    preamble: Option<String>,
    /// The service's own error code, when this came from a modelled error response.
    ///
    /// Kept alongside the formatted message because some commands branch on it —
    /// `configservice subscribe` treats a `404` from `HeadBucket` as "create it" and
    /// anything else as "it exists".
    pub service_error_code: Option<String>,
}

impl Failure {
    pub fn new(code: u8, message: impl std::fmt::Display) -> Self {
        Failure { message: message.to_string(), code, raw: false, preamble: None, service_error_code: None }
    }

    /// The exit code this failure carries.
    pub fn exit_code(&self) -> u8 {
        self.code
    }

    /// The message alone, for the few commands that format their own error line.
    /// A parameter error that argparse prints after its usage block.
    pub fn after_usage(message: impl std::fmt::Display) -> Self {
        let mut failure = Failure::new(exit::PARAM_VALIDATION, message);
        failure.preamble = Some(USAGE_HINT.to_string());
        failure
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn bare(code: u8, message: impl std::fmt::Display) -> Self {
        Failure { message: message.to_string(), code, raw: true, preamble: None, service_error_code: None }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(f) => {
            // The reference prefixes every error with a blank line — verified across
            // service errors, unknown profiles and SSO failures. Cosmetic, but it is a
            // byte difference in stderr and this is a drop-in replacement.
            if let Some(preamble) = &f.preamble {
                eprintln!("\n{preamble}\n");
            }
            if f.raw {
                eprintln!("{}", f.message);
            } else {
                eprintln!("\naws: [ERROR]: {}", f.message);
            }
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

    // Custom commands are not modeled operations, so they are dispatched before the model
    // is consulted — `ecr get-login-password` has no `GetLoginPassword` shape to find.
    if let Some(code) = custom::dispatch(&parsed)? {
        return Ok(code);
    }

    // An unknown service is argparse's `argument command`, one level up from `argument
    // operation`; the wording differs only in that word.
    let model = load_model(&parsed.service)
        .map_err(|_| {
            let services = known_services();
            invalid_choice("command", &parsed.service, services.iter().map(String::as_str))
        })?;

    // The paginator overlay is keyed by CLI service name, which is not always the
    // name the user typed (aliases) nor the model filename.
    let cli_service =
        model.cli_service_name().map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;

    // The command table applies every name-level customization: removals, renames and
    // aliases. It is the same derivation the conformance harness uses, so the two cannot
    // disagree about which commands exist.
    let table = aws_cli_model::command_table::build(
        &model,
        aws_cli_model::surface_overlays::get(),
        aws_cli_model::surface_overlays::custom_surface(),
    )
    .map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;

    // An unknown operation and a removed one are reported identically, since argparse
    // cannot tell the difference between a command that never existed and one v2 deleted.
    let unknown_operation = || {
        invalid_choice("operation", &parsed.operation, table.names.keys().map(String::as_str))
    };
    let wire_name = table.resolve(&parsed.operation).ok_or_else(unknown_operation)?;
    let (op_id, op) = model.operation(wire_name).map_err(|_| unknown_operation())?;
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
            let built = args::build_input_named(
                &model,
                input_shape,
                &parsed.parameters,
                &cli_service,
                &parsed.operation,
            )
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

    // Operations whose output is a streaming blob gain a required trailing positional
    // naming the file to write. The reference injects it universally
    // (`customizations/streamingoutputarg.py`), which is why `s3api get-object` and
    // `polly synthesize-speech` take a filename with no flag.
    let streams_output = model.operation_has_streaming_blob_output(op).unwrap_or(false);
    let outfile = if streams_output {
        match parsed.positionals.first() {
            Some(path) => Some(path.clone()),
            None if parsed.generate_skeleton.is_none() => {
                return Err(Failure::new(
                    exit::PARAM_VALIDATION,
                    aws_cli_runtime::RuntimeError::ParamValidation(
                        "the following arguments are required: outfile".to_string(),
                    ),
                ))
            }
            None => None,
        }
    } else {
        None
    };

    // Required flags are enforced here, before model validation, because the reference
    // reports them with argparse's wording and a usage block rather than as a parameter
    // validation failure.
    let missing =
        args::missing_required_flags(&model, input_shape, &parsed, &cli_service, &parsed.operation);
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
    let mut input = args::build_input_named(
        &model,
        input_shape,
        &parsed.parameters,
        &cli_service,
        &parsed.operation,
    )
    .map_err(Failure::after_usage)?;

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
    // from the client region. Credentials and endpoint resolve here, after the skeleton
    // short-circuit, so `--generate-cli-skeleton` still works with no credentials.
    let client = client::Client::for_operation(
        &model,
        &client::Globals::from_parsed(&parsed),
        op,
        input.as_ref(),
    )?;

    // Client-side validation runs before any network work, exactly as the reference
    // does — the error text and exit code both differ from letting the service reject.
    let validation = aws_cli_protocol::validate::validate(&model, input_shape, input.as_ref());
    if !validation.is_empty() {
        return Err(Failure::new(
            exit::PARAM_VALIDATION,
            aws_cli_runtime::RuntimeError::ParamValidation(validation.report()),
        ));
    }

    // Kept as a closure so pagination can issue the call repeatedly with a different
    // token injected, without re-resolving credentials or the endpoint.
    let issue = |input: Option<&serde_json::Value>| -> Result<serde_json::Value, Failure> {
        client.call_operation(op_id.name(), op, input_shape, output_shape, input)
    };

    // A streaming download never paginates: the body goes to the file and the headers
    // become the printed document.
    if let Some(path) = &outfile {
        let response = client.call_operation_raw(op_id.name(), op, input_shape, input.as_ref())?;
        std::fs::write(path, response.bytes())
            .map_err(|e| Failure::new(exit::GENERAL_ERROR, format!("{path}: {e}")))?;
        let document = match output_shape {
            Some(shape) => aws_cli_protocol::http_binding::bind_output_headers(
                &model,
                shape,
                response.headers(),
            ),
            None => serde_json::Value::Object(Default::default()),
        };
        match aws_cli_output::render_named(op_id.name(), &document, parsed.output) {
            Ok(Some(text)) => print!("{text}"),
            Ok(None) => {}
            Err(e) => return Err(Failure::new(exit::GENERAL_ERROR, e)),
        }
        return Ok(exit::code(exit::SUCCESS));
    }

    // An event-stream output is a sequence of documents that arrive over time, not one
    // document at the end, so it is printed as JSON Lines: one event per line, flushed as
    // it lands. Collecting them into a single array would defeat the point of a stream
    // that may never end — `logs start-live-tail` runs until interrupted.
    if let Some(shape) = output_shape.filter(|s| {
        aws_cli_protocol::eventstream::stream_member(&model, s).is_some()
    }) {
        use std::io::Write;
        let mut stdout = std::io::stdout();
        let mut stream_error = None;
        let mut emit = |event: aws_cli_protocol::eventstream::Event| -> Result<(), Failure> {
            use aws_cli_protocol::eventstream::Event;
            match event {
                Event::Event { name, value } => {
                    let line = serde_json::json!({ name: value });
                    writeln!(stdout, "{line}")
                        .map_err(|e| Failure::new(exit::GENERAL_ERROR, e.to_string()))?;
                    // Per event rather than per buffer: a stream is watched live, and a
                    // line held in a buffer has not been delivered.
                    stdout
                        .flush()
                        .map_err(|e| Failure::new(exit::GENERAL_ERROR, e.to_string()))?;
                }
                // The service ended the stream itself. Recorded rather than returned, so
                // the events already emitted are not discarded by an early exit.
                Event::Exception { code, message, .. } | Event::Error { code, message } => {
                    stream_error = Some(format!(
                        "An error occurred ({code}) when calling the {} operation: {message}",
                        op_id.name()
                    ));
                }
                // Not printed to stdout, which must stay parseable, but not dropped in
                // silence either: a new event type is something the user should see.
                Event::Unknown { message_type, event_type } => {
                    eprintln!(
                        "warning: skipped an unrecognised {message_type} frame{}",
                        event_type.map(|t| format!(" of type {t}")).unwrap_or_default()
                    );
                }
            }
            Ok(())
        };
        // A duplex operation also *sends* events. They come from stdin as JSON Lines, in
        // the same `{"EventName": {...}}` shape the response events print, so the two
        // halves of a conversation are written the same way.
        let duplex = input_shape
            .is_some_and(|s| aws_cli_protocol::eventstream::stream_member(&model, s).is_some());
        if duplex {
            // `stdin().lock()` yields a guard that cannot cross threads, and `Stdin`
            // itself is not `BufRead`; wrapping it gives both.
            let lines =
                Box::new(std::io::BufRead::lines(std::io::BufReader::new(std::io::stdin())));
            client.call_operation_duplex(
                op_id.name(),
                op,
                input_shape,
                shape,
                input.as_ref(),
                lines,
                &mut emit,
            )?;
        } else {
            client.call_operation_events(
                op_id.name(),
                op,
                input_shape,
                shape,
                input.as_ref(),
                &mut emit,
            )?;
        }
        if let Some(message) = stream_error {
            let mut failure = Failure::new(exit::CLIENT_ERROR, message);
            failure.service_error_code = Some("EventStreamError".to_string());
            return Err(failure);
        }
        return Ok(exit::code(exit::SUCCESS));
    }

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

/// argparse's wording for a name that is not one of the choices, with the reference's
/// `Maybe you meant:` block when anything is close enough.
///
/// The suggestions come from Python's `difflib.get_close_matches` at its 0.8 cutoff, so
/// the printed list matches candidate for candidate.
fn invalid_choice<'a>(
    argument: &str,
    typed: &str,
    choices: impl IntoIterator<Item = &'a str>,
) -> Failure {
    let suggestions = aws_cli_model::close_matches::get_close_matches(typed, choices, 3, 0.8);
    let mut message = format!("argument {argument}: Found invalid choice '{typed}'\n");
    if !suggestions.is_empty() {
        message.push_str("\nMaybe you meant:\n");
        for word in &suggestions {
            message.push_str(&format!("\n  * {word}"));
        }
    }
    Failure::new(
        exit::PARAM_VALIDATION,
        // argparse joins its message parts with newlines and then puts a blank line
        // before the usage block. The first part keeps its own trailing newline, so a
        // message with no suggestions ends up one line further from the usage than one
        // that has them -- which is why this appends only two.
        format!(
            "{}\n\n{USAGE_HINT}",
            aws_cli_runtime::RuntimeError::ParamValidation(message)
        ),
    )
}

/// Every `aws <service>` name we can resolve, for the suggestion list.
fn known_services() -> Vec<String> {
    let dir = models_dir();
    if let Ok(bytes) = std::fs::read(dir.join(".awsc-model-index.json")) {
        if let Ok(map) = serde_json::from_slice::<std::collections::BTreeMap<String, String>>(&bytes)
        {
            return map.into_keys().collect();
        }
    }
    Vec::new()
}

pub fn now_unix() -> i64 {
    // Overridable so conformance tests can pin our clock to the same second as a captured
    // reference run; presigned URLs embed the timestamp, so without this the two outputs
    // can only be compared field by field rather than byte for byte.
    if let Ok(fixed) = std::env::var("AWSC_FIXED_TIME") {
        if let Ok(seconds) = fixed.parse() {
            return seconds;
        }
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is after 1970")
        .as_secs() as i64
}

/// Locate and load a service model by CLI service name.
///
/// `models/` is named by aws-sdk-rust's conventions, not the CLI's — `logs` lives in
/// `cloudwatch-logs.json` and `s3api` in `s3.json` — so a name that does not match its
/// filename needs a lookup.
///
/// Scanning the directory to find it costs ~9 seconds: 431 models, 200 MB of JSON, every
/// one parsed until the right one turns up. That was being paid on *every* invocation of
/// `s3`, `logs` and `configservice`. The scan result is therefore cached in the models
/// directory and reused, and rebuilt whenever a lookup misses.
pub fn load_model(cli_service: &str) -> Result<Model, String> {
    let dir = models_dir();

    // The compiled container: one mapped file, a binary search, and shapes decoded only
    // as the command reaches them. The JSON path below stays as a fallback for a models
    // directory that has not been compiled yet.
    if let Some(model) = Model::from_container(&dir, cli_service) {
        return Ok(model);
    }

    // The obvious filename, which is right for most services.
    let direct = dir.join(format!("{cli_service}.json"));
    if let Some(model) = try_load(&direct, cli_service) {
        return Ok(model);
    }

    // The cached index, which covers the rest without touching the other models.
    if let Some(file) = index_lookup(&dir, cli_service) {
        if let Some(model) = try_load(&dir.join(&file), cli_service) {
            return Ok(model);
        }
    }

    // A miss means the index is absent or stale: rebuild it, then answer from it.
    let index = build_index(&dir)?;
    let found = index.get(cli_service).and_then(|file| try_load(&dir.join(file), cli_service));
    write_index(&dir, &index);
    found.ok_or_else(|| format!("unknown service `{cli_service}`"))
}

/// Load `path` only if it really is the service we want.
fn try_load(path: &std::path::Path, cli_service: &str) -> Option<Model> {
    let bytes = std::fs::read(path).ok()?;
    let model = Model::from_json(&bytes).ok()?;
    model.cli_service_name().is_ok_and(|n| n == cli_service).then_some(model)
}

fn index_path(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join(".awsc-model-index.json")
}

fn index_lookup(dir: &std::path::Path, cli_service: &str) -> Option<String> {
    let bytes = std::fs::read(index_path(dir)).ok()?;
    let map: std::collections::BTreeMap<String, String> = serde_json::from_slice(&bytes).ok()?;
    map.get(cli_service).cloned()
}

/// Parse every model once, recording which file each CLI service name lives in.
fn build_index(
    dir: &std::path::Path,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        format!("cannot read models directory {} ({e}); run scripts/fetch-models.sh", dir.display())
    })?;
    let mut index = std::collections::BTreeMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')) {
            continue;
        }
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let Ok(model) = Model::from_json(&bytes) else { continue };
        let Ok(name) = model.cli_service_name() else { continue };
        if let Some(file) = path.file_name() {
            index.insert(name, file.to_string_lossy().into_owned());
        }
    }
    Ok(index)
}

/// Best-effort: a read-only models directory just means the scan repeats.
fn write_index(dir: &std::path::Path, index: &std::collections::BTreeMap<String, String>) {
    if let Ok(text) = serde_json::to_string(index) {
        let _ = std::fs::write(index_path(dir), text);
    }
}

fn models_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("AWSC_MODELS_DIR") {
        return dir.into();
    }
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models")
}
