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

/// One `--steps` token: JSON if it starts like JSON, shorthand otherwise.
fn parse_step(token: &str) -> Result<Value, Failure> {
    let trimmed = token.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return serde_json::from_str(token).map_err(|e| {
            Failure::new(
                exit::PARAM_VALIDATION,
                awsc_runtime::RuntimeError::ParamValidation(format!(
                    "Error parsing parameter '--steps': Invalid JSON: {e}\nJSON received: {token}"
                )),
            )
        });
    }
    awsc_protocol::shorthand::parse(token).map_err(|e| {
        Failure::new(
            exit::PARAM_VALIDATION,
            awsc_runtime::RuntimeError::ParamValidation(format!(
                "Error parsing parameter '--steps': {e}"
            )),
        )
    })
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
    fn cluster_ids_split_on_whitespace() {
        let ids: Vec<&str> = "j-1 j-2  j-3".split_whitespace().collect();
        assert_eq!(ids, vec!["j-1", "j-2", "j-3"]);
    }
}
