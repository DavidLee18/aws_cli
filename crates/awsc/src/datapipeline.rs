//! `aws datapipeline create-default-roles` and `list-runs`.
//!
//! Ports of `customizations/datapipeline/`. Neither is an API call the model describes:
//! `create-default-roles` drives IAM, and `list-runs` runs two Data Pipeline operations
//! and prints a layout of its own.
//!
//! `list-runs` is the only command in the tree with its **own default formatter**. It
//! prints a two-line-per-run block when `--output` was not given, and falls back to the
//! ordinary formatter when it was — which is why the parser has to remember whether the
//! flag was passed rather than just what it resolved to. `--output json` and no `--output`
//! at all produce completely different output here.

use crate::args::Parsed;
use crate::client::{Client, Globals};
use crate::exit;
use crate::Failure;
use serde_json::{json, Value};
use std::process::ExitCode;

/// DescribeObjects takes at most 100 ids, so the id list is walked in chunks.
const MAX_ITEMS_PER_DESCRIBE: usize = 100;

/// The statuses `--status` accepts. Note `shutting_down`, which the reference's own help
/// text omits but its validator accepts.
const VALID_STATUS: &[&str] = &[
    "waiting",
    "pending",
    "cancelled",
    "running",
    "finished",
    "failed",
    "waiting_for_runner",
    "waiting_on_dependencies",
    "shutting_down",
];

const DEPRECATION_NOTICE: &str = "\nSupport for this command has been deprecated and may fail to create these roles\nif they do not already exist. For more information on managing these policies\nmanually see the following documentation:\n\nhttps://docs.aws.amazon.com/datapipeline/latest/DeveloperGuide/dp-iam-roles.html\n";

pub fn dispatch(parsed: &Parsed, globals: &Globals) -> Result<Option<ExitCode>, Failure> {
    match parsed.operation.as_str() {
        "create-default-roles" => create_default_roles(parsed, globals).map(Some),
        "list-runs" => list_runs(parsed, globals).map(Some),
        _ => Ok(None),
    }
}

/// `aws datapipeline create-default-roles`.
///
/// Creates the service role, the resource role and the instance profile the resource role
/// goes in — each only if absent, and the result lists only the ones it actually created.
/// A second run therefore prints an empty list, not an error.
fn create_default_roles(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    crate::custom::take_args(parsed, &[])?;

    let region = crate::custom::resolve_region(globals)
        .ok_or_else(|| Failure::new(exit::CONFIGURATION, awsc_runtime::RuntimeError::NoRegion))?;
    let iam_globals = Globals { region: Some(region), ..globals.clone() };
    let model = crate::load_model("iam").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let iam = Client::new(&model, &iam_globals)?;

    // `warnings.warn` writes to stderr, so it does not pollute a piped result.
    eprintln!("{DEPRECATION_NOTICE}");

    // Both documents are `2008-10-17`, not the 2012 version every other example uses.
    let service_trust = json!({
        "Version": "2008-10-17",
        "Statement": [{
            "Sid": "",
            "Effect": "Allow",
            "Principal": { "Service": ["datapipeline.amazonaws.com", "elasticmapreduce.amazonaws.com"] },
            "Action": "sts:AssumeRole"
        }]
    });
    let resource_trust = json!({
        "Version": "2008-10-17",
        "Statement": [{
            "Sid": "",
            "Effect": "Allow",
            "Principal": { "Service": "ec2.amazonaws.com" },
            "Action": "sts:AssumeRole"
        }]
    });

    let mut result: Vec<Value> = Vec::new();
    for (role, policy_arn, trust) in [
        (
            "DataPipelineDefaultRole",
            "arn:aws:iam::aws:policy/service-role/AWSDataPipelineRole",
            &service_trust,
        ),
        (
            "DataPipelineDefaultResourceRole",
            "arn:aws:iam::aws:policy/service-role/AmazonEC2RoleforDataPipelineRole",
            &resource_trust,
        ),
    ] {
        if let Some(entry) = create_role_if_absent(&iam, role, policy_arn, trust)? {
            result.push(entry);
        }
    }

    // The instance profile carries the resource role's name, and is created even when the
    // role itself already existed — the two are independent objects.
    let profile = "DataPipelineDefaultResourceRole";
    if !exists(iam.call("get-instance-profile", Some(&json!({ "InstanceProfileName": profile }))))?
    {
        iam.call("create-instance-profile", Some(&json!({ "InstanceProfileName": profile })))?;
        iam.call(
            "add-role-to-instance-profile",
            Some(&json!({ "InstanceProfileName": profile, "RoleName": profile })),
        )?;
    }

    render("create_role", &Value::Array(result), parsed)
}

/// Create one role and attach its managed policy, unless the role is already there.
fn create_role_if_absent(
    iam: &Client<'_>,
    role_name: &str,
    policy_arn: &str,
    trust: &Value,
) -> Result<Option<Value>, Failure> {
    if exists(iam.call("get-role", Some(&json!({ "RoleName": role_name }))))? {
        return Ok(None);
    }
    let created = iam.call(
        "create-role",
        Some(&json!({
            "RoleName": role_name,
            // A JSON *string*, not a nested document.
            "AssumeRolePolicyDocument": serde_json::to_string(trust).expect("literal document"),
        })),
    )?;
    iam.call("attach-role-policy", Some(&json!({ "PolicyArn": policy_arn, "RoleName": role_name })))?;

    let version = iam
        .call("get-policy", Some(&json!({ "PolicyArn": policy_arn })))?
        .get("Policy")
        .and_then(|p| p.get("DefaultVersionId"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let document = iam
        .call(
            "get-policy-version",
            Some(&json!({ "PolicyArn": policy_arn, "VersionId": version })),
        )?
        .get("PolicyVersion")
        .and_then(|v| v.get("Document"))
        .cloned()
        .unwrap_or(Value::Null);

    // Only a response that actually carries a `Role` is reported, matching the reference's
    // `if response['Role'] is not None`.
    Ok(created.get("Role").map(|role| json!({ "Role": role, "RolePolicy": document })))
}

/// `NoSuchEntity` means absent. Every other failure is a failure — treating one as
/// "absent" would try to create a role the caller merely cannot see.
fn exists(outcome: Result<Value, Failure>) -> Result<bool, Failure> {
    match outcome {
        Ok(_) => Ok(true),
        Err(failure) if failure.service_error_code.as_deref() == Some("NoSuchEntity") => Ok(false),
        Err(failure) => Err(failure),
    }
}

/// `aws datapipeline list-runs`.
fn list_runs(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let args = crate::custom::take_args(
        parsed,
        &["--pipeline-id", "--status", "--start-interval", "--schedule-interval"],
    )?;
    if !args.contains_key("--pipeline-id") {
        return Err(crate::custom::missing_required(&["--pipeline-id"]));
    }
    let pipeline_id = args.get("--pipeline-id").copied().flatten().unwrap_or_default();

    let interval = |flag: &str| -> Option<Vec<String>> {
        args.get(flag)
            .copied()
            .flatten()
            .map(|raw| raw.split(',').map(|part| part.trim().to_string()).collect())
    };
    let start_interval = interval("--start-interval");
    let schedule_interval = interval("--schedule-interval");
    let statuses = match interval("--status") {
        None => None,
        Some(list) => {
            for status in &list {
                if !VALID_STATUS.contains(&status.as_str()) {
                    return Err(Failure::new(
                        exit::PARAM_VALIDATION,
                        awsc_runtime::RuntimeError::ParamValidation(format!(
                            "Invalid status: {status}, must be one of: {}",
                            VALID_STATUS.join(", ")
                        )),
                    ));
                }
            }
            Some(list)
        }
    };

    let query = build_query(
        start_interval.as_deref(),
        schedule_interval.as_deref(),
        statuses.as_deref(),
        crate::now_unix(),
    );

    let region = crate::custom::resolve_region(globals)
        .ok_or_else(|| Failure::new(exit::CONFIGURATION, awsc_runtime::RuntimeError::NoRegion))?;
    let client_globals = Globals { region: Some(region), ..globals.clone() };
    let model =
        crate::load_model("datapipeline").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let client = Client::new(&model, &client_globals)?;

    // QueryObjects paginates on its own marker rather than through the shared paginator,
    // because this is not the modelled dispatch path.
    let mut ids: Vec<Value> = Vec::new();
    let mut marker: Option<String> = None;
    loop {
        let mut input = json!({
            "pipelineId": pipeline_id,
            "sphere": "INSTANCE",
            "query": query,
        });
        if let Some(marker) = &marker {
            input["marker"] = Value::String(marker.clone());
        }
        let page = client.call("query-objects", Some(&input))?;
        if let Some(Value::Array(page_ids)) = page.get("ids") {
            ids.extend(page_ids.iter().cloned());
        }
        let more = page.get("hasMoreResults").and_then(Value::as_bool).unwrap_or(false);
        marker = page.get("marker").and_then(Value::as_str).map(str::to_string);
        if !more || marker.is_none() {
            break;
        }
    }

    let mut objects: Vec<Value> = Vec::new();
    for chunk in ids.chunks(MAX_ITEMS_PER_DESCRIBE) {
        let described = client.call(
            "describe-objects",
            Some(&json!({ "pipelineId": pipeline_id, "objectIds": chunk })),
        )?;
        if let Some(Value::Array(found)) = described.get("pipelineObjects") {
            objects.extend(found.iter().cloned());
        }
    }

    let converted = convert_described_objects(&objects);
    if parsed.output_given {
        return render("list-runs", &Value::Array(converted), parsed);
    }
    print!("{}", format_runs(&converted));
    Ok(exit::code(exit::SUCCESS))
}

/// The `QueryObjects` selector list.
///
/// With neither interval given the window is the **last four days**, which is why
/// `list-runs` on an old pipeline looks empty rather than erroring.
fn build_query(
    start_interval: Option<&[String]>,
    schedule_interval: Option<&[String]>,
    statuses: Option<&[String]>,
    now: i64,
) -> Value {
    let mut selectors: Vec<Value> = Vec::new();
    let between = |field: &str, values: &[String]| {
        json!({
            "fieldName": field,
            "operator": { "type": "BETWEEN", "values": values }
        })
    };
    match (start_interval, schedule_interval) {
        (None, None) => {
            let window = [timestamp(now - 4 * 86_400), timestamp(now)];
            selectors.push(between("@actualStartTime", &window));
        }
        (start, schedule) => {
            if let Some(values) = start {
                selectors.push(between("@actualStartTime", values));
            }
            if let Some(values) = schedule {
                selectors.push(between("@scheduledStartTime", values));
            }
        }
    }
    if let Some(statuses) = statuses {
        let upper: Vec<String> = statuses.iter().map(|s| s.to_uppercase()).collect();
        selectors.push(json!({
            "fieldName": "@status",
            "operator": { "type": "EQ", "values": upper }
        }));
    }
    json!({ "selectors": selectors })
}

/// `%Y-%m-%dT%H:%M:%S` in UTC, with no offset and no `Z` — the spelling the API expects.
fn timestamp(unix: i64) -> String {
    let stamp = awsc_runtime::sigv4::format_timestamp(unix);
    // `20200913T122640Z` -> `2020-09-13T12:26:40`
    let (date, rest) = stamp.split_at(8);
    let time = rest.trim_start_matches('T').trim_end_matches('Z');
    format!(
        "{}-{}-{}T{}:{}:{}",
        &date[0..4],
        &date[4..6],
        &date[6..8],
        &time[0..2],
        &time[2..4],
        &time[4..6]
    )
}

/// Flatten each object's `fields` list into the object itself, then sort.
fn convert_described_objects(objects: &[Value]) -> Vec<Value> {
    let mut converted: Vec<Value> = objects
        .iter()
        .map(|object| {
            let mut out = serde_json::Map::new();
            out.insert("@id".into(), object.get("id").cloned().unwrap_or(Value::Null));
            out.insert("name".into(), object.get("name").cloned().unwrap_or(Value::Null));
            if let Some(Value::Array(fields)) = object.get("fields") {
                for field in fields {
                    let Some(key) = field.get("key").and_then(Value::as_str) else { continue };
                    // A field carries either a string or a reference; the reference reads
                    // `stringValue` and falls back to `refValue`.
                    let value = field
                        .get("stringValue")
                        .or_else(|| field.get("refValue"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    out.insert(key.to_string(), value);
                }
            }
            Value::Object(out)
        })
        .collect();
    let key = |object: &Value| {
        (
            object.get("@scheduledStartTime").and_then(Value::as_str).unwrap_or("").to_string(),
            object.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
        )
    };
    converted.sort_by_key(key);
    converted
}

/// The two-line-per-run layout, with the reference's exact column widths.
///
/// `%-50.50s` both pads *and truncates* at 50, so a long name is cut rather than pushing
/// the columns out of line.
fn format_runs(runs: &[Value]) -> String {
    let field = |object: &Value, key: &str| -> String {
        object.get(key).and_then(Value::as_str).unwrap_or("").to_string()
    };
    let pad = |text: &str, width: usize| {
        let truncated: String = text.chars().take(width).collect();
        format!("{truncated:<width$}")
    };

    let mut out = String::new();
    out.push_str(&format!(
        "       {}  {}  {}\n",
        pad("Name", 50),
        pad("Scheduled Start", 19),
        pad("Status", 23)
    ));
    let second_header =
        format!("       {}  {}  {}", pad("ID", 50), pad("Started", 19), pad("Ended", 19));
    out.push_str(&second_header);
    out.push('\n');
    out.push_str(&"-".repeat(second_header.chars().count()));
    out.push('\n');

    for (index, run) in runs.iter().enumerate() {
        out.push_str(&format!(
            "{:>4}.  {}  {}  {}\n",
            index + 1,
            pad(&field(run, "@componentParent"), 50),
            pad(&field(run, "@scheduledStartTime"), 19),
            pad(&field(run, "@status"), 23)
        ));
        out.push_str(&format!(
            "       {}  {}  {}\n\n",
            pad(&field(run, "@id"), 50),
            pad(&field(run, "@actualStartTime"), 19),
            pad(&field(run, "@actualEndTime"), 19)
        ));
    }
    out
}

fn render(name: &str, value: &Value, parsed: &Parsed) -> Result<ExitCode, Failure> {
    match awsc_output::render_named(name, value, parsed.output) {
        Ok(Some(text)) => print!("{text}"),
        Ok(None) => {}
        Err(e) => return Err(Failure::new(exit::GENERAL_ERROR, e)),
    }
    Ok(exit::code(exit::SUCCESS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_window_is_the_last_four_days() {
        let query = build_query(None, None, None, 1_600_000_000);
        let selectors = query["selectors"].as_array().expect("selectors");
        assert_eq!(selectors.len(), 1);
        assert_eq!(selectors[0]["fieldName"], "@actualStartTime");
        assert_eq!(
            selectors[0]["operator"]["values"],
            json!(["2020-09-09T12:26:40", "2020-09-13T12:26:40"])
        );
    }

    /// An explicit interval replaces the default window rather than narrowing it.
    #[test]
    fn an_interval_replaces_the_default_window() {
        let start = vec!["2020-01-01T00:00:00".to_string(), "2020-01-02T00:00:00".to_string()];
        let query = build_query(Some(&start), None, None, 1_600_000_000);
        let selectors = query["selectors"].as_array().expect("selectors");
        assert_eq!(selectors.len(), 1);
        assert_eq!(selectors[0]["operator"]["values"], json!(start));
    }

    #[test]
    fn statuses_are_upper_cased() {
        let statuses = vec!["running".to_string(), "waiting_for_runner".to_string()];
        let query = build_query(None, None, Some(&statuses), 0);
        let selectors = query["selectors"].as_array().expect("selectors");
        assert_eq!(selectors[1]["fieldName"], "@status");
        assert_eq!(selectors[1]["operator"]["values"], json!(["RUNNING", "WAITING_FOR_RUNNER"]));
    }

    #[test]
    fn fields_are_flattened_onto_the_object() {
        let objects = vec![json!({
            "id": "@Obj_1", "name": "Obj",
            "fields": [
                {"key": "@status", "stringValue": "FINISHED"},
                {"key": "@componentParent", "refValue": "Parent"}
            ]
        })];
        let converted = convert_described_objects(&objects);
        assert_eq!(converted[0]["@id"], "@Obj_1");
        assert_eq!(converted[0]["@status"], "FINISHED");
        // `refValue` when there is no `stringValue`.
        assert_eq!(converted[0]["@componentParent"], "Parent");
    }

    #[test]
    fn runs_sort_by_scheduled_start_then_name() {
        let objects = vec![
            json!({"id": "b", "name": "b", "fields": [{"key": "@scheduledStartTime", "stringValue": "2020-01-02"}]}),
            json!({"id": "a", "name": "a", "fields": [{"key": "@scheduledStartTime", "stringValue": "2020-01-01"}]}),
        ];
        let converted = convert_described_objects(&objects);
        assert_eq!(converted[0]["@id"], "a");
        assert_eq!(converted[1]["@id"], "b");
    }

    /// `%-50.50s` truncates as well as pads, so the columns cannot be pushed apart.
    #[test]
    fn a_long_name_is_truncated_not_wrapped() {
        let runs = vec![json!({
            "@componentParent": "x".repeat(80),
            "@id": "id",
            "@scheduledStartTime": "2020-01-01T00:00:00",
            "@status": "FINISHED",
        })];
        let text = format_runs(&runs);
        for line in text.lines() {
            assert!(line.chars().count() <= 105, "{}", line.chars().count());
        }
        assert!(text.contains(&"x".repeat(50)));
        assert!(!text.contains(&"x".repeat(51)));
    }

    #[test]
    fn the_header_rule_matches_the_header_width() {
        let text = format_runs(&[]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[2].chars().count(), lines[1].chars().count());
        assert!(lines[2].chars().all(|c| c == '-'));
    }
}
