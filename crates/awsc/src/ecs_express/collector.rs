//! Collecting one view of an Express Gateway service.
//!
//! A port of `customizations/ecs/serviceviewcollector.py`. Every poll asks ECS what the
//! service looks like now and turns the answer into a [`Group`] tree plus an optional
//! line of prose. The two view modes differ only in what they build the tree from:
//!
//! - **RESOURCE** combines the managed resources of *every* active configuration.
//! - **DEPLOYMENT** describes the latest deployment and shows the set difference between
//!   its target revision and its source revisions — what is being added, and what is
//!   being taken away.
//!
//! The collector keeps the last good tree. A poll that cannot produce one (no deployment
//! yet, a revision that has not appeared) replaces only the prose, so the display keeps
//! showing the last real state instead of blinking to empty.

use super::resource::{Group, Node, Resource};
use crate::client::Client;
use serde_json::{json, Value};

/// Something went wrong that should stop monitoring, as distinct from a state we are
/// waiting for.
#[derive(Debug)]
pub struct MonitoringError(pub String);

impl std::fmt::Display for MonitoringError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The error the service returns while it is being torn down. It is not a failure to
/// report; monitoring simply has nothing left to watch.
const INACTIVE_MESSAGE: &str =
    "Cannot call DescribeServiceRevisions for a service that is INACTIVE";

pub enum Mode {
    Resource,
    Deployment,
}

pub struct Collector<'a> {
    client: &'a Client<'a>,
    service_arn: String,
    mode: Mode,
    color: bool,
    /// The last tree and prose that could be built. `None` before the first success.
    pub cached: Option<(Option<Group>, Option<String>)>,
}

impl<'a> Collector<'a> {
    pub fn new(client: &'a Client<'a>, service_arn: &str, mode: Mode, color: bool) -> Self {
        Collector {
            client,
            service_arn: service_arn.to_string(),
            mode,
            color,
            cached: None,
        }
    }

    /// Poll once and render. `Ok(None)` means the service is now inactive.
    pub fn refresh(&mut self) -> Result<(), MonitoringError> {
        let described = match self
            .client
            .call("describe-express-gateway-service", Some(&json!({ "serviceArn": self.service_arn })))
        {
            Ok(described) => described,
            Err(failure) => {
                if is_inactive(&failure) {
                    self.remember(Some(Group::default()), Some("Service is inactive".into()));
                    return Ok(());
                }
                return Err(MonitoringError(failure.message().to_string()));
            }
        };

        let service = described.get("service");
        let usable = service.is_some_and(|service| {
            service.get("serviceArn").is_some() && service.get("activeConfigurations").is_some()
        });
        let Some(service) = service.filter(|_| usable) else {
            self.remember(None, Some("Trying to describe gateway service".into()));
            return Ok(());
        };

        let (managed, info) = match self.mode {
            Mode::Deployment => self.deployment_view(service)?,
            Mode::Resource => self.combined_view(service)?,
        };

        let mut top = vec![
            Node::Leaf(Resource::new(
                "Cluster",
                service.get("cluster").and_then(Value::as_str).map(str::to_string),
            )),
            Node::Leaf(self.service_resource(service)?),
        ];
        if let Some(managed) = managed {
            top.push(Node::Group(managed));
        }
        self.remember(Some(Group::new(top)), info);
        Ok(())
    }

    /// The rendered view: the tree, then the prose, whichever of them exist.
    pub fn view(&self, spinner: &str) -> String {
        let Some((tree, info)) = &self.cached else {
            return "Waiting for initial data".to_string();
        };
        let mut parts = Vec::new();
        if let Some(tree) = tree {
            parts.push(tree.status_string(spinner, 0, self.color));
        }
        if let Some(info) = info {
            parts.push(info.clone());
        }
        parts.join("\n")
    }

    /// A poll that produced no tree keeps the previous one and replaces only the prose.
    fn remember(&mut self, tree: Option<Group>, info: Option<String>) {
        self.cached = match self.cached.take() {
            None => Some((tree, info)),
            Some((previous, _)) => Some((tree.or(previous), info)),
        };
    }

    /// `Service`, carrying the most recent event message as its extra line.
    fn service_resource(&self, service: &Value) -> Result<Resource, MonitoringError> {
        let cluster = service.get("cluster").and_then(Value::as_str).unwrap_or_default();
        let service_arn = service.get("serviceArn").and_then(Value::as_str).unwrap_or_default();
        let described = self
            .client
            .call(
                "describe-services",
                Some(&json!({ "cluster": cluster, "services": [service_arn] })),
            )
            .map_err(|e| MonitoringError(e.message().to_string()))?;
        let services = self.field(&described, "DescribeServices", "services", false)?;
        let first = services.first().cloned().unwrap_or(Value::Null);
        let additional_info = first
            .get("events")
            .and_then(Value::as_array)
            .and_then(|events| events.first())
            .and_then(|event| event.get("message"))
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok(Resource {
            additional_info,
            ..Resource::new("Service", Some(service_arn.to_string()))
        })
    }

    /// Every active configuration's resources, merged.
    fn combined_view(
        &self,
        service: &Value,
    ) -> Result<(Option<Group>, Option<String>), MonitoringError> {
        let arns: Vec<String> = service
            .get("activeConfigurations")
            .and_then(Value::as_array)
            .map(|configs| {
                configs
                    .iter()
                    .filter_map(|config| config.get("serviceRevisionArn"))
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let (groups, _) = self.describe_revisions(&arns)?;
        if groups.is_empty() || groups.len() != arns.len() {
            return Ok((None, Some("Trying to describe service revisions".into())));
        }
        let combined = groups.into_iter().reduce(Group::combine).expect("at least one");
        Ok((Some(combined), None))
    }

    /// What the latest deployment is adding and removing.
    fn deployment_view(
        &self,
        service: &Value,
    ) -> Result<(Option<Group>, Option<String>), MonitoringError> {
        let waiting = || Ok((None, Some("Waiting for a deployment to start".to_string())));
        let service_arn = service.get("serviceArn").and_then(Value::as_str).unwrap_or_default();

        let listed = self
            .client
            .call("list-service-deployments", Some(&json!({ "service": service_arn, "maxResults": 1 })))
            .map_err(|e| MonitoringError(e.message().to_string()))?;
        let deployments =
            self.field(&listed, "ListServiceDeployments", "serviceDeployments", false)?;
        let Some(first) = deployments.first() else { return waiting() };
        let Some(deployment_arn) =
            first.get("serviceDeploymentArn").and_then(Value::as_str).map(str::to_string)
        else {
            return waiting();
        };

        let described = self
            .client
            .call(
                "describe-service-deployments",
                Some(&json!({ "serviceDeploymentArns": [deployment_arn] })),
            )
            .map_err(|e| MonitoringError(e.message().to_string()))?;
        let described =
            self.field(&described, "DescribeServiceDeployments", "serviceDeployments", true)?;
        let Some(deployment) = described.first() else { return waiting() };
        let Some(target_arn) = deployment
            .get("targetServiceRevision")
            .and_then(|revision| revision.get("arn"))
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return waiting();
        };

        let (mut target_groups, target_revisions) =
            self.describe_revisions(std::slice::from_ref(&target_arn))?;
        if target_groups.len() != 1 {
            return Ok((None, Some("Trying to describe service revisions".into())));
        }
        let target = target_groups.remove(0);
        let task_definition = target_revisions
            .first()
            .and_then(|revision| revision.get("taskDefinition"))
            .and_then(Value::as_str)
            .map(str::to_string);

        let source = match deployment.get("sourceServiceRevisions").and_then(Value::as_array) {
            None => Group::default(),
            Some(sources) => {
                let arns: Vec<String> = sources
                    .iter()
                    .filter_map(|revision| revision.get("arn"))
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect();
                let (groups, _) = self.describe_revisions(&arns)?;
                if groups.len() != sources.len() {
                    return Ok((None, Some("Trying to describe service revisions".into())));
                }
                groups.into_iter().reduce(Group::combine).unwrap_or_default()
            }
        };

        let (mut updating, mut disassociating) = target.compare_sets(&source);
        updating.resource_type = Some("Updating".to_string());
        disassociating.resource_type = Some("Disassociating".to_string());

        let mut group = Group::with(
            Some("Deployment"),
            Some(deployment_arn),
            vec![
                Node::Leaf(Resource::new("TargetServiceRevision", Some(target_arn))),
                Node::Leaf(Resource::new("TaskDefinition", task_definition)),
                Node::Group(updating),
                Node::Group(disassociating),
            ],
        );
        group.status =
            deployment.get("status").and_then(Value::as_str).map(str::to_string);
        group.reason =
            deployment.get("statusReason").and_then(Value::as_str).map(str::to_string);
        Ok((Some(group), None))
    }

    /// Describe revisions and turn each into its resource tree.
    fn describe_revisions(
        &self,
        arns: &[String],
    ) -> Result<(Vec<Group>, Vec<Value>), MonitoringError> {
        let described = self
            .client
            .call(
                "describe-service-revisions",
                Some(&json!({ "serviceRevisionArns": arns })),
            )
            .map_err(|e| MonitoringError(e.message().to_string()))?;
        let revisions =
            self.field(&described, "DescribeServiceRevisions", "serviceRevisions", true)?;
        Ok((revisions.iter().map(parse_managed_resources).collect(), revisions))
    }

    /// Check a response's `failures` and pull out the field the caller needs.
    ///
    /// `eventually_consistent` drops `MISSING` failures: a revision that has just been
    /// created is briefly absent, and treating that as an error would end monitoring at
    /// exactly the moment it becomes interesting.
    fn field(
        &self,
        response: &Value,
        operation: &str,
        field: &str,
        eventually_consistent: bool,
    ) -> Result<Vec<Value>, MonitoringError> {
        if let Some(failures) = response.get("failures").and_then(Value::as_array) {
            if !failures.is_empty() {
                let text = |failure: &Value, key: &str| {
                    failure.get(key).and_then(Value::as_str).map(str::to_string)
                };
                if failures
                    .iter()
                    .any(|failure| text(failure, "arn").is_none() || text(failure, "reason").is_none())
                {
                    return Err(MonitoringError(
                        "Invalid failure response: missing arn or reason".to_string(),
                    ));
                }
                let reported: Vec<String> = failures
                    .iter()
                    .filter(|failure| {
                        !eventually_consistent
                            || text(failure, "reason").as_deref() != Some("MISSING")
                    })
                    .map(|failure| {
                        format!(
                            "{} failed with {}",
                            text(failure, "arn").unwrap_or_default(),
                            text(failure, "reason").unwrap_or_default()
                        )
                    })
                    .collect();
                if !reported.is_empty() {
                    return Err(MonitoringError(format!(
                        "{operation}:\n{}",
                        reported.join("\n")
                    )));
                }
            }
        }
        match response.get(field) {
            None | Some(Value::Null) => Err(MonitoringError(format!(
                "{operation} response is missing {field}"
            ))),
            Some(value) => Ok(value.as_array().cloned().unwrap_or_default()),
        }
    }
}

/// Is this the "the service is going away" error rather than a real failure?
fn is_inactive(failure: &crate::Failure) -> bool {
    failure.service_error_code.as_deref() == Some("InvalidParameterException")
        && failure
            .service_error_message
            .as_deref()
            .unwrap_or_else(|| failure.message())
            .contains(INACTIVE_MESSAGE)
}

/// One service revision's `ecsManagedResources`, as a tree.
fn parse_managed_resources(revision: &Value) -> Group {
    let Some(managed) = revision.get("ecsManagedResources").filter(|value| !value.is_null())
    else {
        return Group::default();
    };
    let mut parsed = Vec::new();
    if let Some(paths) = managed.get("ingressPaths").and_then(Value::as_array) {
        parsed.push(Node::Group(Group::with(
            Some("IngressPaths"),
            None,
            paths.iter().map(|path| Node::Group(parse_ingress_path(path))).collect(),
        )));
    }
    if let Some(auto_scaling) = managed.get("autoScaling") {
        let mut resources = Vec::new();
        if let Some(target) = auto_scaling.get("scalableTarget") {
            resources.push(Node::Leaf(parse_resource(target, "ScalableTarget")));
        }
        if let Some(policies) =
            auto_scaling.get("applicationAutoScalingPolicies").and_then(Value::as_array)
        {
            resources.extend(
                policies
                    .iter()
                    .map(|policy| {
                        Node::Leaf(parse_resource(policy, "ApplicationAutoScalingPolicy"))
                    }),
            );
        }
        parsed.push(Node::Group(Group::with(
            Some("AutoScalingConfiguration"),
            None,
            resources,
        )));
    }
    for (key, group_type, member_type) in [
        ("metricAlarms", "MetricAlarms", "MetricAlarm"),
        ("serviceSecurityGroups", "ServiceSecurityGroups", "SecurityGroup"),
        ("logGroups", "LogGroups", "LogGroup"),
    ] {
        if let Some(items) = managed.get(key).and_then(Value::as_array) {
            parsed.push(Node::Group(Group::with(
                Some(group_type),
                None,
                items.iter().map(|item| Node::Leaf(parse_resource(item, member_type))).collect(),
            )));
        }
    }
    Group::new(parsed)
}

fn parse_ingress_path(path: &Value) -> Group {
    // The build order is the display order, and it is not alphabetical: the load
    // balancer's security groups sit between the load balancer and the certificate.
    let mut ordered = Vec::new();
    if let Some(value) = path.get("loadBalancer").filter(|value| !value.is_null()) {
        ordered.push(Node::Leaf(parse_resource(value, "LoadBalancer")));
    }
    if let Some(groups) = path.get("loadBalancerSecurityGroups").and_then(Value::as_array) {
        ordered.extend(
            groups
                .iter()
                .map(|group| Node::Leaf(parse_resource(group, "LoadBalancerSecurityGroup"))),
        );
    }
    for (key, resource_type) in
        [("certificate", "Certificate"), ("listener", "Listener"), ("rule", "Rule")]
    {
        if let Some(value) = path.get(key).filter(|value| !value.is_null()) {
            ordered.push(Node::Leaf(parse_resource(value, resource_type)));
        }
    }
    if let Some(targets) = path.get("targetGroups").and_then(Value::as_array) {
        ordered
            .extend(targets.iter().map(|target| Node::Leaf(parse_resource(target, "TargetGroup"))));
    }
    Group::with(
        Some("IngressPath"),
        path.get("endpoint").and_then(Value::as_str).map(str::to_string),
        ordered,
    )
}

fn parse_resource(value: &Value, resource_type: &str) -> Resource {
    let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
    Resource {
        resource_type: Some(resource_type.to_string()),
        identifier: text("arn"),
        status: text("status"),
        updated_at: text("updatedAt").and_then(|stamp| parse_timestamp(&stamp)),
        reason: text("statusReason"),
        additional_info: None,
    }
}

/// An ISO-8601 timestamp as the API sends it. Anything else is dropped rather than
/// guessed at, since a wrong timestamp would be reported as a real one.
pub fn parse_timestamp(text: &str) -> Option<f64> {
    let bytes = text.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let number = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    let m = if month <= 2 { month + 12 } else { month };
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m - 3) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + hour * 3600 + minute * 60 + second) as f64)
}
