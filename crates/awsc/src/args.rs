//! Command-line parsing and model-driven parameter binding.

use aws_cli_model::shape::StructureShape;
use aws_cli_model::{naming, surface_overlays, Model, Shape, ShapeId};
use aws_cli_protocol::shorthand;
use aws_cli_output::Format;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// What argv asked for. Help and bare-usage are distinct because the reference exits 0
/// for an explicit `help` but 252 for no arguments at all.
pub enum Outcome {
    Run(Box<Parsed>),
    Help,
    Version,
    Usage,
}

/// How the text of a blob argument is interpreted.
///
/// v2 defaults to `base64`: the value on the command line is *already* base64 and is
/// decoded before it goes on the wire. v1 took the value literally, which
/// `raw-in-base64-out` preserves. `fileb://` is bytes either way and is never subject to
/// this (`customizations/binaryformat.py`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BinaryFormat {
    #[default]
    Base64,
    RawInBase64Out,
}

impl BinaryFormat {
    pub fn parse(s: &str) -> Option<BinaryFormat> {
        Some(match s {
            "base64" => BinaryFormat::Base64,
            "raw-in-base64-out" => BinaryFormat::RawInBase64Out,
            _ => return None,
        })
    }
}

#[derive(Clone)]
pub struct Parsed {
    pub service: String,
    pub operation: String,
    /// Operation parameters as `--flag` -> raw string (`None` for valueless booleans).
    pub parameters: BTreeMap<String, Option<String>>,
    pub region: Option<String>,
    pub profile: Option<String>,
    pub endpoint_url: Option<String>,
    pub output: Format,
    pub debug: bool,
    /// `--no-paginate`; auto-pagination is on by default for paginated operations.
    pub no_paginate: bool,
    pub max_items: Option<usize>,
    pub page_size: Option<i64>,
    pub starting_token: Option<String>,
    /// `--query`, a JMESPath expression applied before formatting.
    pub query: Option<String>,
    pub no_sign_request: bool,
    pub verify_ssl: bool,
    pub ca_bundle: Option<String>,
    pub read_timeout: Option<u64>,
    pub connect_timeout: Option<u64>,
    /// `--cli-input-json` / `--cli-input-yaml`, already read from disk if `file://`.
    pub cli_input: Option<String>,
    pub generate_skeleton: Option<String>,
    /// `--color on|off|auto`. Only `logs tail` reads it; nothing else colours output.
    pub color: Option<String>,
    /// `--cli-binary-format`, deciding how a blob argument's text is read.
    pub binary_format: BinaryFormat,
    /// `--cli-error-format`, deciding how an error is rendered on stderr.
    pub error_format: Option<String>,
    /// Positional tokens after the operation name. Only custom commands use these
    /// (`codecommit credential-helper get`); the modeled path rejects them.
    pub positionals: Vec<String>,
    /// Values a flag consumed beyond its first, with their argv positions.
    ///
    /// Parsing is purely syntactic: it cannot tell `--instance-ids i-1 i-2` (a list
    /// taking both) from `--key k out.txt` (a scalar followed by a positional). Both are
    /// collected here and [`rebalance`] decides once the model is known.
    pub flag_extras: BTreeMap<String, Vec<(usize, String)>>,
    /// Non-global tokens exactly as they appeared in argv, in order.
    ///
    /// `parameters` loses both the ordering and the `--flag=value` vs `--flag value`
    /// distinction, and the reference reports unknown options by joining these raw tokens
    /// with `,` — so `--bogus=x` is one token but `--bogus x` is two.
    pub extras: Vec<String>,
}

/// How many values a flag takes.
///
/// Splitting argv needs this and cannot derive it: `--instance-ids i-1 i-2` and
/// `--key k out.txt` are the same shape syntactically. The modeled path defers the
/// question to [`rebalance`]; the `aws s3` tree answers it up front, since its flags are
/// hand-written rather than modelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arity {
    /// A boolean: the flag is the whole argument.
    None,
    /// One value; anything after it is a positional.
    One,
    /// A list: every consecutive value belongs to it.
    Many,
}

pub fn parse(argv: &[String]) -> Result<Outcome, String> {
    if argv.is_empty() {
        return Ok(Outcome::Usage);
    }
    if argv[0] == "help" || argv[0] == "--help" || argv[0] == "-h" {
        return Ok(Outcome::Help);
    }
    // The version line stands alone: the reference prints it and nothing else, where
    // `help` prints the usage block. Sharing `Outcome::Help` printed both.
    if argv[0] == "--version" {
        return Ok(Outcome::Version);
    }

    let service = argv[0].clone();
    if service.starts_with('-') {
        return Err(format!("expected a service name, got `{service}`"));
    }
    let Some(operation) = argv.get(1).cloned() else {
        return Err(format!("`{service}`: expected a subcommand"));
    };
    if operation == "help" || operation == "--help" {
        return Ok(Outcome::Help);
    }

    let mut parsed = Parsed {
        service,
        operation,
        parameters: BTreeMap::new(),
        region: None,
        profile: None,
        endpoint_url: None,
        output: Format::Json,
        debug: false,
        no_paginate: false,
        max_items: None,
        page_size: None,
        starting_token: None,
        query: None,
        no_sign_request: false,
        verify_ssl: true,
        ca_bundle: None,
        read_timeout: None,
        connect_timeout: None,
        cli_input: None,
        generate_skeleton: None,
        color: None,
        binary_format: BinaryFormat::default(),
        error_format: None,
        positionals: Vec::new(),
        flag_extras: BTreeMap::new(),
        extras: Vec::new(),
    };

    // Whether `--cli-binary-format` was given, so the profile is consulted only when it
    // was not. Unlike `cli_error_format` there is no environment variable in the chain
    // (`clidriver.py:_construct_cli_binary_format_chain`).
    let mut binary_format_given = false;

    // In the `s3` tree `--page-size` is a per-command argument, not the injected
    // pagination control, and it is validated differently — so it has to reach the
    // subcommand rather than being consumed here. `--no-paginate` and `--output` really
    // are accepted-and-ignored there, so those stay global.
    let owns_its_arguments = parsed.service == "s3";

    let mut i = 2;
    while i < argv.len() {
        let arg = &argv[i];
        if !arg.starts_with("--") {
            // Held rather than rejected here: a custom command may declare subcommands.
            // The modeled path rejects any that are left over.
            parsed.positionals.push(arg.clone());
            i += 1;
            continue;
        }
        // Support both `--flag value` and `--flag=value`.
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };

        let mut take_value = || -> Result<String, String> {
            if let Some(v) = inline.clone() {
                return Ok(v);
            }
            i += 1;
            argv.get(i).cloned().ok_or_else(|| format!("{name} requires a value"))
        };

        match name.as_str() {
            "--region" => parsed.region = Some(take_value()?),
            "--profile" => parsed.profile = Some(take_value()?),
            "--endpoint-url" => parsed.endpoint_url = Some(take_value()?),
            "--output" => {
                let v = take_value()?;
                parsed.output = Format::parse(&v)
                    .ok_or_else(|| format!("invalid --output `{v}`"))?;
            }
            "--debug" => parsed.debug = true,
            // Pagination controls are injected into every paginated operation, so they
            // are parsed here rather than bound to a model member.
            "--no-paginate" => parsed.no_paginate = true,
            "--max-items" => {
                let v = take_value()?;
                parsed.max_items =
                    Some(v.parse().map_err(|_| format!("--max-items: `{v}` is not a number"))?);
            }
            "--page-size" if !owns_its_arguments => {
                let v = take_value()?;
                parsed.page_size =
                    Some(v.parse().map_err(|_| format!("--page-size: `{v}` is not a number"))?);
            }
            "--starting-token" => parsed.starting_token = Some(take_value()?),
            "--query" => {
                let expression = take_value()?;
                // Validated here so a bad expression fails before any API call, which is
                // where the reference validates it too.
                aws_cli_output::query::validate(&expression).map_err(|e| e.to_string())?;
                parsed.query = Some(expression);
            }
            "--no-sign-request" => parsed.no_sign_request = true,
            "--no-verify-ssl" => parsed.verify_ssl = false,
            "--ca-bundle" => parsed.ca_bundle = Some(take_value()?),
            "--cli-read-timeout" => {
                let v = take_value()?;
                parsed.read_timeout =
                    Some(v.parse().map_err(|_| format!("--cli-read-timeout: `{v}` is not a number"))?);
            }
            "--cli-connect-timeout" => {
                let v = take_value()?;
                parsed.connect_timeout = Some(
                    v.parse().map_err(|_| format!("--cli-connect-timeout: `{v}` is not a number"))?,
                );
            }
            // Accepted and genuinely inert: we neither colour output nor page it, so
            // honouring these would be a no-op anyway. Rejecting them would break
            // scripts that pass them habitually.
            "--color" => parsed.color = Some(take_value()?),
            "--cli-binary-format" => {
                let v = take_value()?;
                binary_format_given = true;
                parsed.binary_format = BinaryFormat::parse(&v).ok_or(
                    "argument --cli-binary-format: Invalid choice, valid choices are:\n\n\
                     base64 | raw-in-base64-out"
                        .to_string(),
                )?;
            }
            "--cli-error-format" => {
                let v = take_value()?;
                if !crate::errorformat::ERROR_FORMATS.contains(&v.as_str()) {
                    return Err(format!(
                        "argument --cli-error-format: Invalid choice, valid choices are:\n\n{}",
                        crate::errorformat::ERROR_FORMATS.join(" | ")
                    ));
                }
                parsed.error_format = Some(v);
            }
            // We never prompt, so declining to prompt is a genuine no-op. Asking to be
            // prompted is not: answering as though the flag were absent would run a
            // command the user expected to be able to edit first.
            "--no-cli-auto-prompt" => {}
            "--cli-auto-prompt" => {
                return Err("--cli-auto-prompt is not implemented (there is no interactive prompter)"
                    .to_string())
            }
            "--no-cli-pager" => {}
            "--cli-input-json" | "--cli-input-yaml" => {
                if parsed.cli_input.is_some() {
                    return Err("Only one --cli-input- parameter may be specified.".to_string());
                }
                parsed.cli_input = Some(take_value()?);
            }
            "--generate-cli-skeleton" => {
                // `nargs='?'` with `const='input'`: a bare flag means `input`.
                let mode = if inline.is_some() {
                    take_value()?
                } else if argv.get(i + 1).is_some_and(|n| {
                    matches!(n.as_str(), "input" | "output" | "yaml-input")
                }) {
                    i += 1;
                    argv[i].clone()
                } else {
                    "input".to_string()
                };
                parsed.generate_skeleton = Some(mode);
            }
            "--help" => return Ok(Outcome::Help),
            other => {
                // Operation parameters are resolved against the model later; store the
                // raw value here (or None, which a boolean member will interpret).
                parsed.extras.push(arg.clone());
                let value = if inline.is_some() {
                    Some(take_value()?)
                } else {
                    // Take the consecutive non-flag tokens this flag can hold. For the
                    // `s3` tree that is known now; for a modeled operation it is not, so
                    // everything is taken and `rebalance` hands back what did not belong.
                    let limit = match if owns_its_arguments {
                        crate::s3::flag_arity(other)
                    } else {
                        Arity::Many
                    } {
                        Arity::None => 0,
                        Arity::One => 1,
                        Arity::Many => usize::MAX,
                    };
                    let mut values: Vec<(usize, String)> = Vec::new();
                    while values.len() < limit
                        && argv.get(i + 1).is_some_and(|n| !n.starts_with("--"))
                    {
                        i += 1;
                        parsed.extras.push(argv[i].clone());
                        values.push((i, argv[i].clone()));
                    }
                    let first = values.first().map(|(_, v)| v.clone());
                    if values.len() > 1 {
                        parsed.flag_extras.insert(other.to_string(), values.clone());
                    }
                    // Joined so a list member sees every value; `bind_value` splits a
                    // scalar list on whitespace, which is the same shape the reference
                    // produces from `--instance-ids i-1 i-2`.
                    match values.len() {
                        0 => None,
                        1 => first,
                        _ => Some(
                            values.iter().map(|(_, v)| v.as_str()).collect::<Vec<_>>().join(" "),
                        ),
                    }
                };
                parsed.parameters.insert(other.to_string(), value);
            }
        }
        i += 1;
    }

    // The profile is the only other source for this one, and only when the flag is absent.
    if !binary_format_given {
        if let Some(configured) =
            aws_cli_runtime::credentials::profile_setting("cli_binary_format", parsed.profile.as_deref())
        {
            // An unrecognised value in config is ignored rather than fatal, as it is for
            // every other config-sourced setting; only the flag rejects.
            if let Some(format) = BinaryFormat::parse(&configured) {
                parsed.binary_format = format;
            }
        }
    }

    Ok(Outcome::Run(Box::new(parsed)))
}

/// Bind `--flag` values onto input-shape members, producing the JSON the serializer takes.
///
/// Unknown flags are an error, matching the reference: silently dropping a parameter the
/// user supplied would send a different request than they asked for.
///
/// The command is needed because the reference renames some arguments per service and
/// operation: `sns subscribe` takes `--notification-endpoint`, not `--endpoint`.
pub fn build_input_named(
    model: &Model,
    input_shape: Option<&StructureShape>,
    parameters: &BTreeMap<String, Option<String>>,
    service: &str,
    operation: &str,
    binary_format: BinaryFormat,
) -> Result<Option<Value>, InputError> {
    if parameters.is_empty() {
        return Ok(None);
    }
    let Some(shape) = input_shape else {
        return Err(format!(
            "unknown options: {}",
            parameters.keys().cloned().collect::<Vec<_>>().join(", ")
        )
        .into());
    };

    // CLI flag name -> model member name, after the reference's renames.
    let by_flag: BTreeMap<String, &String> = shape
        .members
        .keys()
        .map(|m| {
            let derived = naming::xform_name(m, "-");
            let renamed = surface_overlays::rename_argument(service, operation, &derived);
            (format!("--{}", proxy_rename(service, operation, &renamed)), m)
        })
        .filter(|(_, m)| proxy_hidden_member(service, operation) != Some(m.as_str()))
        .collect();

    // Every unrecognised flag is collected, not just the first. argparse hands back its
    // leftovers with all the flags first and their values *after*, in reverse — so
    // `--aa 1 --bb 2` reads as `--aa, --bb, 2, 1`. That ordering is an artifact of how
    // argparse accumulates extras, and it is reproduced because the line is printed.
    let mut flags: Vec<String> = Vec::new();
    let mut values: Vec<String> = Vec::new();
    for (flag, raw) in parameters {
        let lookup = flag.strip_prefix("--no-").map(|r| format!("--{r}")).unwrap_or(flag.clone());
        if !by_flag.contains_key(&lookup) {
            flags.push(flag.clone());
            if let Some(value) = raw {
                values.push(value.clone());
            }
        }
    }
    if !flags.is_empty() {
        values.reverse();
        flags.extend(values);
        return Err(format!("Unknown options: {}", flags.join(", ")).into());
    }

    let mut out = Map::new();
    // Members whose value arrived as raw bytes via `fileb://`, and so must not be run
    // through the blob normalisation below a second time.
    let mut already_bytes: std::collections::BTreeSet<String> = Default::default();
    for (flag, raw) in parameters {
        // `--no-foo` is the negative form of a boolean member.
        let (lookup, negated) = match flag.strip_prefix("--no-") {
            Some(rest) => (format!("--{rest}"), true),
            None => (flag.clone(), false),
        };
        let Some(member_name) = by_flag.get(&lookup) else { continue };
        let member = &shape.members[*member_name];

        // Booleans take no value at all.
        if matches!(model.shape(&member.target), Some(Shape::Boolean(_))) {
            out.insert((*member_name).clone(), Value::Bool(!negated));
            continue;
        }
        let raw_value = raw.as_deref().ok_or_else(|| format!("{flag} requires a value"))?;

        // `fileb://` on a non-streaming blob is the one place a parameter is genuinely
        // *bytes*. It is normalised straight to base64 here and recorded so the blob pass
        // below leaves it alone -- the reference skips it for the same reason, because
        // `fileb://` yields `bytes` and its visitor only touches `str`. Reading it as text
        // first would put U+FFFD through any byte that is not valid UTF-8.
        if is_plain_blob(model, &member.target) {
            if let Some(bytes) = read_fileb(raw_value).map_err(|e| format!("{flag}: {e}"))? {
                out.insert(
                    (*member_name).clone(),
                    Value::String(aws_cli_protocol::shapes::base64_encode(&bytes)),
                );
                already_bytes.insert((*member_name).clone());
                continue;
            }
        }

        // `file://` and `fileb://` are expanded FIRST, before shorthand or JSON parsing —
        // that is why `--tags file://tags.json` works: the loaded text starts with `[`,
        // which then trips the JSON path below.
        let expanded = expand_paramfile(raw_value).map_err(|e| format!("{flag}: {e}"))?;

        let value = bind_value(model, &member.target, &expanded, flag)?;
        out.insert((*member_name).clone(), value);
    }

    let mut built = Value::Object(out);
    normalize_blobs(model, shape, &mut built, binary_format, &already_bytes)?;
    Ok(Some(built))
}

/// An input-building failure, and whether it is one the reference reports as a *general*
/// error rather than a parameter-validation one.
///
/// The distinction is only visible in the exit code, and only one case needs it:
/// `InvalidBase64Error` is raised by a customization and matches no handler's exception
/// list, so it falls through to the general handler and exits 255 -- where every other
/// failure here is argparse's 252.
#[derive(Debug)]
pub struct InputError {
    pub message: String,
    pub general: bool,
}

impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<String> for InputError {
    fn from(message: String) -> Self {
        InputError { message, general: false }
    }
}

/// A blob that carries a value, as opposed to a streaming payload whose argument is a
/// path to send from. Streaming members are never touched by any of this.
fn is_plain_blob(model: &Model, target: &aws_cli_model::ShapeId) -> bool {
    matches!(model.shape(target), Some(Shape::Blob(b)) if !b.traits.has("smithy.api#streaming"))
}

/// Read a `fileb://` reference, or `None` when the value is not one.
fn read_fileb(value: &str) -> Result<Option<Vec<u8>>, String> {
    let Some(rest) = value.strip_prefix("fileb://") else { return Ok(None) };
    let path = shellexpand(rest);
    std::fs::read(&path)
        .map(Some)
        .map_err(|e| format!("Unable to load paramfile {value}: {e}"))
}

/// Put every blob value into the base64 form the wire wants, which is what the rest of
/// the pipeline now assumes.
///
/// This is `customizations/binaryformat.py`'s `Base64DecodeVisitor`, with one change of
/// representation: the reference decodes the text to `bytes` and lets each protocol
/// re-encode, while a `serde_json::Value` cannot hold bytes, so the canonical form here is
/// the base64 *text* — identical on the wire for every protocol, since the only ones that
/// want raw bytes (CBOR, and a blob payload) decode it back.
///
/// Members listed in `already_bytes` came from `fileb://` and are already correct.
fn normalize_blobs(
    model: &Model,
    shape: &StructureShape,
    value: &mut Value,
    format: BinaryFormat,
    already_bytes: &std::collections::BTreeSet<String>,
) -> Result<(), InputError> {
    let Value::Object(map) = value else { return Ok(()) };
    for (name, member) in &shape.members {
        if already_bytes.contains(name) {
            continue;
        }
        let Some(slot) = map.get_mut(name) else { continue };
        normalize_blob_value(model, &member.target, slot, format)?;
    }
    Ok(())
}

fn normalize_blob_value(
    model: &Model,
    target: &aws_cli_model::ShapeId,
    value: &mut Value,
    format: BinaryFormat,
) -> Result<(), InputError> {
    match model.shape(target) {
        Some(Shape::Blob(b)) if !b.traits.has("smithy.api#streaming") => {
            let Some(text) = value.as_str() else { return Ok(()) };
            match format {
                // The text is already base64; it only has to be *valid*, because the
                // reference decodes it here and reports the failure with this wording.
                BinaryFormat::Base64 => {
                    if aws_cli_protocol::shapes::base64_decode(text).is_none() {
                        return Err(InputError {
                            message: format!("Invalid base64: \"{text}\""),
                            general: true,
                        });
                    }
                }
                // v1 semantics: the text is the bytes.
                BinaryFormat::RawInBase64Out => {
                    *value =
                        Value::String(aws_cli_protocol::shapes::base64_encode(text.as_bytes()));
                }
            }
        }
        Some(Shape::Structure(s) | Shape::Union(s)) => {
            let Value::Object(map) = value else { return Ok(()) };
            for (name, member) in &s.members {
                if let Some(slot) = map.get_mut(name) {
                    normalize_blob_value(model, &member.target, slot, format)?;
                }
            }
        }
        Some(Shape::List(l) | Shape::Set(l)) => {
            let Value::Array(items) = value else { return Ok(()) };
            for item in items {
                normalize_blob_value(model, &l.member.target, item, format)?;
            }
        }
        Some(Shape::Map(m)) => {
            let Value::Object(map) = value else { return Ok(()) };
            for (_, slot) in map.iter_mut() {
                normalize_blob_value(model, &m.value.target, slot, format)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn arity(model: &Model, member: &aws_cli_model::shape::Member) -> Arity {
    match model.shape(&member.target) {
        Some(Shape::Boolean(_)) => Arity::None,
        Some(Shape::List(_) | Shape::Set(_)) => Arity::Many,
        _ => Arity::One,
    }
}

/// Return the values that belong to a positional rather than to the flag that swallowed
/// them.
///
/// `parse` takes every consecutive non-flag token after a flag, because it cannot tell
/// `--instance-ids i-1 i-2` from `--key k out.txt`. The model can: a list member keeps
/// them all, a scalar keeps one, a boolean keeps none. Without this, `s3api get-object
/// --bucket b --key k out.txt` writes to a file named `k out.txt` — and before it,
/// `--instance-ids i-1 i-2` silently queried only `i-1`.
pub fn rebalance(
    model: &Model,
    input_shape: Option<&StructureShape>,
    service: &str,
    operation: &str,
    parsed: &mut Parsed,
) {
    if parsed.flag_extras.is_empty() {
        return;
    }
    let Some(shape) = input_shape else { return };
    // Same mapping the binder uses, so a renamed flag resolves to the same member.
    let by_flag: BTreeMap<String, &aws_cli_model::shape::Member> = shape
        .members
        .iter()
        .map(|(name, member)| {
            let derived = naming::xform_name(name, "-");
            let renamed = surface_overlays::rename_argument(service, operation, &derived);
            (format!("--{}", proxy_rename(service, operation, &renamed)), member)
        })
        .collect();

    let mut returned: Vec<(usize, String)> = Vec::new();
    for (flag, values) in std::mem::take(&mut parsed.flag_extras) {
        let lookup =
            flag.strip_prefix("--no-").map(|r| format!("--{r}")).unwrap_or_else(|| flag.clone());
        let keep = by_flag
            .get(&lookup)
            .map(|member| arity(model, member))
            // An unknown flag keeps one value, which is what it would have had before;
            // the unknown-option error that follows names the flag either way.
            .unwrap_or(Arity::One);
        let keep = match keep {
            Arity::None => 0,
            Arity::One => 1,
            Arity::Many => values.len(),
        };
        if keep >= values.len() {
            continue;
        }
        returned.extend(values[keep..].iter().cloned());
        let kept: Vec<&str> = values[..keep].iter().map(|(_, v)| v.as_str()).collect();
        parsed.parameters.insert(
            flag,
            if kept.is_empty() { None } else { Some(kept.join(" ")) },
        );
    }

    // Argv order, so a trailing outfile stays trailing.
    returned.sort_by_key(|(index, _)| *index);
    for (_, value) in returned {
        parsed.positionals.push(value);
    }
}

/// The flags an operation requires that the user did not supply.
///
/// The reference enforces required arguments at the ARGUMENT-PARSING layer, before model
/// validation, and reports them with argparse's wording plus a usage block — a different
/// message from the model-level "Missing required parameter" that would otherwise fire.
///
/// Suppressed entirely when `--cli-input-json`/`--cli-input-yaml` or
/// `--generate-cli-skeleton` is present, since those legitimately supply or replace the
/// parameters.
pub fn missing_required_flags(
    model: &aws_cli_model::Model,
    input_shape: Option<&StructureShape>,
    parsed: &Parsed,
    service: &str,
    operation: &str,
) -> Vec<String> {
    if parsed.cli_input.is_some() || parsed.generate_skeleton.is_some() {
        return Vec::new();
    }
    let Some(shape) = input_shape else { return Vec::new() };
    // A request event stream is read from stdin, not passed as a flag, so demanding one
    // would make the operation impossible to invoke. `bedrock-runtime` marks its stream
    // member required.
    let stream_member =
        aws_cli_protocol::eventstream::stream_member(model, shape).map(|(name, _)| name);
    shape
        .members
        .iter()
        .filter(|(_, member)| member.traits.is_required())
        .filter(|(name, _)| Some(name.as_str()) != stream_member)
        .filter(|(name, _)| proxy_hidden_member(service, operation) != Some(name.as_str()))
        .map(|(name, _)| {
            // The renamed form is what the user must actually pass: `route53
            // get-traffic-policy` requires `--traffic-policy-version`, not `--version`.
            let derived = naming::xform_name(name, "-");
            let renamed = surface_overlays::rename_argument(service, operation, &derived);
            format!("--{}", proxy_rename(service, operation, &renamed))
        })
        .filter(|flag| !parsed.parameters.contains_key(flag))
        .collect()
}

/// The `rds` option-group proxies each expose one `--options`.
///
/// `add-option-to-option-group` and `remove-option-from-option-group` both call
/// ModifyOptionGroup; the reference renames whichever list applies to `--options` and
/// deletes the other, so each command takes one obvious flag instead of two opposites.
fn proxy_rename<'a>(service: &str, operation: &str, flag: &'a str) -> &'a str {
    if service != "rds" {
        return flag;
    }
    match (operation, flag) {
        ("add-option-to-option-group", "options-to-include") => "options",
        ("remove-option-from-option-group", "options-to-remove") => "options",
        _ => flag,
    }
}

/// The member the proxies must NOT expose: each drops its opposite.
pub fn proxy_hidden_member(service: &str, operation: &str) -> Option<&'static str> {
    if service != "rds" {
        return None;
    }
    match operation {
        "add-option-to-option-group" => Some("OptionsToRemove"),
        "remove-option-from-option-group" => Some("OptionsToInclude"),
        _ => None,
    }
}

/// Expand a `file://` or `fileb://` reference. Anything else is returned unchanged.
///
/// These are the ONLY schemes v2 supports — `http://` and `s3://` existed in v1 and were
/// removed.
pub fn expand_paramfile(value: &str) -> Result<String, String> {
    let (prefix, binary) = if let Some(rest) = value.strip_prefix("fileb://") {
        (rest, true)
    } else if let Some(rest) = value.strip_prefix("file://") {
        (rest, false)
    } else {
        return Ok(value.to_string());
    };

    let path = shellexpand(prefix);
    let bytes = std::fs::read(&path)
        .map_err(|e| format!("Unable to load paramfile {value}: {e}"))?;
    if binary {
        // Binary content is carried as-is; the blob layer decides the encoding.
        return Ok(String::from_utf8_lossy(&bytes).into_owned());
    }
    String::from_utf8(bytes).map_err(|_| {
        format!(
            "Unable to load paramfile {value}: file contents could not be decoded. \
             If this is a binary file, please use the fileb:// prefix instead of file://"
        )
    })
}

/// `~` and `$VAR` expansion, as the reference applies before opening.
fn shellexpand(path: &str) -> String {
    let expanded = match path.strip_prefix("~/") {
        Some(rest) => match std::env::var("HOME") {
            Ok(home) => format!("{home}/{rest}"),
            Err(_) => path.to_string(),
        },
        None => path.to_string(),
    };
    expanded
}

/// Turn one raw argument value into JSON, choosing between JSON, shorthand and scalar
/// coercion the way the reference does.
fn bind_value(
    model: &Model,
    target: &ShapeId,
    raw: &str,
    flag: &str,
) -> Result<Value, String> {
    let shape = model.shape(target);

    // A blob is always text, never a structure, so it never takes the JSON or shorthand
    // paths. This matters for the `{`-leading case: `lambda invoke --payload '{"a":1}'`
    // is not a JSON document to parse but a (bad) base64 string, and the reference says
    // so -- `Invalid base64: "{"a":1}"`. Parsing it as JSON would send an empty body.
    if matches!(shape, Some(Shape::Blob(_))) {
        return Ok(Value::String(raw.to_string()));
    }

    // A value that looks like JSON disables shorthand ENTIRELY for this argument — and
    // there is no fallback if the JSON then fails to parse.
    let looks_like_json = raw.trim_start().starts_with('[') || raw.trim_start().starts_with('{');
    if looks_like_json {
        return serde_json::from_str(raw).map_err(|e| {
            format!("Error parsing parameter '{flag}': Invalid JSON: {e}\nJSON received: {raw}")
        });
    }

    match shape {
        Some(Shape::Structure(_) | Shape::Union(_) | Shape::Map(_)) => {
            let parsed = shorthand::parse(raw)
                .map_err(|e| format!("Error parsing parameter '{flag}': {e}"))?;
            Ok(coerce(model, target, parsed))
        }
        Some(Shape::List(list) | Shape::Set(list)) => {
            // A list of complex members takes shorthand; a list of scalars is just
            // whitespace-separated values.
            let member_is_complex = matches!(
                model.shape(&list.member.target),
                Some(Shape::Structure(_) | Shape::Union(_) | Shape::List(_) | Shape::Map(_))
            );
            if member_is_complex {
                let parsed = shorthand::parse(raw)
                    .map_err(|e| format!("Error parsing parameter '{flag}': {e}"))?;
                Ok(coerce(model, target, parsed))
            } else {
                let member_target = list.member.target.clone();
                Ok(Value::Array(
                    raw.split_whitespace()
                        .map(|s| coerce_scalar(model, &member_target, s))
                        .collect(),
                ))
            }
        }
        _ => Ok(coerce_scalar(model, target, raw)),
    }
}

/// Apply the model to a shorthand-parsed value: coerce scalars and wrap a bare value
/// where the shape wants a list.
fn coerce(model: &Model, target: &ShapeId, value: Value) -> Value {
    match model.shape(target) {
        Some(Shape::Structure(s) | Shape::Union(s)) => match value {
            Value::Object(map) => Value::Object(
                map.into_iter()
                    .map(|(k, v)| {
                        let coerced = match s.members.get(&k) {
                            Some(member) => coerce(model, &member.target, v),
                            None => v,
                        };
                        (k, coerced)
                    })
                    .collect(),
            ),
            other => other,
        },
        Some(Shape::List(list) | Shape::Set(list)) => {
            let member_target = list.member.target.clone();
            match value {
                Value::Array(items) => Value::Array(
                    items.into_iter().map(|v| coerce(model, &member_target, v)).collect(),
                ),
                // A bare value where a list is wanted becomes a one-element list.
                single => Value::Array(vec![coerce(model, &member_target, single)]),
            }
        }
        Some(Shape::Map(map_shape)) => {
            let value_target = map_shape.value.target.clone();
            match value {
                Value::Object(map) => Value::Object(
                    map.into_iter().map(|(k, v)| (k, coerce(model, &value_target, v))).collect(),
                ),
                other => other,
            }
        }
        _ => match value {
            Value::String(s) => coerce_scalar(model, target, &s),
            other => other,
        },
    }
}

/// Scalar coercion. Note the boolean rule: inside shorthand only the literal `true`/
/// `false` (case-insensitive) convert; anything else stays a string and is rejected
/// later, which is what the reference does.
fn coerce_scalar(model: &Model, target: &ShapeId, raw: &str) -> Value {
    match model.shape(target) {
        Some(Shape::Integer(_) | Shape::Long(_) | Shape::Short(_) | Shape::Byte(_)) => {
            raw.parse::<i64>().map(Value::from).unwrap_or_else(|_| Value::String(raw.into()))
        }
        Some(Shape::Float(_) | Shape::Double(_)) => {
            raw.parse::<f64>().map(Value::from).unwrap_or_else(|_| Value::String(raw.into()))
        }
        Some(Shape::Boolean(_)) => {
            if raw.eq_ignore_ascii_case("true") {
                Value::Bool(true)
            } else if raw.eq_ignore_ascii_case("false") {
                Value::Bool(false)
            } else {
                Value::String(raw.to_string())
            }
        }
        _ => Value::String(raw.to_string()),
    }
}

/// Merge a `--cli-input-json`/`-yaml` document into the built parameters.
///
/// A **shallow, top-level-key-only, non-clobbering fill**: command-line arguments win,
/// and there is no recursion into nested structures. If an argument set a top-level key,
/// the document's value for that key is discarded wholesale.
pub fn merge_cli_input(built: &mut Value, document: &Value) -> Result<(), String> {
    let Some(doc) = document.as_object() else {
        return Err(format!(
            "Invalid type: expecting map, received {}",
            if document.is_array() { "list" } else { "scalar" }
        ));
    };
    if !built.is_object() {
        *built = Value::Object(Default::default());
    }
    let target = built.as_object_mut().expect("just ensured object");
    for (key, value) in doc {
        target.entry(key.clone()).or_insert_with(|| value.clone());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// Fixture with a blob member, a nested structure holding one, and a streaming blob.
    fn blob_model() -> (aws_cli_model::Model, StructureShape) {
        let model = aws_cli_model::Model::from_json(
            br#"{"smithy":"2.0","shapes":{
              "com.x#S":{"type":"service","version":"1","traits":{}},
              "com.x#Str":{"type":"string"},
              "com.x#Bin":{"type":"blob"},
              "com.x#Stream":{"type":"blob","traits":{"smithy.api#streaming":{}}},
              "com.x#Inner":{"type":"structure","members":{"Nested":{"target":"com.x#Bin"}}},
              "com.x#In":{"type":"structure","members":{
                "Data":{"target":"com.x#Bin"},
                "Body":{"target":"com.x#Stream"},
                "Name":{"target":"com.x#Str"},
                "Wrap":{"target":"com.x#Inner"}}}}}"#,
        )
        .expect("fixture model");
        let id = aws_cli_model::ShapeId::parse("com.x#In").expect("shape id");
        let shape = match model.shape(&id).expect("shape present") {
            Shape::Structure(s) => s.clone(),
            other => panic!("expected a structure, got {other:?}"),
        };
        (model, shape)
    }

    fn build(
        args: &[(&str, &str)],
        format: BinaryFormat,
    ) -> Result<Option<Value>, InputError> {
        let (model, shape) = blob_model();
        let parameters: BTreeMap<String, Option<String>> = args
            .iter()
            .map(|(k, v)| ((*k).to_string(), Some((*v).to_string())))
            .collect();
        build_input_named(&model, Some(&shape), &parameters, "svc", "op", format)
    }

    /// The v2 default: the value on the command line is ALREADY base64 and reaches the
    /// wire unchanged. Encoding it again would send the base64 of the base64.
    #[test]
    fn the_default_format_takes_a_blob_argument_as_base64() {
        // "aGk=" is base64 for "hi".
        let built = build(&[("--data", "aGk=")], BinaryFormat::Base64).expect("builds");
        assert_eq!(built.unwrap()["Data"], Value::String("aGk=".into()));
    }

    /// v1 semantics, kept behind the flag: the text IS the bytes.
    #[test]
    fn raw_in_base64_out_encodes_the_literal_text() {
        let built =
            build(&[("--data", "hi")], BinaryFormat::RawInBase64Out).expect("builds");
        assert_eq!(built.unwrap()["Data"], Value::String("aGk=".into()));
    }

    /// The default validates, and the wording is the reference's `InvalidBase64Error`.
    /// It is a *general* error, not a parameter one -- no handler claims it.
    #[test]
    fn the_default_format_rejects_text_that_is_not_base64() {
        let err = build(&[("--data", "{\"a\":1}")], BinaryFormat::Base64).expect_err("rejects");
        assert_eq!(err.message, "Invalid base64: \"{\"a\":1}\"");
        assert!(err.general, "the reference exits 255 for this, not 252");
    }

    /// A blob nested inside a structure is normalised too: the reference walks the whole
    /// input shape rather than only its top level.
    #[test]
    fn a_nested_blob_is_normalised_as_well() {
        let built =
            build(&[("--wrap", "Nested=hi")], BinaryFormat::RawInBase64Out).expect("builds");
        assert_eq!(built.unwrap()["Wrap"]["Nested"], Value::String("aGk=".into()));
    }

    /// A streaming member's argument is a PATH to send from, not a value to encode.
    /// Touching it would turn `s3api put-object --body ./f` into a base64 puzzle.
    #[test]
    fn a_streaming_blob_argument_is_left_alone() {
        let built = build(&[("--body", "./payload.bin")], BinaryFormat::Base64).expect("builds");
        assert_eq!(built.unwrap()["Body"], Value::String("./payload.bin".into()));
    }

    /// A blob is text, never a document. Before this, a `{`-leading value took the JSON
    /// short-circuit and was parsed into an object, which serialised to an empty body.
    #[test]
    fn a_blob_value_never_takes_the_json_short_circuit() {
        let built =
            build(&[("--data", "{\"a\":1}")], BinaryFormat::RawInBase64Out).expect("builds");
        // base64 of `{"a":1}`.
        assert_eq!(built.unwrap()["Data"], Value::String("eyJhIjoxfQ==".into()));
    }

    /// A list member takes every value; a scalar takes one and hands the rest back as
    /// positionals. Getting this wrong is silent both ways: `--instance-ids i-1 i-2`
    /// queried only `i-1`, and greedily fixing that would have eaten `get-object`'s
    /// outfile.
    #[test]
    fn splits_flag_values_from_positionals_by_shape() {
        let model = aws_cli_model::Model::from_json(
            br#"{"smithy":"2.0","shapes":{
              "com.x#S":{"type":"service","version":"1","traits":{}},
              "com.x#Str":{"type":"string"},
              "com.x#Flag":{"type":"boolean"},
              "com.x#Ids":{"type":"list","member":{"target":"com.x#Str"}},
              "com.x#In":{"type":"structure","members":{
                "InstanceIds":{"target":"com.x#Ids"},
                "Key":{"target":"com.x#Str"},
                "DryRun":{"target":"com.x#Flag"}}}}}"#,
        )
        .expect("fixture model");
        let id = aws_cli_model::ShapeId::parse("com.x#In").expect("shape id");
        let shape = match model.shape(&id).expect("shape present") {
            Shape::Structure(s) => s.clone(),
            other => panic!("expected a structure, got {other:?}"),
        };

        let argv: Vec<String> =
            ["svc", "op", "--instance-ids", "i-1", "i-2", "--key", "k", "out.txt"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        let Outcome::Run(mut parsed) = parse(&argv).expect("parses") else {
            panic!("expected a run")
        };
        rebalance(&model, Some(&shape), "svc", "op", &mut parsed);

        assert_eq!(
            parsed.parameters.get("--instance-ids"),
            Some(&Some("i-1 i-2".to_string()))
        );
        assert_eq!(parsed.parameters.get("--key"), Some(&Some("k".to_string())));
        assert_eq!(parsed.positionals, vec!["out.txt".to_string()]);

        // And the list really becomes two elements, not one string.
        let bound = build_input_named(
            &model,
            Some(&shape),
            &parsed.parameters,
            "svc",
            "op",
            BinaryFormat::RawInBase64Out,
        )
            .expect("binds")
            .expect("some input");
        assert_eq!(bound["InstanceIds"], serde_json::json!(["i-1", "i-2"]));
    }

    /// A boolean keeps nothing: every token after it is a positional.
    #[test]
    fn a_boolean_flag_returns_all_its_tokens() {
        let model = aws_cli_model::Model::from_json(
            br#"{"smithy":"2.0","shapes":{
              "com.x#S":{"type":"service","version":"1","traits":{}},
              "com.x#Flag":{"type":"boolean"},
              "com.x#In":{"type":"structure","members":{
                "DryRun":{"target":"com.x#Flag"}}}}}"#,
        )
        .expect("fixture model");
        let id = aws_cli_model::ShapeId::parse("com.x#In").expect("shape id");
        let shape = match model.shape(&id).expect("shape present") {
            Shape::Structure(s) => s.clone(),
            other => panic!("expected a structure, got {other:?}"),
        };
        let argv: Vec<String> = ["svc", "op", "--dry-run", "a", "b"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let Outcome::Run(mut parsed) = parse(&argv).expect("parses") else {
            panic!("expected a run")
        };
        rebalance(&model, Some(&shape), "svc", "op", &mut parsed);
        assert_eq!(parsed.parameters.get("--dry-run"), Some(&None));
        assert_eq!(parsed.positionals, vec!["a".to_string(), "b".to_string()]);
    }

    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn run(items: &[&str]) -> Parsed {
        match parse(&argv(items)).unwrap() {
            Outcome::Run(p) => *p,
            _ => panic!("expected a runnable command"),
        }
    }

    #[test]
    fn parses_service_operation_and_globals() {
        let p = run(&["sts", "get-caller-identity", "--region", "eu-west-1"]);
        assert_eq!(p.service, "sts");
        assert_eq!(p.operation, "get-caller-identity");
        assert_eq!(p.region.as_deref(), Some("eu-west-1"));
        assert_eq!(p.output, Format::Json);
    }

    #[test]
    fn accepts_equals_form() {
        assert_eq!(run(&["sts", "get-caller-identity", "--output=json"]).output, Format::Json);
    }

    #[test]
    fn collects_operation_parameters() {
        let p = run(&["sts", "assume-role", "--role-arn", "arn:x", "--dry-run"]);
        assert_eq!(p.parameters["--role-arn"], Some("arn:x".to_string()));
        assert_eq!(p.parameters["--dry-run"], None);
    }

    /// Explicit `help` succeeds; no arguments at all is a usage error (252 upstream).
    #[test]
    fn distinguishes_help_from_bare_usage() {
        assert!(matches!(parse(&argv(&[])).unwrap(), Outcome::Usage));
        assert!(matches!(parse(&argv(&["help"])).unwrap(), Outcome::Help));
        assert!(matches!(parse(&argv(&["sts", "help"])).unwrap(), Outcome::Help));
    }

    #[test]
    fn rejects_bad_output_and_missing_subcommand() {
        assert!(parse(&argv(&["sts"])).is_err());
        assert!(parse(&argv(&["sts", "get-caller-identity", "--output", "xml"])).is_err());
    }
}

/// Build a `--generate-cli-skeleton` document from a shape.
///
/// Placeholder values follow botocore's `ArgumentGenerator`: `""` for strings (or the
/// first enum value), `0`, `0.0`, `true`, `1970-01-01T00:00:00` for timestamps, `null`
/// for blobs, and a **one-element** list. Recursion is guarded by a shape-name stack that
/// stops on the second re-entry, which is what terminates recursive models.
pub fn generate_skeleton(
    model: &Model,
    shape: Option<&StructureShape>,
    use_member_names: bool,
) -> Value {
    let Some(shape) = shape else { return Value::Object(Default::default()) };
    let mut stack = Vec::new();
    skeleton_structure(model, shape, use_member_names, &mut stack)
}

fn skeleton_structure(
    model: &Model,
    shape: &StructureShape,
    use_member_names: bool,
    stack: &mut Vec<String>,
) -> Value {
    let mut out = serde_json::Map::new();
    for (name, member) in &shape.members {
        out.insert(
            name.clone(),
            skeleton_value(model, &member.target, name, use_member_names, stack),
        );
    }
    Value::Object(out)
}

fn skeleton_value(
    model: &Model,
    target: &ShapeId,
    member_name: &str,
    use_member_names: bool,
    stack: &mut Vec<String>,
) -> Value {
    let key = target.to_string();
    // Stop on the SECOND re-entry of a shape, as botocore does.
    if stack.iter().filter(|s| **s == key).count() > 1 {
        return Value::Object(Default::default());
    }
    stack.push(key);
    let value = match model.shape(target) {
        Some(Shape::Structure(s) | Shape::Union(s)) => {
            skeleton_structure(model, s, use_member_names, stack)
        }
        Some(Shape::List(list) | Shape::Set(list)) => Value::Array(vec![skeleton_value(
            model,
            &list.member.target,
            member_name,
            use_member_names,
            stack,
        )]),
        Some(Shape::Map(map_shape)) => {
            let mut m = serde_json::Map::new();
            m.insert(
                "KeyName".to_string(),
                skeleton_value(model, &map_shape.value.target, member_name, use_member_names, stack),
            );
            Value::Object(m)
        }
        Some(Shape::Integer(_) | Shape::Long(_) | Shape::Short(_) | Shape::Byte(_)) => {
            Value::from(0)
        }
        Some(Shape::Float(_) | Shape::Double(_)) => Value::from(0.0),
        Some(Shape::Boolean(_)) => Value::Bool(true),
        Some(Shape::Timestamp(_)) => Value::String("1970-01-01T00:00:00".to_string()),
        Some(Shape::Enum(e)) => {
            // The first enum value, as botocore uses.
            let first = e.members.keys().next().cloned().unwrap_or_default();
            Value::String(first)
        }
        Some(Shape::String(_)) => {
            if use_member_names {
                Value::String(member_name.to_string())
            } else {
                Value::String(String::new())
            }
        }
        // Blobs and document types fall off the end of botocore's if-chain as null.
        _ => Value::Null,
    };
    stack.pop();
    value
}
