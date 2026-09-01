//! Prints the conformance divergence report.
//!
//! `cargo run -p aws-cli-conformance` — the day-to-day view of how far the engine is
//! from the reference. Exits non-zero when divergences exist, so CI can gate on it.

use aws_cli_conformance::{
    corpus::{custom_surface_path, customizations_path, models_dir, paginators_path},
    Corpus, Report, Surface,
};
use aws_cli_model::custom_surface::CustomSurface;
use aws_cli_model::customizations::Customizations;
use aws_cli_model::paginators::PaginatorOverlay;

const MAX_LISTED: usize = 12;

fn main() -> std::process::ExitCode {
    let corpus = match Corpus::load_default() {
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::FAILURE;
        }
        Ok(None) => {
            eprintln!(
                "no corpus at {}\nrun: scripts/extract-reference-surface.py",
                Corpus::default_path().display()
            );
            return std::process::ExitCode::FAILURE;
        }
        Ok(Some(c)) => c,
    };

    let paginators = match PaginatorOverlay::load(&paginators_path()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error loading paginator overlay: {e}\nrun: scripts/extract-paginators.py");
            return std::process::ExitCode::FAILURE;
        }
    };

    let customizations = match Customizations::load(&customizations_path()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "error loading customization tables: {e}\nrun: scripts/extract-customizations.py"
            );
            return std::process::ExitCode::FAILURE;
        }
    };

    let custom_surface = match CustomSurface::load(&custom_surface_path()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "error loading custom-surface data: {e}\nrun: scripts/extract-custom-surface.py"
            );
            return std::process::ExitCode::FAILURE;
        }
    };

    let surface = match Surface::from_models_dir(
        &models_dir(),
        &paginators,
        &customizations,
        &custom_surface,
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error deriving surface: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    if surface.services.is_empty() {
        eprintln!(
            "no models in {} -- run: scripts/fetch-models.sh",
            models_dir().display()
        );
        return std::process::ExitCode::FAILURE;
    }

    println!("reference: awscli {}", corpus.awscli_version);
    println!(
        "overlay:   {} paginated operations across {} services (awscli {})",
        paginators.entries(),
        paginators.services(),
        paginators.awscli_version
    );
    println!(
        "corpus:    {} services, {} operations, {} arguments",
        corpus.services.len(),
        corpus.total_operations(),
        corpus.total_arguments()
    );
    println!("vendored:  {} models\n", surface.services.len());

    for (file, err) in &surface.load_errors {
        println!("  MODEL FAILED  {file}: {err}");
    }
    if !surface.excluded.is_empty() {
        println!(
            "excluded (modelled in aws-sdk-rust, not shipped by the CLI): {}\n",
            surface.excluded.join(" ")
        );
    }

    let report = Report::compute(&corpus, &surface);

    if !report.services_unexpected.is_empty() {
        println!("service names we derive that the reference does not have:");
        for name in &report.services_unexpected {
            let file = surface.services[name].model_file.as_str();
            println!("  {name:24} (from {file}.json)");
        }
        println!();
    }

    for svc in &report.compared {
        let status = if svc.is_clean() { "OK  " } else { "DIFF" };
        println!(
            "{status} {:20} {} ops matched, {} exact-arg",
            svc.service, svc.operations_matched, svc.operations_args_exact
        );

        print_list("      missing ops", &svc.operations_missing);
        print_list("      extra ops", &svc.operations_unexpected);
        print_list("      extra by design (event streams)", &svc.operations_extra_by_design);

        for ad in svc.arg_diffs.iter().take(MAX_LISTED) {
            println!("      {} :", ad.operation);
            print_list("          missing", &ad.missing);
            print_list("          extra", &ad.unexpected);
        }
        if svc.arg_diffs.len() > MAX_LISTED {
            println!("      ... {} more operations with arg diffs", svc.arg_diffs.len() - MAX_LISTED);
        }
    }

    let by_design: usize = report.compared.iter().map(|s| s.operations_extra_by_design.len()).sum();
    if by_design > 0 {
        println!(
            "\n{by_design} operations are exposed by design that the reference hides: it drops \
             them only because it cannot read an event stream. A superset, not a divergence."
        );
    }

    println!("{} corpus services have no vendored model yet", report.not_vendored);

    if report.is_clean() {
        println!("\nno divergences across vendored services");
        std::process::ExitCode::SUCCESS
    } else {
        println!("\ndivergences found");
        std::process::ExitCode::FAILURE
    }
}

fn print_list(label: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    let shown: Vec<&str> = items.iter().take(MAX_LISTED).map(|s| s.as_str()).collect();
    let more = items.len().saturating_sub(shown.len());
    let suffix = if more > 0 { format!(" ... +{more}") } else { String::new() };
    println!("{label} ({}): {}{}", items.len(), shown.join(" "), suffix);
}
