//! Regression gate on conformance.
//!
//! Known divergences are expected — the engine is incomplete and customizations are
//! unported. What must not happen is *new* divergence appearing unnoticed, so this
//! ratchets: the numbers below may improve freely, but any regression fails.
//!
//! Update the baselines downward as customizations land. Never upward without a reason
//! recorded in the commit message.

use aws_cli_conformance::{
    corpus::{custom_surface_path, customizations_path, models_dir, paginators_path},
    Corpus, Report, Surface,
};
use aws_cli_model::custom_surface::CustomSurface;
use aws_cli_model::customizations::Customizations;
use aws_cli_model::paginators::PaginatorOverlay;

/// Operations whose derived argument set differs from the reference, as of the last
/// baseline update. See `docs/divergences.md` for the taxonomy.
///
/// Both baselines are measured against the FULL 431-model catalogue and assume it is
/// vendored. Neither is coverage-invariant — the six-model era proved that the hard way:
/// a 97% ratio on the sample dropped to 93% at full coverage, because divergences
/// cluster in services with unported customizations and the sample happened to be ones
/// whose causes were already fixed. Re-baseline only from a full-catalogue run, with the
/// cause noted in the commit message.
/// Zero: as of the custom-surface data landing, the full catalogue is exactly
/// conformant (19,452/19,452 operations, 427/427 services). Any nonzero value is a
/// regression — or a legitimate upstream drift after refetching models/corpus from a
/// newer awscli/aws-sdk-rust, which should be resolved by regenerating the data files,
/// not by raising this.
const MAX_DIVERGING_OPERATIONS: usize = 0;

/// Every compared operation's argument set matches exactly.
const MIN_EXACT_ARG_RATIO: f64 = 1.0;

struct Fixture {
    corpus: Corpus,
    surface: Surface,
}

/// Returns `None` when the corpus or models are absent, so a fresh clone passes
/// `cargo test` before anything has been fetched. The paginator overlay is NOT optional
/// once models are present: it is checked in, and deriving without it is measurably
/// wrong, so a missing overlay is a hard failure rather than a skip.
fn fixture() -> Option<Fixture> {
    let corpus = Corpus::load_default().expect("corpus should parse")?;
    let paginators = PaginatorOverlay::load(&paginators_path())
        .expect("paginator overlay should load -- run scripts/extract-paginators.py");
    let customizations = Customizations::load(&customizations_path())
        .expect("customization tables should load -- run scripts/extract-customizations.py");
    let custom_surface = CustomSurface::load(&custom_surface_path())
        .expect("custom-surface data should load -- run scripts/extract-custom-surface.py");
    let surface =
        Surface::from_models_dir(&models_dir(), &paginators, &customizations, &custom_surface)
            .expect("surface should derive");
    if surface.services.is_empty() {
        return None;
    }
    Some(Fixture { corpus, surface })
}

#[test]
fn every_vendored_model_loads() {
    let Some(f) = fixture() else { return };
    assert!(
        f.surface.load_errors.is_empty(),
        "models failed to load: {:?}",
        f.surface.load_errors
    );
}

/// sts is the reference service for the first vertical slice: it must stay exactly
/// conformant, since divergence there would undermine the slice built on top of it.
#[test]
fn sts_is_fully_conformant() {
    let Some(f) = fixture() else { return };
    let report = Report::compute(&f.corpus, &f.surface);

    let sts = report
        .compared
        .iter()
        .find(|s| s.service == "sts")
        .expect("sts should be vendored and compared");

    assert!(
        sts.is_clean(),
        "sts diverged: missing ops {:?}, extra ops {:?}, arg diffs {:?}",
        sts.operations_missing,
        sts.operations_unexpected,
        sts.arg_diffs
    );
}

#[test]
fn conformance_does_not_regress() {
    let Some(f) = fixture() else { return };
    let report = Report::compute(&f.corpus, &f.surface);

    let compared: usize = report.compared.iter().map(|s| s.operations_matched).sum();
    let exact: usize = report.compared.iter().map(|s| s.operations_args_exact).sum();
    let diverging: usize = report.compared.iter().map(|s| s.arg_diffs.len()).sum();

    assert!(compared > 0, "no operations were compared");

    let ratio = exact as f64 / compared as f64;
    assert!(
        ratio >= MIN_EXACT_ARG_RATIO,
        "exact-arg ratio regressed to {:.3} ({exact}/{compared}); baseline is {MIN_EXACT_ARG_RATIO}",
        ratio
    );
    // Equality, not `<=`: the baseline is zero, so there is nothing below it to allow.
    // Raising it is what the constant's documentation forbids.
    assert_eq!(
        diverging, MAX_DIVERGING_OPERATIONS,
        "diverging operations rose to {diverging}; baseline is {MAX_DIVERGING_OPERATIONS}"
    );
}
