//! `aws emr create-cluster`.
//!
//! A port of `customizations/emr/createcluster.py` and the five helper modules it draws
//! on. It is the largest customization in the reference: fifty-odd flags collapsing into
//! one `RunJobFlow` request, with roughly twenty validations between them.
//!
//! The thing to hold on to while reading is that **there are two eras of EMR** and the
//! command serves both:
//!
//! - `--release-label` (emr-5.x and later) puts applications, EMRFS settings and step
//!   jars straight into the request as structured fields, run by `command-runner.jar`.
//! - `--ami-version` (the 2.x/3.x line) has none of that. Applications become bootstrap
//!   actions and install *steps*, EMRFS becomes a bootstrap action, and every step runs
//!   through a `script-runner.jar` fetched from a regional S3 bucket.
//!
//! Exactly one of the two is required, and which one is given changes what half the
//! other flags mean. That is why so much of this file branches on `release_label`.

use crate::args::Parsed;
use crate::client::Globals;
use crate::exit;
use crate::Failure;
use serde_json::{json, Map, Value};
use std::process::ExitCode;

const EMR_ROLE_NAME: &str = "EMR_DefaultRole";
const EC2_ROLE_NAME: &str = "EMR_EC2_DefaultRole";
const MAX_BOOTSTRAP_ACTIONS: usize = 16;
const DEFAULT_CLUSTER_NAME: &str = "Development Cluster";
const EMRFS_SITE: &str = "emrfs-site";

fn param_error(message: impl std::fmt::Display) -> Failure {
    Failure::new(
        exit::PARAM_VALIDATION,
        awsc_runtime::RuntimeError::ParamValidation(message.to_string()),
    )
}

fn mutually_exclusive(one: &str, two: &str, extra: &str) -> Failure {
    param_error(format!(
        "You cannot specify both {one} and {two} options together.{extra}"
    ))
}

/// `[1, 2, 3]` → `1, 2 and 3`, which is how the reference lists names in its errors.
fn join_names(values: &[String]) -> String {
    match values {
        [] => String::new(),
        [only] => only.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Insert only when the value is truthy, which is what `emrutils.apply_dict` does — and
/// it means an empty string, an empty list and a zero never reach the request.
fn apply(params: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        if truthy(&value) {
            params.insert(key.to_string(), value);
        }
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `--option` / `--no-option`, which are separate flags rather than one tri-state.
///
/// Giving both is an error rather than a last-one-wins, because the two spellings are
/// equally emphatic and guessing which was meant would silently build a different cluster.
fn boolean_pair(
    on: bool,
    on_name: &str,
    off: bool,
    off_name: &str,
) -> Result<bool, Failure> {
    if on && off {
        return Err(param_error(format!(
            "cannot use both {on_name} and {off_name} options together."
        )));
    }
    Ok(on)
}

/// Append to a list already under `key`, or set it if there is none.
fn extend(params: &mut Map<String, Value>, key: &str, values: Vec<Value>) {
    match params.get_mut(key) {
        Some(Value::Array(existing)) => existing.extend(values),
        _ => {
            if !values.is_empty() {
                params.insert(key.to_string(), Value::Array(values));
            }
        }
    }
}

fn text<'v>(value: &'v Value, key: &str) -> Option<&'v str> {
    value.get(key).and_then(Value::as_str)
}

fn objects(value: Option<&Value>) -> Vec<Value> {
    match value {
        Some(Value::Array(items)) => items.clone(),
        Some(other) => vec![other.clone()],
        None => Vec::new(),
    }
}

pub fn run(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let (model, emr_globals) = super::emr::load(globals)?;
    let region = emr_globals.region.clone().unwrap_or_default();
    let params = build(parsed, &region)?;
    // Shorthand has no types: `TargetOnDemandCapacity=1` parses as the string "1", and
    // EMR rejects that. The reference coerces against each argument's declared schema;
    // here the *model's* input shape does the same job for every field at once, which is
    // both less to maintain and harder to get wrong.
    let (_, operation) = model
        .operation("run-job-flow")
        .map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let input = Value::Object(params);
    let input = match operation.input.as_ref() {
        Some(target) => crate::args::coerce(&model, &target.target, input),
        None => input,
    };
    let client = crate::client::Client::new(&model, &emr_globals)?;
    let response = client.call("run-job-flow", Some(&input))?;

    // Only two fields of the response are reported, and only when there is an id: the
    // reference returns an empty document otherwise rather than echoing the API's.
    let result = match response.get("JobFlowId").and_then(Value::as_str) {
        Some(id) => json!({
            "ClusterId": id,
            "ClusterArn": response.get("ClusterArn").cloned().unwrap_or(Value::Null),
        }),
        None => json!({}),
    };
    super::emr::render(&result, parsed)
}

/// Assemble the `RunJobFlow` request. Separate from [`run`] so every branch of it can be
/// tested without an EMR account.
pub(crate) fn build(parsed: &Parsed, region: &str) -> Result<Map<String, Value>, Failure> {
    let flags = [
        "--release-label", "--os-release-label", "--ami-version", "--instance-groups",
        "--instance-type", "--instance-count", "--auto-terminate", "--no-auto-terminate",
        "--instance-fleets", "--name", "--log-uri", "--log-encryption-kms-key-id",
        "--service-role", "--auto-scaling-role", "--use-default-roles", "--configurations",
        "--ec2-attributes", "--termination-protected", "--no-termination-protected",
        "--unhealthy-node-replacement", "--no-unhealthy-node-replacement",
        "--scale-down-behavior", "--visible-to-all-users", "--no-visible-to-all-users",
        "--enable-debugging", "--no-enable-debugging", "--tags", "--bootstrap-actions",
        "--applications", "--emrfs", "--steps", "--additional-info",
        "--restore-from-hbase-backup", "--security-configuration", "--custom-ami-id",
        "--ebs-root-volume-size", "--ebs-root-volume-iops", "--ebs-root-volume-throughput",
        "--repo-upgrade-on-boot", "--kerberos-attributes", "--step-concurrency-level",
        "--step-execution-role-arn", "--managed-scaling-policy",
        "--placement-group-configs", "--auto-termination-policy",
        "--monitoring-configuration", "--extended-support", "--no-extended-support",
        "--session-enabled", "--no-session-enabled",
    ];
    let args = crate::custom::take_args(parsed, &flags)?;
    let value = |flag: &str| args.get(flag).copied().flatten();
    let given = |flag: &str| args.contains_key(flag);
    // A flag whose schema is an *object*: one token, parsed as one value.
    let structure = |flag: &str| -> Result<Option<Value>, Failure> {
        match value(flag) {
            None => Ok(None),
            Some(token) => crate::custom::parse_shorthand_token(token, flag).map(Some),
        }
    };
    // A flag whose schema is an *array*: each token is a separate element, so
    // `--applications Name=Spark Name=Hive` is two applications rather than one with a
    // mangled name.
    let sequence = |flag: &str| -> Result<Option<Value>, Failure> {
        if !args.contains_key(flag) {
            return Ok(None);
        }
        let mut items = Vec::new();
        for token in crate::custom::take_list(parsed, flag) {
            match crate::custom::parse_shorthand_token(token, flag)? {
                // A JSON token may itself be the whole array.
                Value::Array(inner) => items.extend(inner),
                other => items.push(other),
            }
        }
        Ok(Some(Value::Array(items)))
    };
    let number = |flag: &str| -> Result<Option<i64>, Failure> {
        match value(flag) {
            None => Ok(None),
            Some(text) => text
                .parse::<i64>()
                .map(Some)
                .map_err(|_| param_error(format!("Invalid value for {flag}: {text}"))),
        }
    };

    let release_label = value("--release-label");
    let ami_version = value("--ami-version");
    let mut params = Map::new();
    params.insert(
        "Name".to_string(),
        Value::String(value("--name").unwrap_or(DEFAULT_CLUSTER_NAME).to_string()),
    );

    // One era or the other, and exactly one.
    if ami_version.is_some() && release_label.is_some() {
        return Err(mutually_exclusive("--ami-version", "--release-label", ""));
    }
    if ami_version.is_none() && release_label.is_none() {
        return Err(param_error("Either --ami-version or --release-label is required."));
    }

    let use_default_roles = given("--use-default-roles");
    let mut ec2_attributes = structure("--ec2-attributes")?;
    let role_message = " Either choose --use-default-roles or use both --service-role \
                        <roleName> and --ec2-attributes InstanceProfile=<profileName>.";
    if use_default_roles && value("--service-role").is_some() {
        return Err(mutually_exclusive("--use-default-roles", "--service-role", role_message));
    }
    if use_default_roles
        && ec2_attributes.as_ref().is_some_and(|a| a.get("InstanceProfile").is_some())
    {
        return Err(mutually_exclusive(
            "--use-default-roles",
            "--ec2-attributes InstanceProfile",
            role_message,
        ));
    }

    let instance_groups = sequence("--instance-groups")?;
    let instance_fleets = sequence("--instance-fleets")?;
    if instance_groups.is_some() && instance_fleets.is_some() {
        return Err(mutually_exclusive("--instance-groups", "--instance-fleets", ""));
    }

    let mut instances = Map::new();
    match &instance_fleets {
        Some(fleets) => {
            instances.insert("InstanceFleets".to_string(), Value::Array(build_fleets(fleets)));
        }
        None => {
            let groups = build_groups(
                instance_groups.as_ref(),
                value("--instance-type"),
                number("--instance-count")?,
            )?;
            instances.insert("InstanceGroups".to_string(), Value::Array(groups));
        }
    }

    if let Some(release_label) = release_label {
        params.insert("ReleaseLabel".to_string(), Value::String(release_label.to_string()));
        if let Some(configurations) = value("--configurations") {
            let parsed: Value = read_json(configurations).ok_or_else(|| {
                param_error("invalid json argument for option --configurations")
            })?;
            params.insert("Configurations".to_string(), parsed);
        }
    }
    if release_label.is_none() {
        if let Some(ami_version) = ami_version {
            // `\d?\..*` — a version number, however loosely.
            let valid = ami_version
                .split_once('.')
                .is_some_and(|(head, _)| head.is_empty() || head.chars().all(|c| c.is_ascii_digit()) && head.len() <= 1);
            if !valid {
                return Err(param_error(format!(
                    "The supplied AMI version \"{ami_version}\" is invalid. Please see AMI \
                     Versions Supported in Amazon EMR in Amazon Elastic MapReduce Developer \
                     Guide: http://docs.aws.amazon.com/ElasticMapReduce/latest/DeveloperGuide/\
ami-versions-supported.html"
                )));
            }
            params.insert("AmiVersion".to_string(), Value::String(ami_version.to_string()));
        }
    }

    apply(&mut params, "AdditionalInfo", value("--additional-info").map(|text| Value::String(text.to_string())));
    apply(&mut params, "LogUri", value("--log-uri").map(|text| Value::String(text.to_string())));
    apply(&mut params, "OSReleaseLabel", value("--os-release-label").map(|text| Value::String(text.to_string())));
    apply(
        &mut params,
        "LogEncryptionKmsKeyId",
        value("--log-encryption-kms-key-id").map(|text| Value::String(text.to_string())),
    );

    let mut service_role = value("--service-role").map(str::to_string);
    if use_default_roles {
        service_role = Some(EMR_ROLE_NAME.to_string());
        let mut attributes = ec2_attributes.unwrap_or_else(|| json!({}));
        attributes["InstanceProfile"] = Value::String(EC2_ROLE_NAME.to_string());
        ec2_attributes = Some(attributes);
    }
    apply(&mut params, "ServiceRole", service_role.map(Value::String));

    // An autoscaling policy on any group needs a role to assume; without one the cluster
    // would come up and then fail to scale, which is worse than refusing now.
    let auto_scaling_role = value("--auto-scaling-role");
    if instance_groups.is_some() {
        let has_policy = instances
            .get("InstanceGroups")
            .and_then(Value::as_array)
            .is_some_and(|groups| groups.iter().any(|g| g.get("AutoScalingPolicy").is_some()));
        if has_policy && auto_scaling_role.is_none() {
            return Err(param_error(
                "Must specify --auto-scaling-role when configuring an AutoScaling policy \
                 for an instance group.",
            ));
        }
    }
    apply(&mut params, "AutoScalingRole", auto_scaling_role.map(|text| Value::String(text.to_string())));
    apply(&mut params, "ScaleDownBehavior", value("--scale-down-behavior").map(|text| Value::String(text.to_string())));

    // Neither given means `--no-auto-terminate`: a cluster that shuts down the moment its
    // steps finish is not what someone who said nothing expects.
    let auto_terminate = given("--auto-terminate");
    let no_auto_terminate = given("--no-auto-terminate") || !auto_terminate;
    instances.insert(
        "KeepJobFlowAliveWhenNoSteps".to_string(),
        Value::Bool(boolean_pair(
            no_auto_terminate,
            "--no-auto-terminate",
            auto_terminate,
            "--auto-terminate",
        )?),
    );
    instances.insert(
        "TerminationProtected".to_string(),
        Value::Bool(boolean_pair(
            given("--termination-protected"),
            "--termination-protected",
            given("--no-termination-protected"),
            "--no-termination-protected",
        )?),
    );
    if given("--unhealthy-node-replacement") || given("--no-unhealthy-node-replacement") {
        instances.insert(
            "UnhealthyNodeReplacement".to_string(),
            Value::Bool(boolean_pair(
                given("--unhealthy-node-replacement"),
                "--unhealthy-node-replacement",
                given("--no-unhealthy-node-replacement"),
                "--no-unhealthy-node-replacement",
            )?),
        );
    }

    let visible = given("--visible-to-all-users") || !given("--no-visible-to-all-users");
    params.insert(
        "VisibleToAllUsers".to_string(),
        Value::Bool(boolean_pair(
            visible,
            "--visible-to-all-users",
            given("--no-visible-to-all-users"),
            "--no-visible-to-all-users",
        )?),
    );

    // Unconditional, unlike everything else here: the reference assigns `Tags` whether or
    // not any were given, so an empty list is sent.
    params.insert(
        "Tags".to_string(),
        Value::Array(
            crate::custom::take_list(parsed, "--tags")
                .iter()
                .map(|tag| match tag.split_once('=') {
                    Some((key, value)) => json!({ "Key": key, "Value": value }),
                    None => json!({ "Key": tag, "Value": "" }),
                })
                .collect(),
        ),
    );
    params.insert("Instances".to_string(), Value::Object(instances));

    if let Some(attributes) = &ec2_attributes {
        build_ec2_attributes(&mut params, attributes)?;
    }

    let debugging = boolean_pair(
        given("--enable-debugging"),
        "--enable-debugging",
        given("--no-enable-debugging"),
        "--no-enable-debugging",
    )?;
    if debugging {
        if value("--log-uri").is_none() {
            return Err(param_error(
                "LogUri not specified. You must specify a logUri if you enable debugging \
                 when creating a cluster.",
            ));
        }
        let step = match release_label {
            Some(_) => build_step_config(
                "command-runner.jar",
                "Setup Hadoop Debugging",
                "TERMINATE_CLUSTER",
                vec!["state-pusher-script".to_string()],
            ),
            None => build_step_config(
                &super::emr::script_runner(region),
                "Setup Hadoop Debugging",
                "TERMINATE_CLUSTER",
                vec![super::emr::s3_link(region, "/libs/state-pusher/0.1/fetch")],
            ),
        };
        extend(&mut params, "Steps", vec![step]);
    }

    let applications = sequence("--applications")?;
    if let Some(applications) = &applications {
        let listed = objects(Some(applications));
        match release_label {
            // A release-label cluster takes the applications as given.
            Some(_) => {
                params.insert("Applications".to_string(), Value::Array(listed));
            }
            None => {
                let (products, bootstrap, steps) =
                    build_ami_applications(&listed, region, ami_version.unwrap_or_default())?;
                extend(&mut params, "NewSupportedProducts", products);
                extend(&mut params, "BootstrapActions", bootstrap);
                extend(&mut params, "Steps", steps);
            }
        }
    }

    let hbase_restore = structure("--restore-from-hbase-backup")?;
    if let Some(restore) = &hbase_restore {
        let mut args = vec!["emr.hbase.backup.Main".to_string(), "--restore".to_string()];
        if let Some(dir) = text(restore, "Dir") {
            args.push("--backup-dir-to-restore".to_string());
            args.push(dir.to_string());
        }
        if let Some(version) = text(restore, "BackupVersion") {
            args.push("--backup-version".to_string());
            args.push(version.to_string());
        }
        extend(
            &mut params,
            "Steps",
            vec![build_step_config(
                "/home/hadoop/lib/hbase.jar",
                "Restore HBase",
                "CANCEL_AND_WAIT",
                args,
            )],
        );
    }

    if let Some(actions) = sequence("--bootstrap-actions")? {
        build_bootstrap_actions(&mut params, &objects(Some(&actions)))?;
    }

    if let Some(emrfs) = structure("--emrfs")? {
        match release_label {
            Some(_) => {
                // `--configurations` may already carry an `emrfs-site` block, and two of
                // them would leave the cluster's EMRFS settings ambiguous.
                if let Some(Value::Array(configurations)) = params.get("Configurations") {
                    if configurations
                        .iter()
                        .any(|config| text(config, "Classification") == Some(EMRFS_SITE))
                    {
                        return Err(param_error(
                            "EMRFS should be configured either using --configuration or \
                             --emrfs but not both",
                        ));
                    }
                }
                let configuration = build_emrfs_configuration(&emrfs)?;
                extend(&mut params, "Configurations", vec![configuration]);
            }
            None => {
                let actions = build_emrfs_bootstrap_actions(&emrfs, region)?;
                extend(&mut params, "BootstrapActions", actions);
            }
        }
    }

    let steps = sequence("--steps")?;
    if let Some(steps) = &steps {
        let built: Result<Vec<Value>, Failure> = objects(Some(steps))
            .iter()
            .map(|step| super::emr::build_step(step, release_label, region))
            .collect();
        extend(&mut params, "Steps", built?);
    }

    apply(&mut params, "SecurityConfiguration", value("--security-configuration").map(|text| Value::String(text.to_string())));
    apply(&mut params, "CustomAmiId", value("--custom-ami-id").map(|text| Value::String(text.to_string())));
    for (flag, key) in [
        ("--ebs-root-volume-size", "EbsRootVolumeSize"),
        ("--ebs-root-volume-iops", "EbsRootVolumeIops"),
        ("--ebs-root-volume-throughput", "EbsRootVolumeThroughput"),
    ] {
        if let Some(size) = number(flag)? {
            apply(&mut params, key, Some(Value::Number(size.into())));
        }
    }
    apply(&mut params, "RepoUpgradeOnBoot", value("--repo-upgrade-on-boot").map(|text| Value::String(text.to_string())));
    apply(&mut params, "KerberosAttributes", structure("--kerberos-attributes")?);
    if let Some(level) = number("--step-concurrency-level")? {
        params.insert("StepConcurrencyLevel".to_string(), Value::Number(level.into()));
    }
    apply(
        &mut params,
        "StepExecutionRoleArn",
        value("--step-execution-role-arn").map(|text| Value::String(text.to_string())),
    );
    if given("--extended-support") || given("--no-extended-support") {
        params.insert(
            "ExtendedSupport".to_string(),
            Value::Bool(boolean_pair(
                given("--extended-support"),
                "--extended-support",
                given("--no-extended-support"),
                "--no-extended-support",
            )?),
        );
    }
    if given("--session-enabled") || given("--no-session-enabled") {
        params.insert(
            "SessionEnabled".to_string(),
            Value::Bool(boolean_pair(
                given("--session-enabled"),
                "--session-enabled",
                given("--no-session-enabled"),
                "--no-session-enabled",
            )?),
        );
    }
    apply(&mut params, "ManagedScalingPolicy", structure("--managed-scaling-policy")?);
    apply(&mut params, "PlacementGroupConfigs", sequence("--placement-group-configs")?);
    apply(&mut params, "AutoTerminationPolicy", structure("--auto-termination-policy")?);
    if let Some(monitoring) = structure("--monitoring-configuration")? {
        apply(&mut params, "MonitoringConfiguration", Some(monitoring.clone()));
        validate_s3_logging(&monitoring, value("--log-uri"))?;
    }

    validate_required_applications(applications.as_ref(), steps.as_ref(), hbase_restore.is_some())?;
    Ok(params)
}

fn read_json(text: &str) -> Option<Value> {
    serde_json::from_str(text).ok()
}

/// `--instance-groups`, or the `--instance-type`/`--instance-count` shortcut.
///
/// The shortcut builds a MASTER of one and, when the count is above one, a CORE with the
/// **remainder** — so `--instance-count 3` is one master and two core, not three of each.
fn build_groups(
    groups: Option<&Value>,
    instance_type: Option<&str>,
    instance_count: Option<i64>,
) -> Result<Vec<Value>, Failure> {
    if groups.is_none() && instance_type.is_none() {
        return Err(param_error(
            "Must specify either --instance-groups or --instance-type with \
             --instance-count(optional) to configure instance groups.",
        ));
    }
    if groups.is_some() && (instance_type.is_some() || instance_count.is_some()) {
        return Err(param_error(
            "You may not specify --instance-type or --instance-count with \
             --instance-groups, because --instance-type and --instance-count are shortcut \
             options for --instance-groups.",
        ));
    }

    let Some(groups) = groups else {
        let instance_type = instance_type.unwrap_or_default();
        let shortcut = |role: &str, count: i64| {
            json!({
                "InstanceType": instance_type,
                "InstanceCount": count,
                "InstanceRole": role,
                "Name": role,
                "Market": "ON_DEMAND",
            })
        };
        let mut built = vec![shortcut("MASTER", 1)];
        if instance_count.is_some_and(|count| count > 1) {
            built.push(shortcut("CORE", instance_count.unwrap_or(1) - 1));
        }
        return Ok(built);
    };

    Ok(objects(Some(groups))
        .iter()
        .map(|group| {
            let group_type = text(group, "InstanceGroupType").unwrap_or_default();
            let mut config = Map::new();
            config.insert(
                "Name".to_string(),
                Value::String(text(group, "Name").unwrap_or(group_type).to_string()),
            );
            config.insert(
                "InstanceType".to_string(),
                group.get("InstanceType").cloned().unwrap_or(Value::Null),
            );
            config.insert(
                "InstanceCount".to_string(),
                group.get("InstanceCount").cloned().unwrap_or(Value::Null),
            );
            config.insert(
                "InstanceRole".to_string(),
                Value::String(group_type.to_uppercase()),
            );
            match group.get("BidPrice") {
                Some(bid) => {
                    // `OnDemandPrice` means "spot, but bid the on-demand rate", which the
                    // API expresses by leaving BidPrice out of a SPOT group.
                    if text(group, "BidPrice") != Some("OnDemandPrice") {
                        config.insert("BidPrice".to_string(), bid.clone());
                    }
                    config.insert("Market".to_string(), Value::String("SPOT".to_string()));
                }
                None => {
                    config.insert("Market".to_string(), Value::String("ON_DEMAND".to_string()));
                }
            }
            for key in ["EbsConfiguration", "AutoScalingPolicy", "Configurations", "CustomAmiId"] {
                if let Some(value) = group.get(key) {
                    config.insert(key.to_string(), value.clone());
                }
            }
            Value::Object(config)
        })
        .collect())
}

fn build_fleets(fleets: &Value) -> Vec<Value> {
    objects(Some(fleets))
        .iter()
        .map(|fleet| {
            let fleet_type = text(fleet, "InstanceFleetType").unwrap_or_default();
            let mut config = Map::new();
            config.insert(
                "Name".to_string(),
                Value::String(text(fleet, "Name").unwrap_or(fleet_type).to_string()),
            );
            config.insert("InstanceFleetType".to_string(), Value::String(fleet_type.to_string()));
            for key in
                ["TargetOnDemandCapacity", "TargetSpotCapacity", "InstanceTypeConfigs", "Context"]
            {
                if let Some(value) = fleet.get(key) {
                    config.insert(key.to_string(), value.clone());
                }
            }
            // These two are rebuilt member by member rather than copied, so an unknown
            // key inside them is dropped rather than sent.
            for (outer, members) in [
                ("LaunchSpecifications", ["SpotSpecification", "OnDemandSpecification"]),
                (
                    "ResizeSpecifications",
                    ["SpotResizeSpecification", "OnDemandResizeSpecification"],
                ),
            ] {
                if let Some(source) = fleet.get(outer) {
                    let mut rebuilt = Map::new();
                    for member in members {
                        if let Some(value) = source.get(member) {
                            rebuilt.insert(member.to_string(), value.clone());
                        }
                    }
                    config.insert(outer.to_string(), Value::Object(rebuilt));
                }
            }
            Value::Object(config)
        })
        .collect()
}

/// `--ec2-attributes` spreads across two places: most of it lands in `Instances`, but
/// `InstanceProfile` becomes the cluster's `JobFlowRole`.
fn build_ec2_attributes(
    params: &mut Map<String, Value>,
    attributes: &Value,
) -> Result<(), Failure> {
    let has = |key: &str| attributes.get(key).is_some();
    if has("SubnetId") && has("SubnetIds") {
        return Err(mutually_exclusive("SubnetId", "SubnetIds", ""));
    }
    if has("AvailabilityZone") && has("AvailabilityZones") {
        return Err(mutually_exclusive("AvailabilityZone", "AvailabilityZones", ""));
    }
    // A subnet already implies a placement, so naming both is a contradiction rather than
    // extra detail.
    if (has("SubnetId") || has("SubnetIds"))
        && (has("AvailabilityZone") || has("AvailabilityZones"))
    {
        return Err(param_error(
            "You may not specify both a SubnetId and an AvailabilityZone (placement) \
             because ec2SubnetId implies a placement.",
        ));
    }

    let Some(Value::Object(instances)) = params.get_mut("Instances") else {
        return Ok(());
    };
    for (from, to) in [
        ("KeyName", "Ec2KeyName"),
        ("SubnetId", "Ec2SubnetId"),
        ("SubnetIds", "Ec2SubnetIds"),
        ("EmrManagedMasterSecurityGroup", "EmrManagedMasterSecurityGroup"),
        ("EmrManagedSlaveSecurityGroup", "EmrManagedSlaveSecurityGroup"),
        ("ServiceAccessSecurityGroup", "ServiceAccessSecurityGroup"),
        ("AdditionalMasterSecurityGroups", "AdditionalMasterSecurityGroups"),
        ("AdditionalSlaveSecurityGroups", "AdditionalSlaveSecurityGroups"),
    ] {
        if let Some(value) = attributes.get(from) {
            if truthy(value) {
                instances.insert(to.to_string(), value.clone());
            }
        }
    }
    for key in ["AvailabilityZone", "AvailabilityZones"] {
        if let Some(value) = attributes.get(key) {
            let mut placement = Map::new();
            if truthy(value) {
                placement.insert(key.to_string(), value.clone());
            }
            instances.insert("Placement".to_string(), Value::Object(placement));
        }
    }
    if let Some(profile) = attributes.get("InstanceProfile") {
        if truthy(profile) {
            params.insert("JobFlowRole".to_string(), profile.clone());
        }
    }
    Ok(())
}

fn build_bootstrap_actions(
    params: &mut Map<String, Value>,
    actions: &[Value],
) -> Result<(), Failure> {
    let existing = params
        .get("BootstrapActions")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    // The cap counts the actions EMRFS and applications already added, not just these.
    if existing + actions.len() > MAX_BOOTSTRAP_ACTIONS {
        return Err(param_error(
            "maximum number of bootstrap actions for a cluster exceeded.",
        ));
    }
    let built: Vec<Value> = actions
        .iter()
        .map(|action| {
            let mut script = Map::new();
            for key in ["Path", "Args"] {
                if let Some(value) = action.get(key) {
                    if truthy(value) {
                        script.insert(key.to_string(), value.clone());
                    }
                }
            }
            json!({
                "Name": text(action, "Name").unwrap_or("Bootstrap action"),
                "ScriptBootstrapAction": Value::Object(script),
            })
        })
        .collect();
    extend(params, "BootstrapActions", built);
    Ok(())
}

fn build_step_config(jar: &str, name: &str, on_failure: &str, args: Vec<String>) -> Value {
    let mut hadoop = Map::new();
    hadoop.insert("Jar".to_string(), Value::String(jar.to_string()));
    if !args.is_empty() {
        hadoop.insert(
            "Args".to_string(),
            Value::Array(args.into_iter().map(Value::String).collect()),
        );
    }
    json!({
        "Name": name,
        "ActionOnFailure": on_failure,
        "HadoopJarStep": Value::Object(hadoop),
    })
}

fn build_bootstrap_action(name: &str, path: &str, args: Vec<String>) -> Value {
    let mut script = Map::new();
    if !args.is_empty() {
        script.insert(
            "Args".to_string(),
            Value::Array(args.into_iter().map(Value::String).collect()),
        );
    }
    script.insert("Path".to_string(), Value::String(path.to_string()));
    json!({ "Name": name, "ScriptBootstrapAction": Value::Object(script) })
}

/// What an AMI-era application list turns into: supported products, bootstrap actions and
/// install steps, each destined for a different field of the request.
type AmiApplications = (Vec<Value>, Vec<Value>, Vec<Value>);

/// The AMI-era applications: each becomes a bootstrap action, an install step, or a
/// "supported product", depending on which one it is.
fn build_ami_applications(
    applications: &[Value],
    region: &str,
    ami_version: &str,
) -> Result<AmiApplications, Failure> {
    let mut products = Vec::new();
    let mut bootstrap = Vec::new();
    let mut steps = Vec::new();
    let link = |path: &str| super::emr::s3_link(region, path);
    let runner = super::emr::script_runner(region);

    for application in applications {
        let name = text(application, "Name").unwrap_or_default();
        let args = super::emr::string_list(application.get("Args"));
        match name.to_lowercase().as_str() {
            "hive" => {
                steps.push(build_step_config(
                    &runner,
                    "Install Hive",
                    "TERMINATE_CLUSTER",
                    vec![
                        link("/libs/hive/hive-script"),
                        "--install-hive".to_string(),
                        "--base-path".to_string(),
                        link("/libs/hive"),
                        "--hive-versions".to_string(),
                        "latest".to_string(),
                    ],
                ));
                // A `--hive-site` argument adds a second step that installs it.
                if let Some(site) = args.iter().find(|arg| arg.contains("--hive-site")) {
                    steps.push(build_step_config(
                        &runner,
                        "Install Hive Site Configuration",
                        "CANCEL_AND_WAIT",
                        vec![
                            link("/libs/hive/hive-script"),
                            "--base-path".to_string(),
                            // Deliberately region-less: the reference omits the region
                            // here and only here, so this link points at us-east-1.
                            super::emr::s3_link("us-east-1", "/libs/hive"),
                            "--install-hive-site".to_string(),
                            site.clone(),
                            "--hive-versions".to_string(),
                            "latest".to_string(),
                        ],
                    ));
                }
            }
            "pig" => steps.push(build_step_config(
                &runner,
                "Install Pig",
                "TERMINATE_CLUSTER",
                vec![
                    link("/libs/pig/pig-script"),
                    "--install-pig".to_string(),
                    "--base-path".to_string(),
                    link("/libs/pig"),
                    "--pig-versions".to_string(),
                    "latest".to_string(),
                ],
            )),
            "ganglia" => bootstrap.push(build_bootstrap_action(
                "Install Ganglia",
                &link("/bootstrap-actions/install-ganglia"),
                Vec::new(),
            )),
            "hbase" => {
                bootstrap.push(build_bootstrap_action(
                    "Install HBase",
                    &link("/bootstrap-actions/setup-hbase"),
                    Vec::new(),
                ));
                // A string comparison, as in the reference: "3.0" > "2.1" lexicographically
                // and that is how the AMI line was versioned.
                let jar = if ami_version >= "3.0" {
                    "/home/hadoop/lib/hbase.jar"
                } else if ami_version >= "2.1" {
                    "/home/hadoop/lib/hbase-0.92.0.jar"
                } else {
                    return Err(param_error(format!(
                        "AMI version {ami_version} is not compatible with HBase."
                    )));
                };
                steps.push(build_step_config(
                    jar,
                    "Start HBase",
                    "TERMINATE_CLUSTER",
                    vec!["emr.hbase.backup.Main".to_string(), "--start-master".to_string()],
                ));
            }
            "impala" => {
                let mut impala_args = vec![
                    "--base-path".to_string(),
                    link(""),
                    "--impala-version".to_string(),
                    "latest".to_string(),
                ];
                if !args.is_empty() {
                    impala_args.push("--impala-conf".to_string());
                    impala_args.push(args.join(","));
                }
                bootstrap.push(build_bootstrap_action(
                    "Install Impala",
                    &link("/libs/impala/setup-impala"),
                    impala_args,
                ));
            }
            // Anything else is a "supported product", passed through lower-cased.
            _ => products.push(json!({
                "Name": name.to_lowercase(),
                "Args": args,
            })),
        }
    }
    Ok((products, bootstrap, steps))
}

/// The EMRFS properties, in the order the reference inserts them — which is the order
/// they reach the cluster's `emrfs-site` classification.
fn build_emrfs_properties(emrfs: &Value) -> Result<Map<String, Value>, Failure> {
    verify_emrfs(emrfs)?;
    let mut properties = Map::new();
    let upper = |key: &str| text(emrfs, key).map(str::to_uppercase);

    if let Some(consistent) = emrfs.get("Consistent") {
        properties.insert(
            "fs.s3.consistent".to_string(),
            Value::String(scalar(consistent).to_lowercase()),
        );
        for (key, property) in [
            ("RetryCount", "fs.s3.consistent.retryCount"),
            ("RetryPeriod", "fs.s3.consistent.retryPeriodSeconds"),
        ] {
            if let Some(value) = emrfs.get(key) {
                properties.insert(property.to_string(), Value::String(scalar(value)));
            }
        }
    }

    let server_side = emrfs.get("SSE").is_some() || upper("Encryption").as_deref() == Some("SERVERSIDE");
    if server_side {
        let value = emrfs.get("SSE").map(scalar).unwrap_or_else(|| "true".to_string());
        properties.insert(
            "fs.s3.enableServerSideEncryption".to_string(),
            Value::String(value.to_lowercase()),
        );
    }

    let client_side = |provider: &str| {
        upper("Encryption").as_deref() == Some("CLIENTSIDE")
            && upper("ProviderType").as_deref() == Some(provider)
    };
    if client_side("KMS") {
        properties.insert("fs.s3.cse.enabled".to_string(), Value::String("true".into()));
        properties.insert(
            "fs.s3.cse.encryptionMaterialsProvider".to_string(),
            Value::String(
                "com.amazon.ws.emr.hadoop.fs.cse.KMSEncryptionMaterialsProvider".into(),
            ),
        );
        properties.insert(
            "fs.s3.cse.kms.keyId".to_string(),
            emrfs.get("KMSKeyId").cloned().unwrap_or(Value::Null),
        );
    }
    if client_side("CUSTOM") {
        properties.insert("fs.s3.cse.enabled".to_string(), Value::String("true".into()));
        properties.insert(
            "fs.s3.cse.encryptionMaterialsProvider".to_string(),
            emrfs.get("CustomProviderClass").cloned().unwrap_or(Value::Null),
        );
    }

    for arg in super::emr::string_list(emrfs.get("Args")) {
        let (key, value) = match arg.split_once('=') {
            Some((key, value)) => (key.to_string(), value.to_string()),
            None => (arg.clone(), String::new()),
        };
        properties.insert(key, Value::String(value));
    }
    Ok(properties)
}

fn scalar(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

fn build_emrfs_configuration(emrfs: &Value) -> Result<Value, Failure> {
    let mut properties = build_emrfs_properties(emrfs)?;
    if is_custom_cse(emrfs) {
        properties.insert(
            "fs.s3.cse.encryptionMaterialsProvider.uri".to_string(),
            emrfs.get("CustomProviderLocation").cloned().unwrap_or(Value::Null),
        );
    }
    Ok(json!({ "Classification": EMRFS_SITE, "Properties": Value::Object(properties) }))
}

fn is_custom_cse(emrfs: &Value) -> bool {
    text(emrfs, "Encryption").map(str::to_uppercase).as_deref() == Some("CLIENTSIDE")
        && text(emrfs, "ProviderType").map(str::to_uppercase).as_deref() == Some("CUSTOM")
}

fn build_emrfs_bootstrap_actions(emrfs: &Value, region: &str) -> Result<Vec<Value>, Failure> {
    let mut actions = Vec::new();
    if is_custom_cse(emrfs) {
        // The custom provider jar has to be on the node before EMRFS is configured to use
        // it, which is why this action comes first.
        actions.push(build_bootstrap_action(
            "S3 get",
            "file:/usr/share/aws/emr/scripts/s3get",
            vec![
                "-s".to_string(),
                text(emrfs, "CustomProviderLocation").unwrap_or_default().to_string(),
                "-d".to_string(),
                "/usr/share/aws/emr/auxlib".to_string(),
                "-f".to_string(),
            ],
        ));
    }
    let properties = build_emrfs_properties(emrfs)?;
    let mut args = Vec::new();
    for (key, value) in &properties {
        let text = scalar(value);
        args.push("-e".to_string());
        args.push(if text.is_empty() { key.clone() } else { format!("{key}={text}") });
    }
    actions.push(build_bootstrap_action(
        "Setup EMRFS",
        &super::emr::s3_link(region, "/bootstrap-actions/configure-hadoop"),
        args,
    ));
    Ok(actions)
}

fn verify_emrfs(emrfs: &Value) -> Result<(), Failure> {
    let upper = |key: &str| text(emrfs, key).map(str::to_uppercase);
    if let Some(encryption) = upper("Encryption") {
        if !["SERVERSIDE", "CLIENTSIDE"].contains(&encryption.as_str()) {
            return Err(param_error(format!(
                "The encryption type \"{}\" is invalid. You must specify either ServerSide \
                 or ClientSide",
                text(emrfs, "Encryption").unwrap_or_default()
            )));
        }
    }
    if emrfs.get("SSE").is_some() && emrfs.get("Encryption").is_some() {
        return Err(param_error(format!(
            "Both SSE={} and Encryption={} are configured for --emrfs. You must specify \
             only one of the two.",
            scalar(emrfs.get("SSE").unwrap_or(&Value::Null)),
            text(emrfs, "Encryption").unwrap_or_default()
        )));
    }
    if upper("Encryption").as_deref() == Some("CLIENTSIDE") {
        match upper("ProviderType") {
            None => {
                return Err(param_error(
                    "The following required parameters are missing for --emrfs \
                     Encryption=ClientSide: ProviderType.",
                ))
            }
            Some(provider) if !["KMS", "CUSTOM"].contains(&provider.as_str()) => {
                return Err(param_error(format!(
                    "The client side encryption type \"{}\" is not supported. You must \
                     specify either KMS or Custom",
                    text(emrfs, "ProviderType").unwrap_or_default()
                )))
            }
            Some(provider) if provider == "KMS" => {
                require(emrfs, &["KMSKeyId"], "--emrfs Encryption=ClientSide,ProviderType=KMS")?
            }
            Some(_) => require(
                emrfs,
                &["CustomProviderLocation", "CustomProviderClass"],
                "--emrfs Encryption=ClientSide,ProviderType=Custom",
            )?,
        }
    }
    // A child setting without its parent feature is a typo, not a preference — saying so
    // beats launching a cluster that quietly ignores it.
    if emrfs.get("Consistent").is_none() {
        forbid(
            emrfs,
            &["RetryCount", "RetryPeriod"],
            "--emrfs Consistent=true/false",
        )?;
    }
    if !(text(emrfs, "Encryption").map(str::to_uppercase).as_deref() == Some("CLIENTSIDE")
        && text(emrfs, "ProviderType").map(str::to_uppercase).as_deref() == Some("KMS"))
    {
        forbid(emrfs, &["KMSKeyId"], "--emrfs Encryption=ClientSide,ProviderType=KMS")?;
    }
    if !is_custom_cse(emrfs) {
        forbid(
            emrfs,
            &["CustomProviderLocation", "CustomProviderClass"],
            "--emrfs Encryption=ClientSide,ProviderType=Custom",
        )?;
    }
    Ok(())
}

fn require(emrfs: &Value, keys: &[&str], object: &str) -> Result<(), Failure> {
    let missing: Vec<String> =
        keys.iter().filter(|key| emrfs.get(**key).is_none()).map(|key| key.to_string()).collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(param_error(format!(
        "The following required parameters are missing for {object}: {}.",
        join_names(&missing)
    )))
}

fn forbid(emrfs: &Value, keys: &[&str], parent: &str) -> Result<(), Failure> {
    let present: Vec<String> =
        keys.iter().filter(|key| emrfs.get(**key).is_some()).map(|key| key.to_string()).collect();
    if present.is_empty() {
        return Ok(());
    }
    Err(param_error(format!(
        "{parent} is not specified. Thus,  following parameters are invalid: {}",
        join_names(&present)
    )))
}

/// A Hive, Pig or Impala step needs its application installed, and an HBase restore needs
/// HBase — otherwise the cluster starts and the step fails on it.
fn validate_required_applications(
    applications: Option<&Value>,
    steps: Option<&Value>,
    hbase_restore: bool,
) -> Result<(), Failure> {
    let specified: Vec<String> = objects(applications)
        .iter()
        .filter_map(|app| text(app, "Name"))
        .map(str::to_lowercase)
        .collect();
    let mut missing: Vec<String> = Vec::new();
    for step in objects(steps) {
        let Some(step_type) = text(&step, "Type") else { continue };
        let lowered = step_type.to_lowercase();
        if ["hive", "pig", "impala"].contains(&lowered.as_str())
            && !specified.contains(&lowered)
        {
            let titled = title_case(&lowered);
            if !missing.contains(&titled) {
                missing.push(titled);
            }
        }
    }
    if hbase_restore && !specified.iter().any(|app| app == "hbase") {
        let hbase = "Hbase".to_string();
        if !missing.contains(&hbase) {
            missing.push(hbase);
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    // Sorted, because the reference collects them in a `set` and joins it — the order is
    // not the steps' order there either, and sorting at least makes ours repeatable.
    missing.sort();
    Err(param_error(format!(
        "Some of the steps require the following applications to be installed: {}. Please \
         install the applications using --applications.",
        missing.join(", ")
    )))
}

/// Python's `str.title()` on a single lowercase word.
fn title_case(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

const LOG_TYPES: [&str; 3] = ["system-logs", "application-logs", "persistent-ui-logs"];
const LOG_POLICIES: [&str; 3] = ["emr-managed", "on-customer-s3only", "disabled"];

/// `--monitoring-configuration` decides where logs go, and that has to agree with
/// `--log-uri`: a policy that writes to the customer's bucket needs one, and a
/// configuration that disables logging entirely must not have one.
fn validate_s3_logging(monitoring: &Value, log_uri: Option<&str>) -> Result<(), Failure> {
    let Some(Value::Object(logging)) = monitoring.get("S3LoggingConfiguration") else {
        return Ok(());
    };
    let mut needs_uri = false;
    let mut all_disabled = true;
    for (log_type, policy) in logging {
        if !LOG_TYPES.contains(&log_type.as_str()) {
            return Err(param_error(format!(
                "Invalid log type specified for the current configuration: {log_type}"
            )));
        }
        let policy = policy.as_str().unwrap_or_default();
        if !LOG_POLICIES.contains(&policy) {
            return Err(param_error(format!(
                "Invalid policy specified for the current configuration: {policy}"
            )));
        }
        if log_type == "persistent-ui-logs" && policy == "on-customer-s3only" {
            return Err(param_error(
                "Invalid policy for log type 'persistent-ui-logs'. on-customer-s3only is \
                 not supported.",
            ));
        }
        if log_type == "system-logs" || log_type == "application-logs" {
            if policy != "disabled" {
                all_disabled = false;
            }
            if policy == "emr-managed" || policy == "on-customer-s3only" {
                needs_uri = true;
            }
        }
    }
    if needs_uri && log_uri.is_none() {
        return Err(param_error(
            "A valid S3 location (LogUri) is required for the current configuration.",
        ));
    }
    if all_disabled && log_uri.is_some() && !needs_uri {
        return Err(param_error(
            "LogUri must not be specified when system-logs and application-logs are both \
             disabled.",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a request from a command line, the way `main` would.
    fn request(argv: &[&str]) -> Result<Map<String, Value>, Failure> {
        let mut full = vec!["awsc".to_string(), "emr".to_string(), "create-cluster".to_string()];
        full.extend(argv.iter().map(|arg| arg.to_string()));
        let parsed = match crate::args::parse(&full).expect("parses") {
            crate::args::Outcome::Run(parsed) => *parsed,
            _ => panic!("expected a command"),
        };
        build(&parsed, "us-east-1")
    }

    fn ok(argv: &[&str]) -> Map<String, Value> {
        request(argv).expect("builds")
    }

    fn err(argv: &[&str]) -> String {
        request(argv).expect_err("refuses").message().to_string()
    }

    /// The smallest cluster there is, and the defaults it implies. Every one of these is
    /// a decision the command makes on the user's behalf.
    #[test]
    fn the_minimal_cluster_carries_its_defaults() {
        let params = ok(&["--release-label", "emr-6.15.0", "--instance-type", "m5.xlarge"]);
        assert_eq!(params["Name"], "Development Cluster");
        assert_eq!(params["ReleaseLabel"], "emr-6.15.0");
        assert_eq!(params["VisibleToAllUsers"], true);
        // Tags is present even when empty; the reference assigns it unconditionally.
        assert_eq!(params["Tags"], json!([]));
        let instances = &params["Instances"];
        assert_eq!(instances["KeepJobFlowAliveWhenNoSteps"], true);
        assert_eq!(instances["TerminationProtected"], false);
        assert_eq!(
            instances["InstanceGroups"],
            json!([{
                "InstanceType": "m5.xlarge", "InstanceCount": 1,
                "InstanceRole": "MASTER", "Name": "MASTER", "Market": "ON_DEMAND",
            }])
        );
        // Nothing else leaks in.
        assert!(params.get("Steps").is_none());
        assert!(params.get("BootstrapActions").is_none());
        assert!(params.get("Applications").is_none());
    }

    /// `--instance-count` splits into one master and the *remainder* as core, which is
    /// not what "count" suggests on its own.
    #[test]
    fn the_instance_count_shortcut_reserves_one_for_the_master() {
        let params = ok(&[
            "--release-label", "emr-6.15.0", "--instance-type", "m5.xlarge",
            "--instance-count", "3",
        ]);
        let groups = params["Instances"]["InstanceGroups"].as_array().expect("groups");
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0]["InstanceRole"], "MASTER");
        assert_eq!(groups[0]["InstanceCount"], 1);
        assert_eq!(groups[1]["InstanceRole"], "CORE");
        assert_eq!(groups[1]["InstanceCount"], 2);

        // A count of one is a master and nothing else.
        let single = ok(&[
            "--release-label", "emr-6.15.0", "--instance-type", "m5.xlarge",
            "--instance-count", "1",
        ]);
        assert_eq!(single["Instances"]["InstanceGroups"].as_array().expect("groups").len(), 1);
    }

    /// `BidPrice=OnDemandPrice` is not a price: it means "spot, at the on-demand rate",
    /// which the API expresses by omitting BidPrice from a SPOT group.
    #[test]
    fn on_demand_price_becomes_a_spot_group_with_no_bid() {
        let params = ok(&[
            "--release-label", "emr-6.15.0",
            "--instance-groups",
            "InstanceGroupType=MASTER,InstanceType=m5.xlarge,InstanceCount=1,BidPrice=OnDemandPrice",
        ]);
        let group = &params["Instances"]["InstanceGroups"][0];
        assert_eq!(group["Market"], "SPOT");
        assert!(group.get("BidPrice").is_none(), "the sentinel must not be sent as a price");
        assert_eq!(group["Name"], "MASTER", "the name defaults to the group type");

        let priced = ok(&[
            "--release-label", "emr-6.15.0",
            "--instance-groups",
            "InstanceGroupType=CORE,InstanceType=m5.xlarge,InstanceCount=2,BidPrice=0.10",
        ]);
        assert_eq!(priced["Instances"]["InstanceGroups"][0]["BidPrice"], "0.10");
        assert_eq!(priced["Instances"]["InstanceGroups"][0]["Market"], "SPOT");
    }

    /// `--use-default-roles` fills in both halves, and conflicts with either given by hand.
    #[test]
    fn default_roles_fill_in_both_the_service_role_and_the_instance_profile() {
        let params = ok(&[
            "--release-label", "emr-6.15.0", "--instance-type", "m5.xlarge",
            "--use-default-roles",
        ]);
        assert_eq!(params["ServiceRole"], "EMR_DefaultRole");
        assert_eq!(params["JobFlowRole"], "EMR_EC2_DefaultRole");

        let message = err(&[
            "--release-label", "emr-6.15.0", "--instance-type", "m5.xlarge",
            "--use-default-roles", "--service-role", "MyRole",
        ]);
        assert!(message.contains("You cannot specify both --use-default-roles and --service-role"));
        assert!(message.contains("Either choose --use-default-roles"), "{message}");

        let message = err(&[
            "--release-label", "emr-6.15.0", "--instance-type", "m5.xlarge",
            "--use-default-roles", "--ec2-attributes", "InstanceProfile=MyProfile",
        ]);
        assert!(message.contains("--ec2-attributes InstanceProfile"), "{message}");
    }

    /// One era or the other, never both and never neither.
    #[test]
    fn a_release_label_or_an_ami_version_is_required_but_not_both() {
        assert!(err(&["--instance-type", "m5.xlarge"])
            .contains("Either --ami-version or --release-label is required."));
        assert!(err(&[
            "--instance-type", "m5.xlarge", "--release-label", "emr-6.15.0",
            "--ami-version", "3.1.0",
        ])
        .contains("You cannot specify both --ami-version and --release-label"));
        assert!(err(&["--instance-type", "m5.xlarge", "--ami-version", "nonsense"])
            .contains("is invalid"));
    }

    #[test]
    fn the_two_ways_of_describing_capacity_are_exclusive() {
        assert!(err(&[
            "--release-label", "emr-6.15.0",
            "--instance-groups", "InstanceGroupType=MASTER,InstanceType=m5.xlarge,InstanceCount=1",
            "--instance-fleets", "InstanceFleetType=MASTER,TargetOnDemandCapacity=1",
        ])
        .contains("You cannot specify both --instance-groups and --instance-fleets"));

        assert!(err(&[
            "--release-label", "emr-6.15.0",
            "--instance-groups", "InstanceGroupType=MASTER,InstanceType=m5.xlarge,InstanceCount=1",
            "--instance-type", "m5.xlarge",
        ])
        .contains("shortcut options for --instance-groups"));

        assert!(err(&["--release-label", "emr-6.15.0"])
            .contains("Must specify either --instance-groups or --instance-type"));
    }

    /// Debugging adds a step, and which jar runs it depends on the era.
    #[test]
    fn debugging_adds_a_step_and_needs_somewhere_to_write() {
        assert!(err(&[
            "--release-label", "emr-6.15.0", "--instance-type", "m5.xlarge",
            "--enable-debugging",
        ])
        .contains("LogUri not specified"));

        let params = ok(&[
            "--release-label", "emr-6.15.0", "--instance-type", "m5.xlarge",
            "--enable-debugging", "--log-uri", "s3://my-logs/",
        ]);
        let step = &params["Steps"][0];
        assert_eq!(step["Name"], "Setup Hadoop Debugging");
        assert_eq!(step["ActionOnFailure"], "TERMINATE_CLUSTER");
        assert_eq!(step["HadoopJarStep"]["Jar"], "command-runner.jar");
        assert_eq!(step["HadoopJarStep"]["Args"], json!(["state-pusher-script"]));

        // The AMI era runs it through a regional script-runner instead.
        let old = ok(&[
            "--ami-version", "3.1.0", "--instance-type", "m1.large",
            "--enable-debugging", "--log-uri", "s3://my-logs/",
        ]);
        assert_eq!(
            old["Steps"][0]["HadoopJarStep"]["Jar"],
            "s3://us-east-1.elasticmapreduce/libs/script-runner/script-runner.jar"
        );
    }

    /// A release-label cluster takes applications as given; an AMI cluster turns them
    /// into bootstrap actions and install steps.
    #[test]
    fn applications_mean_different_things_in_the_two_eras() {
        let modern = ok(&[
            "--release-label", "emr-6.15.0", "--instance-type", "m5.xlarge",
            "--applications", "Name=Spark", "Name=Hive",
        ]);
        assert_eq!(modern["Applications"], json!([{"Name": "Spark"}, {"Name": "Hive"}]));
        assert!(modern.get("Steps").is_none());

        let legacy = ok(&[
            "--ami-version", "3.1.0", "--instance-type", "m1.large",
            "--applications", "Name=Hive", "Name=Ganglia",
        ]);
        assert!(legacy.get("Applications").is_none());
        assert_eq!(legacy["Steps"][0]["Name"], "Install Hive");
        assert_eq!(legacy["BootstrapActions"][0]["Name"], "Install Ganglia");
    }

    /// A step that needs an application refuses when it is not being installed.
    #[test]
    fn a_hive_step_without_hive_is_refused() {
        let message = err(&[
            "--release-label", "emr-6.15.0", "--instance-type", "m5.xlarge",
            "--steps", "Type=Hive,Args=[-f,s3://x/script.q]",
        ]);
        assert!(
            message.contains("require the following applications to be installed: Hive"),
            "{message}"
        );

        // With the application named, it builds.
        let params = ok(&[
            "--release-label", "emr-6.15.0", "--instance-type", "m5.xlarge",
            "--applications", "Name=Hive",
            "--steps", "Type=Hive,Args=[-f,s3://x/script.q]",
        ]);
        assert_eq!(params["Steps"].as_array().expect("steps").len(), 1);

        // And an HBase restore needs HBase.
        assert!(err(&[
            "--release-label", "emr-6.15.0", "--instance-type", "m5.xlarge",
            "--restore-from-hbase-backup", "Dir=s3://my-backups/",
        ])
        .contains("Hbase"));
    }

    /// EMRFS becomes a configuration on a release-label cluster and a bootstrap action on
    /// an AMI one — the same flag, two entirely different requests.
    #[test]
    fn emrfs_lands_in_configurations_or_in_bootstrap_actions() {
        let modern = ok(&[
            "--release-label", "emr-6.15.0", "--instance-type", "m5.xlarge",
            "--emrfs", "Encryption=ServerSide",
        ]);
        assert_eq!(
            modern["Configurations"],
            json!([{
                "Classification": "emrfs-site",
                "Properties": {"fs.s3.enableServerSideEncryption": "true"},
            }])
        );

        let legacy = ok(&[
            "--ami-version", "3.1.0", "--instance-type", "m1.large",
            "--emrfs", "Encryption=ServerSide",
        ]);
        let action = &legacy["BootstrapActions"][0];
        assert_eq!(action["Name"], "Setup EMRFS");
        assert_eq!(
            action["ScriptBootstrapAction"]["Args"],
            json!(["-e", "fs.s3.enableServerSideEncryption=true"])
        );
    }

    /// The EMRFS validations, which are the fiddliest part of the command.
    #[test]
    fn emrfs_arguments_are_checked_against_each_other() {
        let base = ["--release-label", "emr-6.15.0", "--instance-type", "m5.xlarge"];
        let with = |emrfs: &str| {
            let mut argv = base.to_vec();
            argv.extend(["--emrfs", emrfs]);
            err(&argv)
        };
        assert!(with("Encryption=Sideways").contains("is invalid"));
        assert!(with("SSE=true,Encryption=ServerSide").contains("You must specify only one"));
        assert!(with("Encryption=ClientSide").contains("missing for --emrfs Encryption=ClientSide: ProviderType"));
        assert!(with("Encryption=ClientSide,ProviderType=Nope").contains("not supported"));
        assert!(with("Encryption=ClientSide,ProviderType=KMS").contains("KMSKeyId"));
        assert!(
            with("Encryption=ClientSide,ProviderType=Custom,CustomProviderClass=C")
                .contains("CustomProviderLocation")
        );
        // A child key without its parent feature.
        assert!(with("RetryCount=5").contains("Consistent=true/false is not specified"));
        assert!(with("KMSKeyId=abc").contains("ProviderType=KMS is not specified"));
    }

    /// A subnet already implies a placement, so naming both is a contradiction.
    #[test]
    fn ec2_attributes_reject_contradictory_placements() {
        let base = ["--release-label", "emr-6.15.0", "--instance-type", "m5.xlarge"];
        let with = |attributes: &str| {
            let mut argv = base.to_vec();
            argv.extend(["--ec2-attributes", attributes]);
            err(&argv)
        };
        assert!(with("SubnetId=subnet-1,SubnetIds=[subnet-2]").contains("SubnetId and SubnetIds"));
        assert!(with("AvailabilityZone=us-east-1a,AvailabilityZones=[us-east-1b]")
            .contains("AvailabilityZone and AvailabilityZones"));
        assert!(with("SubnetId=subnet-1,AvailabilityZone=us-east-1a")
            .contains("because ec2SubnetId implies a placement"));

        let params = ok(&[
            "--release-label", "emr-6.15.0", "--instance-type", "m5.xlarge",
            "--ec2-attributes", "KeyName=my-key,SubnetId=subnet-1,InstanceProfile=MyProfile",
        ]);
        assert_eq!(params["Instances"]["Ec2KeyName"], "my-key");
        assert_eq!(params["Instances"]["Ec2SubnetId"], "subnet-1");
        // The instance profile goes to the cluster, not to Instances.
        assert_eq!(params["JobFlowRole"], "MyProfile");
        assert!(params["Instances"].get("InstanceProfile").is_none());
    }

    /// An autoscaling policy needs a role to assume.
    #[test]
    fn an_autoscaling_policy_requires_a_role() {
        let groups = "InstanceGroupType=CORE,InstanceType=m5.xlarge,InstanceCount=2,\
                      AutoScalingPolicy={Constraints={MinCapacity=1,MaxCapacity=4}}";
        assert!(err(&["--release-label", "emr-6.15.0", "--instance-groups", groups])
            .contains("Must specify --auto-scaling-role"));
        let params = ok(&[
            "--release-label", "emr-6.15.0", "--instance-groups", groups,
            "--auto-scaling-role", "EMR_AutoScaling_DefaultRole",
        ]);
        assert_eq!(params["AutoScalingRole"], "EMR_AutoScaling_DefaultRole");
    }

    /// Both spellings of a switch at once is an error, not a last-one-wins.
    #[test]
    fn a_switch_and_its_negation_together_are_refused() {
        assert!(err(&[
            "--release-label", "emr-6.15.0", "--instance-type", "m5.xlarge",
            "--termination-protected", "--no-termination-protected",
        ])
        .contains("cannot use both --termination-protected and --no-termination-protected"));
    }

    /// The flags that only appear when asked for, and the ones that appear regardless.
    #[test]
    fn optional_switches_are_absent_unless_given() {
        let params = ok(&["--release-label", "emr-6.15.0", "--instance-type", "m5.xlarge"]);
        for absent in ["ExtendedSupport", "SessionEnabled", "StepConcurrencyLevel"] {
            assert!(params.get(absent).is_none(), "{absent} must not be sent unasked");
        }
        assert!(params["Instances"].get("UnhealthyNodeReplacement").is_none());

        let asked = ok(&[
            "--release-label", "emr-6.15.0", "--instance-type", "m5.xlarge",
            "--no-extended-support", "--session-enabled", "--unhealthy-node-replacement",
            "--step-concurrency-level", "5",
        ]);
        assert_eq!(asked["ExtendedSupport"], false);
        assert_eq!(asked["SessionEnabled"], true);
        assert_eq!(asked["StepConcurrencyLevel"], 5);
        assert_eq!(asked["Instances"]["UnhealthyNodeReplacement"], true);
    }

    #[test]
    fn tags_split_on_the_first_equals() {
        let params = ok(&[
            "--release-label", "emr-6.15.0", "--instance-type", "m5.xlarge",
            "--tags", "env=prod", "expr=a=b", "bare",
        ]);
        assert_eq!(
            params["Tags"],
            json!([
                {"Key": "env", "Value": "prod"},
                {"Key": "expr", "Value": "a=b"},
                {"Key": "bare", "Value": ""},
            ])
        );
    }

    /// Instance fleets are rebuilt member by member, so an unknown key is dropped.
    #[test]
    fn fleets_keep_only_the_members_the_api_knows() {
        let params = ok(&[
            "--release-label", "emr-6.15.0",
            "--instance-fleets",
            "InstanceFleetType=MASTER,TargetOnDemandCapacity=1,\
             InstanceTypeConfigs=[{InstanceType=m5.xlarge}]",
        ]);
        let fleet = &params["Instances"]["InstanceFleets"][0];
        assert_eq!(fleet["Name"], "MASTER");
        assert_eq!(fleet["InstanceFleetType"], "MASTER");
        // Still a string here: shorthand is untyped, and `run` coerces the whole request
        // against the model's input shape on the way out. The wire form is checked
        // against a stand-in rather than here, since coercion needs the catalogue.
        assert_eq!(fleet["TargetOnDemandCapacity"], "1");
        assert!(fleet.get("TargetSpotCapacity").is_none());
    }

    /// The monitoring configuration has to agree with `--log-uri`.
    #[test]
    fn s3_logging_policies_are_checked_against_the_log_uri() {
        let base = ["--release-label", "emr-6.15.0", "--instance-type", "m5.xlarge"];
        let with = |monitoring: &str, log_uri: Option<&str>| {
            let mut argv = base.to_vec();
            argv.extend(["--monitoring-configuration", monitoring]);
            if let Some(uri) = log_uri {
                argv.extend(["--log-uri", uri]);
            }
            request(&argv)
        };
        assert!(with("S3LoggingConfiguration={system-logs=emr-managed}", None)
            .expect_err("refuses")
            .message()
            .contains("A valid S3 location (LogUri) is required"));
        assert!(with("S3LoggingConfiguration={persistent-ui-logs=on-customer-s3only}", None)
            .expect_err("refuses")
            .message()
            .contains("Invalid policy for log type 'persistent-ui-logs'"));
        assert!(with("S3LoggingConfiguration={nonsense=emr-managed}", None)
            .expect_err("refuses")
            .message()
            .contains("Invalid log type"));
        assert!(with("S3LoggingConfiguration={system-logs=emr-managed}", Some("s3://logs/"))
            .is_ok());
    }
}
