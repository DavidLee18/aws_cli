//! Progress rendering for the transfer commands.
//!
//! Deliberately different from the reference in several ways, all requested:
//!
//! - **The source is scanned fully before any transfer starts**, so the totals are exact
//!   from the first frame. The reference streams its listing into the transfer and shows
//!   `~` estimates plus `(calculating...)` until the listing finishes. The scan itself is
//!   not silent, though: [`Counter`] reports what it has found so far, because on a large
//!   tree the scan is a visible pause before anything else appears.
//! - **Every line is clamped to the terminal width.** The reference pads to the previous
//!   line's length and relies on `\r`; on a narrow terminal that line wraps, `\r` returns
//!   to the start of only the *last* screen row, and the progress bar smears down the
//!   screen leaving duplicated rows behind. Truncating to the measured width means each
//!   physical row is rewritten in place, always.
//! - **Up to [`MAX_ROWS`] in-flight files are listed above the summary**, each with its
//!   own percentage and ETA, and each removed the moment that file finishes.
//! - **The summary carries an ETA.**
//!
//! Two things make the display move at a usable rate. A [`Progress`] owns a ticker thread
//! that redraws at [`FRAME`], so the bar advances on its own rather than only when a
//! transfer event happens; and the transfer commands feed it byte counts from the body
//! stream, so an 8 MiB part reports about a hundred and thirty times instead of once.
//! Before both of those, a slow uplink showed one update every several seconds.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// At most this many in-flight files are listed above the summary.
pub const MAX_ROWS: usize = 5;

/// How often the ticker redraws. Fast enough to look live, slow enough that the writes
/// are nowhere near the cost of the transfer.
const FRAME: Duration = Duration::from_millis(200);

/// One file currently being transferred.
struct Slot {
    /// Identifies the file across the parts that make it up. For an upload that is the
    /// local path; for a download, the key.
    key: String,
    label: String,
    size: u64,
    done: u64,
    started: Instant,
}

/// The state the workers touch. Separate from [`Progress`] so a watcher handed to the
/// transport can hold it and outlive the borrow the worker had.
///
/// Two mutexes, and the order between them matters: `screen` is always taken **before**
/// `active`, never the other way round. `print_above` holds `screen` across erase, print
/// and redraw so a tick cannot land in the gap, and building the frame needs `active`
/// while it does.
struct Inner {
    total_files: u64,
    total_bytes: u64,
    files_done: AtomicU64,
    bytes_done: AtomicU64,
    started: Instant,
    /// Serialises writes and remembers how many rows the last frame occupied.
    screen: Mutex<usize>,
    enabled: AtomicBool,
    active: Mutex<Vec<Slot>>,
}

impl Inner {
    /// Add to the running total, and to the named file's own total when it is still
    /// listed. Called from the body stream, so it must stay cheap.
    fn advance(&self, key: &str, count: u64) {
        self.bytes_done.fetch_add(count, Ordering::Relaxed);
        let mut active = self.active.lock().expect("progress mutex poisoned");
        if let Some(slot) = active.iter_mut().find(|s| s.key == key) {
            slot.done = slot.done.saturating_add(count);
        }
    }

    /// Take back bytes that a failed attempt reported. Saturating rather than wrapping:
    /// a retry after a partial write must not make the totals negative-by-underflow.
    fn rewind(&self, key: &str, count: u64) {
        self.bytes_done.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
            Some(v.saturating_sub(count))
        })
        .expect("the update never returns None");
        let mut active = self.active.lock().expect("progress mutex poisoned");
        if let Some(slot) = active.iter_mut().find(|s| s.key == key) {
            slot.done = slot.done.saturating_sub(count);
        }
    }
}

/// Reports one file's body bytes as they move.
///
/// One per request, because retries are per request: `restart` has to take back what
/// *this* attempt reported and nothing else, so the count it undoes is its own.
struct SlotWatch {
    inner: Arc<Inner>,
    key: String,
    attempt: AtomicU64,
}

impl awsc_runtime::http::Watcher for SlotWatch {
    fn advance(&self, count: u64) {
        self.attempt.fetch_add(count, Ordering::Relaxed);
        self.inner.advance(&self.key, count);
    }

    fn restart(&self) {
        let sent = self.attempt.swap(0, Ordering::Relaxed);
        self.inner.rewind(&self.key, sent);
    }
}

pub struct Progress {
    inner: Arc<Inner>,
    /// The redraw thread, and the flag that stops it.
    ticker: Option<std::thread::JoinHandle<()>>,
    running: Arc<AtomicBool>,
}

impl Progress {
    pub fn new(total_files: u64, total_bytes: u64, enabled: bool) -> Progress {
        let enabled = enabled && std::io::stderr().is_terminal();
        let inner = Arc::new(Inner {
            total_files,
            total_bytes,
            files_done: AtomicU64::new(0),
            bytes_done: AtomicU64::new(0),
            started: Instant::now(),
            screen: Mutex::new(0),
            enabled: AtomicBool::new(enabled),
            active: Mutex::new(Vec::new()),
        });
        let running = Arc::new(AtomicBool::new(enabled));
        // Without this the bar only moves when something reports, so a stalled or simply
        // slow transfer looks identical to a hung one.
        let ticker = enabled.then(|| {
            let inner = Arc::clone(&inner);
            let running = Arc::clone(&running);
            std::thread::spawn(move || {
                while running.load(Ordering::Relaxed) {
                    std::thread::sleep(FRAME);
                    if running.load(Ordering::Relaxed) {
                        draw(&inner);
                    }
                }
            })
        });
        Progress { inner, ticker, running }
    }

    /// Start listing a file, or join the one already listed under `key`.
    ///
    /// Joining is what a multipart transfer needs: every part of one object reports into
    /// the same row, and the row appears when the first part starts.
    pub fn begin_file(&self, key: &str, label: &str, size: u64) {
        let mut active = self.inner.active.lock().expect("progress mutex poisoned");
        if active.iter().any(|s| s.key == key) {
            return;
        }
        active.push(Slot {
            key: key.to_string(),
            label: label.to_string(),
            size,
            done: 0,
            started: Instant::now(),
        });
    }

    /// A watcher for one request against `key`, to hand to the transport.
    pub fn watch(&self, key: &str) -> Arc<dyn awsc_runtime::http::Watcher> {
        Arc::new(SlotWatch {
            inner: Arc::clone(&self.inner),
            key: key.to_string(),
            attempt: AtomicU64::new(0),
        })
    }

    /// Count bytes against a file without a watcher — the copy path, where the bytes move
    /// inside S3 and never pass through this process.
    pub fn add_file_bytes(&self, key: &str, count: u64) {
        self.inner.advance(key, count);
    }

    /// The file is done: stop listing it and count it.
    pub fn finish_file(&self, key: &str) {
        // Scoped, so the redraw below does not take `screen` while holding `active`. See
        // the lock order noted on [`Inner`].
        {
            let mut active = self.inner.active.lock().expect("progress mutex poisoned");
            active.retain(|s| s.key != key);
        }
        self.inner.files_done.fetch_add(1, Ordering::Relaxed);
        draw(&self.inner);
    }

    /// Print a completed-transfer line above the bar, without leaving a torn bar behind.
    pub fn println(&self, text: &str) {
        self.print_above(text, false);
    }

    /// Print a line on stderr above the bar. For warnings and errors, which belong on
    /// stderr next to the bar rather than on stdout with the results.
    pub fn eprintln(&self, text: &str) {
        self.print_above(text, true);
    }

    /// Erase the frame, write one line, and put the frame back — all under the screen
    /// lock, so a tick cannot redraw the bar into the gap and end up written over.
    ///
    /// Result lines go to stdout, matching the reference, so they survive a redirect
    /// while the bar (stderr) does not; warnings and errors stay on stderr.
    fn print_above(&self, text: &str, to_stderr: bool) {
        let mut rows = self.inner.screen.lock().expect("progress mutex poisoned");
        erase_frame(&mut rows);
        if to_stderr {
            eprintln!("{text}");
        } else {
            println!("{text}");
        }
        if self.inner.enabled.load(Ordering::Relaxed) {
            let lines = frame_lines(&self.inner);
            write_frame(&mut rows, &lines);
        }
    }

    /// Clear the bar for good and stop the ticker.
    pub fn clear(&self) {
        self.running.store(false, Ordering::Relaxed);
        self.inner.enabled.store(false, Ordering::Relaxed);
        let mut rows = self.inner.screen.lock().expect("progress mutex poisoned");
        erase_frame(&mut rows);
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        // Joined rather than detached: a ticker still running after the command has
        // printed its last line would draw a bar over the shell prompt.
        if let Some(ticker) = self.ticker.take() {
            let _ = ticker.join();
        }
        // `clear` has usually run already and this erases nothing. The case it covers is
        // `--dryrun`, which returns without clearing and would otherwise leave whatever
        // frame the ticker managed to draw sitting under the last result line.
        self.inner.enabled.store(false, Ordering::Relaxed);
        let mut rows = self.inner.screen.lock().expect("progress mutex poisoned");
        erase_frame(&mut rows);
    }
}

/// Render one frame: the in-flight files, then the summary.
fn draw(inner: &Inner) {
    if !inner.enabled.load(Ordering::Relaxed) {
        return;
    }
    let mut rows = inner.screen.lock().expect("progress mutex poisoned");
    // Re-checked under the lock: `clear` may have run since, and drawing after that would
    // put the bar back after the command believed it gone.
    if !inner.enabled.load(Ordering::Relaxed) {
        return;
    }
    let lines = frame_lines(inner);
    write_frame(&mut rows, &lines);
}

/// The lines of one frame: the in-flight files, then the summary.
fn frame_lines(inner: &Inner) -> Vec<String> {
    let bytes = inner.bytes_done.load(Ordering::Relaxed);
    let files = inner.files_done.load(Ordering::Relaxed);
    let elapsed = inner.started.elapsed().as_secs_f64().max(0.001);
    let rate = bytes as f64 / elapsed;

    let mut lines: Vec<String> = Vec::with_capacity(MAX_ROWS + 2);
    {
        let active = inner.active.lock().expect("progress mutex poisoned");
        for slot in active.iter().take(MAX_ROWS) {
            lines.push(file_line(slot));
        }
        if active.len() > MAX_ROWS {
            lines.push(format!("  … and {} more", active.len() - MAX_ROWS));
        }
    }

    // The remaining bytes at the rate achieved so far. Not the instantaneous rate: on a
    // link that fluctuates, that produces an ETA that jumps around too much to read.
    let remaining = inner.total_bytes.saturating_sub(bytes);
    lines.push(format!(
        "Completed {}/{} ({}/s) with {} file(s) remaining, ETA {}",
        super::human_readable_size(bytes),
        super::human_readable_size(inner.total_bytes),
        super::human_readable_size(rate as u64),
        inner.total_files.saturating_sub(files),
        format_eta(remaining, rate),
    ));

    lines
}

/// One in-flight file: how far along it is, how fast, and how long is left.
fn file_line(slot: &Slot) -> String {
    // A zero-byte file is complete by definition, and dividing by its size is not.
    let percent = (slot.done.min(slot.size) * 100).checked_div(slot.size).unwrap_or(100);
    let elapsed = slot.started.elapsed().as_secs_f64().max(0.001);
    let rate = slot.done as f64 / elapsed;
    format!(
        "  {:>3}% {}/{} {} {}",
        percent,
        super::human_readable_size(slot.done),
        super::human_readable_size(slot.size),
        format_eta(slot.size.saturating_sub(slot.done), rate),
        slot.label,
    )
}

/// Wipe the frame off the screen, leaving the cursor where it began.
fn erase_frame(rows: &mut usize) {
    if *rows == 0 {
        return;
    }
    let mut out = String::new();
    if *rows > 1 {
        out.push_str(&format!("\x1b[{}A", *rows - 1));
    }
    out.push('\r');
    for i in 0..*rows {
        if i > 0 {
            out.push('\n');
        }
        out.push_str("\x1b[2K");
    }
    if *rows > 1 {
        out.push_str(&format!("\x1b[{}A", *rows - 1));
    }
    out.push('\r');
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(out.as_bytes());
    let _ = err.flush();
    *rows = 0;
}

/// Write a frame, rewriting the rows the previous one used.
///
/// The cursor is left at the start of the last row, which is where the next frame expects
/// to find it. A frame with fewer rows than the last blanks the leftovers rather than
/// leaving a stale row on screen below the bar.
fn write_frame(rows: &mut usize, lines: &[String]) {
    let out = frame_bytes(rows, lines, terminal_width());
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(out.as_bytes());
    let _ = err.flush();
}

/// The escape sequence for one frame, split out so it can be tested without a terminal.
///
/// `rows` is updated to the number of rows this frame occupies.
fn frame_bytes(rows: &mut usize, lines: &[String], width: usize) -> String {
    let mut out = String::new();
    if *rows > 1 {
        out.push_str(&format!("\x1b[{}A", *rows - 1));
    }
    out.push('\r');
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str("\x1b[2K");
        out.push_str(&truncate_to_width(line, width));
    }
    let extra = rows.saturating_sub(lines.len());
    for _ in 0..extra {
        out.push_str("\n\x1b[2K");
    }
    if extra > 0 {
        out.push_str(&format!("\x1b[{extra}A"));
    }
    out.push('\r');
    *rows = lines.len();
    out
}

/// `remaining` bytes at `rate` bytes per second, as `H:MM:SS`.
///
/// A rate too small to divide by yields `--:--:--` rather than a number of hours that is
/// really "no idea yet" — which is what the first frame of every transfer would show.
pub fn format_eta(remaining: u64, rate: f64) -> String {
    if rate < 1.0 {
        return "--:--:--".to_string();
    }
    let seconds = (remaining as f64 / rate) as u64;
    // Anything beyond a day is not an estimate anybody acts on.
    if seconds >= 24 * 3600 {
        return "> 1 day".to_string();
    }
    format!("{}:{:02}:{:02}", seconds / 3600, (seconds / 60) % 60, seconds % 60)
}

/// Reports what a scan has found so far.
///
/// The scan happens before the first transfer starts, and on a large tree it is a pause
/// with nothing on screen — which reads as a hang. This is the only thing shown during
/// it, and it is erased before the bar takes over.
pub struct Counter {
    enabled: bool,
    count: AtomicU64,
    /// Milliseconds since `started` at the last draw, so the write rate is bounded no
    /// matter how fast the walk goes.
    last: AtomicU64,
    started: Instant,
    noun: &'static str,
}

impl Counter {
    pub fn new(enabled: bool, noun: &'static str) -> Counter {
        Counter {
            enabled: enabled && std::io::stderr().is_terminal(),
            count: AtomicU64::new(0),
            last: AtomicU64::new(0),
            started: Instant::now(),
            noun,
        }
    }

    /// Note `found` more entries, redrawing at most every [`FRAME`].
    pub fn add(&self, found: u64) {
        if !self.enabled {
            return;
        }
        let total = self.count.fetch_add(found, Ordering::Relaxed) + found;
        let now = self.started.elapsed().as_millis() as u64;
        let last = self.last.load(Ordering::Relaxed);
        if now.saturating_sub(last) < FRAME.as_millis() as u64 {
            return;
        }
        // A losing racer skips this frame rather than drawing a second one.
        if self.last.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err() {
            return;
        }
        let text = format!("Counting {}… {total}", self.noun);
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "\r\x1b[2K{}", truncate_to_width(&text, terminal_width()));
        let _ = err.flush();
    }

    /// Erase the line, so the transfer's own display starts on a clean row.
    pub fn clear(&self) {
        if !self.enabled {
            return;
        }
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "\r\x1b[2K");
        let _ = err.flush();
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

    #[test]
    fn formats_an_eta_as_hours_minutes_seconds() {
        assert_eq!(format_eta(100, 10.0), "0:00:10");
        assert_eq!(format_eta(7_265, 1.0), "2:01:05");
    }

    /// The first frame of every transfer has no rate yet, and must not claim to know.
    #[test]
    fn admits_when_it_cannot_estimate() {
        assert_eq!(format_eta(1_000_000, 0.0), "--:--:--");
        assert_eq!(format_eta(u64::MAX, 1.0), "> 1 day");
    }

    /// A finished file is worth 100%, however big it is — including the zero-byte case,
    /// which would otherwise divide by zero.
    #[test]
    fn an_empty_file_is_complete() {
        let slot = Slot {
            key: "k".to_string(),
            label: "k".to_string(),
            size: 0,
            done: 0,
            started: Instant::now(),
        };
        assert!(file_line(&slot).contains("100%"));
    }

    fn hidden(files: u64, bytes: u64) -> Progress {
        // `enabled: false` keeps the tests off the terminal; the accounting under test
        // runs either way.
        Progress::new(files, bytes, false)
    }

    #[test]
    fn tracks_bytes_against_the_file_that_moved_them() {
        let progress = hidden(1, 100);
        progress.begin_file("a", "a", 100);
        progress.watch("a").advance(30);
        assert_eq!(progress.inner.bytes_done.load(Ordering::Relaxed), 30);
        let active = progress.inner.active.lock().expect("mutex");
        assert_eq!(active[0].done, 30);
    }

    /// The retry case. A part that fails halfway and is re-sent must not be counted
    /// twice — the totals would run past the file size and the ETA would go backwards.
    #[test]
    fn a_retry_takes_back_what_it_reported() {
        let progress = hidden(1, 100);
        progress.begin_file("a", "a", 100);
        let watch = progress.watch("a");
        watch.advance(40);
        watch.restart();
        assert_eq!(progress.inner.bytes_done.load(Ordering::Relaxed), 0);
        watch.advance(100);
        assert_eq!(progress.inner.bytes_done.load(Ordering::Relaxed), 100);
    }

    /// Only the attempt that failed is taken back, not another request's bytes: each
    /// request gets its own watcher for exactly this reason.
    #[test]
    fn a_retry_does_not_rewind_a_sibling_part() {
        let progress = hidden(1, 200);
        progress.begin_file("a", "a", 200);
        let first = progress.watch("a");
        let second = progress.watch("a");
        first.advance(100);
        second.advance(50);
        second.restart();
        assert_eq!(progress.inner.bytes_done.load(Ordering::Relaxed), 100);
    }

    /// Every part of one object reports into one row, and the row appears once.
    #[test]
    fn parts_of_one_file_share_a_row() {
        let progress = hidden(1, 100);
        progress.begin_file("a", "a", 100);
        progress.begin_file("a", "a", 100);
        assert_eq!(progress.inner.active.lock().expect("mutex").len(), 1);
        progress.finish_file("a");
        assert!(progress.inner.active.lock().expect("mutex").is_empty());
        assert_eq!(progress.inner.files_done.load(Ordering::Relaxed), 1);
    }

    /// However many files are in flight, the display stays a bounded number of rows.
    #[test]
    fn lists_at_most_five_files() {
        let progress = hidden(9, 900);
        for i in 0..9 {
            progress.begin_file(&format!("f{i}"), &format!("f{i}"), 100);
        }
        let active = progress.inner.active.lock().expect("mutex");
        let rows = active.len().min(MAX_ROWS);
        assert_eq!(rows, MAX_ROWS);
        assert_eq!(active.len() - MAX_ROWS, 4);
    }

    /// A shrinking frame must blank the rows it no longer uses, or the last line of the
    /// previous frame stays on screen below the bar for the rest of the command.
    #[test]
    fn a_shorter_frame_clears_what_it_vacates() {
        let mut rows = 0;
        let three: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        frame_bytes(&mut rows, &three, 80);
        assert_eq!(rows, 3);
        let out = frame_bytes(&mut rows, &["a".to_string()], 80);
        assert_eq!(rows, 1);
        // Two rows written after the one line, each erased, then the cursor back up.
        assert!(out.contains("\n\x1b[2K\n\x1b[2K"), "{out:?}");
        assert!(out.contains("\x1b[2A"), "{out:?}");
    }

    /// Every row is clamped, not just the summary: a long file name would otherwise wrap
    /// and the cursor arithmetic would be one row out for the rest of the transfer.
    #[test]
    fn every_row_fits_the_terminal() {
        let mut rows = 0;
        let long = vec!["x".repeat(200), "y".repeat(200)];
        let out = frame_bytes(&mut rows, &long, 40);
        for row in out.split('\n') {
            let text: String = row.replace('\r', "");
            let printable = text.replace("\x1b[2K", "");
            let printable = printable.trim_start_matches(|c: char| c == '\x1b' || c == '[' || c.is_ascii_digit() || c == 'A');
            assert!(printable.chars().count() <= 40, "{printable:?}");
        }
    }

    /// Bytes reported for a file that has already finished still count toward the total.
    /// A late chunk from an aborted part must not panic or be lost.
    #[test]
    fn a_late_chunk_is_harmless() {
        let progress = hidden(1, 100);
        progress.begin_file("a", "a", 100);
        progress.finish_file("a");
        progress.watch("a").advance(10);
        assert_eq!(progress.inner.bytes_done.load(Ordering::Relaxed), 10);
    }
}
