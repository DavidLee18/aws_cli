//! The botocore paginator overlay.
//!
//! Whether an operation paginates — and therefore receives the injected
//! `--starting-token` / `--max-items` / `--page-size` flags — is decided by botocore's
//! `paginators-1.json`, not by the Smithy `smithy.api#paginated` trait. The two dialects
//! genuinely disagree (measured at full catalogue: 349 missing + 1,455 spurious flag
//! instances when deriving from the Smithy trait alone), so the botocore data is vendored
//! as an overlay: `scripts/extract-paginators.py` -> `data/paginators.json`.
//!
//! The full paginator configs are retained verbatim, not just the fields surface
//! derivation needs today — the pagination *runtime* will consume `input_token` /
//! `output_token` / `result_key` from the same table later.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct PaginatorOverlay {
    #[serde(default)]
    pub awscli_version: String,
    /// CLI service name -> CLI operation name -> paginator config.
    services: BTreeMap<String, BTreeMap<String, Paginator>>,
}

/// One entry from a `paginators-1.json` `pagination` map, keys preserved verbatim.
#[derive(Debug, Deserialize)]
pub struct Paginator {
    /// Present iff the operation has a client-side page-size control; its presence is
    /// what gates the injected `--page-size` flag.
    pub limit_key: Option<String>,
    /// The rest of the config (`input_token`, `output_token`, `result_key`,
    /// `more_results`, `non_aggregate_keys`, ...), kept for the pagination runtime.
    #[serde(flatten)]
    pub config: BTreeMap<String, serde_json::Value>,
}

impl PaginatorOverlay {
    pub fn load(path: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        Self::from_json(&bytes).map_err(|e| format!("parsing {}: {e}", path.display()))
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    /// The paginator for a CLI operation, or `None` if it does not paginate.
    pub fn get(&self, service: &str, operation: &str) -> Option<&Paginator> {
        self.services.get(service)?.get(operation)
    }

    pub fn services(&self) -> usize {
        self.services.len()
    }

    pub fn entries(&self) -> usize {
        self.services.values().map(|s| s.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
        "awscli_version": "test",
        "services": {
            "ec2": {
                "describe-instances": {
                    "input_token": "NextToken",
                    "output_token": "NextToken",
                    "limit_key": "MaxResults",
                    "result_key": "Reservations"
                },
                "describe-reserved-instances-modifications": {
                    "input_token": "NextToken",
                    "output_token": "NextToken",
                    "result_key": "ReservedInstancesModifications"
                }
            }
        }
    }"#;

    #[test]
    fn distinguishes_limit_key_presence() {
        let overlay = PaginatorOverlay::from_json(FIXTURE.as_bytes()).unwrap();

        let with = overlay.get("ec2", "describe-instances").unwrap();
        assert_eq!(with.limit_key.as_deref(), Some("MaxResults"));

        // The case that motivated keeping limit_key optional: paginates, but with no
        // client-side page size, so the reference injects no --page-size.
        let without = overlay.get("ec2", "describe-reserved-instances-modifications").unwrap();
        assert_eq!(without.limit_key, None);
        assert_eq!(without.config["result_key"], "ReservedInstancesModifications");
    }

    #[test]
    fn non_paginated_is_none() {
        let overlay = PaginatorOverlay::from_json(FIXTURE.as_bytes()).unwrap();
        assert!(overlay.get("ec2", "run-instances").is_none());
        assert!(overlay.get("nosuch", "describe-instances").is_none());
    }
}
