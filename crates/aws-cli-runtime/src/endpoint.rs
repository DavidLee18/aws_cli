//! Endpoint resolution, driven by each service's Smithy endpoint ruleset.
//!
//! The ruleset is the authority: it decides global endpoints, dualstack, FIPS and
//! per-partition DNS suffixes, and it can return a signing region that differs from the
//! client region. See [`crate::rules`] for the interpreter, which passes AWS's own
//! 14,112-case conformance suite.

use crate::rules::{partitions::Partitions, value::Value, Engine, RuleSet, RulesError};
use aws_cli_model::Model;
use std::collections::BTreeMap;

/// A resolved endpoint plus the region and name to sign for.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub url: String,
    pub host: String,
    pub signing_region: String,
    pub signing_name: String,
}

#[derive(Debug, thiserror::Error)]
pub enum EndpointError {
    #[error("service model has no endpoint ruleset")]
    NoRuleSet,
    #[error("endpoint ruleset is malformed: {0}")]
    Malformed(String),
    #[error("{0}")]
    Rules(#[from] RulesError),
    /// Worded to match the reference exactly.
    #[error(
        "An error occurred (NoRegion): You must specify a region. \
         You can also configure your region by running \"aws configure\"."
    )]
    NoRegion,
}

/// Parameters the caller supplies to the ruleset.
#[derive(Debug, Default, Clone)]
pub struct EndpointParams {
    pub region: Option<String>,
    pub use_dual_stack: bool,
    pub use_fips: bool,
    /// `--endpoint-url`; the ruleset honours it via the `Endpoint` builtin.
    pub endpoint_url: Option<String>,
}

/// Resolve the endpoint for an operation.
///
/// Ruleset parameters are matched by their `builtIn` identifier rather than by name, so
/// service-specific spellings resolve correctly without a per-service table.
pub fn resolve(
    model: &Model,
    params: &EndpointParams,
) -> Result<Endpoint, EndpointError> {
    let ruleset_json = model
        .service()
        .ok()
        .and_then(|s| s.traits.get("smithy.rules#endpointRuleSet").cloned())
        .ok_or(EndpointError::NoRuleSet)?;

    let ruleset: RuleSet = serde_json::from_value(ruleset_json)
        .map_err(|e| EndpointError::Malformed(e.to_string()))?;

    // With no region configured, substitute the service's global pseudo-region if it has
    // one (`sts` -> `aws-global`), else report NoRegion — exactly botocore's rule at
    // regions.py:274-281. Doing this BEFORE evaluation matters: the ruleset itself has
    // no notion of "no region" and would just report a missing-region error.
    let partitions = Partitions::embedded();
    let region = match &params.region {
        Some(r) if !r.is_empty() => Some(r.clone()),
        _ => match partitions.partition_endpoint(&endpoint_prefix(model)) {
            Some(pseudo) => Some(pseudo.to_string()),
            None => return Err(EndpointError::NoRegion),
        },
    };

    let mut values: BTreeMap<String, Value> = BTreeMap::new();
    for (name, decl) in &ruleset.parameters {
        let value = match decl.built_in.as_deref() {
            Some("AWS::Region") => {
                region.clone().map(Value::String).unwrap_or(Value::None)
            }
            Some("AWS::UseDualStack") => Value::Bool(params.use_dual_stack),
            Some("AWS::UseFIPS") => Value::Bool(params.use_fips),
            Some("SDK::Endpoint") => params
                .endpoint_url
                .clone()
                .map(Value::String)
                .unwrap_or(Value::None),
            // Everything else (S3 path-style, STS global endpoint, account-id modes)
            // takes its declared default. Wiring those to real config is future work;
            // the defaults are what the reference uses absent explicit configuration.
            _ => Value::None,
        };
        if value.is_set() {
            values.insert(name.clone(), value);
        }
    }

    let engine = Engine::new();
    let resolved = engine.resolve(&ruleset, &values)?;

    let host = host_of(&resolved.url).unwrap_or_default();
    // The ruleset's signing region wins where it supplies one: STS's global endpoint
    // resolves to sts.amazonaws.com but signs for us-east-1, not for `aws-global`.
    let signing_region = resolved.signing_region.or(region).unwrap_or_default();
    let signing_name = resolved
        .signing_name
        .unwrap_or_else(|| default_signing_name(model));

    Ok(Endpoint {
        url: ensure_trailing_slash(&resolved.url),
        host,
        signing_region,
        signing_name,
    })
}

/// The sigv4 signing name when the ruleset does not override it.
fn default_signing_name(model: &Model) -> String {
    model
        .service()
        .ok()
        .and_then(|s| s.traits.get("aws.auth#sigv4").cloned())
        .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_string))
        .unwrap_or_else(|| endpoint_prefix(model))
}

/// The service's endpoint prefix, which is also its key in the global-service table.
fn endpoint_prefix(model: &Model) -> String {
    model
        .service()
        .ok()
        .and_then(|s| s.traits.get("aws.api#service").cloned())
        .and_then(|v| v.get("endpointPrefix").and_then(|p| p.as_str()).map(str::to_string))
        .unwrap_or_default()
}

fn ensure_trailing_slash(url: &str) -> String {
    // Only a bare authority gets a slash; a ruleset that produced a path keeps it.
    if url.split_once("://").map(|(_, rest)| rest.contains('/')).unwrap_or(false) {
        url.to_string()
    } else {
        format!("{url}/")
    }
}

fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    Some(rest.split('/').next()?.to_string())
}

/// The region to use, following botocore's precedence.
///
/// `AWS_REGION` beats `AWS_DEFAULT_REGION` (`clidriver.py:314-442`); the profile's
/// `region` key is consulted last. IMDS fallback is not implemented.
pub fn resolve_region(explicit: Option<&str>, profile_region: Option<&str>) -> Option<String> {
    explicit
        .map(str::to_string)
        .or_else(|| std::env::var("AWS_REGION").ok().filter(|s| !s.is_empty()))
        .or_else(|| std::env::var("AWS_DEFAULT_REGION").ok().filter(|s| !s.is_empty()))
        .or_else(|| profile_region.map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_host_from_url() {
        assert_eq!(
            host_of("https://sts.us-east-1.amazonaws.com/").as_deref(),
            Some("sts.us-east-1.amazonaws.com")
        );
        assert_eq!(host_of("http://localhost:4566").as_deref(), Some("localhost:4566"));
    }

    #[test]
    fn adds_trailing_slash_only_to_bare_authorities() {
        assert_eq!(ensure_trailing_slash("https://sts.amazonaws.com"), "https://sts.amazonaws.com/");
        assert_eq!(
            ensure_trailing_slash("https://s3.amazonaws.com/bucket"),
            "https://s3.amazonaws.com/bucket"
        );
    }
}
