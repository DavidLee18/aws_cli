//! The compiled container must describe exactly what the JSON models describe.
//!
//! Every other test in this crate builds a `Model` with [`Model::from_json`], so none of
//! them exercise the lazy container path the binary actually uses. A silent divergence
//! here would not surface as an error — it would surface as a missing operation or a
//! member that quietly stops being serialized, which is the failure mode this project
//! keeps meeting.
//!
//! Skipped when `models/models.bin` is absent, so a checkout without
//! `scripts/fetch-models.sh` + `compile-models` still runs the rest of the suite.

use aws_cli_model::shape::Shape;
use aws_cli_model::{Model, ShapeId};
use std::path::{Path, PathBuf};

fn models_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models")
}

fn container_available() -> bool {
    models_dir().join("models.bin").exists()
}

/// Every model file, paired with the CLI service name it declares.
fn json_models() -> Vec<(String, Model)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(models_dir()) else { return out };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter(|p| !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')))
        .collect();
    paths.sort();
    for path in paths {
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let Ok(model) = Model::from_json(&bytes) else { continue };
        let Ok(name) = model.cli_service_name() else { continue };
        out.push((name, model));
    }
    out
}

/// Walk everything reachable from `id` and render it, so the comparison covers the whole
/// closure a command would serialize rather than just the top-level shape.
fn closure(model: &Model, id: &ShapeId, depth: usize, out: &mut String) {
    if depth > 6 {
        return;
    }
    let Some(shape) = model.shape(id) else {
        out.push_str(&format!("{id} -> MISSING\n"));
        return;
    };
    out.push_str(&format!("{id} -> {shape:?}\n"));
    let targets: Vec<ShapeId> = match shape {
        Shape::Structure(s) | Shape::Union(s) => {
            s.members.values().map(|m| m.target.clone()).collect()
        }
        Shape::List(l) | Shape::Set(l) => vec![l.member.target.clone()],
        Shape::Map(m) => vec![m.key.target.clone(), m.value.target.clone()],
        Shape::Operation(op) => {
            let mut t = Vec::new();
            if let Some(i) = &op.input {
                t.push(i.target.clone());
            }
            if let Some(o) = &op.output {
                t.push(o.target.clone());
            }
            t
        }
        _ => Vec::new(),
    };
    for target in targets {
        closure(model, &target, depth + 1, out);
    }
}

/// Service identity and the full operation list must match for every service.
#[test]
fn every_service_matches() {
    if !container_available() {
        eprintln!("models/models.bin absent; skipping");
        return;
    }
    let mut checked = 0;
    for (cli_name, json) in json_models() {
        let Some(packed) = Model::from_container(&models_dir(), &cli_name) else {
            panic!("container is missing service `{cli_name}`");
        };
        assert_eq!(
            packed.service_id(),
            json.service_id(),
            "service id differs for `{cli_name}`",
        );

        let mut from_json: Vec<&str> = json.operation_names().collect();
        let mut from_container: Vec<&str> = packed.operation_names().collect();
        from_json.sort_unstable();
        from_container.sort_unstable();
        assert_eq!(
            from_container, from_json,
            "operation list differs for `{cli_name}`",
        );
        checked += 1;
    }
    assert!(checked > 400, "expected the full catalogue, checked only {checked}");
}

/// For a spread of services and protocols, every operation's whole reachable shape
/// closure must render identically from both stores.
#[test]
fn operation_closures_match() {
    if !container_available() {
        eprintln!("models/models.bin absent; skipping");
        return;
    }
    // One per protocol family, plus the two biggest models and a few with heavy
    // customization.
    const SAMPLE: &[&str] =
        &["s3api", "sts", "iam", "ec2", "dynamodb", "lambda", "logs", "cloudfront", "sqs", "sns"];

    for service in SAMPLE {
        let bytes = match find_model_bytes(service) {
            Some(b) => b,
            None => continue,
        };
        let json = Model::from_json(&bytes).expect("model parses");
        let packed =
            Model::from_container(&models_dir(), service).expect("container has the service");

        for name in json.operation_names() {
            let (json_id, _) = json.operation(name).expect("operation resolves from json");
            let (packed_id, _) =
                packed.operation(name).expect("operation resolves from container");
            assert_eq!(json_id, packed_id, "{service} {name}: operation id differs");

            let mut a = String::new();
            let mut b = String::new();
            closure(&json, json_id, 0, &mut a);
            closure(&packed, packed_id, 0, &mut b);
            assert_eq!(a, b, "{service} {name}: shape closure differs");
        }
    }
}

/// The raw bytes of the model declaring `cli_service`.
///
/// Walks the directory once. An earlier version called `json_models()` per service, which
/// re-parsed all 431 models each time and made this test take 107 seconds.
fn find_model_bytes(cli_service: &str) -> Option<Vec<u8>> {
    let entries = std::fs::read_dir(models_dir()).ok()?;
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let matches = Model::from_json(&bytes)
            .ok()
            .and_then(|m| m.cli_service_name().ok())
            .is_some_and(|n| n == cli_service);
        if matches {
            return Some(bytes);
        }
    }
    None
}
