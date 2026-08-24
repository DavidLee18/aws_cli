//! The binary and the harness must agree about which commands exist.
//!
//! Four divergences were found by hand before this test existed, all the same shape: the
//! harness applied a customization table and the binary did not, so we accepted commands
//! the reference lacks and rejected ones it has. The report said "no divergences"
//! throughout, because a harness that re-derives the surface can only check its own
//! derivation.
//!
//! Both sides now build their operation names from `aws_cli_model::command_table`. This
//! asserts they really do, for every vendored model — so a future table added to one and
//! not the other fails here rather than in someone's terminal.

use aws_cli_conformance::corpus::{custom_surface_path, customizations_path, models_dir};
use aws_cli_model::custom_surface::CustomSurface;
use aws_cli_model::customizations::Customizations;
use aws_cli_model::{command_table, Model};

#[test]
fn command_table_matches_the_corpus_for_every_service() {
    let Ok(customizations) = Customizations::load(&customizations_path()) else {
        return; // no extracted data: nothing to check
    };
    let Ok(Some(corpus)) = aws_cli_conformance::corpus::Corpus::load_default() else {
        return; // corpus not extracted: nothing to check
    };
    let Ok(custom_surface) = CustomSurface::load(&custom_surface_path()) else { return };
    let Ok(entries) = std::fs::read_dir(models_dir()) else { return };

    let mut checked = 0usize;
    let mut problems: Vec<String> = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if path.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')) {
            continue;
        }
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let Ok(model) = Model::from_json(&bytes) else { continue };
        if !model.is_cli_service().unwrap_or(false) {
            continue;
        }
        let Ok(table) = command_table::build(&model, &customizations, &custom_surface) else {
            continue;
        };
        let Some(reference) = corpus.services.get(&table.service) else { continue };
        checked += 1;

        // Every name the binary accepts must be one the reference has, except where we
        // deliberately go further. The converse is not asserted here: the reference also
        // has custom commands and waiters, which this table does not model.
        for typed in table.names.keys() {
            if reference.operations.contains_key(typed) {
                continue;
            }
            // The reference drops these because it cannot read an event stream. We can,
            // so accepting them is the intended divergence rather than a regression —
            // and listing them here keeps the test catching every *unintended* one.
            if aws_cli_model::customizations::is_event_stream_operation(&table.service, typed) {
                continue;
            }
            problems.push(format!("{} accepts `{typed}`, reference does not", table.service));
        }
    }

    assert!(checked > 100, "expected to check many services, checked {checked}");
    assert!(
        problems.is_empty(),
        "binary would accept {} command(s) the reference rejects:\n{}",
        problems.len(),
        problems.iter().take(25).cloned().collect::<Vec<_>>().join("\n")
    );
}
