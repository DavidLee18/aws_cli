//! Compile `models/*.json` into the single mapped container `models/models.bin`.
//!
//! Run after `scripts/fetch-models.sh`. This is the only place that parses the whole
//! catalogue; the CLI itself never does.
//!
//! Naming deliberately goes through [`Model::cli_service_name`] rather than being
//! re-derived here. A second implementation of the CLI-name rules is exactly the kind of
//! duplicate derivation that has broken this project before, and a mismatch would show up
//! as a service that simply cannot be found.

use aws_cli_model::db::Writer;
use aws_cli_model::Model;

fn main() -> std::process::ExitCode {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "models".to_string());
    let dir = std::path::PathBuf::from(dir);

    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("cannot read {} ({e}); run scripts/fetch-models.sh", dir.display());
            return std::process::ExitCode::FAILURE;
        }
    };

    let mut paths: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter(|p| !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')))
        .collect();
    paths.sort();

    let mut writer = Writer::new();
    let mut compiled = 0usize;
    let mut skipped: Vec<String> = Vec::new();

    for path in &paths {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                skipped.push(format!("{}: {e}", path.display()));
                continue;
            }
        };

        // Parsed twice on purpose: as a typed model for the naming and operation index,
        // and as a raw document to recover each shape's JSON verbatim. Re-serializing the
        // typed form instead would risk a round-trip that silently drops a trait we do not
        // model yet.
        let model = match Model::from_json(&bytes) {
            Ok(m) => m,
            Err(e) => {
                skipped.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        let cli_name = match model.cli_service_name() {
            Ok(n) => n,
            Err(e) => {
                skipped.push(format!("{}: {e}", path.display()));
                continue;
            }
        };

        let raw: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(e) => {
                skipped.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        let Some(shapes) = raw.get("shapes").and_then(|s| s.as_object()) else {
            skipped.push(format!("{}: no shapes object", path.display()));
            continue;
        };

        // `serde_json` is built with `preserve_order`, so re-serializing a shape keeps its
        // members in model order — which the CLI's output ordering depends on.
        let shapes: Vec<(String, String)> =
            shapes.iter().map(|(id, value)| (id.clone(), value.to_string())).collect();

        let operations: Vec<(String, String)> = model
            .operation_names()
            .map(str::to_string)
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|name| {
                let (id, _) = model.operation(&name).ok()?;
                Some((name, id.to_string()))
            })
            .collect();

        writer.add_service(cli_name, model.service_id().to_string(), shapes, operations);
        compiled += 1;
    }

    let out = writer.finish();
    let final_path = dir.join("models.bin");
    let temp_path = dir.join("models.bin.tmp");
    // Written and renamed rather than truncated in place: the reader maps this file, and a
    // half-written container is a torn read rather than a clean failure.
    if let Err(e) = std::fs::write(&temp_path, &out) {
        eprintln!("cannot write {} ({e})", temp_path.display());
        return std::process::ExitCode::FAILURE;
    }
    if let Err(e) = std::fs::rename(&temp_path, &final_path) {
        eprintln!("cannot rename into {} ({e})", final_path.display());
        return std::process::ExitCode::FAILURE;
    }

    eprintln!(
        "compiled {compiled} services into {} ({:.1} MB)",
        final_path.display(),
        out.len() as f64 / 1_048_576.0
    );
    for note in &skipped {
        eprintln!("  skipped {note}");
    }
    std::process::ExitCode::SUCCESS
}
