//! The partitions table behind `aws.partition`.

use super::value::Value;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::LazyLock;

const EMBEDDED_JSON: &str = include_str!("../../../../data/partitions.json");

#[derive(Debug, Deserialize)]
struct Table {
    partitions: Vec<Partition>,
    /// partition id -> service -> pseudo-region used when no region is configured.
    #[serde(default)]
    partition_endpoints: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize)]
struct Partition {
    #[allow(dead_code)]
    id: String,
    #[serde(rename = "regionRegex")]
    region_regex: String,
    regions: Vec<String>,
    outputs: BTreeMap<String, serde_json::Value>,
}

pub struct Partitions {
    table: Table,
}

static EMBEDDED: LazyLock<Table> = LazyLock::new(|| {
    serde_json::from_str(EMBEDDED_JSON).expect("embedded data/partitions.json is malformed")
});

impl Partitions {
    /// The table vendored at build time by `scripts/extract-partitions.py`.
    pub fn embedded() -> Self {
        Partitions {
            table: Table {
                partitions: EMBEDDED
                    .partitions
                    .iter()
                    .map(|p| Partition {
                        id: p.id.clone(),
                        region_regex: p.region_regex.clone(),
                        regions: p.regions.clone(),
                        outputs: p.outputs.clone(),
                    })
                    .collect(),
                partition_endpoints: EMBEDDED.partition_endpoints.clone(),
            },
        }
    }

    /// The pseudo-region botocore substitutes for a service when no region is
    /// configured, or `None` if the service is regional and must have one.
    ///
    /// This is why `sts get-caller-identity` works with no region (resolving to
    /// `aws-global`) while `ec2 describe-regions` reports NoRegion. It comes from the
    /// legacy `endpoints.json`, not from the rulesets. With no region there is no
    /// partition either, so the `aws` partition is used — the same place botocore's
    /// partition iteration lands.
    pub fn partition_endpoint(&self, service: &str) -> Option<&str> {
        self.table.partition_endpoints.get("aws")?.get(service).map(|s| s.as_str())
    }

    /// Resolve a region to its partition outputs.
    ///
    /// Exact region membership first, then the partition's `regionRegex`, then the `aws`
    /// partition as the fallback — an unknown region is never an error, which is what
    /// lets rulesets handle new regions before the table catches up.
    pub fn resolve(&self, region: &str) -> Value {
        for p in &self.table.partitions {
            if p.regions.iter().any(|r| r == region) {
                return outputs_to_value(p);
            }
        }
        for p in &self.table.partitions {
            if region_matches(&p.region_regex, region) {
                return outputs_to_value(p);
            }
        }
        match self.table.partitions.iter().find(|p| p.outputs.get("name").and_then(|n| n.as_str()) == Some("aws")) {
            Some(p) => outputs_to_value(p),
            None => Value::None,
        }
    }
}

fn outputs_to_value(p: &Partition) -> Value {
    Value::Record(
        p.outputs.iter().map(|(k, v)| (k.clone(), Value::from(v))).collect(),
    )
}

/// Match a region against a partition's `regionRegex` without a regex engine.
///
/// The corpus uses exactly one shape: `^(alt1|alt2|...)\-\w+\-\d+$`, sometimes with a
/// literal prefix (`^us\-iso\-\w+\-\d+$`). Rather than depend on a regex crate in the
/// runtime, that grammar is matched directly; anything unrecognised returns false and
/// falls through to the `aws` default, which is the same outcome the reference reaches
/// for an unknown region.
fn region_matches(pattern: &str, region: &str) -> bool {
    let body = pattern.trim_start_matches('^').trim_end_matches('$');
    let unescaped = body.replace("\\-", "-");

    let Some(rest) = unescaped.strip_suffix("-\\w+-\\d+") else { return false };

    // The head is either an alternation group or a literal prefix.
    let prefixes: Vec<String> = if let Some(group) = rest.strip_prefix('(').and_then(|g| g.strip_suffix(')')) {
        group.split('|').map(str::to_string).collect()
    } else {
        vec![rest.to_string()]
    };

    for prefix in prefixes {
        let Some(tail) = region.strip_prefix(&prefix) else { continue };
        let Some(tail) = tail.strip_prefix('-') else { continue };
        // Remaining must be `\w+-\d+`. `\w` is [A-Za-z0-9_] and notably EXCLUDES `-`,
        // which is what keeps the broad `aws` pattern from swallowing `us-iso-west-7`
        // before the narrower `aws-iso` pattern gets a chance.
        let Some((word, digits)) = tail.rsplit_once('-') else { continue };
        if !word.is_empty()
            && word.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            && !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_known_regions_exactly() {
        let p = Partitions::embedded();
        let aws = p.resolve("us-east-1");
        assert_eq!(aws.get_path("name").as_str(), Some("aws"));
        assert_eq!(aws.get_path("dnsSuffix").as_str(), Some("amazonaws.com"));
        assert_eq!(aws.get_path("implicitGlobalRegion").as_str(), Some("us-east-1"));

        assert_eq!(p.resolve("cn-north-1").get_path("name").as_str(), Some("aws-cn"));
        assert_eq!(
            p.resolve("cn-north-1").get_path("dnsSuffix").as_str(),
            Some("amazonaws.com.cn")
        );
        assert_eq!(p.resolve("us-gov-west-1").get_path("name").as_str(), Some("aws-us-gov"));
    }

    /// `aws-global` is a real entry in the region list, which is how STS's global
    /// endpoint resolves.
    #[test]
    fn resolves_pseudo_regions() {
        let p = Partitions::embedded();
        assert_eq!(p.resolve("aws-global").get_path("name").as_str(), Some("aws"));
        assert_eq!(p.resolve("aws-cn-global").get_path("name").as_str(), Some("aws-cn"));
    }

    /// A region absent from the table must still land in the right partition via regex.
    #[test]
    fn resolves_unlisted_regions_by_regex() {
        let p = Partitions::embedded();
        assert_eq!(p.resolve("eu-west-99").get_path("name").as_str(), Some("aws"));
        assert_eq!(p.resolve("cn-south-9").get_path("name").as_str(), Some("aws-cn"));
        assert_eq!(p.resolve("us-iso-west-7").get_path("name").as_str(), Some("aws-iso"));
    }

    #[test]
    fn unknown_regions_fall_back_to_aws() {
        let p = Partitions::embedded();
        assert_eq!(p.resolve("not-a-region").get_path("name").as_str(), Some("aws"));
        assert_eq!(p.resolve("").get_path("name").as_str(), Some("aws"));
    }

    /// The global-service table is what makes a region optional for STS but required
    /// for EC2 — the distinction the reference draws.
    #[test]
    fn knows_which_services_have_a_global_endpoint() {
        let p = Partitions::embedded();
        assert_eq!(p.partition_endpoint("sts"), Some("aws-global"));
        assert_eq!(p.partition_endpoint("iam"), Some("aws-global"));
        assert_eq!(p.partition_endpoint("route53"), Some("aws-global"));
        assert_eq!(p.partition_endpoint("ec2"), None);
        assert_eq!(p.partition_endpoint("dynamodb"), None);
    }

    #[test]
    fn region_regex_matcher_handles_corpus_shapes() {
        assert!(region_matches(r"^(us|eu|ap)\-\w+\-\d+$", "us-east-1"));
        assert!(region_matches(r"^(us|eu|ap)\-\w+\-\d+$", "eu-central-2"));
        assert!(!region_matches(r"^(us|eu|ap)\-\w+\-\d+$", "cn-north-1"));
        assert!(region_matches(r"^us\-iso\-\w+\-\d+$", "us-iso-east-1"));
        assert!(!region_matches(r"^us\-iso\-\w+\-\d+$", "us-east-1"));
        // Missing the digit suffix.
        assert!(!region_matches(r"^(us)\-\w+\-\d+$", "us-east"));
        // `\w+` excludes `-`, so the broad aws pattern must NOT match an iso region --
        // otherwise partition lookup order silently decides the answer.
        assert!(!region_matches(r"^(us|eu|ap)\-\w+\-\d+$", "us-iso-west-7"));
        assert!(!region_matches(r"^(us|eu|ap)\-\w+\-\d+$", "us-gov-east-1"));
    }
}
