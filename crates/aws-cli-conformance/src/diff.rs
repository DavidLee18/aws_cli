//! Diffing our derived surface against the reference corpus.
//!
//! Divergences are the product here. Each one is either a bug in our engine or a
//! customization we have not ported yet, and the report is what turns "400 services"
//! into a finite, ordered worklist.

use crate::corpus::Corpus;
use crate::surface::Surface;
use std::collections::BTreeSet;

#[derive(Debug, Default)]
pub struct Report {
    /// Services in the corpus that no vendored model maps to.
    pub services_missing: Vec<String>,
    /// Services we derive that the reference has no such command for. Usually a naming
    /// bug: the model exists but we compute the wrong `aws <name>`.
    pub services_unexpected: Vec<String>,
    /// Per-service detail, only for services present on both sides.
    pub compared: Vec<ServiceDiff>,
    /// Services in the corpus we cannot check because their model is not vendored.
    pub not_vendored: usize,
}

#[derive(Debug)]
pub struct ServiceDiff {
    pub service: String,
    pub model_file: String,
    pub operations_missing: Vec<String>,
    pub operations_unexpected: Vec<String>,
    pub operations_matched: usize,
    pub arg_diffs: Vec<ArgDiff>,
    /// Operations whose argument sets matched exactly.
    pub operations_args_exact: usize,
}

#[derive(Debug)]
pub struct ArgDiff {
    pub operation: String,
    pub missing: Vec<String>,
    pub unexpected: Vec<String>,
}

impl ServiceDiff {
    pub fn is_clean(&self) -> bool {
        self.operations_missing.is_empty()
            && self.operations_unexpected.is_empty()
            && self.arg_diffs.is_empty()
    }
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.services_unexpected.is_empty() && self.compared.iter().all(|s| s.is_clean())
    }

    pub fn compute(corpus: &Corpus, surface: &Surface) -> Report {
        let mut report = Report::default();

        let ours: BTreeSet<&String> = surface.services.keys().collect();
        let theirs: BTreeSet<&String> = corpus.services.keys().collect();

        // Only services we actually vendored a model for can be meaningfully compared;
        // the rest are "not yet fetched", not failures.
        report.not_vendored = theirs.difference(&ours).count();

        for name in ours.difference(&theirs) {
            report.services_unexpected.push((*name).clone());
        }

        for (name, ours_svc) in &surface.services {
            let Some(theirs_svc) = corpus.services.get(name) else { continue };

            let our_ops: BTreeSet<&String> = ours_svc.operations.keys().collect();
            let their_ops: BTreeSet<&String> = theirs_svc.operations.keys().collect();

            let mut diff = ServiceDiff {
                service: name.clone(),
                model_file: ours_svc.model_file.clone(),
                operations_missing: their_ops.difference(&our_ops).map(|s| (*s).clone()).collect(),
                operations_unexpected: our_ops.difference(&their_ops).map(|s| (*s).clone()).collect(),
                operations_matched: our_ops.intersection(&their_ops).count(),
                arg_diffs: Vec::new(),
                operations_args_exact: 0,
            };

            for op in our_ops.intersection(&their_ops) {
                let ours_args = &ours_svc.operations[*op];
                let theirs_args: BTreeSet<String> =
                    theirs_svc.operations[*op].all_arguments().cloned().collect();

                let missing: Vec<String> = theirs_args.difference(ours_args).cloned().collect();
                let unexpected: Vec<String> = ours_args.difference(&theirs_args).cloned().collect();

                if missing.is_empty() && unexpected.is_empty() {
                    diff.operations_args_exact += 1;
                } else {
                    diff.arg_diffs.push(ArgDiff {
                        operation: (*op).clone(),
                        missing,
                        unexpected,
                    });
                }
            }

            report.compared.push(diff);
        }

        report
    }
}
