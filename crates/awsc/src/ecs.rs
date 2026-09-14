//! `aws ecs deploy`: register a task definition and roll it out through CodeDeploy.
//!
//! A port of `customizations/ecs/deploy.py`. Six steps, three services: describe the ECS
//! service, validate the CodeDeploy application and deployment group, register the new
//! task definition, splice its ARN into the appspec, create the deployment, and wait.
//!
//! Three things decide whether the deployment CodeDeploy receives is the right one:
//!
//! - **The appspec is JSON *or* YAML**, tried in that order — the reference falls back to
//!   ruamel's safe loader, which is the same YAML 1.2 reader `--cli-input-yaml` uses.
//! - **The ARN is spliced in by case-insensitive key lookup.** `resources`, `properties`
//!   and `taskDefinition` are each found however they are spelled, because appspecs in
//!   the wild capitalise them inconsistently; the *existing* spelling is then written
//!   back, not a normalised one.
//! - **The revision's `sha256` is over the re-serialised appspec**, not over the file.
//!   CodeDeploy checks it against the `content` we send, so hashing the original bytes
//!   would fail every deployment.
//!
//! The wait is not the waiter's own timeout either: it is the deployment group's
//! configured wait plus ten minutes, clamped to [30, 360].

use crate::args::Parsed;
use crate::client::{Client, Globals};
use crate::exit;
use crate::Failure;
use serde_json::{json, Value};
use std::process::ExitCode;

/// Added to the deployment group's configured wait, as headroom.
const TIMEOUT_BUFFER_MIN: u64 = 10;
const DEFAULT_DELAY_SEC: u64 = 15;
const MAX_WAIT_MIN: u64 = 360;
/// Names longer than this are truncated when a default application or group name is
/// derived from them.
const MAX_CHAR_LENGTH: usize = 46;

const FLAGS: &[&str] = &[
    "--service",
    "--task-definition",
    "--codedeploy-appspec",
    "--cluster",
    "--codedeploy-application",
    "--codedeploy-deployment-group",
];

pub fn dispatch(parsed: &Parsed, globals: &Globals) -> Result<Option<ExitCode>, Failure> {
    match parsed.operation.as_str() {
        "deploy" => deploy(parsed, globals).map(Some),
        _ => Ok(None),
    }
}

fn deploy(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let args = crate::custom::take_args(parsed, FLAGS)?;
    let required = ["--service", "--task-definition", "--codedeploy-appspec"];
    let missing: Vec<&str> =
        required.into_iter().filter(|flag| !args.contains_key(flag)).collect();
    if !missing.is_empty() {
        return Err(crate::custom::missing_required(&missing));
    }
    let value = |flag: &str| args.get(flag).copied().flatten().unwrap_or_default();
    let service = value("--service");

    let task_def: Value = serde_json::from_str(&read_file(value("--task-definition"))?)
        .map_err(|e| {
            Failure::new(
                exit::GENERAL_ERROR,
                format!("Unable to load file at {}: {e}", value("--task-definition")),
            )
        })?;
    let mut appspec = parse_appspec(&read_file(value("--codedeploy-appspec"))?, value("--codedeploy-appspec"))?;

    let region = crate::custom::resolve_region(globals)
        .ok_or_else(|| Failure::new(exit::CONFIGURATION, awsc_runtime::RuntimeError::NoRegion))?;

    // ECS is the service the user named, so `--endpoint-url` applies to it.
    let ecs_globals = Globals { region: Some(region.clone()), ..globals.clone() };
    let ecs_model = crate::load_model("ecs").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let ecs = Client::new(&ecs_model, &ecs_globals)?;

    // `if cluster is None or ''` — the reference's test is a Python quirk that is always
    // false for the empty string, but an absent flag does fall back to "default".
    let cluster = match args.get("--cluster").copied().flatten() {
        Some(name) if !name.is_empty() => name,
        _ => "default",
    };
    let described = ecs
        .call("describe-services", Some(&json!({ "cluster": cluster, "services": [service] })))
        .map_err(|e| client_error("describe ECS service", &e))?;
    let services = described.get("services").and_then(Value::as_array).cloned().unwrap_or_default();
    let Some(details) = services.first() else {
        return Err(Failure::new(
            exit::GENERAL_ERROR,
            format!("Service '{service}' not found in cluster '{cluster}'"),
        ));
    };
    let service_arn = details.get("serviceArn").and_then(Value::as_str).unwrap_or_default();
    let service_name = details.get("serviceName").and_then(Value::as_str).unwrap_or_default();
    let cluster_arn = details.get("clusterArn").and_then(Value::as_str).unwrap_or_default();
    // `arn:aws:ecs:region:account:cluster/NAME` — the segment after the slash.
    let cluster_name = cluster_arn.split('/').nth(1).unwrap_or_default();

    let app_name = match args.get("--codedeploy-application").copied().flatten() {
        Some(given) => given.to_string(),
        None => format!("AppECS-{}", ecs_suffix(cluster_name, service_name)),
    };
    let group_name = match args.get("--codedeploy-deployment-group").copied().flatten() {
        Some(given) => given.to_string(),
        None => format!("DgpECS-{}", ecs_suffix(cluster_name, service_name)),
    };

    // CodeDeploy is a *different* service, so the user's `--endpoint-url` does not follow.
    let cd_globals = Globals { region: Some(region), ..globals.for_service("codedeploy") };
    let cd_model =
        crate::load_model("deploy").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let codedeploy = Client::new(&cd_model, &cd_globals)?;

    let application = codedeploy
        .call("get-application", Some(&json!({ "applicationName": app_name })))
        .map_err(|e| client_error("describe Code Deploy application", &e))?;
    let group = codedeploy
        .call(
            "get-deployment-group",
            Some(&json!({ "applicationName": app_name, "deploymentGroupName": group_name })),
        )
        .map_err(|e| client_error("describe Code Deploy deployment group", &e))?;

    validate(&application, &group, &app_name, &group_name, service_name, service_arn, cluster_name, cluster_arn)?;
    let wait_minutes = configured_wait(&group);

    let registered = ecs
        .call("register-task-definition", Some(&task_def))
        .map_err(|e| client_error("register ECS task definition", &e))?;
    let task_def_arn = registered
        .get("taskDefinition")
        .and_then(|t| t.get("taskDefinitionArn"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    println!("Successfully registered new ECS task definition {task_def_arn}");

    update_task_def_arn(&mut appspec, &task_def_arn)?;
    // `json.dumps` with its *default* separators, which put a space after `,` and `:`.
    // The hash is over exactly these bytes, so this is self-consistent either way — but
    // the request body is a byte difference from the reference's, and this is a drop-in
    // replacement, so it matches.
    let content = python_json(&appspec);
    let digest = {
        use sha2::Digest;
        format!("{:x}", sha2::Sha256::digest(content.as_bytes()))
    };
    let deployment = codedeploy
        .call(
            "create-deployment",
            Some(&json!({
                "applicationName": app_name,
                "deploymentGroupName": group_name,
                "revision": {
                    "revisionType": "AppSpecContent",
                    "appSpecContent": { "content": content, "sha256": digest },
                },
            })),
        )
        .map_err(|e| client_error("create deployment", &e))?;
    let deployment_id = deployment
        .get("deploymentId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    println!("Successfully created deployment {deployment_id}");

    let wait_minutes = clamp_wait(wait_minutes);
    println!("Waiting for {deployment_id} to succeed (will wait up to {wait_minutes} minutes)...");
    use std::io::Write;
    let _ = std::io::stdout().flush();

    let waiter = awsc_model::waiters::get("deploy", "deployment-successful").ok_or_else(|| {
        Failure::new(exit::GENERAL_ERROR, "the codedeploy deployment-successful waiter is missing")
    })?;
    crate::wait::run_with(
        &codedeploy,
        waiter,
        "deployment-successful",
        Some(&json!({ "deploymentId": deployment_id })),
        DEFAULT_DELAY_SEC,
        wait_minutes * 60 / DEFAULT_DELAY_SEC,
    )?;

    println!("Successfully deployed {task_def_arn} to service '{service_name}'");
    let _ = std::io::stdout().flush();
    Ok(exit::code(exit::SUCCESS))
}

/// `json.dumps(value)` with Python's default separators: `", "` between items and `": "`
/// after a key. serde_json's compact form omits both spaces.
fn python_json(value: &Value) -> String {
    struct Spaced;
    impl serde_json::ser::Formatter for Spaced {
        fn begin_array_value<W: ?Sized + std::io::Write>(
            &mut self,
            writer: &mut W,
            first: bool,
        ) -> std::io::Result<()> {
            if first {
                Ok(())
            } else {
                writer.write_all(b", ")
            }
        }
        fn begin_object_key<W: ?Sized + std::io::Write>(
            &mut self,
            writer: &mut W,
            first: bool,
        ) -> std::io::Result<()> {
            if first {
                Ok(())
            } else {
                writer.write_all(b", ")
            }
        }
        fn begin_object_value<W: ?Sized + std::io::Write>(
            &mut self,
            writer: &mut W,
        ) -> std::io::Result<()> {
            writer.write_all(b": ")
        }
    }
    let mut buffer = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(&mut buffer, Spaced);
    serde::Serialize::serialize(value, &mut serializer).expect("an appspec serializes");
    String::from_utf8(buffer).expect("serde_json emits UTF-8")
}

/// `Failed to {action}:\n{error}` — the reference wraps the client error rather than
/// letting it surface as itself, so the message names what it was trying to do.
fn client_error(action: &str, failure: &Failure) -> Failure {
    Failure::new(exit::GENERAL_ERROR, format!("Failed to {action}:\n{}", failure.message()))
}

fn read_file(path: &str) -> Result<String, Failure> {
    // `os.path.expandvars(os.path.expanduser(path))` before opening.
    let expanded = crate::args::shellexpand_public(path);
    std::fs::read_to_string(&expanded)
        .map_err(|e| Failure::new(exit::GENERAL_ERROR, format!("Unable to load file at {path}: {e}")))
}

/// JSON first, then YAML — which is how an appspec is allowed to be either.
fn parse_appspec(text: &str, path: &str) -> Result<Value, Failure> {
    if let Ok(value) = serde_json::from_str(text) {
        return Ok(value);
    }
    crate::yaml::parse(text)
        .map_err(|e| Failure::new(exit::GENERAL_ERROR, format!("Unable to load file at {path}: {e}")))
}

/// `<cluster>-<service>`, each truncated to 46 characters.
fn ecs_suffix(cluster: &str, service: &str) -> String {
    let cut = |text: &str| text.chars().take(MAX_CHAR_LENGTH).collect::<String>();
    let cluster = if cluster.is_empty() { "default".to_string() } else { cut(cluster) };
    format!("{cluster}-{}", cut(service))
}

/// The key `object` uses for `wanted`, however it is capitalised.
fn find_key(object: &Value, wanted: &str) -> Option<String> {
    object
        .as_object()?
        .keys()
        .find(|key| key.to_lowercase() == wanted.to_lowercase())
        .cloned()
}

fn missing_property(resource: &str, property: &str) -> Failure {
    Failure::new(
        exit::GENERAL_ERROR,
        format!("Resource '{resource}' must include property '{property}'"),
    )
}

/// Write the new task definition ARN into every resource in the appspec.
fn update_task_def_arn(appspec: &mut Value, arn: &str) -> Result<(), Failure> {
    let resources_key = find_key(appspec, "resources")
        .ok_or_else(|| missing_property("codedeploy-appspec", "resources"))?;
    let Some(resources) = appspec.get_mut(&resources_key).and_then(Value::as_array_mut) else {
        return Err(missing_property("codedeploy-appspec", "resources"));
    };
    for resource in resources.iter_mut() {
        let names: Vec<String> =
            resource.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
        for name in names {
            let content = &resource[&name];
            let properties_key =
                find_key(content, "properties").ok_or_else(|| missing_property(&name, "properties"))?;
            let task_def_key = find_key(&content[&properties_key], "taskDefinition")
                .ok_or_else(|| missing_property(&properties_key, "taskDefinition"))?;
            // The existing spelling is written back, not a normalised one.
            resource[&name][&properties_key][&task_def_key] = Value::String(arn.to_string());
        }
    }
    Ok(())
}

/// Both resources must be on the ECS compute platform, and the deployment group must
/// target this exact service and cluster — by name or by ARN, since either can be stored.
#[allow(clippy::too_many_arguments)]
fn validate(
    application: &Value,
    group: &Value,
    app_name: &str,
    group_name: &str,
    service: &str,
    service_arn: &str,
    cluster: &str,
    cluster_arn: &str,
) -> Result<(), Failure> {
    let platform = |value: &Value, path: &[&str]| -> String {
        let mut current = value;
        for step in path {
            current = match current.get(step) {
                Some(next) => next,
                None => return String::new(),
            };
        }
        current.as_str().unwrap_or_default().to_string()
    };
    if platform(application, &["application", "computePlatform"]) != "ECS" {
        return Err(Failure::new(
            exit::GENERAL_ERROR,
            format!("Application '{app_name}' must support 'ECS' compute platform"),
        ));
    }
    if platform(group, &["deploymentGroupInfo", "computePlatform"]) != "ECS" {
        return Err(Failure::new(
            exit::GENERAL_ERROR,
            format!("Deployment Group '{group_name}' must support 'ECS' compute platform"),
        ));
    }
    let targets = group
        .get("deploymentGroupInfo")
        .and_then(|info| info.get("ecsServices"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for target in targets {
        let target_service = target.get("serviceName").and_then(Value::as_str).unwrap_or_default();
        if target_service != service && target_service != service_arn {
            return Err(Failure::new(
                exit::GENERAL_ERROR,
                format!("deployment group '{group_name}' does not target ECS service '{service}'"),
            ));
        }
        let target_cluster = target.get("clusterName").and_then(Value::as_str).unwrap_or_default();
        if target_cluster != cluster && target_cluster != cluster_arn {
            return Err(Failure::new(
                exit::GENERAL_ERROR,
                format!("deployment group '{group_name}' does not target ECS cluster '{cluster}'"),
            ));
        }
    }
    Ok(())
}

/// The deployment group's own wait, plus headroom. `None` when the group did not describe
/// a blue/green configuration.
fn configured_wait(group: &Value) -> Option<u64> {
    let blue_green = group.get("deploymentGroupInfo")?.get("blueGreenDeploymentConfiguration")?;
    let ready = blue_green.get("deploymentReadyOption")?.get("waitTimeInMinutes")?.as_u64()?;
    let terminate = blue_green
        .get("terminateBlueInstancesOnDeploymentSuccess")?
        .get("terminationWaitTimeInMinutes")?
        .as_u64()?;
    Some(ready + terminate + TIMEOUT_BUFFER_MIN)
}

/// At least 30 minutes, at most 360 — so a group configured to wait an hour gets an hour
/// plus headroom, and one with no configuration still gets half an hour.
fn clamp_wait(minutes: Option<u64>) -> u64 {
    match minutes {
        Some(minutes) if minutes > MAX_WAIT_MIN => MAX_WAIT_MIN,
        Some(minutes) if minutes >= 30 => minutes,
        _ => 30,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Python's default separators, which is what the appspec hash is taken over.
    #[test]
    fn the_appspec_is_serialized_the_way_python_does() {
        let value = serde_json::json!({"a": 1, "b": [1, 2], "c": {"d": "e"}});
        assert_eq!(python_json(&value), r#"{"a": 1, "b": [1, 2], "c": {"d": "e"}}"#);
    }

    #[test]
    fn default_names_truncate_each_half_at_46_characters() {
        let long = "x".repeat(60);
        let suffix = ecs_suffix(&long, &long);
        assert_eq!(suffix, format!("{}-{}", "x".repeat(46), "x".repeat(46)));
        // An empty cluster name is the literal "default".
        assert_eq!(ecs_suffix("", "svc"), "default-svc");
    }

    /// Appspec keys are matched case-insensitively and written back in the spelling the
    /// file used — a normalised key would be a different document to CodeDeploy.
    #[test]
    fn the_arn_is_spliced_in_whatever_the_case() {
        let mut appspec = serde_json::json!({
            "Resources": [{
                "my-service": {
                    "Type": "AWS::ECS::Service",
                    "Properties": { "TaskDefinition": "old", "LoadBalancerInfo": {"x": 1} }
                }
            }]
        });
        update_task_def_arn(&mut appspec, "arn:new").expect("splices");
        assert_eq!(appspec["Resources"][0]["my-service"]["Properties"]["TaskDefinition"], "arn:new");
        // Everything else is untouched, including the spelling of the keys.
        assert_eq!(appspec["Resources"][0]["my-service"]["Properties"]["LoadBalancerInfo"]["x"], 1);
    }

    #[test]
    fn a_missing_property_names_the_resource() {
        let mut appspec = serde_json::json!({"resources": [{"svc": {"properties": {}}}]});
        let failure = update_task_def_arn(&mut appspec, "arn").expect_err("refuses");
        assert!(failure.message().contains("must include property 'taskDefinition'"), "{}", failure.message());

        let mut no_resources = serde_json::json!({"version": "0.0"});
        assert!(update_task_def_arn(&mut no_resources, "arn").is_err());
    }

    #[test]
    fn an_appspec_may_be_yaml_or_json() {
        let yaml = "version: 0.0\nresources:\n  - svc:\n      properties:\n        taskDefinition: old\n";
        let parsed = parse_appspec(yaml, "f.yaml").expect("yaml parses");
        assert_eq!(parsed["resources"][0]["svc"]["properties"]["taskDefinition"], "old");
        let json_text = r#"{"version": "0.0", "resources": []}"#;
        assert_eq!(parse_appspec(json_text, "f.json").expect("json parses")["resources"], serde_json::json!([]));
    }

    #[test]
    fn the_wait_is_the_groups_plus_ten_minutes_clamped() {
        let group = serde_json::json!({"deploymentGroupInfo": {"blueGreenDeploymentConfiguration": {
            "deploymentReadyOption": {"waitTimeInMinutes": 60},
            "terminateBlueInstancesOnDeploymentSuccess": {"terminationWaitTimeInMinutes": 5}
        }}});
        assert_eq!(configured_wait(&group), Some(75));
        assert_eq!(clamp_wait(Some(75)), 75);
        // Below the floor and above the ceiling.
        assert_eq!(clamp_wait(Some(5)), 30);
        assert_eq!(clamp_wait(Some(1000)), MAX_WAIT_MIN);
        assert_eq!(clamp_wait(None), 30);
        assert_eq!(configured_wait(&serde_json::json!({})), None);
    }

    #[test]
    fn validation_rejects_a_group_that_targets_something_else() {
        let application = serde_json::json!({"application": {"computePlatform": "ECS"}});
        let group = serde_json::json!({"deploymentGroupInfo": {
            "computePlatform": "ECS",
            "ecsServices": [{"serviceName": "other", "clusterName": "c"}]
        }});
        let failure =
            validate(&application, &group, "app", "dgp", "svc", "arn:svc", "c", "arn:c")
                .expect_err("refuses");
        assert!(failure.message().contains("does not target ECS service 'svc'"));

        // The ARN is accepted where the name is expected, and the other way round.
        let group = serde_json::json!({"deploymentGroupInfo": {
            "computePlatform": "ECS",
            "ecsServices": [{"serviceName": "arn:svc", "clusterName": "arn:c"}]
        }});
        assert!(validate(&application, &group, "app", "dgp", "svc", "arn:svc", "c", "arn:c").is_ok());
    }

    #[test]
    fn validation_requires_the_ecs_compute_platform() {
        let server = serde_json::json!({"application": {"computePlatform": "Server"}});
        let group = serde_json::json!({"deploymentGroupInfo": {"computePlatform": "ECS", "ecsServices": []}});
        assert!(validate(&server, &group, "app", "dgp", "s", "a", "c", "ac").is_err());
    }
}
