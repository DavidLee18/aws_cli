//! Name transforms that must match botocore exactly.
//!
//! The CLI's command and argument names are derived from shape names by botocore's
//! `xform_name`. Any divergence here shows up as a command the reference CLI accepts and
//! ours rejects, so this is a direct port of that function rather than a fresh design.
//!
//! Reference: `botocore/utils.py::xform_name`.

use crate::shape::Traits;
use regex::Regex;
use std::sync::LazyLock;

static FIRST_CAP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(.)([A-Z][a-z]+)").unwrap());
static END_CAP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([a-z0-9])([A-Z])").unwrap());
/// Trailing runs of caps followed by a plural `s`: `ARNs`, `ACLs`, `SSEKMSKeyIds`.
static SPECIAL_CASE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Z]{2,}s$").unwrap());

/// Names botocore's regexes cannot produce, seeded into its `_xform_cache`.
///
/// Without these, `DescribeCachediSCSIVolumes` becomes `describe-cachedi-scsi-volumes`
/// instead of `describe-cached-iscsi-volumes` — a name the CLI does not accept, making
/// the operation unreachable. 38 entries covering the iSCSI, WhatsApp, PartiQL and HITs
/// families.
static XFORM_OVERRIDES: LazyLock<std::collections::HashMap<String, String>> =
    LazyLock::new(|| {
        #[derive(serde::Deserialize)]
        struct Table {
            overrides: std::collections::HashMap<String, String>,
        }
        let embedded = include_str!("../../../data/xform-cache.json");
        let parsed: Table =
            serde_json::from_str(embedded).expect("embedded data/xform-cache.json is malformed");
        parsed.overrides
    });

/// Port of botocore's `xform_name` with a configurable separator.
pub fn xform_name(name: &str, sep: &str) -> String {
    // The seeded cache wins over the regexes, as it does upstream.
    if sep == "-" {
        if let Some(seeded) = XFORM_OVERRIDES.get(name) {
            return seeded.clone();
        }
    }
    xform_name_computed(name, sep)
}

/// The two regex passes, without the seeded cache.
fn xform_name_computed(name: &str, sep: &str) -> String {
    // botocore treats a name already containing the separator as pre-transformed.
    if name.contains(sep) {
        return name.to_string();
    }

    let owned;
    let mut name = name;
    if let Some(m) = SPECIAL_CASE.find(name) {
        owned = format!("{}{}{}", &name[..m.start()], sep, m.as_str().to_lowercase());
        name = &owned;
    }

    let s1 = FIRST_CAP.replace_all(name, format!("${{1}}{sep}${{2}}"));
    END_CAP.replace_all(&s1, format!("${{1}}{sep}${{2}}")).to_lowercase()
}

/// `GetCallerIdentity` -> `get-caller-identity`.
pub fn to_cli_name(shape_name: &str) -> String {
    xform_name(shape_name, "-")
}

/// The `aws <name>` command for a service.
///
/// Resolved through the generated `sdkId` table rather than derived, because no rule
/// recovers botocore's directory name -- see [`crate::service_names`] for the measured
/// hit rates and why `endpointPrefix` in particular is wrong for 122 services.
///
/// Falls back to hyphenating `sdkId` (87.9% accurate) only for services absent from the
/// table, which means a model newer than the vendored reference. Regenerating the table
/// is the fix; the fallback just keeps such a service reachable in the meantime.
pub fn cli_service_name(service_traits: &Traits) -> String {
    let svc = service_traits.get("aws.api#service");
    let Some(sdk_id) = svc.and_then(|s| s.get("sdkId")).and_then(|p| p.as_str()) else {
        return String::new();
    };
    match crate::service_names::lookup(sdk_id) {
        Some(name) => name.to_string(),
        None => sdk_id.to_lowercase().replace(' ', "-"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ground truth captured by running `awscli.botocore.xform_name(n, '-')` against the
    /// reference install (awscli 2.36.22). These are assertions about botocore's actual
    /// behaviour, not about what a tidy transform would produce — see
    /// `reproduces_botocore_quirks` for cases where those differ.
    #[test]
    fn matches_botocore_xform() {
        let cases = [
            ("GetCallerIdentity", "get-caller-identity"),
            ("DescribeInstances", "describe-instances"),
            ("ListBuckets", "list-buckets"),
            ("PutObject", "put-object"),
            ("CreateVPC", "create-vpc"),
            ("s3", "s3"),
            ("EC2", "ec2"),
            ("ListDBInstances", "list-db-instances"),
            ("DescribeDBLogFiles", "describe-db-log-files"),
            ("GetObjectACL", "get-object-acl"),
            ("Ec2Instance", "ec2-instance"),
            ("Sha256Digest", "sha256-digest"),
        ];
        for (input, want) in cases {
            assert_eq!(to_cli_name(input), want, "xform_name({input})");
        }
    }

    #[test]
    fn handles_trailing_acronym_plurals() {
        assert_eq!(to_cli_name("ListARNs"), "list-arns");
        assert_eq!(to_cli_name("GetACLs"), "get-acls");
    }

    /// These names are unreachable without the seeded cache: the regexes produce a
    /// different spelling from the one the CLI accepts.
    #[test]
    fn applies_the_seeded_xform_cache() {
        assert_eq!(to_cli_name("DescribeCachediSCSIVolumes"), "describe-cached-iscsi-volumes");
        assert_eq!(to_cli_name("CreateStorediSCSIVolume"), "create-stored-iscsi-volume");
        assert_eq!(to_cli_name("ListHITsForQualificationType"), "list-hits-for-qualification-type");
        assert_eq!(to_cli_name("ExecutePartiQLStatement"), "execute-partiql-statement");
        assert_eq!(
            to_cli_name("AssociateWhatsAppBusinessAccount"),
            "associate-whatsapp-business-account"
        );
        // Every override genuinely differs from what the regexes compute.
        for (name, expected) in XFORM_OVERRIDES.iter() {
            assert_ne!(
                xform_name_computed(name, "-"),
                *expected,
                "{name} would not need an override"
            );
        }
    }

    /// botocore's transform is two regex passes with no acronym dictionary, so some
    /// names come out awkwardly. A drop-in replacement has to match the awkward output:
    /// `IPv6Address` really is exposed as `i-pv6-address`.
    #[test]
    fn reproduces_botocore_quirks() {
        assert_eq!(to_cli_name("IPv6Address"), "i-pv6-address");
        assert_eq!(to_cli_name("SSEKMSKeyId"), "ssekms-key-id");
    }

    #[test]
    fn passes_through_already_transformed() {
        assert_eq!(to_cli_name("get-caller-identity"), "get-caller-identity");
    }
}
