//! Pure-data surface customizations extracted from the reference CLI.
//!
//! The reference applies hand-written customizations that add, remove, rename and alias
//! commands and arguments (`customizations/argrename.py`, `removals.py`, `signin.py`,
//! ...). The purely tabular ones are extracted verbatim by
//! `scripts/extract-customizations.py` into `data/customizations.json` and applied as
//! data here; behavioural customizations (the `s3` tree, `--zip-file` hoisting, ...)
//! are ported as code separately.
//!
//! Ordering matters: argument renames are applied to *final* CLI names — after
//! `--no-` boolean expansion — because several rules rename the generated negative form
//! itself (`ec2.create-image.no-no-reboot` -> `reboot`).

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Default, Deserialize)]
pub struct Customizations {
    #[serde(default)]
    pub awscli_version: String,
    /// `<service>.<operation>.<old-cli-arg>` -> new arg name. `*` wildcards service or
    /// operation. The old name is REMOVED (hard rename).
    #[serde(default)]
    pub argument_renames: BTreeMap<String, String>,
    /// `<service>.<operation>.<existing-arg>` -> extra alias that also parses. Both
    /// names live in the arg table; the alias is hidden from help only.
    #[serde(default)]
    pub hidden_argument_aliases: BTreeMap<String, String>,
    /// CLI service -> operations deleted from the command table.
    #[serde(default)]
    pub removed_operations: BTreeMap<String, Vec<String>>,
    /// CLI service -> old op -> new op (old gone).
    #[serde(default)]
    pub operation_renames: BTreeMap<String, BTreeMap<String, String>>,
    /// CLI service -> old op -> new op (both parse; old hidden from help).
    #[serde(default)]
    pub operation_aliases: BTreeMap<String, BTreeMap<String, String>>,
    #[serde(default)]
    pub agent_toolkit: AgentToolkit,
}

/// `agent-toolkit` deletes every modeled command NOT in its allowlist, then renames the
/// survivors (`customizations/agenttoolkit/__init__.py`).
#[derive(Debug, Default, Deserialize)]
pub struct AgentToolkit {
    #[serde(default)]
    pub modeled_allowlist: Vec<String>,
    #[serde(default)]
    pub renames: BTreeMap<String, String>,
}

/// One parsed `service.operation.argument` rule key.
struct RuleKey<'a> {
    service: &'a str,
    operation: &'a str,
    argument: &'a str,
}

impl<'a> RuleKey<'a> {
    /// Keys split cleanly on `.` because service, operation and argument names never
    /// contain dots themselves.
    fn parse(key: &'a str) -> Option<Self> {
        let mut parts = key.splitn(3, '.');
        Some(Self {
            service: parts.next()?,
            operation: parts.next()?,
            argument: parts.next()?,
        })
    }

    fn matches(&self, service: &str, operation: &str) -> bool {
        (self.service == "*" || self.service == service)
            && (self.operation == "*" || self.operation == operation)
    }
}

impl Customizations {
    pub fn load(path: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        serde_json::from_slice(&bytes).map_err(|e| format!("parsing {}: {e}", path.display()))
    }
}

/// Operations the reference drops only because it cannot read an event stream.
///
/// `data/customizations.json` is a faithful record of what botocore does, and stays that
/// way so a refresh from upstream does not have to be hand-edited. This is the list of
/// removals we deliberately do not honour: the CLI decodes `vnd.amazon.eventstream`, so
/// there is no reason to hide the operations that use it.
///
/// Operations with an event stream on the *input* side stay removed. Those are duplex and
/// need SigV4's rolling per-frame signature, which the blocking transport cannot do; a
/// command that connects and then cannot send would be worse than one that is absent.
pub(crate) const EVENT_STREAM_OPERATIONS: &[(&str, &str)] = &[
    ("bedrock-agent-runtime", "agentic-retrieve-stream"),
    ("bedrock-agent-runtime", "invoke-agent"),
    ("bedrock-agent-runtime", "invoke-flow"),
    ("bedrock-agent-runtime", "invoke-inline-agent"),
    ("bedrock-agent-runtime", "optimize-prompt"),
    ("bedrock-agent-runtime", "retrieve-and-generate-stream"),
    ("bedrock-agentcore", "invoke-agent-runtime-command"),
    ("bedrock-agentcore", "invoke-code-interpreter"),
    ("bedrock-agentcore", "invoke-harness"),
    ("bedrock-runtime", "converse-stream"),
    ("bedrock-runtime", "invoke-model-with-response-stream"),
    ("devops-agent", "send-message"),
    ("iotsitewise", "invoke-assistant"),
    ("kinesis", "subscribe-to-shard"),
    ("lambda", "invoke-with-response-stream"),
    ("logs", "get-log-object"),
    ("sagemaker-runtime", "invoke-endpoint-with-response-stream"),
];

/// Whether this removal is one we deliberately do not honour.
pub fn is_event_stream_operation(service: &str, operation: &str) -> bool {
    EVENT_STREAM_OPERATIONS.contains(&(service, operation))
}

impl Customizations {
    /// Is this operation deleted from the command table?
    pub fn is_removed(&self, service: &str, operation: &str) -> bool {
        if is_event_stream_operation(service, operation) {
            return false;
        }
        if let Some(ops) = self.removed_operations.get(service) {
            if ops.iter().any(|o| o == operation) {
                return true;
            }
        }
        // agent-toolkit's allowlist inverts the logic: modeled ops NOT listed are gone.
        if service == "agent-toolkit"
            && !self.agent_toolkit.modeled_allowlist.is_empty()
            && !self.agent_toolkit.modeled_allowlist.iter().any(|o| o == operation)
        {
            return true;
        }
        false
    }

    /// The final name(s) an operation is reachable by: hard renames replace the name,
    /// aliases add a second one. Returns (primary_name, extra_alias).
    pub fn operation_names(
        &self,
        service: &str,
        operation: &str,
    ) -> (String, Option<String>) {
        if service == "agent-toolkit" {
            if let Some(new) = self.agent_toolkit.renames.get(operation) {
                return (new.clone(), None);
            }
        }
        if let Some(new) = self.operation_renames.get(service).and_then(|m| m.get(operation)) {
            return (new.clone(), None);
        }
        if let Some(new) = self.operation_aliases.get(service).and_then(|m| m.get(operation)) {
            // Old name still parses; the alias becomes the documented form.
            return (operation.to_string(), Some(new.clone()));
        }
        (operation.to_string(), None)
    }

    /// Apply argument renames and hidden aliases to a finished argument set.
    ///
    /// `args` holds final CLI flag names (`--foo`). Renames drop the old flag and insert
    /// the new; aliases insert an extra flag alongside the existing one.
    pub fn apply_argument_rules(
        &self,
        service: &str,
        operation: &str,
        args: &mut std::collections::BTreeSet<String>,
    ) {
        for (key, new_name) in &self.argument_renames {
            let Some(rule) = RuleKey::parse(key) else { continue };
            if !rule.matches(service, operation) {
                continue;
            }
            let old_flag = format!("--{}", rule.argument);
            if args.remove(&old_flag) {
                args.insert(format!("--{new_name}"));
            }
        }
        for (key, alias) in &self.hidden_argument_aliases {
            let Some(rule) = RuleKey::parse(key) else { continue };
            if !rule.matches(service, operation) {
                continue;
            }
            if args.contains(&format!("--{}", rule.argument)) {
                args.insert(format!("--{alias}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn fixture() -> Customizations {
        serde_json::from_str(
            r#"{
                "argument_renames": {
                    "ec2.create-image.no-no-reboot": "reboot",
                    "ec2.*.no-egress": "ingress",
                    "eks.create-cluster.version": "kubernetes-version"
                },
                "hidden_argument_aliases": {
                    "mgn.*.source-server-ids": "source-server-i-ds"
                },
                "removed_operations": { "ec2": ["import-instance", "import-volume"] },
                "operation_renames": { "signin": { "create-o-auth2-token-with-iam": "create-oauth2-token-with-iam" } },
                "operation_aliases": { "signin": { "create-o-auth2-token": "create-oauth2-token" } }
            }"#,
        )
        .unwrap()
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn renames_drop_old_and_add_new() {
        let c = fixture();
        let mut args = set(&["--no-reboot", "--no-no-reboot", "--name"]);
        c.apply_argument_rules("ec2", "create-image", &mut args);
        assert!(args.contains("--reboot"), "renamed negative form");
        assert!(!args.contains("--no-no-reboot"), "old name gone");
        assert!(args.contains("--no-reboot"), "unrelated arg untouched");
    }

    #[test]
    fn wildcard_operation_matches_everywhere() {
        let c = fixture();
        for op in ["create-network-acl-entry", "delete-network-acl-entry"] {
            let mut args = set(&["--egress", "--no-egress"]);
            c.apply_argument_rules("ec2", op, &mut args);
            assert!(args.contains("--ingress"), "{op}");
            assert!(!args.contains("--no-egress"), "{op}");
        }
        // ...but not on other services.
        let mut args = set(&["--no-egress"]);
        c.apply_argument_rules("s3api", "put-thing", &mut args);
        assert!(args.contains("--no-egress"));
    }

    #[test]
    fn aliases_keep_both_names() {
        let c = fixture();
        let mut args = set(&["--source-server-ids"]);
        c.apply_argument_rules("mgn", "describe-source-servers", &mut args);
        assert!(args.contains("--source-server-ids"));
        assert!(args.contains("--source-server-i-ds"));
    }

    #[test]
    fn alias_requires_the_base_arg_to_exist() {
        let c = fixture();
        let mut args = set(&["--other"]);
        c.apply_argument_rules("mgn", "some-op", &mut args);
        assert!(!args.contains("--source-server-i-ds"));
    }

    #[test]
    fn removals_and_operation_names() {
        let c = fixture();
        assert!(c.is_removed("ec2", "import-instance"));
        assert!(!c.is_removed("ec2", "describe-instances"));

        let (name, alias) = c.operation_names("signin", "create-o-auth2-token");
        assert_eq!(name, "create-o-auth2-token");
        assert_eq!(alias.as_deref(), Some("create-oauth2-token"));

        let (name, alias) = c.operation_names("signin", "create-o-auth2-token-with-iam");
        assert_eq!(name, "create-oauth2-token-with-iam");
        assert_eq!(alias, None);
    }
}
