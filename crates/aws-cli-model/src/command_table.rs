//! The command table for one service: which `aws <service> <operation>` names exist, and
//! which modelled operation each resolves to.
//!
//! This exists because the binary and the conformance harness were deriving the same
//! thing twice. Both were individually reasonable and they disagreed: the harness applied
//! the removal, rename and alias tables while the binary applied none of them, so the
//! binary accepted commands the reference does not have and rejected ones it does. The
//! report said "no divergences" throughout, because a harness that re-derives the surface
//! can only report on its own derivation.
//!
//! One implementation, consumed by both, with a test asserting they agree.

use crate::custom_surface::CustomSurface;
use crate::customizations::Customizations;
use crate::Model;
use std::collections::BTreeMap;

/// The name the user types -> the derived name the model is indexed by.
///
/// The value is the *derived* CLI name (`get-o-tel-enrichment`), not the wire name, since
/// that is what [`Model::operation`] resolves. Aliases appear as extra keys pointing at
/// the same value; renamed operations appear only under their new name, because the
/// derived spelling stops working.
pub fn build(
    model: &Model,
    customizations: &Customizations,
    custom_surface: &CustomSurface,
) -> Result<Table, String> {
    let cli_service = model.cli_service_name().map_err(|e| e.to_string())?;
    let mut names: BTreeMap<String, String> = BTreeMap::new();

    // `operation_names` already yields CLI-spelled names; the wire name comes from
    // resolving each one back through the model.
    for derived in model.operation_names().map(|s| s.to_string()).collect::<Vec<_>>() {
        // Two independent reasons a modelled operation is not a command: v2 deleted it,
        // or a customization replaced it with different ones (`rds modify-option-group`
        // becomes add-option-to/remove-option-from-option-group).
        if customizations.is_removed(&cli_service, &derived)
            || custom_surface.is_replaced(&cli_service, &derived)
        {
            continue;
        }
        let (primary, alias) = customizations.operation_names(&cli_service, &derived);
        if let Some(alias) = alias {
            names.insert(alias, derived.clone());
        }
        names.insert(primary, derived);
    }

    // Two commands that proxy an operation the customization removed. `rds
    // modify-option-group` is split in two so each half takes a single `--options`, and
    // both still invoke ModifyOptionGroup underneath.
    if cli_service == "rds" && model.operation("modify-option-group").is_ok() {
        for proxy in ["add-option-to-option-group", "remove-option-from-option-group"] {
            names.insert(proxy.to_string(), "modify-option-group".to_string());
        }
    }

    Ok(Table { service: cli_service, names })
}

#[derive(Debug, Clone)]
pub struct Table {
    pub service: String,
    pub names: BTreeMap<String, String>,
}

impl Table {
    /// The derived name a typed command resolves to, ready for `Model::operation`.
    pub fn resolve(&self, typed: &str) -> Option<&str> {
        self.names.get(typed).map(String::as_str)
    }

    pub fn contains(&self, typed: &str) -> bool {
        self.names.contains_key(typed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn customizations() -> Customizations {
        let text = include_str!("../../../data/customizations.json");
        serde_json::from_str(text).expect("customizations.json")
    }

    fn custom_surface() -> CustomSurface {
        let text = include_str!("../../../data/custom-surface.json");
        serde_json::from_str(text).expect("custom-surface.json")
    }

    fn table_for(file: &str) -> Table {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../models")
            .join(file);
        let bytes = std::fs::read(&path).expect("model should be vendored");
        let model = Model::from_json(&bytes).expect("model should parse");
        build(&model, &customizations(), &custom_surface()).expect("table should build")
    }

    /// Removed commands are absent, and their neighbours survive.
    #[test]
    fn drops_removed_commands() {
        let ses = table_for("ses.json");
        assert!(!ses.contains("delete-verified-email-address"));
        assert!(!ses.contains("verify-email-address"));
        assert!(!ses.contains("list-verified-email-addresses"));
        assert!(ses.contains("send-email"));
    }

    /// A rename REPLACES the derived name: the old spelling must stop working.
    #[test]
    fn renames_replace_the_derived_name() {
        let signin = table_for("signin.json");
        assert!(signin.contains("create-oauth2-token-with-iam"));
        assert!(!signin.contains("create-o-auth2-token-with-iam"), "derived name should be gone");
    }

    /// An alias ADDS a name: both spellings work, and both reach the same operation.
    #[test]
    fn aliases_keep_both_names() {
        let cw = table_for("cloudwatch.json");
        assert!(cw.contains("get-otel-enrichment"), "alias should resolve");
        assert!(cw.contains("get-o-tel-enrichment"), "derived name should still resolve");
        assert_eq!(cw.resolve("get-otel-enrichment"), cw.resolve("get-o-tel-enrichment"));
    }

    /// An operation a customization replaces is not a command, but the commands that
    /// replace it are — and they resolve back to it.
    #[test]
    fn drops_replaced_operations_but_keeps_their_proxies() {
        let rds = table_for("rds.json");
        assert!(!rds.contains("modify-option-group"), "replaced by two other commands");
        assert_eq!(rds.resolve("add-option-to-option-group"), Some("modify-option-group"));
        assert_eq!(rds.resolve("remove-option-from-option-group"), Some("modify-option-group"));
        assert!(rds.contains("describe-db-instances"));
    }

    /// The seeded xform cache still applies underneath.
    #[test]
    fn honours_the_seeded_name_cache() {
        let sg = table_for("storage-gateway.json");
        assert!(sg.contains("describe-cached-iscsi-volumes"));
        assert_eq!(
            sg.resolve("describe-cached-iscsi-volumes"),
            Some("describe-cached-iscsi-volumes")
        );
    }
}
