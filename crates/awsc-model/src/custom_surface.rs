//! Per-service surface customizations and the botocore waiter catalogue.
//!
//! Three data classes extracted by `scripts/extract-custom-surface.py` into
//! `data/custom-surface.json`:
//!
//! - **Modeled-op argument patches** — hand-written customizations that add or remove
//!   flags on modeled operations (`ec2/secgroupsimplify.py`, `putmetricdata.py`, ...).
//!   Vendored as surface data now; each becomes real behaviour when its customization
//!   is ported.
//! - **Custom commands** — `BasicCommand`s added per service (`ecr get-login-password`,
//!   `deploy push`, ...). These never receive the injected universal flags (they build
//!   arg tables on a different event), so their argument lists are complete as stored.
//! - **Waiters** — botocore's `waiters-2.json` catalogue. Like pagination, the Smithy
//!   `smithy.waiters#waitable` dialect disagrees with botocore (Smithy models autoscaling
//!   waiters the reference CLI does not have), so waiter commands derive from this
//!   overlay, not the trait.

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug, Default, Deserialize)]
pub struct CustomSurface {
    #[serde(default)]
    pub awscli_version: String,
    /// service -> operation -> flags to add/remove after generic derivation.
    #[serde(default)]
    pub modeled_arg_patches: BTreeMap<String, BTreeMap<String, ArgPatch>>,
    /// service -> command (possibly "parent sub") -> final argument names.
    #[serde(default)]
    pub custom_commands: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    /// service -> waiter cli name -> underlying operation cli name.
    #[serde(default)]
    pub waiters: BTreeMap<String, BTreeMap<String, String>>,
    /// Modeled operations deleted because a customization replaces them
    /// (`rds modify-option-group`, parts of `emr`).
    #[serde(default)]
    pub replaced_operations: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ArgPatch {
    #[serde(default)]
    pub add: Vec<String>,
    #[serde(default)]
    pub remove: Vec<String>,
}

impl CustomSurface {
    pub fn load(path: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        serde_json::from_slice(&bytes).map_err(|e| format!("parsing {}: {e}", path.display()))
    }

    pub fn is_replaced(&self, service: &str, operation: &str) -> bool {
        self.replaced_operations
            .get(service)
            .is_some_and(|ops| ops.iter().any(|o| o == operation))
    }

    /// Apply the add/remove patch for one operation, if any.
    pub fn apply_patch(&self, service: &str, operation: &str, args: &mut BTreeSet<String>) {
        let Some(patch) = self.modeled_arg_patches.get(service).and_then(|m| m.get(operation))
        else {
            return;
        };
        for flag in &patch.remove {
            args.remove(flag);
        }
        for flag in &patch.add {
            args.insert(flag.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_applies_removes_then_adds() {
        let cs: CustomSurface = serde_json::from_str(
            r#"{
                "modeled_arg_patches": {
                    "ec2": { "run-instances": { "add": ["--count"], "remove": ["--min-count", "--max-count"] } }
                },
                "replaced_operations": { "rds": ["modify-option-group"] }
            }"#,
        )
        .unwrap();

        let mut args: BTreeSet<String> =
            ["--min-count", "--max-count", "--image-id"].iter().map(|s| s.to_string()).collect();
        cs.apply_patch("ec2", "run-instances", &mut args);
        assert!(args.contains("--count"));
        assert!(!args.contains("--min-count"));
        assert!(args.contains("--image-id"));

        // No patch -> untouched.
        let mut other = args.clone();
        cs.apply_patch("ec2", "describe-instances", &mut other);
        assert_eq!(other, args);

        assert!(cs.is_replaced("rds", "modify-option-group"));
        assert!(!cs.is_replaced("rds", "describe-db-instances"));
    }
}
