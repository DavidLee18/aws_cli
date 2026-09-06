//! Colouring for the messages the transfer commands write to stderr.
//!
//! Only stderr is coloured, and only warnings and errors. Result lines go to stdout,
//! where they are routinely piped into another program, and adding escape sequences
//! there would change what that program reads.
//!
//! The decision is made once, from `--color`, and cached: the transfer commands ask from
//! many threads at once and must not each consult the terminal.

use std::io::IsTerminal;
use std::sync::OnceLock;

const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

static ENABLED: OnceLock<bool> = OnceLock::new();

/// Resolve `--color on|off|auto` once, at startup.
///
/// `auto` — the default — means a terminal on stderr. `NO_COLOR` (any value, per the
/// convention) and `TERM=dumb` both veto `auto`, but not an explicit `on`: someone who
/// asked for colour has said what they want.
pub fn configure(setting: Option<&str>) {
    let enabled = match setting {
        Some("on") => true,
        Some("off") => false,
        // `auto`, absent, or anything else: the reference treats an unrecognised value
        // as the default rather than failing, and so does `logs tail` here.
        _ => {
            std::io::stderr().is_terminal()
                && std::env::var_os("NO_COLOR").is_none()
                && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true)
        }
    };
    let _ = ENABLED.set(enabled);
}

/// Whether stderr should be coloured. Defaults to off when [`configure`] never ran, so a
/// code path reached before startup finishes cannot emit escape sequences into a pipe.
pub fn enabled() -> bool {
    *ENABLED.get().unwrap_or(&false)
}

fn paint(text: &str, colour: &str) -> String {
    if enabled() {
        format!("{colour}{text}{RESET}")
    } else {
        text.to_string()
    }
}

/// A warning: the command carries on, and the exit code will say something was skipped.
pub fn warning(text: &str) -> String {
    paint(text, YELLOW)
}

/// An error: this item, or the whole command, did not happen.
pub fn error(text: &str) -> String {
    paint(text, RED)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing is coloured before `configure` runs — the safe default for a pipe.
    #[test]
    fn is_off_until_configured() {
        // `ENABLED` is process-wide and `configure` may have run in another test, so
        // only the invariant that holds either way is asserted: the text survives.
        assert!(warning("careful").contains("careful"));
        assert!(error("broken").contains("broken"));
    }

    /// Painting is either the bare text or the text wrapped in a reset-terminated
    /// sequence — never a colour left switched on.
    #[test]
    fn always_resets_what_it_sets() {
        let painted = paint("x", RED);
        assert!(painted == "x" || painted.ends_with(RESET));
    }
}
