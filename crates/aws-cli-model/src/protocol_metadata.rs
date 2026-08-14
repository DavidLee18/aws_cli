//! Per-service protocol metadata that the Smithy models do not carry.
//!
//! Currently one field: `targetPrefix`, which awsJson protocols put in `X-Amz-Target` as
//! `{targetPrefix}.{OperationName}`.
//!
//! The Smithy models omit it entirely. The service *shape name* matches botocore for 149
//! of 152 awsJson services — but not `cloudtrail`, `codeconnections` or
//! `codestar-connections`, which use a fully-qualified prefix such as
//! `com.amazonaws.cloudtrail.v20131101.CloudTrail_20131101`. Deriving it would send those
//! three a wrong `X-Amz-Target` and fail every call, so the table is extracted by
//! `scripts/extract-protocol-metadata.py` and embedded here.

use serde::Deserialize;
use std::collections::HashMap;
use std::sync::LazyLock;

const EMBEDDED: &str = include_str!("../../../data/protocol-metadata.json");

#[derive(Deserialize)]
struct Table {
    services: HashMap<String, ServiceMetadata>,
}

#[derive(Deserialize)]
struct ServiceMetadata {
    #[serde(rename = "targetPrefix")]
    target_prefix: Option<String>,
}

static TABLE: LazyLock<Table> = LazyLock::new(|| {
    serde_json::from_str(EMBEDDED).expect("embedded data/protocol-metadata.json is malformed")
});

/// The `X-Amz-Target` prefix for a service, looked up by Smithy `sdkId`.
pub fn target_prefix(sdk_id: &str) -> Option<&'static str> {
    TABLE.services.get(sdk_id)?.target_prefix.as_deref()
}

pub fn len() -> usize {
    TABLE.services.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_populated() {
        assert!(len() >= 150, "expected 150+ services, got {}", len());
    }

    /// The common case, where the prefix happens to equal the service shape name.
    #[test]
    fn resolves_the_ordinary_prefixes() {
        assert_eq!(target_prefix("DynamoDB"), Some("DynamoDB_20120810"));
        assert_eq!(target_prefix("CloudWatch Logs"), Some("Logs_20140328"));
    }

    /// The three services that motivated vendoring this: a derived prefix would be
    /// wrong and every call would fail.
    #[test]
    fn resolves_fully_qualified_prefixes() {
        assert_eq!(
            target_prefix("CloudTrail"),
            Some("com.amazonaws.cloudtrail.v20131101.CloudTrail_20131101")
        );
        assert_eq!(
            target_prefix("CodeConnections"),
            Some("com.amazonaws.codeconnections.CodeConnections_20231201")
        );
    }

    #[test]
    fn unknown_service_is_none() {
        assert_eq!(target_prefix("Definitely Not A Service"), None);
    }
}
