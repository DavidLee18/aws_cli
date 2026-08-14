//! Every model a custom command loads by name must actually resolve.
//!
//! `configservice subscribe` shipped briefly loading the S3 model as `"s3"`, which is the
//! *high-level command tree* and resolves to nothing — the modelled service is `s3api`.
//! That failed only on the live path, well past where any unit test looked. This walks the
//! source for `load_model("...")` so a new command cannot reintroduce it.

use std::collections::BTreeSet;

#[test]
fn every_load_model_name_resolves() {
    let mut names = BTreeSet::new();
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for entry in std::fs::read_dir(&src).expect("reading src") {
        let path = entry.expect("dir entry").path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("reading source");
        let mut rest = text.as_str();
        while let Some(start) = rest.find("load_model(\"") {
            rest = &rest[start + "load_model(\"".len()..];
            if let Some(end) = rest.find('"') {
                names.insert(rest[..end].to_string());
            }
        }
    }
    assert!(!names.is_empty(), "found no load_model call sites to check");

    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models");
    for name in &names {
        let found = std::fs::read_dir(&dir)
            .expect("models directory; run scripts/fetch-models.sh")
            .flatten()
            .any(|entry| {
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "json") {
                    return false;
                }
                std::fs::read(&path)
                    .ok()
                    .and_then(|bytes| aws_cli_model::Model::from_json(&bytes).ok())
                    .is_some_and(|model| model.cli_service_name().is_ok_and(|n| &n == name))
            });
        assert!(found, "no model resolves to the CLI service name `{name}`");
    }
}
