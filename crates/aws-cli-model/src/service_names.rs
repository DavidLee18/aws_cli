//! Mapping from Smithy `sdkId` to the `aws <command>` service name.
//!
//! The CLI's service command name is botocore's data-directory name, which the Smithy
//! models do not carry. Measured against the reference install, no rule recovers it:
//!
//! | rule | correct |
//! |---|---|
//! | `endpointPrefix` | 308/430 (71.6%) |
//! | `serviceId` lowercased, spaces removed | 273/430 (63.5%) |
//! | `serviceId` lowercased, spaces to hyphens | 378/430 (87.9%) |
//!
//! Guessing would rename `aws cloudwatch` to `aws monitoring` and get 122 services wrong,
//! so the mapping is generated from the reference by
//! `scripts/extract-service-names.py` and embedded here. `serviceId` is unique across all
//! 430 services and is the same value Smithy publishes as `aws.api#service.sdkId`.
//!
//! The table also captures CLI-layer renames (`S3` -> `s3api`, `CodeDeploy` -> `deploy`)
//! because the generator reads the command-table key rather than the directory name.

use serde::Deserialize;
use std::collections::HashMap;
use std::sync::LazyLock;

const EMBEDDED: &str = include_str!("../../../data/service-names.json");

#[derive(Deserialize)]
struct Table {
    services: HashMap<String, String>,
}

static TABLE: LazyLock<HashMap<String, String>> = LazyLock::new(|| {
    // A malformed embedded table is a build-time authoring error, not a runtime
    // condition -- there is no sensible way to continue without it.
    let parsed: Table =
        serde_json::from_str(EMBEDDED).expect("embedded data/service-names.json is malformed");
    parsed.services
});

/// The `aws <command>` name for a Smithy `sdkId`, or `None` if the service is not in the
/// reference CLI (a model newer than the vendored table, typically).
pub fn lookup(sdk_id: &str) -> Option<&'static str> {
    TABLE.get(sdk_id).map(|s| s.as_str())
}

/// Number of services in the embedded table.
pub fn len() -> usize {
    TABLE.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_populated() {
        assert!(len() >= 430, "expected 430+ services, got {}", len());
    }

    /// The cases that motivated the table: names no rule would produce.
    #[test]
    fn resolves_names_no_rule_would_produce() {
        // endpointPrefix for CloudWatch is `monitoring`; the command is `cloudwatch`.
        assert_eq!(lookup("CloudWatch"), Some("cloudwatch"));
        // endpointPrefix for CloudWatch Logs is `logs`, and so is the command.
        assert_eq!(lookup("CloudWatch Logs"), Some("logs"));
    }

    /// CLI-layer renames must be captured, not just directory names.
    #[test]
    fn captures_cli_renames() {
        assert_eq!(lookup("S3"), Some("s3api"));
        assert_eq!(lookup("CodeDeploy"), Some("deploy"));
    }

    #[test]
    fn unknown_service_is_none() {
        assert_eq!(lookup("Definitely Not A Service"), None);
    }
}
