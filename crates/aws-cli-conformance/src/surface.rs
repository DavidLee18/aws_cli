//! Deriving our command surface from Smithy models.
//!
//! This is the Rust side of the comparison: given the vendored Smithy models, work out
//! what `aws <service> <operation> --<arg>` commands we would expose. Diffing this
//! against the golden corpus is what tells us whether the model-driven engine reproduces
//! the Python CLI.

use aws_cli_model::custom_surface::CustomSurface;
use aws_cli_model::customizations::Customizations;
use aws_cli_model::naming;
use aws_cli_model::paginators::PaginatorOverlay;
use aws_cli_model::{Model, Shape};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug, Default)]
pub struct Surface {
    /// Derived CLI service name -> operations.
    pub services: BTreeMap<String, ServiceSurface>,
    /// Model files that failed to load, as (filename, error).
    pub load_errors: Vec<(String, String)>,
    /// Model files whose service the reference CLI does not ship (their `sdkId` has no
    /// entry in the service-names table): aws-sdk-rust carries a few services the CLI
    /// deliberately lacks (`cloudwatch-events`, `transcribe-streaming`, ...).
    pub excluded: Vec<String>,
}

#[derive(Debug, Default)]
pub struct ServiceSurface {
    /// The model filename this came from, which is often *not* the CLI service name.
    pub model_file: String,
    pub operations: BTreeMap<String, BTreeSet<String>>,
}

impl Surface {
    /// Load every `*.json` model in `dir` and derive the surface.
    ///
    /// The paginator overlay is required, not optional: deriving pagination flags from
    /// the Smithy trait instead is measurably wrong (1,800+ flag divergences at full
    /// catalogue), and a silent fallback would report those as engine bugs.
    pub fn from_models_dir(
        dir: &Path,
        paginators: &PaginatorOverlay,
        customizations: &Customizations,
        custom_surface: &CustomSurface,
    ) -> Result<Self, String> {
        let mut surface = Surface::default();

        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(surface); // absent models/ yields an empty surface, not an error
        };

        let mut files: Vec<_> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            // Skip our own dotfiles: the model-name index cache lives in this directory
            // and is not a Smithy model.
            .filter(|p| !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')))
            .collect();
        files.sort();

        for path in files {
            let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
            match Self::load_one(&path, paginators, customizations, custom_surface) {
                Ok(Some((cli_name, svc))) => {
                    surface.services.insert(cli_name, svc);
                }
                Ok(None) => surface.excluded.push(stem),
                Err(e) => surface.load_errors.push((stem, e)),
            }
        }
        Ok(surface)
    }

    /// `Ok(None)` means the model is valid but the reference CLI does not ship the
    /// service, so it has no place in the surface.
    fn load_one(
        path: &Path,
        paginators: &PaginatorOverlay,
        customizations: &Customizations,
        custom_surface: &CustomSurface,
    ) -> Result<Option<(String, ServiceSurface)>, String> {
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        let model = Model::from_json(&bytes).map_err(|e| e.to_string())?;
        if !model.is_cli_service().map_err(|e| e.to_string())? {
            return Ok(None);
        }
        let cli_name = model.cli_service_name().map_err(|e| e.to_string())?;

        // Phase 1: generic derivation for every surviving modeled operation.
        //
        // Removals come from the shared command table, so the harness and the binary
        // agree by construction rather than by two implementations happening to match.
        let table = aws_cli_model::command_table::build(&model, customizations, custom_surface)
            .map_err(|e| e.to_string())?;
        let mut base_args: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for op_name in model.operation_names().map(|s| s.to_string()).collect::<Vec<_>>() {
            if customizations.is_removed(&cli_name, &op_name)
                || custom_surface.is_replaced(&cli_name, &op_name)
            {
                continue;
            }
            debug_assert!(
                table.names.values().any(|w| naming::to_cli_name(w) == op_name)
                    || table.contains(&op_name)
                    || custom_surface.is_replaced(&cli_name, &op_name),
                "{cli_name} {op_name} survived the harness filter but not the command table"
            );
            let args = operation_arguments(&model, &cli_name, &op_name, paginators)?;
            base_args.insert(op_name, args);
        }

        let mut operations = BTreeMap::new();

        // Phase 2: waiters, from the botocore catalogue (the Smithy waitable trait
        // disagrees with it, exactly like pagination). Waiter arg tables are built on
        // the `wait.<name>` event path, where per-operation customization hooks
        // (renames, patches) never fire — so waiters copy the PRE-patch args.
        if let Some(waiters) = custom_surface.waiters.get(&cli_name) {
            for (waiter_name, op_name) in waiters {
                if let Some(args) = base_args.get(op_name) {
                    operations.insert(format!("wait {waiter_name}"), args.clone());
                }
            }
        }

        // Phase 3: per-operation customization data — renames, aliases, arg patches.
        for (op_name, mut args) in base_args {
            let (primary, alias) = customizations.operation_names(&cli_name, &op_name);
            // Argument rules key on the name the user types; aliases share one arg
            // table with their primary, so applying under the primary covers both.
            customizations.apply_argument_rules(&cli_name, &primary, &mut args);
            custom_surface.apply_patch(&cli_name, &primary, &mut args);
            if let Some(alias) = alias {
                operations.insert(alias, args.clone());
            }
            operations.insert(primary, args);
        }

        // Phase 4: custom BasicCommands. Complete as stored — they receive none of the
        // injected flags (their arg tables build on a different event).
        if let Some(commands) = custom_surface.custom_commands.get(&cli_name) {
            for (command, args) in commands {
                operations.insert(command.clone(), args.iter().cloned().collect());
            }
        }

        Ok(Some((
            cli_name,
            ServiceSurface {
                model_file: path.file_stem().unwrap_or_default().to_string_lossy().to_string(),
                operations,
            },
        )))
    }
}

/// Flags the reference CLI injects into *every* operation, regardless of model.
const UNIVERSAL_ARGS: &[&str] =
    &["--cli-input-json", "--cli-input-yaml", "--generate-cli-skeleton"];

/// Flags the reference CLI injects into paginated operations. Which operations qualify —
/// and whether `--page-size` is included — comes from the botocore paginator overlay,
/// NOT from `smithy.api#paginated`; the dialects disagree.
const PAGINATION_ARGS: &[&str] = &["--max-items", "--starting-token"];
const PAGE_SIZE_ARG: &str = "--page-size";

/// The `--flag` names an operation would expose.
///
/// Every top-level input member becomes `--<xform_name(member)>`, plus the flags the CLI
/// injects universally and for paginated operations. The reference additionally applies
/// customizations (renames, removals, flattening) that the Smithy models don't describe —
/// finding exactly where that matters is the point of diffing against the corpus.
fn operation_arguments(
    model: &Model,
    cli_service_name: &str,
    op_cli_name: &str,
    paginators: &PaginatorOverlay,
) -> Result<BTreeSet<String>, String> {
    let (_, op) = model.operation(op_cli_name).map_err(|e| e.to_string())?;

    // Streaming-blob output flips two things at once: a positional `outfile` argument
    // appears (streamingoutputarg.py), and because `outfile` is in the arg table the
    // cliinput/skeleton customizations skip the operation entirely — so the universal
    // flags are suppressed, not merely joined by outfile.
    let streaming_output = model
        .operation_has_streaming_blob_output(op)
        .map_err(|e| e.to_string())?;

    let mut injected: BTreeSet<String> = if streaming_output {
        std::iter::once("outfile".to_string()).collect()
    } else {
        UNIVERSAL_ARGS.iter().map(|s| s.to_string()).collect()
    };
    if let Some(paginator) = paginators.get(cli_service_name, op_cli_name) {
        injected.extend(PAGINATION_ARGS.iter().map(|s| s.to_string()));
        if paginator.limit_key.is_some() {
            injected.insert(PAGE_SIZE_ARG.to_string());
        }
    }

    let Some(input) = model.operation_input(op).map_err(|e| e.to_string())? else {
        return Ok(injected);
    };

    // Every input member becomes a flag, including ones bound to the URI, headers or the
    // HTTP payload -- those affect where a value is placed on the wire, not whether the
    // user can supply it. We deliberately filter nothing here and let the diff tell us
    // where that assumption breaks, rather than encoding a guess.
    let mut args = injected;
    for (member_name, member) in &input.members {
        args.insert(format!("--{}", naming::xform_name(member_name, "-")));

        // Boolean members additionally get a `--no-` negative form in the reference CLI.
        // On EC2 only, toplevelbool.py extends this to "structure of a single boolean
        // member named Value" (DisableApiTermination and friends), which surfaces as
        // `--opt` / `--no-opt` just like a plain boolean.
        if is_boolean(model, member)
            || (cli_service_name == "ec2" && is_single_bool_value_struct(model, member))
        {
            args.insert(format!("--no-{}", naming::xform_name(member_name, "-")));
        }
    }
    Ok(args)
}

fn is_boolean(model: &Model, member: &aws_cli_model::Member) -> bool {
    matches!(model.shape(&member.target), Some(Shape::Boolean(_)))
}

/// The `toplevelbool.py` pattern: a structure whose only member is `Value: boolean`.
fn is_single_bool_value_struct(model: &Model, member: &aws_cli_model::Member) -> bool {
    let Some(Shape::Structure(s)) = model.shape(&member.target) else { return false };
    if s.members.len() != 1 {
        return false;
    }
    let Some(value) = s.members.get("Value") else { return false };
    matches!(model.shape(&value.target), Some(Shape::Boolean(_)))
}
