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
        _ => Ok(None),
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

    #[test]
    fn cluster_ids_split_on_whitespace() {
        let ids: Vec<&str> = "j-1 j-2  j-3".split_whitespace().collect();
        assert_eq!(ids, vec!["j-1", "j-2", "j-3"]);
    }
}
