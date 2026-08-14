//! Progress rendering for the transfer commands.
//!
//! Deliberately different from the reference in two ways, both requested:
//!
//! - **The source is scanned fully before any transfer starts**, so the totals are exact
//!   from the first frame. The reference streams its listing into the transfer and shows
//!   `~` estimates plus `(calculating...)` until the listing finishes.
//! - **Every line is clamped to the terminal width.** The reference pads to the previous
//!   line's length and relies on `\r`; on a narrow terminal that line wraps, `\r` returns
//!   to the start of only the *last* screen row, and the progress bar smears down the
//!   screen leaving duplicated rows behind. Truncating to the measured width means one
//!   physical row is rewritten in place, always.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

pub struct Progress {
    pub total_files: u64,
    pub total_bytes: u64,
    files_done: AtomicU64,
    bytes_done: AtomicU64,
    started: std::time::Instant,
    /// Serialises writes and remembers whether a bar is currently on screen.
    line: Mutex<bool>,
    enabled: AtomicBool,
}

impl Progress {
    pub fn new(total_files: u64, total_bytes: u64, enabled: bool) -> Progress {
        Progress {
            total_files,
            total_bytes,
            files_done: AtomicU64::new(0),
            bytes_done: AtomicU64::new(0),
            started: std::time::Instant::now(),
            line: Mutex::new(false),
            enabled: AtomicBool::new(enabled && std::io::stderr().is_terminal()),
        }
    }

    pub fn add_bytes(&self, count: u64) {
        self.bytes_done.fetch_add(count, Ordering::Relaxed);
        self.draw();
    }

    pub fn finish_file(&self) {
        self.files_done.fetch_add(1, Ordering::Relaxed);
        self.draw();
    }

    /// Print a completed-transfer line above the bar, without leaving a torn bar behind.
    pub fn println(&self, text: &str) {
        let mut on_screen = self.line.lock().expect("progress mutex poisoned");
        let mut err = std::io::stderr().lock();
        if *on_screen {
            // Erase the bar before writing, so the two never interleave on one row.
            let _ = write!(err, "\r\x1b[2K");
            *on_screen = false;
        }
        let _ = err.flush();
        // Result lines go to stdout, matching the reference, so they survive a redirect
        // while the bar (stderr) does not.
        println!("{text}");
        drop(on_screen);
        self.draw();
    }

    fn draw(&self) {
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        let mut on_screen = self.line.lock().expect("progress mutex poisoned");
        let bytes = self.bytes_done.load(Ordering::Relaxed);
        let files = self.files_done.load(Ordering::Relaxed);
        let elapsed = self.started.elapsed().as_secs_f64().max(0.001);
        let rate = bytes as f64 / elapsed;

        let text = format!(
            "Completed {}/{} ({}/s) with {} file(s) remaining",
            super::human_readable_size(bytes),
            super::human_readable_size(self.total_bytes),
            super::human_readable_size(rate as u64),
            self.total_files.saturating_sub(files),
        );

        // Clamp to the terminal so the line occupies exactly one row and `\r` rewrites it.
        let width = terminal_width();
        let clamped = truncate_to_width(&text, width);
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "\r\x1b[2K{clamped}");
        let _ = err.flush();
        *on_screen = true;
    }

    /// Clear the bar for good.
    pub fn clear(&self) {
        let mut on_screen = self.line.lock().expect("progress mutex poisoned");
        if *on_screen {
            let mut err = std::io::stderr().lock();
            let _ = write!(err, "\r\x1b[2K");
            let _ = err.flush();
            *on_screen = false;
        }
    }
}

/// The terminal width, from `ioctl` and then `COLUMNS`, defaulting to 80.
pub fn terminal_width() -> usize {
    #[cfg(unix)]
    {
        // SAFETY: `winsize` is plain data and `ioctl` fills it in; a non-zero return means
        // it did not, and the value is discarded.
        unsafe {
            let mut size: libc::winsize = std::mem::zeroed();
            if libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut size) == 0
                && size.ws_col > 0
            {
                return size.ws_col as usize;
            }
        }
    }
    std::env::var("COLUMNS").ok().and_then(|v| v.parse().ok()).filter(|w| *w > 0).unwrap_or(80)
}

/// Cut `text` to at most `width` columns, on a character boundary.
///
/// Counts `char`s rather than bytes so a multi-byte path is not split mid-character. Wide
/// East Asian glyphs still count as one, which can leave the line a column short of the
/// edge — that errs toward not wrapping, which is the failure that matters.
pub fn truncate_to_width(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if text.chars().count() <= width {
        return text.to_string();
    }
    let keep = width.saturating_sub(1);
    let mut out: String = text.chars().take(keep).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaves_short_lines_alone() {
        assert_eq!(truncate_to_width("hello", 80), "hello");
        assert_eq!(truncate_to_width("hello", 5), "hello");
    }

    /// The whole point: the result must never exceed the width, or it wraps and `\r`
    /// leaves a duplicated row behind.
    #[test]
    fn never_exceeds_the_width() {
        for width in 1..40 {
            let out = truncate_to_width("a rather long progress line indeed", width);
            assert!(out.chars().count() <= width, "width {width} produced {out:?}");
        }
    }

    /// Truncation happens on character boundaries, not bytes.
    #[test]
    fn splits_multibyte_text_safely() {
        let text = "café-café-café";
        let out = truncate_to_width(text, 6);
        assert_eq!(out.chars().count(), 6);
        assert!(out.starts_with("café"));
    }

    #[test]
    fn reports_a_usable_terminal_width() {
        assert!(terminal_width() > 0);
    }
}
