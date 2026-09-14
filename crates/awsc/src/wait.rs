//! `aws <service> wait <name>`: poll an operation until an acceptor matches.
//!
//! 381 of these exist across 71 services and none of them worked here before — every
//! `wait` reported `Found invalid choice 'wait'`, which is a lot of scripts to break for
//! a command family that is mostly a loop.
//!
//! The loop is botocore's, and the details that matter are the ones a reimplementation
//! gets wrong by being reasonable:
//!
//! - **The delay is fixed, not backed off.** `delay` seconds between attempts, at most
//!   `maxAttempts` of them, so the total wait is exactly the product. Exponential backoff
//!   would silently change every documented timeout.
//! - **The first attempt happens immediately**, and the delay is paid *between* attempts,
//!   so a resource that is already ready returns without waiting at all.
//! - **An error is not necessarily a failure.** Half of these waiters exist to wait for
//!   something to appear, so `error` acceptors turn a specific error code into `retry`
//!   or `success`; an error matching nothing is what actually fails.
//! - **A failure state exits 255**, not 254, because botocore raises `WaiterError` rather
//!   than reporting the service's error.

use crate::client::Client;
use crate::exit;
use crate::Failure;
use awsc_model::waiters::{Acceptor, Waiter};
use serde_json::Value;
use std::process::ExitCode;

/// What one poll produced.
enum Outcome {
    Response(Value),
    ServiceError { code: String, status: Option<u16>, failure: Failure },
}

/// Run `waiter`, polling `operation` with `input`.
pub fn run(
    client: &Client<'_>,
    waiter: &Waiter,
    waiter_name: &str,
    input: Option<&Value>,
) -> Result<ExitCode, Failure> {
    let mut attempt = 0u64;
    loop {
        attempt += 1;
        let outcome = match client.call(&waiter.operation, input) {
            Ok(response) => Outcome::Response(response),
            Err(failure) => match failure.service_error_code.clone() {
                Some(code) => {
                    Outcome::ServiceError { code, status: failure.http_status, failure }
                }
                // A transport or configuration problem is not something a waiter can wait
                // out: it is reported as itself.
                None => return Err(failure),
            },
        };

        match decide(&waiter.acceptors, &outcome)? {
            Some(State::Success) => return Ok(exit::code(exit::SUCCESS)),
            Some(State::Failure) => {
                return Err(Failure::new(
                    exit::GENERAL_ERROR,
                    format!(
                        "Waiter {} failed: Waiter encountered a terminal failure state",
                        waiter_cli_name(waiter_name)
                    ),
                ))
            }
            // No acceptor matched, or one said retry.
            Some(State::Retry) | None => {}
        }

        // An unmatched *error* is the error. Only once no acceptor claimed it, because
        // `error` acceptors are how "not there yet" is spelled.
        if let Outcome::ServiceError { failure, .. } = outcome {
            return Err(failure);
        }

        if attempt >= waiter.max_attempts {
            return Err(Failure::new(
                exit::GENERAL_ERROR,
                format!(
                    "Waiter {} failed: Max attempts exceeded",
                    waiter_cli_name(waiter_name)
                ),
            ));
        }
        std::thread::sleep(std::time::Duration::from_secs(waiter.delay));
    }
}

/// botocore names the waiter in its error with the API spelling (`InstanceRunning`), not
/// the CLI's (`instance-running`).
fn waiter_cli_name(cli_name: &str) -> String {
    cli_name
        .split('-')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum State {
    Success,
    Failure,
    Retry,
}

/// The first acceptor that matches wins; `None` means none did.
fn decide(acceptors: &[Acceptor], outcome: &Outcome) -> Result<Option<State>, Failure> {
    for acceptor in acceptors {
        if matches(acceptor, outcome)? {
            return Ok(Some(match acceptor.state.as_str() {
                "success" => State::Success,
                "failure" => State::Failure,
                _ => State::Retry,
            }));
        }
    }
    Ok(None)
}

fn matches(acceptor: &Acceptor, outcome: &Outcome) -> Result<bool, Failure> {
    match acceptor.matcher.as_str() {
        "error" => Ok(match outcome {
            Outcome::ServiceError { code, .. } => match &acceptor.expected {
                // `"expected": true` means *any* error, which is how "it is gone" is
                // spelled for a waiter that polls a delete.
                Value::Bool(any) => *any,
                Value::String(expected) => code == expected,
                _ => false,
            },
            // `"expected": false` matches a *successful* response.
            Outcome::Response(_) => acceptor.expected == Value::Bool(false),
        }),
        "status" => {
            let expected = acceptor.expected.as_i64().unwrap_or(-1);
            Ok(match outcome {
                Outcome::ServiceError { status, .. } => {
                    status.map(i64::from) == Some(expected)
                }
                // A response that reached us was a 2xx; the status acceptors that care
                // about success all name 200.
                Outcome::Response(_) => expected == 200,
            })
        }
        matcher @ ("path" | "pathAll" | "pathAny") => {
            let Outcome::Response(response) = outcome else { return Ok(false) };
            let Some(expression) = acceptor.argument.as_deref() else { return Ok(false) };
            let selected = awsc_output::query::apply(response, expression)
                .map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;
            Ok(match matcher {
                "path" => selected == acceptor.expected,
                // An empty list matches neither: `all` over nothing is vacuously true in
                // Rust and *false* in botocore, which requires at least one element.
                "pathAll" => match selected.as_array() {
                    Some(items) => {
                        !items.is_empty() && items.iter().all(|item| *item == acceptor.expected)
                    }
                    None => false,
                },
                _ => match selected.as_array() {
                    Some(items) => items.contains(&acceptor.expected),
                    None => false,
                },
            })
        }
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn acceptor(state: &str, matcher: &str, argument: Option<&str>, expected: Value) -> Acceptor {
        // The struct is deserialize-only, so build it the way the data does.
        serde_json::from_value(json!({
            "state": state,
            "matcher": matcher,
            "argument": argument,
            "expected": expected,
        }))
        .expect("an acceptor")
    }

    fn response(value: Value) -> Outcome {
        Outcome::Response(value)
    }

    fn error(code: &str, status: Option<u16>) -> Outcome {
        Outcome::ServiceError {
            code: code.to_string(),
            status,
            failure: Failure::new(exit::CLIENT_ERROR, "boom"),
        }
    }

    #[test]
    fn a_path_matcher_compares_the_selected_value() {
        let a = acceptor("success", "path", Some("deploymentInfo.status"), json!("Succeeded"));
        assert!(matches(&a, &response(json!({"deploymentInfo": {"status": "Succeeded"}}))).unwrap());
        assert!(!matches(&a, &response(json!({"deploymentInfo": {"status": "Failed"}}))).unwrap());
    }

    /// `pathAll` over an empty list is **false**, not vacuously true — a fleet with no
    /// instances is not a fleet of running instances.
    #[test]
    fn path_all_requires_at_least_one_element() {
        let a = acceptor("success", "pathAll", Some("Instances[].State"), json!("running"));
        assert!(matches(&a, &response(json!({"Instances": []}))).is_ok_and(|m| !m));
        assert!(matches(
            &a,
            &response(json!({"Instances": [{"State": "running"}, {"State": "running"}]}))
        )
        .unwrap());
        assert!(!matches(
            &a,
            &response(json!({"Instances": [{"State": "running"}, {"State": "pending"}]}))
        )
        .unwrap());
    }

    #[test]
    fn path_any_needs_only_one() {
        let a = acceptor("failure", "pathAny", Some("Instances[].State"), json!("terminated"));
        assert!(matches(
            &a,
            &response(json!({"Instances": [{"State": "running"}, {"State": "terminated"}]}))
        )
        .unwrap());
    }

    /// `"expected": true` on an error matcher means any error at all, which is how a
    /// "does not exist" waiter spells success.
    #[test]
    fn an_error_matcher_can_accept_any_error() {
        let any = acceptor("success", "error", None, json!(true));
        assert!(matches(&any, &error("NoSuchBucket", Some(404))).unwrap());
        assert!(!matches(&any, &response(json!({}))).unwrap());

        let named = acceptor("retry", "error", None, json!("InvalidVpcID.NotFound"));
        assert!(matches(&named, &error("InvalidVpcID.NotFound", Some(400))).unwrap());
        assert!(!matches(&named, &error("AccessDenied", Some(403))).unwrap());

        // `false` matches a successful response, not an error.
        let none = acceptor("retry", "error", None, json!(false));
        assert!(matches(&none, &response(json!({}))).unwrap());
        assert!(!matches(&none, &error("X", None)).unwrap());
    }

    #[test]
    fn a_status_matcher_reads_the_http_status() {
        let gone = acceptor("success", "status", None, json!(404));
        assert!(matches(&gone, &error("NotFound", Some(404))).unwrap());
        assert!(!matches(&gone, &error("Denied", Some(403))).unwrap());

        let ok = acceptor("success", "status", None, json!(200));
        assert!(matches(&ok, &response(json!({}))).unwrap());
    }

    /// The first matching acceptor decides, in order.
    #[test]
    fn the_first_match_wins() {
        let acceptors = vec![
            acceptor("failure", "path", Some("s"), json!("bad")),
            acceptor("success", "path", Some("s"), json!("good")),
        ];
        assert_eq!(
            decide(&acceptors, &response(json!({"s": "good"}))).unwrap(),
            Some(State::Success)
        );
        assert_eq!(
            decide(&acceptors, &response(json!({"s": "bad"}))).unwrap(),
            Some(State::Failure)
        );
        assert_eq!(decide(&acceptors, &response(json!({"s": "other"}))).unwrap(), None);
    }

    #[test]
    fn the_error_names_the_waiter_the_way_botocore_does() {
        assert_eq!(waiter_cli_name("instance-running"), "InstanceRunning");
        assert_eq!(waiter_cli_name("deployment-successful"), "DeploymentSuccessful");
    }
}
