//! `aws emr-containers update-role-trust-policy`, `create-role-associations` and
//! `delete-role-associations`.
//!
//! Ports of `customizations/emrcontainers/`. All three connect an IAM role to the service
//! accounts EMR on EKS runs pods under, by two different mechanisms: the trust-policy
//! command edits the role's own assume-role document (IRSA), while the association
//! commands create EKS **pod identity associations** — API calls, not `kubectl`.
//!
//! The piece that decides whether any of it works is the **base36 role name**. EMR names
//! its service accounts `emr-containers-sa-<framework>-<component>-<account>-<base36>`,
//! where the base36 is the role name read as a big-endian integer over its bytes and
//! re-expressed in base 36. Python does that with bignums; here it is long division over a
//! byte array, because a 64-character role name is a 512-bit number. Get it wrong and
//! every name is subtly different from the one EMR will present, so the association exists
//! and never matches.

use crate::args::Parsed;
use crate::client::{Client, Globals};
use crate::exit;
use crate::Failure;
use serde_json::{json, Value};
use std::process::ExitCode;

/// The service-account name EMR derives for a pod.
fn service_account(framework: &str, component: &str, account: &str, base36: &str) -> String {
    format!("emr-containers-sa-{framework}-{component}-{account}-{base36}")
}

pub fn dispatch(parsed: &Parsed, globals: &Globals) -> Result<Option<ExitCode>, Failure> {
    match parsed.operation.as_str() {
        "update-role-trust-policy" => update_role_trust_policy(parsed, globals).map(Some),
        "create-role-associations" => role_associations(parsed, globals, true).map(Some),
        "delete-role-associations" => role_associations(parsed, globals, false).map(Some),
        _ => Ok(None),
    }
}

/// base36, the way `Base36.encode` does it: the string's bytes as one big integer, then
/// repeatedly divided by 36.
///
/// `str_to_int` multiplies by **256** per character and adds `ord(char)`, so this is the
/// byte string read big-endian — and for a non-ASCII name Python's `ord` yields a code
/// point above 255, which this cannot reproduce and which EMR would not produce either.
pub fn base36(text: &str) -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut digits: Vec<u8> = text.bytes().collect();
    if digits.iter().all(|byte| *byte == 0) {
        return "0".to_string();
    }
    let mut out = Vec::new();
    // Long division over the byte array, most significant first, until nothing is left.
    while digits.iter().any(|byte| *byte != 0) {
        let mut remainder = 0u32;
        let mut quotient = Vec::with_capacity(digits.len());
        for byte in &digits {
            let current = remainder * 256 + *byte as u32;
            quotient.push((current / 36) as u8);
            remainder = current % 36;
        }
        out.push(ALPHABET[remainder as usize]);
        // Drop leading zeros so the loop terminates.
        let first = quotient.iter().position(|d| *d != 0).unwrap_or(quotient.len());
        digits = quotient[first..].to_vec();
    }
    out.reverse();
    String::from_utf8(out).expect("the alphabet is ASCII")
}

/// The EKS cluster's account id and OIDC provider, from one DescribeCluster.
fn cluster_identity(
    globals: &Globals,
    region: &str,
    cluster_name: &str,
) -> Result<(String, String), Failure> {
    // Note: the reference builds this client **without** `endpoint_url` — `--iam-endpoint`
    // redirects IAM only, and the global flag is not passed through either.
    let eks_globals = Globals { region: Some(region.to_string()), ..globals.for_service("eks") };
    let model = crate::load_model("eks").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let eks = Client::new(&model, &eks_globals)?;
    let described = eks.call("describe-cluster", Some(&json!({ "name": cluster_name })))?;
    let cluster = described.get("cluster").cloned().unwrap_or(Value::Null);

    let arn = cluster.get("arn").and_then(Value::as_str).unwrap_or_default();
    // `arn:aws:eks:region:ACCOUNT:cluster/name` — field 4.
    let account = arn.split(':').nth(4).unwrap_or_default().to_string();

    let issuer = cluster
        .get("identity")
        .and_then(|i| i.get("oidc"))
        .and_then(|o| o.get("issuer"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    // `issuer.split('https://')[1]` — an issuer without the scheme is an IndexError there,
    // and an empty provider here.
    let provider = issuer.split("https://").nth(1).unwrap_or_default().to_string();
    Ok((account, provider))
}

fn update_role_trust_policy(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let flags = ["--cluster-name", "--namespace", "--role-name", "--iam-endpoint", "--dry-run"];
    let args = crate::custom::take_args(parsed, &flags)?;
    let missing: Vec<&str> = ["--cluster-name", "--namespace", "--role-name"]
        .into_iter()
        .filter(|flag| !args.contains_key(flag))
        .collect();
    if !missing.is_empty() {
        return Err(crate::custom::missing_required(&missing));
    }
    let value = |flag: &str| args.get(flag).copied().flatten().unwrap_or_default();
    let cluster_name = value("--cluster-name");
    let namespace = value("--namespace");
    let role_name = value("--role-name");
    let dry_run = args.contains_key("--dry-run");

    let region = crate::custom::resolve_region(globals)
        .ok_or_else(|| Failure::new(exit::CONFIGURATION, awsc_runtime::RuntimeError::NoRegion))?;
    let (account, provider) = cluster_identity(globals, &region, cluster_name)?;

    let statement = trust_statement(
        crate::custom::policy_partition(&region),
        &account,
        &provider,
        namespace,
        &base36(role_name),
    );

    let mut iam_globals = Globals { region: Some(region.clone()), ..globals.for_service("iam") };
    if let Some(endpoint) = args.get("--iam-endpoint").copied().flatten() {
        iam_globals.endpoint_url = Some(endpoint.to_string());
    }
    let model = crate::load_model("iam").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let iam = Client::new(&model, &iam_globals)?;

    let mut document = iam
        .call("get-role", Some(&json!({ "RoleName": role_name })))?
        .get("Role")
        .and_then(|role| role.get("AssumeRolePolicyDocument"))
        .cloned()
        .unwrap_or(Value::Null);

    if statement_exists(&statement, &document) {
        println!("Trust policy statement already exists for role {role_name}. No changes were made!");
        return Ok(exit::code(exit::SUCCESS));
    }

    match document.get_mut("Statement").and_then(Value::as_array_mut) {
        Some(statements) => statements.push(statement),
        None => {
            if !document.is_object() {
                document = json!({});
            }
            document["Statement"] = json!([statement]);
        }
    }

    if dry_run {
        // The merged document, so it can be reviewed before it is sent — and it is *not*
        // sent in this mode. `indent=2` here, where the association commands use 4.
        println!("{}", indented(&document, b"  "));
        return Ok(exit::code(exit::SUCCESS));
    }
    iam.call(
        "update-assume-role-policy",
        Some(&json!({
            "RoleName": role_name,
            "PolicyDocument": serde_json::to_string(&document).expect("a document serializes"),
        })),
    )?;
    println!("Successfully updated trust policy of role {role_name}");
    Ok(exit::code(exit::SUCCESS))
}

/// The statement EMR on EKS needs on the role: a web-identity trust for the cluster's OIDC
/// provider, narrowed to the service accounts derived from this role's base36 name.
fn trust_statement(
    partition: &str,
    account: &str,
    provider: &str,
    namespace: &str,
    base36_role: &str,
) -> Value {
    json!({
        "Effect": "Allow",
        "Principal": {
            "Federated": format!("arn:{partition}:iam::{account}:oidc-provider/{provider}")
        },
        "Action": "sts:AssumeRoleWithWebIdentity",
        "Condition": {
            "StringLike": {
                format!("{provider}:sub"):
                    format!("system:serviceaccount:{namespace}:emr-containers-sa-*-*-{account}-{base36_role}")
            }
        }
    })
}

/// Is an equal statement already in the document?
///
/// The comparison is structural and recursive, and **length-sensitive at every level**:
/// a statement carrying an extra key does not match, so a hand-edited trust policy gets a
/// second statement rather than being left alone.
fn statement_exists(expected: &Value, document: &Value) -> bool {
    let Some(statements) = document.get("Statement").and_then(Value::as_array) else {
        return false;
    };
    statements.iter().any(|existing| matches_dict(expected, existing))
}

fn matches_dict(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
        (Value::Object(expected), Value::Object(actual)) => {
            if expected.len() != actual.len() {
                return false;
            }
            expected.iter().all(|(key, value)| match actual.get(key) {
                Some(other) => matches_dict(value, other),
                None => false,
            })
        }
        (expected, actual) => expected == actual,
    }
}

/// `create-role-associations` and `delete-role-associations`, which differ only in what
/// they do with the service-account list they both derive the same way.
fn role_associations(
    parsed: &Parsed,
    globals: &Globals,
    create: bool,
) -> Result<ExitCode, Failure> {
    let flags = [
        "--cluster-name",
        "--namespace",
        "--role-name",
        "--type",
        "--operator-namespace",
        "--service-account-name",
    ];
    let args = crate::custom::take_args(parsed, &flags)?;
    let missing: Vec<&str> = ["--cluster-name", "--namespace", "--role-name"]
        .into_iter()
        .filter(|flag| !args.contains_key(flag))
        .collect();
    if !missing.is_empty() {
        return Err(crate::custom::missing_required(&missing));
    }
    let value = |flag: &str| args.get(flag).copied().flatten().unwrap_or_default();
    let cluster_name = value("--cluster-name");
    let namespace = value("--namespace");
    let role_name = value("--role-name");
    // `parsed_args.type or "start_job_run"`, so an empty value falls back too.
    let kind = match args.get("--type").copied().flatten() {
        Some(text) if !text.is_empty() => text,
        _ => "start_job_run",
    };
    let operator_namespace = args
        .get("--operator-namespace")
        .copied()
        .flatten()
        .filter(|text| !text.is_empty())
        .unwrap_or(namespace);

    let region = crate::custom::resolve_region(globals)
        .ok_or_else(|| Failure::new(exit::CONFIGURATION, awsc_runtime::RuntimeError::NoRegion))?;
    let (account, _) = cluster_identity(globals, &region, cluster_name)?;
    let role_arn = format!(
        "arn:{}:iam::{account}:role/{role_name}",
        crate::custom::policy_partition(&region)
    );

    let pairs = match args.get("--service-account-name").copied().flatten() {
        // An explicit service account replaces the derived set entirely.
        Some(name) if !name.is_empty() => vec![(name.to_string(), namespace.to_string())],
        _ => service_accounts(kind, namespace, operator_namespace, &account, &base36(role_name))?,
    };

    let eks_globals = Globals { region: Some(region.clone()), ..globals.for_service("eks") };
    let model = crate::load_model("eks").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let eks = Client::new(&model, &eks_globals)?;

    let mut results: Vec<Value> = Vec::new();
    for (service_account_name, namespace) in &pairs {
        if create {
            match eks.call(
                "create-pod-identity-association",
                Some(&json!({
                    "clusterName": cluster_name,
                    "namespace": namespace,
                    "roleArn": role_arn,
                    "serviceAccount": service_account_name,
                })),
            ) {
                Ok(result) => {
                    results.push(result.get("association").cloned().unwrap_or(result));
                }
                // An association that is already there is reported and skipped. Anything
                // else rolls back what this run created, so a half-configured role is not
                // left behind.
                Err(failure)
                    if failure.service_error_code.as_deref() == Some("ResourceInUseException") =>
                {
                    eprintln!(
                        "Skipping pod identity association creation because pod identity \
                         association already exists for service account \
                         {service_account_name} and role {role_name} in namespace \
                         {namespace}: {}",
                        failure.service_error_message.as_deref().unwrap_or_default()
                    );
                }
                Err(failure) => {
                    for created in &results {
                        eprintln!(
                            "Rolling back association for service account \
                             {service_account_name} and role {role_name} in namespace \
                             {namespace} as an error was encountered"
                        );
                        let _ = eks.call(
                            "delete-pod-identity-association",
                            Some(&json!({
                                "clusterName": created.get("clusterName"),
                                "associationId": created.get("associationId"),
                            })),
                        );
                    }
                    return Err(Failure::new(
                        exit::GENERAL_ERROR,
                        format!(
                            "Failed to create pod identity association for service account \
                             {service_account_name}, role {role_name} in namespace \
                             {namespace}: {}",
                            failure.service_error_message.as_deref().unwrap_or(failure.message())
                        ),
                    ));
                }
            }
        } else {
            let listed = eks.call(
                "list-pod-identity-associations",
                Some(&json!({
                    "clusterName": cluster_name,
                    "namespace": namespace,
                    "serviceAccount": service_account_name,
                })),
            )?;
            let associations = listed
                .get("associations")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for association in associations {
                let id = association.get("associationId").cloned().unwrap_or(Value::Null);
                let deleted = eks.call(
                    "delete-pod-identity-association",
                    Some(&json!({ "clusterName": cluster_name, "associationId": id })),
                )?;
                results.push(deleted.get("association").cloned().unwrap_or(deleted));
            }
        }
    }

    // `if result:` — an empty list prints nothing at all. `indent=4` here, where
    // `update-role-trust-policy --dry-run` uses 2; and `uni_print` adds no newline.
    if !results.is_empty() {
        print!("{}", indented(&Value::Array(results), b"    "));
    }
    Ok(exit::code(exit::SUCCESS))
}

/// The service-account/namespace pairs for one association type.
fn service_accounts(
    kind: &str,
    namespace: &str,
    operator_namespace: &str,
    account: &str,
    base36_role: &str,
) -> Result<Vec<(String, String)>, Failure> {
    let derived = |framework: &str, components: &[&str]| -> Vec<(String, String)> {
        components
            .iter()
            .map(|component| {
                (
                    service_account(framework, component, account, base36_role),
                    namespace.to_string(),
                )
            })
            .collect()
    };
    Ok(match kind {
        "start_job_run" => derived("spark", &["client", "driver", "executor"]),
        "interactive_endpoint" => derived("spark", &["jeg", "jeg-kernel", "session"]),
        "spark_operator" => {
            let mut pairs = vec![(
                "emr-containers-sa-spark-operator".to_string(),
                operator_namespace.to_string(),
            )];
            pairs.extend(derived("spark", &["driver", "executor"]));
            pairs
        }
        "flink_operator" => {
            let mut pairs = vec![(
                "emr-containers-sa-flink-operator".to_string(),
                operator_namespace.to_string(),
            )];
            pairs.extend(derived("flink", &["jobmanager", "taskmanager"]));
            pairs
        }
        "livy" => vec![
            ("emr-containers-sa-livy".to_string(), operator_namespace.to_string()),
            // Note the singular `emr-container-` here: it is spelled that way in the
            // reference, and a "corrected" spelling would not match what Livy presents.
            ("emr-container-sa-spark-livy".to_string(), namespace.to_string()),
        ],
        other => {
            return Err(Failure::after_usage(awsc_runtime::RuntimeError::ParamValidation(
                format!(
                    "argument --type: Invalid choice, valid choices are:\n\n\
                     start_job_run | interactive_endpoint | spark_operator | \
                     flink_operator | livy\n\nGot: {other}"
                ),
            )))
        }
    })
}

/// `json.dumps(value, indent=N)`, which the two commands here disagree about: the
/// trust-policy dry run prints 2 and the association commands print 4.
fn indented(value: &Value, indent: &[u8]) -> String {
    let mut buffer = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(indent);
    let mut serializer = serde_json::Serializer::with_formatter(&mut buffer, formatter);
    serde::Serialize::serialize(value, &mut serializer).expect("a JSON value serializes");
    String::from_utf8(buffer).expect("serde_json emits UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two commands in this file print JSON at different indents, which is the sort of
    /// detail a diff catches and a reader never would.
    #[test]
    fn the_two_commands_indent_differently() {
        let value = json!({"a": {"b": 1}});
        assert_eq!(indented(&value, b"  "), "{\n  \"a\": {\n    \"b\": 1\n  }\n}");
        assert_eq!(indented(&value, b"    "), "{\n    \"a\": {\n        \"b\": 1\n    }\n}");
    }

    /// The bytes read big-endian, then re-expressed in base 36. These are the values
    /// Python's `Base36().encode` produces, which is what EMR's service-account names
    /// carry — a different encoding names accounts that will never match.
    #[test]
    fn base36_matches_pythons_big_integer_encoding() {
        // ord('A') = 65 -> "1t"
        assert_eq!(base36("A"), "1t");
        // "AB" -> 65*256 + 66 = 16706 -> "cw2"
        assert_eq!(base36("AB"), "cw2");
        assert_eq!(base36("a"), "2p");
        assert_eq!(base36(""), "0");
        // A realistic role name, checked against the reference implementation rather than
        // against arithmetic done by hand — which got the two-character case wrong first.
        assert_eq!(
            base36("MyEMRContainersExecutionRole"),
            "z8bqjd1v3fv8jqpvzzytg1i1c3ox1p1wdh15m8xtdut"
        );
    }

    #[test]
    fn the_trust_statement_names_the_provider_and_the_base36_role() {
        let statement = trust_statement("aws", "123456789012", "oidc.eks.x", "ns", "abc");
        assert_eq!(
            statement["Principal"]["Federated"],
            "arn:aws:iam::123456789012:oidc-provider/oidc.eks.x"
        );
        assert_eq!(
            statement["Condition"]["StringLike"]["oidc.eks.x:sub"],
            "system:serviceaccount:ns:emr-containers-sa-*-*-123456789012-abc"
        );
    }

    /// A statement with an extra key is not the same statement, so the command appends a
    /// second one rather than deciding it is already there.
    #[test]
    fn matching_is_length_sensitive_at_every_level() {
        let expected = json!({"a": 1, "b": {"c": 2}});
        assert!(matches_dict(&expected, &json!({"a": 1, "b": {"c": 2}})));
        assert!(!matches_dict(&expected, &json!({"a": 1, "b": {"c": 2, "d": 3}})));
        assert!(!matches_dict(&expected, &json!({"a": 1})));
    }

    #[test]
    fn an_absent_document_has_no_statement() {
        assert!(!statement_exists(&json!({}), &Value::Null));
        assert!(!statement_exists(&json!({}), &json!({"Version": "2012-10-17"})));
    }

    #[test]
    fn each_association_type_names_its_own_service_accounts() {
        let pairs = service_accounts("start_job_run", "ns", "ns", "111", "xy").expect("valid");
        assert_eq!(
            pairs.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(),
            vec![
                "emr-containers-sa-spark-client-111-xy",
                "emr-containers-sa-spark-driver-111-xy",
                "emr-containers-sa-spark-executor-111-xy"
            ]
        );

        // The operator's own account goes in the operator namespace; the pods stay in the
        // job namespace.
        let pairs = service_accounts("spark_operator", "jobs", "operators", "111", "xy")
            .expect("valid");
        assert_eq!(pairs[0], ("emr-containers-sa-spark-operator".into(), "operators".into()));
        assert_eq!(pairs[1].1, "jobs");

        // The singular `emr-container-` spelling is the reference's.
        let pairs = service_accounts("livy", "ns", "ops", "111", "xy").expect("valid");
        assert_eq!(pairs[1].0, "emr-container-sa-spark-livy");
    }

    #[test]
    fn an_unknown_type_is_refused() {
        assert!(service_accounts("bogus", "ns", "ns", "1", "x").is_err());
    }
}
