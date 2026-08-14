//! Command-line parsing and model-driven parameter binding.

use aws_cli_model::shape::StructureShape;
use aws_cli_model::{naming, Model, Shape};
use aws_cli_output::Format;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// What argv asked for. Help and bare-usage are distinct because the reference exits 0
/// for an explicit `help` but 252 for no arguments at all.
pub enum Outcome {
    Run(Box<Parsed>),
    Help,
    Usage,
}

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
}

pub fn parse(argv: &[String]) -> Result<Outcome, String> {
    if argv.is_empty() {
        return Ok(Outcome::Usage);
    }
    if argv[0] == "help" || argv[0] == "--help" || argv[0] == "-h" {
        return Ok(Outcome::Help);
    }
    if argv[0] == "--version" {
        println!("aws-cli-rs/{}", env!("CARGO_PKG_VERSION"));
        return Ok(Outcome::Help);
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
    };

    let mut i = 2;
    while i < argv.len() {
        let arg = &argv[i];
        if !arg.starts_with("--") {
            return Err(format!("unexpected positional argument `{arg}`"));
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
            "--page-size" => {
                let v = take_value()?;
                parsed.page_size =
                    Some(v.parse().map_err(|_| format!("--page-size: `{v}` is not a number"))?);
            }
            "--starting-token" => parsed.starting_token = Some(take_value()?),
            "--help" => return Ok(Outcome::Help),
            other => {
                // Operation parameters are resolved against the model later; store the
                // raw value here (or None, which a boolean member will interpret).
                let value = if inline.is_some() {
                    Some(take_value()?)
                } else if argv.get(i + 1).is_some_and(|n| !n.starts_with("--")) {
                    i += 1;
                    Some(argv[i].clone())
                } else {
                    None
                };
                parsed.parameters.insert(other.to_string(), value);
            }
        }
        i += 1;
    }

    Ok(Outcome::Run(Box::new(parsed)))
}

/// Bind `--flag` values onto input-shape members, producing the JSON the serializer takes.
///
/// Unknown flags are an error, matching the reference: silently dropping a parameter the
/// user supplied would send a different request than they asked for.
pub fn build_input(
    model: &Model,
    input_shape: Option<&StructureShape>,
    parameters: &BTreeMap<String, Option<String>>,
) -> Result<Option<Value>, String> {
    if parameters.is_empty() {
        return Ok(None);
    }
    let Some(shape) = input_shape else {
        return Err(format!(
            "unknown options: {}",
            parameters.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    };

    // CLI flag name -> model member name.
    let by_flag: BTreeMap<String, &String> = shape
        .members
        .keys()
        .map(|m| (format!("--{}", naming::xform_name(m, "-")), m))
        .collect();

    let mut out = Map::new();
    for (flag, raw) in parameters {
        // `--no-foo` is the negative form of a boolean member.
        let (lookup, negated) = match flag.strip_prefix("--no-") {
            Some(rest) => (format!("--{rest}"), true),
            None => (flag.clone(), false),
        };
        let Some(member_name) = by_flag.get(&lookup) else {
            return Err(format!("unknown option: {flag}"));
        };
        let member = &shape.members[*member_name];

        let value = match model.shape(&member.target) {
            Some(Shape::Boolean(_)) => Value::Bool(!negated),
            Some(Shape::Integer(_) | Shape::Long(_) | Shape::Short(_) | Shape::Byte(_)) => {
                let v = raw.as_deref().ok_or_else(|| format!("{flag} requires a value"))?;
                Value::from(v.parse::<i64>().map_err(|_| format!("{flag}: `{v}` is not an integer"))?)
            }
            Some(Shape::Float(_) | Shape::Double(_)) => {
                let v = raw.as_deref().ok_or_else(|| format!("{flag} requires a value"))?;
                Value::from(v.parse::<f64>().map_err(|_| format!("{flag}: `{v}` is not a number"))?)
            }
            Some(Shape::List(_) | Shape::Set(_)) => {
                let v = raw.as_deref().ok_or_else(|| format!("{flag} requires a value"))?;
                Value::Array(v.split_whitespace().map(|s| Value::String(s.to_string())).collect())
            }
            _ => Value::String(
                raw.clone().ok_or_else(|| format!("{flag} requires a value"))?,
            ),
        };
        out.insert((*member_name).clone(), value);
    }
    Ok(Some(Value::Object(out)))
}

#[cfg(test)]
mod tests {
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
