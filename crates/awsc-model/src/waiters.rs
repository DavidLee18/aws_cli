//! botocore's waiter definitions, embedded.
//!
//! `aws <service> wait <name>` polls an operation until an acceptor matches. The Smithy
//! models this port derives from carry waiters of their own
//! (`smithy.waiters#waitable`), and they are **not** usable here: Smithy specifies
//! exponential backoff between `minDelay` and `maxDelay` with no attempt limit, where
//! botocore polls `maxAttempts` times at a fixed `delay` and then fails. A waiter derived
//! from the Smithy trait would look right and never time out, so these come from
//! botocore's `waiters-2.json` by way of `scripts/extract-waiters.py`.
//!
//! `custom_surface.waiters` remains the authority on *which* waiters the reference
//! exposes; this says how to run them.

use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::LazyLock;

#[derive(Debug, Deserialize)]
struct Table {
    waiters: BTreeMap<String, BTreeMap<String, Waiter>>,
}

#[derive(Debug, Deserialize)]
pub struct Waiter {
    /// The CLI-spelled operation to poll.
    pub operation: String,
    /// Seconds between attempts. Fixed, not backed off.
    pub delay: u64,
    /// How many times to poll before giving up.
    #[serde(rename = "maxAttempts")]
    pub max_attempts: u64,
    pub acceptors: Vec<Acceptor>,
}

#[derive(Debug, Deserialize)]
pub struct Acceptor {
    /// `success`, `failure` or `retry`.
    pub state: String,
    /// `path`, `pathAll`, `pathAny`, `status` or `error`.
    pub matcher: String,
    /// The JMESPath expression, for the three path matchers.
    #[serde(default)]
    pub argument: Option<String>,
    /// What the matcher compares against: a string for `path`/`error`, a number for
    /// `status`, and for `error` sometimes a boolean meaning "any error at all".
    pub expected: Value,
}

static EMBEDDED: LazyLock<Table> = LazyLock::new(|| {
    let text = include_str!("../data/waiters.json");
    serde_json::from_str(text).expect("embedded data/waiters.json is malformed")
});

/// One waiter, by CLI service name and CLI waiter name.
pub fn get(service: &str, waiter: &str) -> Option<&'static Waiter> {
    EMBEDDED.waiters.get(service)?.get(waiter)
}

/// Every waiter name a service has, sorted.
pub fn names(service: &str) -> Vec<&'static str> {
    EMBEDDED
        .waiters
        .get(service)
        .map(|waiters| waiters.keys().map(String::as_str).collect())
        .unwrap_or_default()
}

/// How many waiters are embedded, for the test that the data actually shipped.
pub fn count() -> usize {
    EMBEDDED.waiters.values().map(BTreeMap::len).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalogue_is_embedded_and_populated() {
        assert!(count() > 350, "expected botocore's full set, got {}", count());
    }

    /// The one `ecs deploy` needs, and a good canary: three acceptors, one success and
    /// two failures, polled every 15 seconds.
    #[test]
    fn codedeploy_deployment_successful_is_complete() {
        let waiter = get("deploy", "deployment-successful").expect("present");
        assert_eq!(waiter.operation, "get-deployment");
        assert_eq!(waiter.delay, 15);
        assert_eq!(waiter.max_attempts, 120);
        assert_eq!(waiter.acceptors.len(), 3);
        assert_eq!(waiter.acceptors[0].state, "success");
        assert_eq!(waiter.acceptors[0].argument.as_deref(), Some("deploymentInfo.status"));
    }

    /// Every waiter the reference exposes must have a definition here, or `wait` would
    /// offer a command it cannot run.
    #[test]
    fn every_waiter_in_the_surface_has_a_definition() {
        let surface = crate::surface_overlays::custom_surface();
        let mut missing = Vec::new();
        for (service, waiters) in &surface.waiters {
            for waiter in waiters.keys() {
                if get(service, waiter).is_none() {
                    missing.push(format!("{service} wait {waiter}"));
                }
            }
        }
        assert!(missing.is_empty(), "{} waiters have no definition: {missing:?}", missing.len());
    }

    #[test]
    fn an_unknown_service_or_waiter_is_none() {
        assert!(get("not-a-service", "x").is_none());
        assert!(get("ec2", "not-a-waiter").is_none());
        assert!(names("not-a-service").is_empty());
        assert!(names("ec2").contains(&"instance-running"));
    }
}
