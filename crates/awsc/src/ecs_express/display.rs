//! The two ways monitoring shows itself.
//!
//! **TEXT-ONLY** prints one timestamped line per resource that *changed* since the last
//! poll, which is what makes it usable in a log or a pipe. **INTERACTIVE** redraws the
//! whole tree in place with a spinner, and lets the reader scroll it.
//!
//! The interactive one is a divergence worth stating plainly: the reference builds it
//! with `prompt_toolkit`, a full terminal UI toolkit. This draws the same thing — a
//! framed, scrollable viewport over the tree with a status line, `up`/`down` to scroll
//! and `q` to quit — directly with ANSI escapes, because a toolkit for one command is
//! not a trade worth making. The pixels are ours; the behaviour is the reference's.

use super::collector::{Collector, MonitoringError};

const POLL_SECONDS: u64 = 5;
/// The reference's braille spinner, one frame every 100 ms.
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const RESET: &str = "\x1b[0m";

/// Why monitoring stopped.
pub enum Ended {
    Complete,
    TimedOut,
    Interrupted,
    Failed(MonitoringError),
}

/// TEXT-ONLY: timestamped lines, one per changed resource.
pub fn text_only(
    collector: &mut Collector<'_>,
    timeout: std::time::Duration,
    color: bool,
    interrupted: &std::sync::atomic::AtomicBool,
) -> Ended {
    print_line(&format!("[{}] Starting monitoring...", now()));
    let started = std::time::Instant::now();
    let mut previous = Vec::new();
    let ended = loop {
        if interrupted.load(std::sync::atomic::Ordering::SeqCst) {
            break Ended::Interrupted;
        }
        if started.elapsed() > timeout {
            print_line(&format!("[{}] Monitoring timeout reached!", now()));
            break Ended::TimedOut;
        }
        if let Err(e) = collector.refresh() {
            break Ended::Failed(e);
        }
        let timestamp = now();
        if let Some((tree, info)) = &collector.cached {
            if let Some(tree) = tree {
                let (changed, updated) = tree.changed_since(&previous);
                previous = updated;
                for resource in changed {
                    print_line(&resource.stream_string(&timestamp, color));
                }
            }
            if let Some(info) = info {
                print_line(&format!("[{timestamp}] {info}"));
                if info == "Service is inactive" {
                    break Ended::Complete;
                }
            }
        }
        if !sleep_unless_interrupted(POLL_SECONDS, interrupted) {
            break Ended::Interrupted;
        }
    };

    match &ended {
        Ended::Interrupted => print_line(&format!("[{}] Monitoring stopped by user", now())),
        Ended::Failed(e) => print_line(&format!("[{}] Error: {e}", now())),
        _ => {}
    }
    print_line(&format!("[{}] Monitoring complete!", now()));
    print!("{RESET}");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    ended
}

fn print_line(text: &str) {
    println!("{text}");
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

/// Sleep, waking early if the user interrupted. Returns false if they did.
fn sleep_unless_interrupted(
    seconds: u64,
    interrupted: &std::sync::atomic::AtomicBool,
) -> bool {
    for _ in 0..seconds * 20 {
        if interrupted.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    true
}

/// The current time as the reference stamps it: UTC, to the second.
fn now() -> String {
    let unix = crate::now_unix();
    let (year, month, day, hour, minute, second) = civil(unix);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}Z")
}

fn civil(unix: i64) -> (i64, i64, i64, i64, i64, i64) {
    let days = unix.div_euclid(86_400);
    let seconds = unix.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, seconds / 3600, (seconds % 3600) / 60, seconds % 60)
}

/// The scrollable viewport the interactive mode draws.
///
/// Split from the drawing so the geometry — which lines are on screen, where the scroll
/// stops — can be tested without a terminal.
pub struct Viewport {
    pub scroll: usize,
    pub rows: usize,
    pub columns: usize,
}

impl Viewport {
    /// How many screen lines a logical line occupies once wrapped.
    fn wrapped(line: &str, columns: usize) -> usize {
        let width = visible_width(line);
        if width == 0 || columns == 0 {
            1
        } else {
            width.div_ceil(columns)
        }
    }

    /// Total screen lines the text needs at this width.
    pub fn total_lines(&self, text: &str) -> usize {
        text.lines().map(|line| Viewport::wrapped(line, self.columns)).sum()
    }

    /// The furthest the view can scroll before the last line reaches the bottom.
    pub fn max_scroll(&self, text: &str) -> usize {
        self.total_lines(text).saturating_sub(self.rows)
    }

    /// Pull the scroll back when the content shrank under it.
    pub fn clamp(&mut self, text: &str) {
        self.scroll = self.scroll.min(self.max_scroll(text));
    }
}

/// The width of a line as the terminal sees it, ignoring ANSI colour escapes.
pub fn visible_width(line: &str) -> usize {
    let mut width = 0usize;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // CSI ... final byte in @..~; anything else is a two-character sequence.
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
        } else {
            width += 1;
        }
    }
    width
}


/// INTERACTIVE: redraw the tree in place, with a spinner and scrolling.
///
/// The data is polled on a background thread every five seconds while the foreground
/// redraws at 10 Hz, which is what keeps the spinner moving between polls. The reference
/// does the same with two asyncio tasks.
#[cfg(unix)]
pub fn interactive(
    collector: &mut Collector<'_>,
    timeout: std::time::Duration,
    color: bool,
) -> Ended {
    let raw = match RawMode::enter() {
        Some(raw) => raw,
        // Without raw mode there is no key handling, so there is no interactive mode.
        None => return Ended::Failed(MonitoringError(
            "Interactive mode requires a TTY (terminal). Use --mode TEXT-ONLY for \
             non-interactive environments."
                .to_string(),
        )),
    };
    print!("\x1b[?1049h\x1b[?25l");
    let _ = std::io::Write::flush(&mut std::io::stdout());

    let started = std::time::Instant::now();
    let mut view = Viewport { scroll: 0, rows: 0, columns: 0 };
    let mut frame = 0usize;
    let mut next_poll = std::time::Instant::now();
    let mut text = "Waiting for initial data".to_string();
    let ended = loop {
        if started.elapsed() > timeout {
            break Ended::TimedOut;
        }
        if std::time::Instant::now() >= next_poll {
            if let Err(e) = collector.refresh() {
                break Ended::Failed(e);
            }
            text = collector.view("{SPINNER}");
            next_poll = std::time::Instant::now()
                + std::time::Duration::from_secs(POLL_SECONDS);
        }
        let spinner = SPINNER_FRAMES[frame % SPINNER_FRAMES.len()];
        frame += 1;
        let rendered = text.replace("{SPINNER}", spinner);
        let (rows, columns) = terminal_size();
        // Frame top, frame bottom and the status line.
        view.rows = rows.saturating_sub(3);
        view.columns = columns.saturating_sub(4);
        view.clamp(&rendered);
        draw(&rendered, &view, spinner, rows, columns);

        match read_key(std::time::Duration::from_millis(100)) {
            Some(Key::Quit) => break Ended::Complete,
            Some(Key::Up) => view.scroll = view.scroll.saturating_sub(1),
            Some(Key::Down) if view.scroll < view.max_scroll(&rendered) => view.scroll += 1,
            Some(Key::Down) => {}
            None => {}
        }
    };

    print!("\x1b[?25h\x1b[?1049l");
    drop(raw);
    let final_text = text.replace("{SPINNER}", "");
    let _ = color;
    match &ended {
        Ended::TimedOut => println!("{final_text}\nMonitoring timed out!"),
        Ended::Failed(_) => {}
        _ => println!("{final_text}\nMonitoring Complete!"),
    }
    print!("{RESET}");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    ended
}

#[cfg(not(unix))]
pub fn interactive(
    _collector: &mut Collector<'_>,
    _timeout: std::time::Duration,
    _color: bool,
) -> Ended {
    Ended::Failed(MonitoringError(
        "Interactive mode requires a TTY (terminal). Use --mode TEXT-ONLY for \
         non-interactive environments."
            .to_string(),
    ))
}

enum Key {
    Up,
    Down,
    Quit,
}

/// One screen: a framed viewport over the scrolled text, then the status line.
#[cfg(unix)]
fn draw(text: &str, view: &Viewport, spinner: &str, rows: usize, columns: usize) {
    use std::fmt::Write as _;
    let inner_width = columns.saturating_sub(4);
    // Wrap first, so scrolling counts screen lines rather than logical ones.
    let mut screen_lines: Vec<String> = Vec::new();
    for line in text.lines() {
        if visible_width(line) <= inner_width || inner_width == 0 {
            screen_lines.push(line.to_string());
        } else {
            screen_lines.extend(wrap(line, inner_width));
        }
    }

    let mut out = String::from("\x1b[H\x1b[2J");
    let _ = writeln!(out, "┌{}┐", "─".repeat(columns.saturating_sub(2)));
    for index in 0..view.rows {
        let line = screen_lines.get(view.scroll + index).map(String::as_str).unwrap_or("");
        let padding = inner_width.saturating_sub(visible_width(line));
        let _ = writeln!(out, "│ {line}{}{RESET} │", " ".repeat(padding));
    }
    let _ = writeln!(out, "└{}┘", "─".repeat(columns.saturating_sub(2)));
    let status = format!("Getting updates... {spinner} | up/down to scroll, q to quit");
    let _ = write!(out, "{status}");
    let _ = rows;
    print!("{out}");
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

/// Break one long line into screen-width pieces, counting only visible characters so a
/// colour escape does not eat part of the width.
#[cfg(unix)]
fn wrap(line: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut used = 0usize;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            current.push(c);
            if chars.peek() == Some(&'[') {
                current.push(chars.next().expect("peeked"));
                for c in chars.by_ref() {
                    current.push(c);
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        current.push(c);
        used += 1;
        if used == width {
            out.push(std::mem::take(&mut current));
            used = 0;
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

#[cfg(unix)]
fn terminal_size() -> (usize, usize) {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: `ioctl` with TIOCGWINSZ only fills the struct.
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0
        && size.ws_row > 0
        && size.ws_col > 0
    {
        (size.ws_row as usize, size.ws_col as usize)
    } else {
        (24, 80)
    }
}

/// Read one key, waiting at most `timeout`.
#[cfg(unix)]
fn read_key(timeout: std::time::Duration) -> Option<Key> {
    use std::io::Read;
    let mut poll = libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 };
    // SAFETY: one descriptor, and the struct outlives the call.
    let ready = unsafe { libc::poll(&mut poll, 1, timeout.as_millis() as libc::c_int) };
    if ready <= 0 {
        return None;
    }
    let mut buffer = [0u8; 3];
    let read = std::io::stdin().read(&mut buffer).ok()?;
    match &buffer[..read] {
        [b'q'] | [3] => Some(Key::Quit),
        // The arrow keys arrive as CSI A and CSI B.
        [0x1b, b'[', b'A'] => Some(Key::Up),
        [0x1b, b'[', b'B'] => Some(Key::Down),
        _ => None,
    }
}

/// The terminal put into raw mode for as long as this value lives.
#[cfg(unix)]
struct RawMode(libc::termios);

#[cfg(unix)]
impl RawMode {
    fn enter() -> Option<RawMode> {
        // SAFETY: `isatty` only inspects the descriptor.
        if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
            return None;
        }
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: both calls only read or write the struct we own.
        unsafe {
            if libc::tcgetattr(libc::STDIN_FILENO, &mut original) != 0 {
                return None;
            }
            let mut raw = original;
            // No echo and no line buffering: keys have to arrive as they are pressed.
            raw.c_lflag &= !(libc::ICANON | libc::ECHO);
            raw.c_cc[libc::VMIN] = 0;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
                return None;
            }
        }
        Some(RawMode(original))
    }
}

#[cfg(unix)]
impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: restoring the settings this type replaced. Skipping it would leave the
        // user's shell with no echo.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Colour escapes take no space on screen, so a coloured line must measure the same
    /// as the plain one — otherwise wrapping and scrolling are wrong wherever there is
    /// colour, which is everywhere.
    #[test]
    fn ansi_escapes_do_not_count_toward_the_width() {
        assert_eq!(visible_width("plain"), 5);
        assert_eq!(visible_width("\x1b[36mplain\x1b[0m"), 5);
        assert_eq!(visible_width("\x1b[0m"), 0);
        assert_eq!(visible_width(""), 0);
    }

    #[test]
    fn scrolling_stops_at_the_last_screen_line() {
        let view = Viewport { scroll: 0, rows: 3, columns: 10 };
        let text = "a\nb\nc\nd\ne";
        assert_eq!(view.total_lines(text), 5);
        assert_eq!(view.max_scroll(text), 2);
        // A wrapped line counts as the lines it actually takes.
        let wide = "0123456789012345\nb";
        assert_eq!(view.total_lines(wide), 3);
        // Nothing to scroll when everything fits.
        assert_eq!(view.max_scroll("a\nb"), 0);
    }

    #[test]
    fn the_scroll_is_pulled_back_when_the_content_shrinks() {
        let mut view = Viewport { scroll: 7, rows: 3, columns: 80 };
        view.clamp("a\nb\nc\nd\ne");
        assert_eq!(view.scroll, 2);
    }
}
