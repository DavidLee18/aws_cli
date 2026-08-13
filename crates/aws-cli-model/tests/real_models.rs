//! Loader tests against the real vendored models in `models/`.
//!
//! Unit tests cover the transforms in isolation; these confirm the loader survives
//! contact with actual AWS models, which are far messier (7MB of EC2, recursive shapes,
//! resource-nested operations).
//!
//! Skipped with a warning when `models/` has not been populated -- run
//! `scripts/fetch-models.sh` first.

use aws_cli_model::{Model, Protocol};
use std::path::PathBuf;

fn models_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models")
}

fn load(name: &str) -> Option<Model> {
    let path = models_dir().join(format!("{name}.json"));
    let bytes = std::fs::read(&path).ok()?;
    Some(Model::from_json(&bytes).unwrap_or_else(|e| panic!("loading {name}: {e}")))
}

/// Every vendored model parses, exposes exactly one service, and resolves its protocol.
#[test]
fn all_vendored_models_load() {
    let expected = [
        ("sts", Protocol::AwsQuery),
        ("s3", Protocol::RestXml),
        ("ec2", Protocol::Ec2Query),
        ("dynamodb", Protocol::AwsJson1_0),
        ("cloudwatch-logs", Protocol::AwsJson1_1),
        ("lambda", Protocol::RestJson1),
    ];

    let mut checked = 0;
    for (name, want_protocol) in expected {
        let Some(model) = load(name) else { continue };
        assert_eq!(model.protocol().unwrap(), want_protocol, "protocol for {name}");
        assert!(
            model.operation_names().count() > 0,
            "{name} exposes no operations"
        );
        checked += 1;
    }

    assert!(
        checked > 0,
        "no models found in {} -- run scripts/fetch-models.sh",
        models_dir().display()
    );
}

/// The lookup path the CLI actually takes: kebab-case command name to operation shape,
/// then to its input structure.
#[test]
fn resolves_sts_get_caller_identity() {
    let Some(model) = load("sts") else { return };

    assert_eq!(model.cli_service_name().unwrap(), "sts");

    let (id, op) = model.operation("get-caller-identity").expect("operation lookup");
    assert_eq!(id.name(), "GetCallerIdentity");

    // GetCallerIdentity takes an (empty) input struct and returns Account/Arn/UserId.
    let output = model.operation_output(op).unwrap().expect("has output");
    for field in ["Account", "Arn", "UserId"] {
        assert!(output.members.contains_key(field), "output missing {field}");
    }
}

/// EC2 is the stress case: ~600 operations and the largest shape graph in the catalogue.
#[test]
fn resolves_ec2_describe_instances() {
    let Some(model) = load("ec2") else { return };

    let (_, op) = model.operation("describe-instances").expect("operation lookup");
    let input = model.operation_input(op).unwrap().expect("has input");
    assert!(input.members.contains_key("Filters"));
    assert!(input.members.contains_key("MaxResults"));

    assert!(
        model.operation_names().count() > 500,
        "expected EC2 to expose 500+ operations, got {}",
        model.operation_names().count()
    );
}

/// Operation names must come out in the same kebab-case botocore produces, since that is
/// what users type. Spot-check a few well-known commands per service.
#[test]
fn operation_names_are_cli_shaped() {
    let cases = [
        ("s3", "list-buckets"),
        ("s3", "put-object"),
        ("dynamodb", "batch-get-item"),
        ("lambda", "invoke"),
        ("cloudwatch-logs", "create-log-group"),
    ];

    for (service, command) in cases {
        let Some(model) = load(service) else { continue };
        assert!(
            model.operation(command).is_ok(),
            "{service} should expose `{command}`; sample of what it does expose: {:?}",
            model.operation_names().take(5).collect::<Vec<_>>()
        );
    }
}
