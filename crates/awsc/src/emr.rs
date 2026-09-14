//! The `aws emr` custom commands.
//!
//! A port of `customizations/emr/`, which is the largest customization in the reference —
//! fourteen commands, and `create-cluster` alone takes fifty arguments with a shorthand
//! grammar of its own. They land here a few at a time; the ones not yet ported report
//! that rather than guessing.
//!
//! What they have in common is that the *command names are not the API's*.
//! `terminate-clusters` calls `TerminateJobFlows`, `modify-cluster-attributes` calls up to
//! four different Set* operations, and the flags are EMR's vocabulary rather than the
//! model's. That is the whole reason these are customizations.

use crate::args::Parsed;
use crate::client::{Client, Globals};
use crate::exit;
use crate::Failure;
use serde_json::{json, Value};
use std::process::ExitCode;

pub fn dispatch(parsed: &Parsed, globals: &Globals) -> Result<Option<ExitCode>, Failure> {
    match parsed.operation.as_str() {
        "terminate-clusters" => terminate_clusters(parsed, globals).map(Some),
        "modify-cluster-attributes" => modify_cluster_attributes(parsed, globals).map(Some),
        "add-steps" => add_steps(parsed, globals).map(Some),
        "install-applications" => install_applications(parsed, globals).map(Some),
        "create-hbase-backup"
        | "restore-from-hbase-backup"
        | "schedule-hbase-backup"
        | "disable-hbase-backups" => hbase(parsed, globals).map(Some),
        _ => Ok(None),
    }
}

/// `aws emr add-steps --cluster-id j-1 --steps Type=Spark,Args=[...]`.
///
/// Every step type becomes the same `HadoopJarStep`; what differs is *which jar* and what
/// is prepended to the arguments — and that turns on whether the cluster is release-based
/// (EMR 4.x and later) or AMI-based (3.x and 2.x). A release-based cluster runs
/// `command-runner.jar` with a command name; an AMI-based one runs a jar fetched from a
/// regional S3 bucket. So the command asks the cluster first: `DescribeCluster`, read
/// `ReleaseLabel`, then build.
fn add_steps(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let args = crate::custom::take_args(
        parsed,
        &["--cluster-id", "--steps", "--execution-role-arn"],
    )?;
    let Some(Some(cluster_id)) = args.get("--cluster-id").copied() else {
        return Err(crate::custom::missing_required(&["--cluster-id"]));
    };
    let step_tokens = crate::custom::take_list(parsed, "--steps");
    if step_tokens.is_empty() {
        return Err(crate::custom::missing_required(&["--steps"]));
    }

    let (model, globals) = load(globals)?;
    let client = emr_client(&model, &globals)?;
    let region = globals.region.clone().unwrap_or_default();

    let release_label = client
        .call("describe-cluster", Some(&json!({ "ClusterId": cluster_id })))?
        .get("Cluster")
        .and_then(|cluster| cluster.get("ReleaseLabel"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let mut steps = Vec::with_capacity(step_tokens.len());
    for token in step_tokens {
        let parsed_step = parse_step(token)?;
        steps.push(build_step(&parsed_step, release_label.as_deref(), &region)?);
    }

    let mut input = json!({ "JobFlowId": cluster_id, "Steps": steps });
    if let Some(arn) = args.get("--execution-role-arn").copied().flatten() {
        input["ExecutionRoleArn"] = Value::String(arn.to_string());
    }
    let response = client.call("add-job-flow-steps", Some(&input))?;
    render(&response, parsed)
}

/// The four HBase commands.
///
/// All of them are the same shape: build an argument list for `emr.hbase.backup.Main`,
/// wrap it in one step against `/home/hadoop/lib/hbase.jar`, and add it to the cluster.
/// The step is always `CANCEL_AND_WAIT` — a backup that cannot start should leave the
/// cluster alone rather than terminate it.
///
/// Like `install-applications`, these are AMI-era commands and are refused on a
/// release-based cluster.
fn hbase(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let flags = [
        "--cluster-id",
        "--dir",
        "--backup-version",
        "--type",
        "--interval",
        "--unit",
        "--start-time",
        "--consistent",
        "--full",
        "--incremental",
    ];
    let args = crate::custom::take_args(parsed, &flags)?;
    let Some(Some(cluster_id)) = args.get("--cluster-id").copied() else {
        return Err(crate::custom::missing_required(&["--cluster-id"]));
    };
    let value = |flag: &str| args.get(flag).copied().flatten();
    let given = |flag: &str| args.contains_key(flag);

    let (step_name, step_args) = match parsed.operation.as_str() {
        "create-hbase-backup" => {
            let Some(dir) = value("--dir") else {
                return Err(crate::custom::missing_required(&["--dir"]));
            };
            let mut built = vec![
                "emr.hbase.backup.Main".to_string(),
                "--backup".to_string(),
                "--backup-dir".to_string(),
                dir.to_string(),
            ];
            if given("--consistent") {
                built.push("--consistent".to_string());
            }
            ("Backup HBase", built)
        }
        "restore-from-hbase-backup" => {
            let Some(dir) = value("--dir") else {
                return Err(crate::custom::missing_required(&["--dir"]));
            };
            let mut built = vec![
                "emr.hbase.backup.Main".to_string(),
                "--restore".to_string(),
                // Note `--backup-dir`, not the `--backup-dir-to-restore` the constants
                // also define: the restore path uses the same flag as a backup.
                "--backup-dir".to_string(),
                dir.to_string(),
            ];
            if let Some(version) = value("--backup-version") {
                built.push("--backup-version".to_string());
                built.push(version.to_string());
            }
            ("Restore HBase", built)
        }
        "schedule-hbase-backup" => ("Modify Backup Schedule", schedule_args(&args)?),
        // `disable-hbase-backups` shares the *schedule* step name, since both modify the
        // same schedule.
        _ => ("Modify Backup Schedule", disable_args(&args)?),
    };

    let (model, globals) = load(globals)?;
    let client = emr_client(&model, &globals)?;
    if let Some(release) = client
        .call("describe-cluster", Some(&json!({ "ClusterId": cluster_id })))?
        .get("Cluster")
        .and_then(|cluster| cluster.get("ReleaseLabel"))
        .and_then(Value::as_str)
    {
        return Err(Failure::new(
            exit::PARAM_VALIDATION,
            awsc_runtime::RuntimeError::ParamValidation(format!(
                "{} is not supported with '{release}' release.",
                parsed.operation
            )),
        ));
    }

    let step = json!({
        "Name": step_name,
        "ActionOnFailure": "CANCEL_AND_WAIT",
        "HadoopJarStep": { "Jar": "/home/hadoop/lib/hbase.jar", "Args": step_args },
    });
    let response = client.call(
        "add-job-flow-steps",
        Some(&json!({ "JobFlowId": cluster_id, "Steps": [step] })),
    )?;
    render(&response, parsed)
}

/// `schedule-hbase-backup`: the interval flags are named after the backup *type*, so a
/// full backup and an incremental one write different flags with the same values.
fn schedule_args(
    args: &std::collections::BTreeMap<&str, Option<&str>>,
) -> Result<Vec<String>, Failure> {
    let value = |flag: &str| args.get(flag).copied().flatten();
    let missing: Vec<&str> = ["--type", "--dir", "--interval", "--unit"]
        .into_iter()
        .filter(|flag| value(flag).is_none())
        .collect();
    if !missing.is_empty() {
        return Err(crate::custom::missing_required(&missing));
    }
    let kind = value("--type").unwrap_or_default().to_lowercase();
    if kind != "full" && kind != "incremental" {
        return Err(param_error("invalid type. type should be either full or incremental."));
    }
    let unit = value("--unit").unwrap_or_default().to_lowercase();
    if !["minutes", "hours", "days"].contains(&unit.as_str()) {
        return Err(param_error(
            "invalid unit. unit should be one of the following values: minutes, hours or days.",
        ));
    }

    let mut built = vec![
        "emr.hbase.backup.Main".to_string(),
        "--set-scheduled-backup".to_string(),
        "true".to_string(),
        "--backup-dir".to_string(),
        value("--dir").unwrap_or_default().to_string(),
    ];
    if args.contains_key("--consistent") {
        built.push("--consistent".to_string());
    }
    let full = kind == "full";
    built.push(
        if full { "--full-backup-time-interval" } else { "--incremental-backup-time-interval" }
            .to_string(),
    );
    built.push(value("--interval").unwrap_or_default().to_string());
    built.push(
        if full { "--full-backup-time-unit" } else { "--incremental-backup-time-unit" }
            .to_string(),
    );
    built.push(unit);
    built.push("--start-time".to_string());
    // No `--start-time` means the literal string `now`, not an absent argument.
    built.push(value("--start-time").unwrap_or("now").to_string());
    Ok(built)
}

/// `disable-hbase-backups`: at least one of the two must be named, since disabling
/// neither would be a no-op the user did not ask for.
fn disable_args(
    args: &std::collections::BTreeMap<&str, Option<&str>>,
) -> Result<Vec<String>, Failure> {
    let full = args.contains_key("--full");
    let incremental = args.contains_key("--incremental");
    if !full && !incremental {
        return Err(param_error("Should specify at least one of --full and --incremental."));
    }
    let mut built = vec![
        "emr.hbase.backup.Main".to_string(),
        "--set-scheduled-backup".to_string(),
        "false".to_string(),
    ];
    if full {
        built.push("--disable-full-backups".to_string());
    }
    if incremental {
        built.push("--disable-incremental-backups".to_string());
    }
    Ok(built)
}

fn param_error(message: &str) -> Failure {
    Failure::new(
        exit::PARAM_VALIDATION,
        awsc_runtime::RuntimeError::ParamValidation(message.to_string()),
    )
}

/// Applications the reference knows about at all.
const APPLICATIONS: &[&str] =
    &["HIVE", "PIG", "HBASE", "GANGLIA", "IMPALA", "SPARK", "MAPR", "MAPR_M3", "MAPR_M5", "MAPR_M7"];

/// The two that can be added to a cluster that is already running.
const INSTALLABLE: &[&str] = &["HIVE", "PIG"];

/// `aws emr install-applications --cluster-id j-1 --applications Name=Hive`.
///
/// Only Hive and Pig, and only on an **AMI-based** cluster: the whole command is a
/// leftover from EMR 2.x/3.x, where installing an application meant running a script
/// step. A release-based cluster is refused outright, because there is nothing sensible
/// to translate the request into.
fn install_applications(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let args = crate::custom::take_args(parsed, &["--cluster-id", "--applications"])?;
    let Some(Some(cluster_id)) = args.get("--cluster-id").copied() else {
        return Err(crate::custom::missing_required(&["--cluster-id"]));
    };
    let tokens = crate::custom::take_list(parsed, "--applications");
    if tokens.is_empty() {
        return Err(crate::custom::missing_required(&["--applications"]));
    }
    let applications: Vec<Value> =
        tokens.iter().map(|token| parse_shorthand(token, "--applications")).collect::<Result<_, _>>()?;
    check_installable(&applications)?;

    let (model, globals) = load(globals)?;
    let client = emr_client(&model, &globals)?;
    let region = globals.region.clone().unwrap_or_default();

    // The release check is the reference's, and it happens before anything is built.
    if let Some(release) = client
        .call("describe-cluster", Some(&json!({ "ClusterId": cluster_id })))?
        .get("Cluster")
        .and_then(|cluster| cluster.get("ReleaseLabel"))
        .and_then(Value::as_str)
    {
        return Err(Failure::new(
            exit::PARAM_VALIDATION,
            awsc_runtime::RuntimeError::ParamValidation(format!(
                "install-applications is not supported with '{release}' release."
            )),
        ));
    }

    let steps = install_steps(&applications, &region);
    let response = client.call(
        "add-job-flow-steps",
        Some(&json!({ "JobFlowId": cluster_id, "Steps": steps })),
    )?;
    render(&response, parsed)
}

/// Every application must be one the reference knows, and one that can be installed on a
/// running cluster — the two messages differ, and so does what the user should do next.
fn check_installable(applications: &[Value]) -> Result<(), Failure> {
    for application in applications {
        let name = application.get("Name").and_then(Value::as_str).unwrap_or_default();
        let upper = name.to_uppercase();
        let message = if APPLICATIONS.contains(&upper.as_str()) {
            if INSTALLABLE.contains(&upper.as_str()) {
                continue;
            }
            format!(
                "{name} cannot be installed on a running cluster. 'Name' should be one of \
                 the following: {}",
                INSTALLABLE.join(", ")
            )
        } else {
            format!(
                "Unknown application: {name}. 'Name' should be one of the following: {}",
                APPLICATIONS.join(", ")
            )
        };
        return Err(Failure::new(
            exit::PARAM_VALIDATION,
            awsc_runtime::RuntimeError::ParamValidation(message),
        ));
    }
    Ok(())
}

/// The steps that install Hive or Pig, in the order the applications were given.
///
/// Hive contributes a second step when its `Args` carry a `--hive-site` path — and that
/// one is `CANCEL_AND_WAIT` where the install itself is `TERMINATE_CLUSTER`, because a
/// missing site configuration is recoverable and a missing Hive is not.
fn install_steps(applications: &[Value], region: &str) -> Vec<Value> {
    let mut steps = Vec::new();
    for application in applications {
        let name = application.get("Name").and_then(Value::as_str).unwrap_or_default();
        let args = string_list(application.get("Args"));
        match name.to_uppercase().as_str() {
            "HIVE" => {
                steps.push(install_step(
                    "Install Hive",
                    "TERMINATE_CLUSTER",
                    region,
                    vec![
                        s3_link(region, "/libs/hive/hive-script"),
                        "--install-hive".to_string(),
                        "--base-path".to_string(),
                        s3_link(region, "/libs/hive"),
                        "--hive-versions".to_string(),
                        "latest".to_string(),
                    ],
                ));
                if let Some(path) = args.iter().find(|arg| arg.contains("--hive-site")) {
                    // `--hive-site=s3://...`: the value is the whole argument, split off
                    // after the `=`.
                    let path = path.split_once('=').map(|(_, v)| v).unwrap_or(path);
                    steps.push(install_step(
                        "Install Hive Site Configuration",
                        "CANCEL_AND_WAIT",
                        region,
                        vec![
                            s3_link(region, "/libs/hive/hive-script"),
                            "--base-path".to_string(),
                            // Note: the reference builds this one with **no region**, so
                            // it always points at us-east-1. Reproduced deliberately.
                            s3_link("us-east-1", "/libs/hive"),
                            "--install-hive-site".to_string(),
                            path.to_string(),
                            "--hive-versions".to_string(),
                            "latest".to_string(),
                        ],
                    ));
                }
            }
            "PIG" => steps.push(install_step(
                "Install Pig",
                "TERMINATE_CLUSTER",
                region,
                vec![
                    s3_link(region, "/libs/pig/pig-script"),
                    "--install-pig".to_string(),
                    "--base-path".to_string(),
                    s3_link(region, "/libs/pig"),
                    "--pig-versions".to_string(),
                    "latest".to_string(),
                ],
            )),
            _ => {}
        }
    }
    steps
}

fn install_step(name: &str, on_failure: &str, region: &str, args: Vec<String>) -> Value {
    json!({
        "Name": name,
        "ActionOnFailure": on_failure,
        "HadoopJarStep": { "Jar": script_runner(region), "Args": args },
    })
}

/// One shorthand-or-JSON token, named for the error message.
fn parse_shorthand(token: &str, flag: &str) -> Result<Value, Failure> {
    let trimmed = token.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return serde_json::from_str(token).map_err(|e| {
            Failure::new(
                exit::PARAM_VALIDATION,
                awsc_runtime::RuntimeError::ParamValidation(format!(
                    "Error parsing parameter '{flag}': Invalid JSON: {e}\nJSON received: {token}"
                )),
            )
        });
    }
    awsc_protocol::shorthand::parse(token).map_err(|e| {
        Failure::new(
            exit::PARAM_VALIDATION,
            awsc_runtime::RuntimeError::ParamValidation(format!(
                "Error parsing parameter '{flag}': {e}"
            )),
        )
    })
}

/// One `--steps` token: JSON if it starts like JSON, shorthand otherwise.
fn parse_step(token: &str) -> Result<Value, Failure> {
    parse_shorthand(token, "--steps")
}

/// The regional bucket an AMI-based cluster fetches its jars from.
fn s3_link(region: &str, relative_path: &str) -> String {
    let region = if region.is_empty() { "us-east-1" } else { region };
    format!("s3://{region}.elasticmapreduce{relative_path}")
}

fn script_runner(region: &str) -> String {
    s3_link(region, "/libs/script-runner/script-runner.jar")
}

/// `k1=v1,k2=v2` — a key with no `=` gets an empty value rather than being dropped.
fn key_value_list(raw: &str) -> Vec<Value> {
    raw.split(',')
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) => json!({ "Key": key, "Value": value }),
            None => json!({ "Key": pair, "Value": "" }),
        })
        .collect()
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_string).unwrap_or_else(|| item.to_string()))
            .collect(),
        Some(Value::String(single)) => vec![single.clone()],
        _ => Vec::new(),
    }
}

fn missing(structure: &str, field: &str) -> Failure {
    Failure::new(
        exit::PARAM_VALIDATION,
        awsc_runtime::RuntimeError::ParamValidation(format!(
            "The following required parameters are missing for {structure}: {field}."
        )),
    )
}

/// Turn one parsed step into the `StepConfig` the API takes.
fn build_step(
    step: &Value,
    release_label: Option<&str>,
    region: &str,
) -> Result<Value, Failure> {
    let field = |name: &str| step.get(name).and_then(Value::as_str);
    let kind = field("Type").unwrap_or("custom_jar").to_lowercase();
    let args = string_list(step.get("Args"));
    let release = release_label.is_some();

    // Every type resolves to a jar plus the arguments that go in front of the user's.
    let (default_name, jar, mut leading) = match kind.as_str() {
        "custom_jar" => {
            let Some(jar) = field("Jar") else {
                return Err(missing("CustomJARStepConfig", "Jar"));
            };
            ("Custom JAR", jar.to_string(), Vec::new())
        }
        "streaming" => {
            if args.is_empty() {
                return Err(missing("StreamingStepConfig", "Args"));
            }
            if release {
                ("Streaming program", "command-runner.jar".to_string(), vec!["hadoop-streaming".to_string()])
            } else {
                (
                    "Streaming program",
                    "/home/hadoop/contrib/streaming/hadoop-streaming.jar".to_string(),
                    Vec::new(),
                )
            }
        }
        "hive" | "pig" => {
            let structure = if kind == "hive" { "HiveStepConfig" } else { "PigStepConfig" };
            if args.is_empty() {
                return Err(missing(structure, "Args"));
            }
            let mut leading = Vec::new();
            if release {
                leading.push(format!("{kind}-script"));
            } else {
                leading.push(s3_link(region, &format!("/libs/{kind}/{kind}-script")));
            }
            leading.push(format!("--run-{kind}-script"));
            if !release {
                // An AMI-based cluster has to be told which version to run; a
                // release-based one already knows.
                leading.push(format!("--{kind}-versions"));
                leading.push("latest".to_string());
            }
            leading.push("--args".to_string());
            (
                if kind == "hive" { "Hive program" } else { "Pig program" },
                runner_jar(release, region),
                leading,
            )
        }
        "impala" => {
            // Impala never ran on a release-based cluster, so this is not a missing
            // feature — the step type genuinely does not exist there.
            if release {
                return Err(Failure::new(
                    exit::PARAM_VALIDATION,
                    awsc_runtime::RuntimeError::ParamValidation(
                        "The step type impala is not supported.".to_string(),
                    ),
                ));
            }
            if args.is_empty() {
                return Err(missing("ImpalaStepConfig", "Args"));
            }
            (
                "Impala program",
                script_runner(region),
                vec![s3_link(region, "/libs/impala/setup-impala"), "--run-impala-script".to_string()],
            )
        }
        "spark" => {
            if args.is_empty() {
                return Err(missing("SparkStepConfig", "Args"));
            }
            let leading = if release {
                vec!["spark-submit".to_string()]
            } else {
                vec!["/home/hadoop/spark/bin/spark-submit".to_string()]
            };
            ("Spark application", runner_jar(release, region), leading)
        }
        other => {
            return Err(Failure::new(
                exit::PARAM_VALIDATION,
                awsc_runtime::RuntimeError::ParamValidation(format!(
                    "The step type {other} is not supported."
                )),
            ))
        }
    };
    leading.extend(args);

    let mut jar_config = json!({ "Jar": jar });
    if !leading.is_empty() {
        jar_config["Args"] = json!(leading);
    }
    if let Some(main_class) = field("MainClass") {
        jar_config["MainClass"] = Value::String(main_class.to_string());
    }
    if let Some(properties) = field("Properties") {
        jar_config["Properties"] = Value::Array(key_value_list(properties));
    }

    let mut config = json!({
        "Name": field("Name").unwrap_or(default_name),
        // `CONTINUE` unless the step says otherwise: a failed step does not stop the
        // cluster by default.
        "ActionOnFailure": field("ActionOnFailure").unwrap_or("CONTINUE"),
        "HadoopJarStep": jar_config,
    });

    let mut monitoring = serde_json::Map::new();
    if let Some(log_uri) = field("LogUri") {
        monitoring.insert("LogUri".into(), Value::String(log_uri.to_string()));
    }
    if let Some(key) = field("EncryptionKeyArn") {
        monitoring.insert("EncryptionKeyArn".into(), Value::String(key.to_string()));
    }
    if !monitoring.is_empty() {
        config["StepMonitoringConfiguration"] =
            json!({ "S3MonitoringConfiguration": Value::Object(monitoring) });
    }
    Ok(config)
}

fn runner_jar(release: bool, region: &str) -> String {
    if release {
        "command-runner.jar".to_string()
    } else {
        script_runner(region)
    }
}

fn emr_client<'a>(
    model: &'a awsc_model::Model,
    globals: &Globals,
) -> Result<Client<'a>, Failure> {
    Client::new(model, globals)
}

fn load(globals: &Globals) -> Result<(awsc_model::Model, Globals), Failure> {
    let region = crate::custom::resolve_region(globals)
        .ok_or_else(|| Failure::new(exit::CONFIGURATION, awsc_runtime::RuntimeError::NoRegion))?;
    let model = crate::load_model("emr").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    Ok((model, Globals { region: Some(region), ..globals.clone() }))
}

/// `aws emr terminate-clusters --cluster-ids j-1 j-2`.
///
/// One `TerminateJobFlows` for the whole list — the API takes them together, so this is
/// all-or-nothing rather than a loop, and a bad id fails the request for every cluster
/// named alongside it.
fn terminate_clusters(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let args = crate::custom::take_args(parsed, &["--cluster-ids"])?;
    let Some(Some(ids)) = args.get("--cluster-ids").copied() else {
        return Err(crate::custom::missing_required(&["--cluster-ids"]));
    };
    // `nargs='+'`: the values arrive space-joined from the argument layer.
    let ids: Vec<&str> = ids.split_whitespace().collect();
    if ids.is_empty() {
        return Err(crate::custom::missing_required(&["--cluster-ids"]));
    }

    let (model, globals) = load(globals)?;
    let client = emr_client(&model, &globals)?;
    let response = client.call("terminate-job-flows", Some(&json!({ "JobFlowIds": ids })))?;
    render(&response, parsed)
}

/// `aws emr modify-cluster-attributes`.
///
/// Four independent attributes, each with a `--x` / `--no-x` pair, and each its **own API
/// call** — so a command that sets two of them makes two requests and can half-succeed.
/// The calls go out in a fixed order: visibility, termination protection, auto-terminate,
/// unhealthy-node replacement.
fn modify_cluster_attributes(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let flags = [
        "--cluster-id",
        "--visible-to-all-users",
        "--no-visible-to-all-users",
        "--termination-protected",
        "--no-termination-protected",
        "--auto-terminate",
        "--no-auto-terminate",
        "--unhealthy-node-replacement",
        "--no-unhealthy-node-replacement",
    ];
    let args = crate::custom::take_args(parsed, &flags)?;
    let Some(Some(cluster_id)) = args.get("--cluster-id").copied() else {
        return Err(crate::custom::missing_required(&["--cluster-id"]));
    };
    let given = |flag: &str| args.contains_key(flag);

    // Each pair is checked for the both-given case before anything is sent, so a command
    // that contradicts itself does not set the first attribute and then fail.
    for (yes, no) in [
        ("--visible-to-all-users", "--no-visible-to-all-users"),
        ("--termination-protected", "--no-termination-protected"),
        ("--auto-terminate", "--no-auto-terminate"),
        ("--unhealthy-node-replacement", "--no-unhealthy-node-replacement"),
    ] {
        if given(yes) && given(no) {
            return Err(Failure::new(
                exit::PARAM_VALIDATION,
                awsc_runtime::RuntimeError::ParamValidation(format!(
                    "You cannot specify both {yes} and {no} options together."
                )),
            ));
        }
    }
    if !flags.iter().skip(1).any(|flag| given(flag)) {
        return Err(Failure::new(
            exit::PARAM_VALIDATION,
            awsc_runtime::RuntimeError::ParamValidation(
                "Must specify one of the following boolean options: \
                 --visible-to-all-users|--no-visible-to-all-users, \
                 --termination-protected|--no-termination-protected, \
                 --auto-terminate|--no-auto-terminate, \
                 --unhealthy-node-replacement|--no-unhealthy-node-replacement."
                    .to_string(),
            ),
        ));
    }

    let (model, globals) = load(globals)?;
    let client = emr_client(&model, &globals)?;
    for (operation, input) in attribute_calls(cluster_id, &|flag| given(flag)) {
        let response = client.call(&operation, Some(&input))?;
        render(&response, parsed)?;
    }
    Ok(exit::code(exit::SUCCESS))
}

/// The Set* calls one invocation makes, in the order the reference makes them.
///
/// Each attribute is its own request, so setting two of them can half-succeed — the
/// second failing leaves the first applied. That is the reference's behaviour and worth
/// knowing before scripting against it.
fn attribute_calls(cluster_id: &str, given: &dyn Fn(&str) -> bool) -> Vec<(String, Value)> {
    let ids = json!([cluster_id]);
    let mut calls = Vec::new();
    for (yes, no, operation, member, invert) in [
        (
            "--visible-to-all-users",
            "--no-visible-to-all-users",
            "set-visible-to-all-users",
            "VisibleToAllUsers",
            false,
        ),
        (
            "--termination-protected",
            "--no-termination-protected",
            "set-termination-protection",
            "TerminationProtected",
            false,
        ),
        // `--auto-terminate` sets `KeepJobFlowAliveWhenNoSteps` to its *opposite*: the
        // flag says "shut down when idle", the member says "stay alive".
        (
            "--auto-terminate",
            "--no-auto-terminate",
            "set-keep-job-flow-alive-when-no-steps",
            "KeepJobFlowAliveWhenNoSteps",
            true,
        ),
        (
            "--unhealthy-node-replacement",
            "--no-unhealthy-node-replacement",
            "set-unhealthy-node-replacement",
            "UnhealthyNodeReplacement",
            false,
        ),
    ] {
        if !given(yes) && !given(no) {
            continue;
        }
        let on = given(yes) && !given(no);
        let value = if invert { !on } else { on };
        calls.push((operation.to_string(), json!({ "JobFlowIds": ids, member: value })));
    }
    calls
}

/// These responses go through the ordinary formatter, unlike most custom commands.
fn render(value: &Value, parsed: &Parsed) -> Result<ExitCode, Failure> {
    // An empty response prints nothing at all, which is what these Set* calls return.
    if value.as_object().is_some_and(serde_json::Map::is_empty) {
        return Ok(exit::code(exit::SUCCESS));
    }
    match awsc_output::render_named("result", value, parsed.output) {
        Ok(Some(text)) => print!("{text}"),
        Ok(None) => {}
        Err(e) => return Err(Failure::new(exit::GENERAL_ERROR, e)),
    }
    Ok(exit::code(exit::SUCCESS))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calls(flags: &[&str]) -> Vec<(String, Value)> {
        let given = |flag: &str| flags.contains(&flag);
        attribute_calls("j-1", &given)
    }

    /// `--auto-terminate` sets `KeepJobFlowAliveWhenNoSteps` to its *opposite*: the flag
    /// says "shut down when idle", the member says "stay alive". Getting this backwards
    /// would keep a cluster running and cost money quietly.
    #[test]
    fn auto_terminate_inverts_the_member_it_sets() {
        let made = calls(&["--auto-terminate"]);
        assert_eq!(made[0].0, "set-keep-job-flow-alive-when-no-steps");
        assert_eq!(made[0].1["KeepJobFlowAliveWhenNoSteps"], json!(false));

        let made = calls(&["--no-auto-terminate"]);
        assert_eq!(made[0].1["KeepJobFlowAliveWhenNoSteps"], json!(true));
    }

    #[test]
    fn each_attribute_is_its_own_call_in_a_fixed_order() {
        let made = calls(&["--unhealthy-node-replacement", "--visible-to-all-users"]);
        assert_eq!(
            made.iter().map(|(op, _)| op.as_str()).collect::<Vec<_>>(),
            vec!["set-visible-to-all-users", "set-unhealthy-node-replacement"]
        );
        assert_eq!(made[0].1["VisibleToAllUsers"], json!(true));
        assert_eq!(made[0].1["JobFlowIds"], json!(["j-1"]));
    }

    #[test]
    fn the_negative_flag_sets_false() {
        let made = calls(&["--no-termination-protected"]);
        assert_eq!(made[0].0, "set-termination-protection");
        assert_eq!(made[0].1["TerminationProtected"], json!(false));
    }

    #[test]
    fn nothing_given_makes_no_calls() {
        assert!(calls(&[]).is_empty());
    }

    fn step(json_text: &str, release: Option<&str>) -> Value {
        build_step(&serde_json::from_str(json_text).expect("a step"), release, "eu-west-1")
            .expect("builds")
    }

    /// A release-based cluster runs `command-runner.jar` with a command name; an
    /// AMI-based one runs a jar fetched from a regional S3 bucket. Same step, two
    /// completely different configs.
    #[test]
    fn the_release_label_decides_the_jar() {
        let release = step(r#"{"Type": "Spark", "Args": ["--class", "Main"]}"#, Some("emr-6.0.0"));
        assert_eq!(release["HadoopJarStep"]["Jar"], "command-runner.jar");
        assert_eq!(release["HadoopJarStep"]["Args"], json!(["spark-submit", "--class", "Main"]));

        let ami = step(r#"{"Type": "Spark", "Args": ["--class", "Main"]}"#, None);
        assert_eq!(
            ami["HadoopJarStep"]["Jar"],
            "s3://eu-west-1.elasticmapreduce/libs/script-runner/script-runner.jar"
        );
        assert_eq!(
            ami["HadoopJarStep"]["Args"],
            json!(["/home/hadoop/spark/bin/spark-submit", "--class", "Main"])
        );
    }

    /// An AMI-based Hive step also has to be told which version to run.
    #[test]
    fn hive_prepends_its_script_and_version_only_without_a_release() {
        let release = step(r#"{"Type": "Hive", "Args": ["-f", "s3://x.q"]}"#, Some("emr-6.0.0"));
        assert_eq!(
            release["HadoopJarStep"]["Args"],
            json!(["hive-script", "--run-hive-script", "--args", "-f", "s3://x.q"])
        );

        let ami = step(r#"{"Type": "Hive", "Args": ["-f", "s3://x.q"]}"#, None);
        assert_eq!(
            ami["HadoopJarStep"]["Args"],
            json!([
                "s3://eu-west-1.elasticmapreduce/libs/hive/hive-script",
                "--run-hive-script",
                "--hive-versions",
                "latest",
                "--args",
                "-f",
                "s3://x.q"
            ])
        );
    }

    #[test]
    fn a_custom_jar_step_is_the_default_type() {
        let built = step(r#"{"Jar": "s3://my/jar.jar", "Args": ["a"], "MainClass": "M"}"#, None);
        assert_eq!(built["Name"], "Custom JAR");
        assert_eq!(built["ActionOnFailure"], "CONTINUE");
        assert_eq!(built["HadoopJarStep"]["Jar"], "s3://my/jar.jar");
        assert_eq!(built["HadoopJarStep"]["MainClass"], "M");
    }

    #[test]
    fn a_name_and_failure_action_override_the_defaults() {
        let built = step(
            r#"{"Type": "Streaming", "Name": "mine", "ActionOnFailure": "TERMINATE_CLUSTER", "Args": ["-input", "x"]}"#,
            Some("emr-6.0.0"),
        );
        assert_eq!(built["Name"], "mine");
        assert_eq!(built["ActionOnFailure"], "TERMINATE_CLUSTER");
        assert_eq!(built["HadoopJarStep"]["Args"][0], "hadoop-streaming");
    }

    #[test]
    fn properties_become_key_value_pairs() {
        let built = step(r#"{"Jar": "j", "Properties": "a=1,b=2,c"}"#, None);
        assert_eq!(
            built["HadoopJarStep"]["Properties"],
            json!([{"Key": "a", "Value": "1"}, {"Key": "b", "Value": "2"}, {"Key": "c", "Value": ""}])
        );
    }

    #[test]
    fn log_settings_become_a_monitoring_block_only_when_given() {
        let plain = step(r#"{"Jar": "j"}"#, None);
        assert!(plain.get("StepMonitoringConfiguration").is_none());
        let logged = step(r#"{"Jar": "j", "LogUri": "s3://logs/"}"#, None);
        assert_eq!(
            logged["StepMonitoringConfiguration"]["S3MonitoringConfiguration"]["LogUri"],
            "s3://logs/"
        );
    }

    /// Impala never ran on a release-based cluster, so this is a refusal, not a gap.
    #[test]
    fn impala_is_refused_on_a_release_based_cluster() {
        let parsed: Value = serde_json::from_str(r#"{"Type": "Impala", "Args": ["x"]}"#).expect("step");
        assert!(build_step(&parsed, Some("emr-6.0.0"), "us-east-1").is_err());
        assert!(build_step(&parsed, None, "us-east-1").is_ok());
    }

    #[test]
    fn a_step_missing_its_required_field_is_refused() {
        let no_jar: Value = serde_json::from_str(r#"{"Type": "Custom_JAR"}"#).expect("step");
        assert!(build_step(&no_jar, None, "us-east-1").is_err());
        let no_args: Value = serde_json::from_str(r#"{"Type": "Hive"}"#).expect("step");
        assert!(build_step(&no_args, None, "us-east-1").is_err());
        let unknown: Value = serde_json::from_str(r#"{"Type": "Nope"}"#).expect("step");
        assert!(build_step(&unknown, None, "us-east-1").is_err());
    }

    #[test]
    fn hive_installs_with_a_second_step_only_for_a_site_path() {
        let plain: Vec<Value> = vec![json!({"Name": "Hive"})];
        let steps = install_steps(&plain, "eu-west-1");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0]["Name"], "Install Hive");
        assert_eq!(steps[0]["ActionOnFailure"], "TERMINATE_CLUSTER");
        assert_eq!(
            steps[0]["HadoopJarStep"]["Jar"],
            "s3://eu-west-1.elasticmapreduce/libs/script-runner/script-runner.jar"
        );

        let with_site: Vec<Value> =
            vec![json!({"Name": "Hive", "Args": ["--hive-site=s3://conf/hive-site.xml"]})];
        let steps = install_steps(&with_site, "eu-west-1");
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[1]["Name"], "Install Hive Site Configuration");
        // Recoverable, so it waits rather than killing the cluster.
        assert_eq!(steps[1]["ActionOnFailure"], "CANCEL_AND_WAIT");
        let args = steps[1]["HadoopJarStep"]["Args"].as_array().expect("args");
        assert!(args.contains(&json!("s3://conf/hive-site.xml")));
        // The reference builds this base path with no region, so it is always us-east-1
        // even on a eu-west-1 cluster. Reproduced rather than corrected.
        assert!(args.contains(&json!("s3://us-east-1.elasticmapreduce/libs/hive")));
    }

    #[test]
    fn pig_installs_with_its_own_script() {
        let steps = install_steps(&[json!({"Name": "PIG"})], "us-east-1");
        assert_eq!(steps[0]["Name"], "Install Pig");
        assert_eq!(
            steps[0]["HadoopJarStep"]["Args"][0],
            "s3://us-east-1.elasticmapreduce/libs/pig/pig-script"
        );
    }

    /// A known application that cannot be added to a running cluster gets a different
    /// message from one the reference has never heard of.
    #[test]
    fn the_two_rejection_messages_differ() {
        let hbase = check_installable(&[json!({"Name": "HBase"})]).expect_err("refuses");
        assert!(hbase.message().contains("cannot be installed on a running cluster"), "{}", hbase.message());

        let nope = check_installable(&[json!({"Name": "Nope"})]).expect_err("refuses");
        assert!(nope.message().contains("Unknown application: Nope"), "{}", nope.message());

        assert!(check_installable(&[json!({"Name": "hive"}), json!({"Name": "PIG"})]).is_ok());
    }

    fn flags(pairs: &[(&str, Option<&str>)]) -> std::collections::BTreeMap<&'static str, Option<&'static str>> {
        // Leaked so the map can hold `&'static str` the way `take_args` produces.
        pairs
            .iter()
            .map(|(flag, value)| {
                let flag: &'static str = Box::leak(flag.to_string().into_boxed_str());
                let value = value.map(|v| -> &'static str { Box::leak(v.to_string().into_boxed_str()) });
                (flag, value)
            })
            .collect()
    }

    /// The interval flags are named after the backup *type*, so the same numbers go out
    /// under different flags for a full and an incremental backup.
    #[test]
    fn the_schedule_flags_are_named_after_the_backup_type() {
        let full = schedule_args(&flags(&[
            ("--type", Some("full")),
            ("--dir", Some("s3://b/")),
            ("--interval", Some("2")),
            ("--unit", Some("days")),
        ]))
        .expect("builds");
        assert!(full.contains(&"--full-backup-time-interval".to_string()));
        assert!(full.contains(&"--full-backup-time-unit".to_string()));

        let incremental = schedule_args(&flags(&[
            ("--type", Some("incremental")),
            ("--dir", Some("s3://b/")),
            ("--interval", Some("2")),
            ("--unit", Some("days")),
        ]))
        .expect("builds");
        assert!(incremental.contains(&"--incremental-backup-time-interval".to_string()));
    }

    /// An absent `--start-time` is the literal string `now`, not an omitted argument.
    #[test]
    fn no_start_time_means_the_word_now() {
        let built = schedule_args(&flags(&[
            ("--type", Some("full")),
            ("--dir", Some("s3://b/")),
            ("--interval", Some("1")),
            ("--unit", Some("hours")),
        ]))
        .expect("builds");
        let at = built.iter().position(|a| a == "--start-time").expect("has start-time");
        assert_eq!(built[at + 1], "now");
    }

    #[test]
    fn the_schedule_type_and_unit_are_validated() {
        let bad_type = schedule_args(&flags(&[
            ("--type", Some("sideways")),
            ("--dir", Some("d")),
            ("--interval", Some("1")),
            ("--unit", Some("days")),
        ]));
        assert!(bad_type.expect_err("refuses").message().contains("invalid type"));

        let bad_unit = schedule_args(&flags(&[
            ("--type", Some("full")),
            ("--dir", Some("d")),
            ("--interval", Some("1")),
            ("--unit", Some("fortnights")),
        ]));
        assert!(bad_unit.expect_err("refuses").message().contains("invalid unit"));
    }

    /// Disabling neither backup would be a no-op nobody asked for.
    #[test]
    fn disabling_requires_naming_at_least_one() {
        assert!(disable_args(&flags(&[])).is_err());
        let full = disable_args(&flags(&[("--full", None)])).expect("builds");
        assert_eq!(full[2], "false");
        assert!(full.contains(&"--disable-full-backups".to_string()));
        assert!(!full.contains(&"--disable-incremental-backups".to_string()));

        let both = disable_args(&flags(&[("--full", None), ("--incremental", None)]))
            .expect("builds");
        assert!(both.contains(&"--disable-incremental-backups".to_string()));
    }

    #[test]
    fn cluster_ids_split_on_whitespace() {
        let ids: Vec<&str> = "j-1 j-2  j-3".split_whitespace().collect();
        assert_eq!(ids, vec!["j-1", "j-2", "j-3"]);
    }
}
