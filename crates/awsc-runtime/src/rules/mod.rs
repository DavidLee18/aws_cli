//! The Smithy endpoint-rules interpreter.
//!
//! Each service model carries a `smithy.rules#endpointRuleSet`: a decision tree over
//! typed parameters that yields either an endpoint (URL, headers, auth properties) or an
//! error. It is what decides global endpoints, dualstack, FIPS and per-partition DNS
//! suffixes — none of which is derivable from the service name alone.
//!
//! Models also ship `smithy.rules#endpointTests`, so the interpreter is validated
//! against AWS's own conformance suite rather than against our expectations; see
//! `tests/endpoint_rules.rs`.

pub mod functions;
pub mod partitions;
pub mod value;

use partitions::Partitions;
use serde::Deserialize;
use std::collections::BTreeMap;
use value::Value;

#[derive(Debug, thiserror::Error)]
pub enum RulesError {
    #[error("malformed endpoint ruleset: {0}")]
    Malformed(String),
    /// The ruleset itself said no — the message is AWS's own wording.
    #[error("{0}")]
    Rule(String),
    #[error("no endpoint rule matched")]
    NoMatch,
}

#[derive(Debug, Deserialize)]
pub struct RuleSet {
    #[serde(default)]
    pub parameters: BTreeMap<String, Parameter>,
    pub rules: Vec<Rule>,
}

#[derive(Debug, Deserialize)]
pub struct Parameter {
    #[serde(rename = "type")]
    pub param_type: String,
    #[serde(default)]
    pub required: bool,
    pub default: Option<serde_json::Value>,
    #[serde(rename = "builtIn")]
    pub built_in: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Rule {
    Endpoint {
        #[serde(default)]
        conditions: Vec<Condition>,
        endpoint: EndpointRule,
    },
    Error {
        #[serde(default)]
        conditions: Vec<Condition>,
        error: serde_json::Value,
    },
    Tree {
        #[serde(default)]
        conditions: Vec<Condition>,
        rules: Vec<Rule>,
    },
}

#[derive(Debug, Deserialize)]
pub struct Condition {
    pub fn_name_placeholder: Option<()>,
    #[serde(rename = "fn")]
    pub function: String,
    #[serde(default)]
    pub argv: Vec<serde_json::Value>,
    /// Binds the result into scope for later conditions and templates.
    pub assign: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct EndpointRule {
    pub url: serde_json::Value,
    #[serde(default)]
    pub properties: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub headers: BTreeMap<String, Vec<serde_json::Value>>,
}

/// A resolved endpoint.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedEndpoint {
    pub url: String,
    /// Signing overrides from `authSchemes`, when the ruleset supplies them. The signing
    /// region often differs from the client region (STS's global endpoint signs
    /// `us-east-1` while the client region is `aws-global`).
    pub signing_region: Option<String>,
    pub signing_name: Option<String>,
    pub auth_scheme: Option<String>,
    pub headers: BTreeMap<String, Vec<String>>,
}

/// Evaluation scope: parameters plus everything bound by `assign`.
struct Scope {
    bindings: BTreeMap<String, Value>,
}

impl Scope {
    fn get(&self, name: &str) -> Value {
        self.bindings.get(name).cloned().unwrap_or(Value::None)
    }
}

pub struct Engine {
    partitions: Partitions,
}

impl Default for Engine {
    fn default() -> Self {
        Engine::new()
    }
}

impl Engine {
    pub fn new() -> Self {
        Engine { partitions: Partitions::embedded() }
    }

    /// Evaluate a ruleset against caller-supplied parameters.
    ///
    /// Missing parameters take their declared default; a required parameter with neither
    /// a value nor a default is an error, mirroring the reference's own validation.
    pub fn resolve(
        &self,
        ruleset: &RuleSet,
        params: &BTreeMap<String, Value>,
    ) -> Result<ResolvedEndpoint, RulesError> {
        let mut bindings = BTreeMap::new();
        for (name, decl) in &ruleset.parameters {
            let value = match params.get(name) {
                Some(v) if v.is_set() => v.clone(),
                _ => match &decl.default {
                    Some(d) => Value::from(d),
                    None => Value::None,
                },
            };
            if decl.required && !value.is_set() {
                return Err(RulesError::Malformed(format!(
                    "required parameter `{name}` was not provided and has no default"
                )));
            }
            bindings.insert(name.clone(), value);
        }
        // Caller-supplied names not declared by the ruleset are kept: harmless, and it
        // lets tests pass through extra context.
        for (k, v) in params {
            bindings.entry(k.clone()).or_insert_with(|| v.clone());
        }

        let scope = Scope { bindings };
        self.eval_rules(&ruleset.rules, &scope)
    }

    fn eval_rules(&self, rules: &[Rule], scope: &Scope) -> Result<ResolvedEndpoint, RulesError> {
        for rule in rules {
            let (conditions, _) = match rule {
                Rule::Endpoint { conditions, .. } => (conditions, ()),
                Rule::Error { conditions, .. } => (conditions, ()),
                Rule::Tree { conditions, .. } => (conditions, ()),
            };

            // Conditions bind into a child scope: assignments are visible to later
            // conditions in the same rule and to everything nested beneath it, but must
            // not leak to sibling rules that did not match.
            let Some(child) = self.eval_conditions(conditions, scope) else { continue };

            return match rule {
                Rule::Endpoint { endpoint, .. } => self.build_endpoint(endpoint, &child),
                Rule::Error { error, .. } => {
                    Err(RulesError::Rule(self.eval_value(error, &child).to_template_string()))
                }
                Rule::Tree { rules, .. } => match self.eval_rules(rules, &child) {
                    // A tree whose own rules all fail falls through to the next sibling,
                    // rather than aborting: this is what makes the corpus's
                    // "specific cases then generic fallback" trees work.
                    Err(RulesError::NoMatch) => continue,
                    other => other,
                },
            };
        }
        Err(RulesError::NoMatch)
    }

    /// Returns the extended scope if every condition passes, else `None`.
    fn eval_conditions(&self, conditions: &[Condition], scope: &Scope) -> Option<Scope> {
        let mut bindings = scope.bindings.clone();
        for condition in conditions {
            let child = Scope { bindings: bindings.clone() };
            let args: Vec<Value> =
                condition.argv.iter().map(|a| self.eval_value(a, &child)).collect();
            let result = functions::call(&condition.function, &args, &self.partitions);

            if !result.is_truthy() {
                return None;
            }
            if let Some(name) = &condition.assign {
                bindings.insert(name.clone(), result);
            }
        }
        Some(Scope { bindings })
    }

    /// Evaluate an argv entry: a `{"ref"}`, a nested `{"fn"}`, a template string, or a
    /// literal.
    ///
    /// Plain objects and arrays are evaluated ELEMENTWISE rather than converted
    /// wholesale, because template strings appear nested inside them — an endpoint's
    /// `properties.authSchemes[0].signingRegion` is commonly
    /// `"{PartitionResult#implicitGlobalRegion}"`, and a raw conversion would hand the
    /// signer that literal text.
    fn eval_value(&self, json: &serde_json::Value, scope: &Scope) -> Value {
        match json {
            serde_json::Value::Object(map) => {
                if let Some(name) = map.get("ref").and_then(|r| r.as_str()) {
                    return scope.get(name);
                }
                if let Some(func) = map.get("fn").and_then(|f| f.as_str()) {
                    let argv = map.get("argv").and_then(|a| a.as_array()).cloned().unwrap_or_default();
                    let args: Vec<Value> =
                        argv.iter().map(|a| self.eval_value(a, scope)).collect();
                    return functions::call(func, &args, &self.partitions);
                }
                Value::Record(
                    map.iter().map(|(k, v)| (k.clone(), self.eval_value(v, scope))).collect(),
                )
            }
            serde_json::Value::Array(items) => {
                Value::Array(items.iter().map(|v| self.eval_value(v, scope)).collect())
            }
            serde_json::Value::String(s) => Value::String(self.interpolate(s, scope)),
            other => Value::from(other),
        }
    }

    /// Expand `{Name}` and `{Name#path}` templates.
    ///
    /// `{{` and `}}` are literal braces.
    fn interpolate(&self, template: &str, scope: &Scope) -> String {
        if !template.contains('{') {
            return template.to_string();
        }
        let mut out = String::with_capacity(template.len());
        let mut chars = template.chars().peekable();

        while let Some(c) = chars.next() {
            match c {
                '{' if chars.peek() == Some(&'{') => {
                    chars.next();
                    out.push('{');
                }
                '}' if chars.peek() == Some(&'}') => {
                    chars.next();
                    out.push('}');
                }
                '{' => {
                    let mut expr = String::new();
                    for c in chars.by_ref() {
                        if c == '}' {
                            break;
                        }
                        expr.push(c);
                    }
                    // `Name#a.b[0]` is a reference plus an attribute path.
                    let value = match expr.split_once('#') {
                        Some((name, path)) => scope.get(name).get_path(path),
                        None => scope.get(&expr),
                    };
                    out.push_str(&value.to_template_string());
                }
                other => out.push(other),
            }
        }
        out
    }

    fn build_endpoint(
        &self,
        rule: &EndpointRule,
        scope: &Scope,
    ) -> Result<ResolvedEndpoint, RulesError> {
        let url = self.eval_value(&rule.url, scope).to_template_string();

        // `authSchemes` is a list; the first entry is the one the SDKs use.
        let auth = rule
            .properties
            .get("authSchemes")
            .map(|v| self.eval_value(v, scope))
            .and_then(|v| match v {
                Value::Array(items) => items.into_iter().next(),
                other => Some(other),
            });

        let (signing_region, signing_name, auth_scheme) = match &auth {
            Some(a) => (
                a.get_path("signingRegion").as_str().map(str::to_string),
                a.get_path("signingName").as_str().map(str::to_string),
                a.get_path("name").as_str().map(str::to_string),
            ),
            None => (None, None, None),
        };

        let headers = rule
            .headers
            .iter()
            .map(|(k, vs)| {
                (
                    k.clone(),
                    vs.iter().map(|v| self.eval_value(v, scope).to_template_string()).collect(),
                )
            })
            .collect();

        Ok(ResolvedEndpoint { url, signing_region, signing_name, auth_scheme, headers })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ruleset(v: serde_json::Value) -> RuleSet {
        serde_json::from_value(v).unwrap()
    }

    fn params(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn interpolates_refs_and_attribute_paths() {
        let rs = ruleset(json!({
            "parameters": {"Region": {"type": "string", "required": true}},
            "rules": [{
                "type": "endpoint",
                "conditions": [
                    {"fn": "aws.partition", "argv": [{"ref": "Region"}], "assign": "P"}
                ],
                "endpoint": {"url": "https://svc.{Region}.{P#dnsSuffix}"}
            }]
        }));
        let ep = Engine::new()
            .resolve(&rs, &params(&[("Region", Value::String("cn-north-1".into()))]))
            .unwrap();
        assert_eq!(ep.url, "https://svc.cn-north-1.amazonaws.com.cn");
    }

    #[test]
    fn applies_defaults_and_rejects_missing_required() {
        let rs = ruleset(json!({
            "parameters": {
                "UseFIPS": {"type": "boolean", "required": true, "default": false},
                "Region": {"type": "string", "required": true}
            },
            "rules": [{
                "type": "endpoint",
                "conditions": [{"fn": "booleanEquals", "argv": [{"ref": "UseFIPS"}, false]}],
                "endpoint": {"url": "https://plain"}
            }]
        }));
        let engine = Engine::new();
        // UseFIPS defaults to false.
        assert!(engine
            .resolve(&rs, &params(&[("Region", Value::String("us-east-1".into()))]))
            .is_ok());
        // Region has no default.
        assert!(matches!(
            engine.resolve(&rs, &params(&[])),
            Err(RulesError::Malformed(_))
        ));
    }

    #[test]
    fn first_matching_rule_wins_and_trees_fall_through() {
        let rs = ruleset(json!({
            "parameters": {"UseFIPS": {"type": "boolean", "required": true, "default": false}},
            "rules": [
                {
                    // A tree that matches its own condition but whose children all fail:
                    // evaluation must continue to the sibling below, not abort.
                    "type": "tree",
                    "conditions": [],
                    "rules": [{
                        "type": "endpoint",
                        "conditions": [{"fn": "booleanEquals", "argv": [{"ref": "UseFIPS"}, true]}],
                        "endpoint": {"url": "https://fips"}
                    }]
                },
                {"type": "endpoint", "conditions": [], "endpoint": {"url": "https://fallback"}}
            ]
        }));
        let engine = Engine::new();
        assert_eq!(engine.resolve(&rs, &params(&[])).unwrap().url, "https://fallback");
        assert_eq!(
            engine.resolve(&rs, &params(&[("UseFIPS", Value::Bool(true))])).unwrap().url,
            "https://fips"
        );
    }

    #[test]
    fn error_rules_surface_their_message() {
        let rs = ruleset(json!({
            "parameters": {},
            "rules": [{"type": "error", "conditions": [], "error": "Invalid configuration"}]
        }));
        match Engine::new().resolve(&rs, &params(&[])) {
            Err(RulesError::Rule(m)) => assert_eq!(m, "Invalid configuration"),
            other => panic!("expected a rule error, got {other:?}"),
        }
    }

    #[test]
    fn extracts_auth_scheme_overrides() {
        let rs = ruleset(json!({
            "parameters": {},
            "rules": [{
                "type": "endpoint",
                "conditions": [],
                "endpoint": {
                    "url": "https://sts.amazonaws.com",
                    "properties": {"authSchemes": [
                        {"name": "sigv4", "signingName": "sts", "signingRegion": "us-east-1"}
                    ]}
                }
            }]
        }));
        let ep = Engine::new().resolve(&rs, &params(&[])).unwrap();
        assert_eq!(ep.signing_region.as_deref(), Some("us-east-1"));
        assert_eq!(ep.signing_name.as_deref(), Some("sts"));
        assert_eq!(ep.auth_scheme.as_deref(), Some("sigv4"));
    }

    #[test]
    fn assignments_do_not_leak_to_sibling_rules() {
        let rs = ruleset(json!({
            "parameters": {"Region": {"type": "string", "required": true}},
            "rules": [
                {
                    "type": "endpoint",
                    "conditions": [
                        {"fn": "aws.partition", "argv": [{"ref": "Region"}], "assign": "P"},
                        {"fn": "booleanEquals", "argv": [true, false]}
                    ],
                    "endpoint": {"url": "https://never"}
                },
                {
                    // P must be unbound here, so this interpolates to an empty string.
                    "type": "endpoint",
                    "conditions": [],
                    "endpoint": {"url": "https://second/{P#dnsSuffix}"}
                }
            ]
        }));
        let ep = Engine::new()
            .resolve(&rs, &params(&[("Region", Value::String("us-east-1".into()))]))
            .unwrap();
        assert_eq!(ep.url, "https://second/");
    }

    #[test]
    fn escapes_double_braces() {
        let engine = Engine::new();
        let scope = Scope { bindings: params(&[("A", Value::String("x".into()))]) };
        assert_eq!(engine.interpolate("{{literal}} {A}", &scope), "{literal} x");
    }
}
