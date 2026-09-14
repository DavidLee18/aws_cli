//! `aws ecs monitor-express-gateway-service`.

pub mod collector;
pub mod display;
pub mod resource;

use crate::args::Parsed;
use crate::client::{Client, Globals};
use crate::exit;
use crate::Failure;
use collector::{Collector, Mode};
use display::Ended;
use std::process::ExitCode;

pub fn dispatch(parsed: &Parsed, globals: &Globals) -> Result<Option<ExitCode>, Failure> {
    match parsed.operation.as_str() {
        "monitor-express-gateway-service" => monitor(parsed, globals).map(Some),
        _ => Ok(None),
    }
}

/// `aws ecs monitor-express-gateway-service`.
///
/// Polls the service every five seconds and shows what its managed resources are doing,
/// until the user stops it or the timeout expires. Two view modes decide *what* is shown
/// and two display modes decide *how*; the four combinations are independent.
///
/// The one thing worth knowing before reading it: **monitoring failures exit 1**, not the
/// usual 254 or 255. The reference returns `1` from `_run_main` for both a bad display
/// mode and a `MonitoringError`, and a returned value is the exit code.
fn monitor(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let args = crate::custom::take_args(
        parsed,
        &["--service-arn", "--resource-view", "--mode", "--timeout"],
    )?;
    let value = |flag: &str| args.get(flag).copied().flatten();
    let Some(service_arn) = value("--service-arn") else {
        return Err(crate::custom::missing_required(&["--service-arn"]));
    };

    let resource_view = value("--resource-view").unwrap_or("RESOURCE");
    let mode = match resource_view {
        "RESOURCE" => Mode::Resource,
        "DEPLOYMENT" => Mode::Deployment,
        other => return Err(invalid_choice("--resource-view", other, &["RESOURCE", "DEPLOYMENT"])),
    };
    if let Some(requested) = value("--mode") {
        if !["INTERACTIVE", "TEXT-ONLY"].contains(&requested) {
            return Err(invalid_choice("--mode", requested, &["INTERACTIVE", "TEXT-ONLY"]));
        }
    }
    let timeout_minutes: u64 = match value("--timeout") {
        None => 30,
        Some(text) => text.parse().map_err(|_| {
            Failure::new(
                exit::PARAM_VALIDATION,
                awsc_runtime::RuntimeError::ParamValidation(format!(
                    "Invalid value for --timeout: {text}"
                )),
            )
        })?,
    };

    // The display mode is chosen before anything else happens, so a request that cannot
    // be honoured costs no API call.
    let interactive = match value("--mode") {
        None => stdout_is_a_terminal(),
        Some("INTERACTIVE") => {
            if !stdout_is_a_terminal() {
                // Printed bare and exits 1: the reference writes this itself rather than
                // raising, so it carries no `An error occurred` decoration.
                eprintln!(
                    "aws: [ERROR]: Interactive mode requires a TTY (terminal). \
                     Use --mode TEXT-ONLY for non-interactive environments."
                );
                return Ok(exit::code(1));
            }
            true
        }
        Some(_) => false,
    };

    let color = match parsed.color.as_deref() {
        Some("on") => true,
        Some("off") => false,
        // `auto`, which is the default: colour only when someone is watching.
        _ => stdout_is_a_terminal(),
    };

    let region = crate::custom::resolve_region(globals)
        .ok_or_else(|| Failure::new(exit::CONFIGURATION, awsc_runtime::RuntimeError::NoRegion))?;
    // `--endpoint-url` reaches ECS here, as the reference passes it; below the flag
    // botocore still applies `AWS_ENDPOINT_URL_ECS`, so that is the fallback.
    let ecs_globals = Globals {
        region: Some(region),
        endpoint_url: globals
            .endpoint_url
            .clone()
            .or_else(|| Globals::endpoint_from_environment("ecs")),
        ..globals.clone()
    };
    let model = crate::load_model("ecs").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let client = Client::new(&model, &ecs_globals)?;
    let mut collector = Collector::new(&client, service_arn, mode, color);

    let timeout = std::time::Duration::from_secs(timeout_minutes * 60);
    let ended = if interactive {
        display::interactive(&mut collector, timeout, color)
    } else {
        let interrupted = install_interrupt_handler();
        display::text_only(&mut collector, timeout, color, interrupted)
    };

    match ended {
        Ended::Failed(e) => {
            // Already reported inline by TEXT-ONLY; the interactive mode has torn the
            // screen down by now and has not.
            if interactive {
                eprintln!("Error monitoring service: {e}");
            }
            Ok(exit::code(1))
        }
        _ => Ok(exit::code(exit::SUCCESS)),
    }
}

fn invalid_choice(flag: &str, given: &str, choices: &[&str]) -> Failure {
    Failure::new(
        exit::PARAM_VALIDATION,
        awsc_runtime::RuntimeError::ParamValidation(format!(
            "argument {flag}: Invalid choice: '{given}', maybe you meant:\n\n{}",
            choices.iter().map(|choice| format!("  * {choice}")).collect::<Vec<_>>().join("\n")
        )),
    )
}

/// Ctrl-C sets a flag rather than killing the process, so TEXT-ONLY mode can print its
/// closing lines. The reference gets this from Python's `KeyboardInterrupt`.
fn install_interrupt_handler() -> &'static std::sync::atomic::AtomicBool {
    static INTERRUPTED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    #[cfg(unix)]
    {
        extern "C" fn handle(_signal: libc::c_int) {
            INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
        unsafe { libc::signal(libc::SIGINT, handle as *const () as libc::sighandler_t) };
    }
    &INTERRUPTED
}

#[cfg(unix)]
fn stdout_is_a_terminal() -> bool {
    // SAFETY: `isatty` only inspects the descriptor.
    unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 }
}

#[cfg(not(unix))]
fn stdout_is_a_terminal() -> bool {
    false
}
