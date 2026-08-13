//! Validates the endpoint-rules interpreter against AWS's own conformance suite.
//!
//! Every service model ships `smithy.rules#endpointTests` next to its ruleset: thousands
//! of cases pairing input parameters with the expected URL, auth properties or error.
//! Running all of them is a far stronger check than any test we could write, and it is
//! the reason the interpreter can be trusted across services we have never called.
//!
//! Skips cleanly when `models/` has not been fetched.

use aws_cli_runtime::rules::{value::Value, Engine, RuleSet, RulesError};
use serde_json::Value as Json;
use std::collections::BTreeMap;
use std::path::PathBuf;

fn models_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models")
}

struct Outcome {
    services: usize,
    passed: usize,
    failed: Vec<String>,
    skipped_no_ruleset: usize,
}

/// Pull the service shape's rules traits out of a model file.
fn service_traits(model: &Json) -> Option<&serde_json::Map<String, Json>> {
    model
        .get("shapes")?
        .as_object()?
        .values()
        .find(|s| s.get("type").and_then(|t| t.as_str()) == Some("service"))?
        .get("traits")?
        .as_object()
}

fn run_all() -> Outcome {
    let engine = Engine::new();
    let mut outcome =
        Outcome { services: 0, passed: 0, failed: Vec::new(), skipped_no_ruleset: 0 };

    let Ok(entries) = std::fs::read_dir(models_dir()) else { return outcome };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();

    for path in paths {
        let service = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let Ok(model): Result<Json, _> = serde_json::from_slice(&bytes) else { continue };
        let Some(traits) = service_traits(&model) else { continue };

        let (Some(ruleset_json), Some(tests)) = (
            traits.get("smithy.rules#endpointRuleSet"),
            traits.get("smithy.rules#endpointTests"),
        ) else {
            outcome.skipped_no_ruleset += 1;
            continue;
        };

        let ruleset: RuleSet = match serde_json::from_value(ruleset_json.clone()) {
            Ok(r) => r,
            Err(e) => {
                outcome.failed.push(format!("{service}: ruleset did not parse: {e}"));
                continue;
            }
        };

        let Some(cases) = tests.get("testCases").and_then(|c| c.as_array()) else { continue };
        outcome.services += 1;

        for (i, case) in cases.iter().enumerate() {
            let label = case
                .get("documentation")
                .and_then(|d| d.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| format!("case {i}"));

            let params: BTreeMap<String, Value> = case
                .get("params")
                .and_then(|p| p.as_object())
                .map(|o| o.iter().map(|(k, v)| (k.clone(), Value::from(v))).collect())
                .unwrap_or_default();

            let expect = case.get("expect");
            let got = engine.resolve(&ruleset, &params);

            match (expect.and_then(|e| e.get("endpoint")), expect.and_then(|e| e.get("error"))) {
                (Some(want), _) => match got {
                    Err(e) => outcome
                        .failed
                        .push(format!("{service} / {label}: expected an endpoint, got {e}")),
                    Ok(ep) => {
                        let want_url = want.get("url").and_then(|u| u.as_str()).unwrap_or_default();
                        if ep.url != want_url {
                            outcome.failed.push(format!(
                                "{service} / {label}: url\n     want {want_url}\n     got  {}",
                                ep.url
                            ));
                            continue;
                        }
                        if let Some(msg) = auth_mismatch(want, &ep) {
                            outcome.failed.push(format!("{service} / {label}: {msg}"));
                            continue;
                        }
                        outcome.passed += 1;
                    }
                },
                (None, Some(want_err)) => match got {
                    Ok(ep) => outcome.failed.push(format!(
                        "{service} / {label}: expected error {:?}, got endpoint {}",
                        want_err.as_str().unwrap_or_default(),
                        ep.url
                    )),
                    Err(RulesError::Rule(msg)) => {
                        let want = want_err.as_str().unwrap_or_default();
                        if msg == want {
                            outcome.passed += 1;
                        } else {
                            outcome.failed.push(format!(
                                "{service} / {label}: error text\n     want {want}\n     got  {msg}"
                            ));
                        }
                    }
                    // NoMatch is how a ruleset rejects an input that has no error rule;
                    // the suite still expects *an* error, so count it as a pass only when
                    // the expected text is empty. Otherwise report it.
                    Err(e) => outcome
                        .failed
                        .push(format!("{service} / {label}: expected a rule error, got {e}")),
                },
                (None, None) => outcome.passed += 1, // no expectation to check
            }
        }
    }
    outcome
}

/// Compare the auth properties the suite asserts, when it asserts any.
fn auth_mismatch(want: &Json, got: &aws_cli_runtime::rules::ResolvedEndpoint) -> Option<String> {
    let scheme = want
        .get("properties")?
        .get("authSchemes")?
        .as_array()?
        .first()?;

    if let Some(region) = scheme.get("signingRegion").and_then(|r| r.as_str()) {
        if got.signing_region.as_deref() != Some(region) {
            return Some(format!(
                "signingRegion want {region:?} got {:?}",
                got.signing_region
            ));
        }
    }
    if let Some(name) = scheme.get("signingName").and_then(|n| n.as_str()) {
        if got.signing_name.as_deref() != Some(name) {
            return Some(format!("signingName want {name:?} got {:?}", got.signing_name));
        }
    }
    None
}

#[test]
fn passes_aws_endpoint_conformance_suite() {
    let outcome = run_all();

    if outcome.services == 0 {
        eprintln!("no models with endpoint tests found -- run scripts/fetch-models.sh");
        return;
    }

    let total = outcome.passed + outcome.failed.len();
    eprintln!(
        "endpoint conformance: {}/{} cases across {} services ({} models had no ruleset)",
        outcome.passed, total, outcome.services, outcome.skipped_no_ruleset
    );

    // The failure list is long while the interpreter is being brought up; dumping it in
    // full makes it analysable without recompiling.
    if let Ok(path) = std::env::var("AWSC_DUMP_ENDPOINT_FAILURES") {
        std::fs::write(&path, outcome.failed.join("\n")).expect("dump path should be writable");
        eprintln!("wrote {} failures to {path}", outcome.failed.len());
    }

    if !outcome.failed.is_empty() {
        let shown = outcome.failed.iter().take(25).cloned().collect::<Vec<_>>().join("\n  ");
        panic!(
            "{} of {} endpoint test cases failed:\n  {}{}",
            outcome.failed.len(),
            total,
            shown,
            if outcome.failed.len() > 25 {
                format!("\n  ... and {} more", outcome.failed.len() - 25)
            } else {
                String::new()
            }
        );
    }
}
