//! `aws logs tail` — the one custom command that streams.
//!
//! Written directly to stdout: `--output` and `--query` are silently ignored, matching the
//! reference, which never touches the formatter here.
//!
//! Ported from `awscli/customizations/logs/tail.py`. `aws logs start-live-tail` is a
//! *different* command in `startlivetail.py` and is not implemented — in particular
//! `--mode` belongs to that command, not this one.

use crate::args::Parsed;
use crate::client::{Client, Globals};
use crate::exit;
use crate::Failure;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{IsTerminal, Write};
use std::process::ExitCode;

const TIMESTAMP_COLOR: &str = "\x1b[32m"; // colorama.Fore.GREEN
const STREAM_NAME_COLOR: &str = "\x1b[36m"; // colorama.Fore.CYAN
const RESET: &str = "\x1b[0m";
/// The reference sleeps 5 seconds between polls once it has caught up.
const FOLLOW_SLEEP: std::time::Duration = std::time::Duration::from_secs(5);

pub fn run(parsed: &Parsed, globals: &Globals) -> Result<ExitCode, Failure> {
    let options = Options::parse(parsed)?;

    let model = crate::load_model("logs").map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
    let client = Client::new(&model, globals)?;

    let mut request = json!({
        "logGroupName": options.group_name,
        // Always sent, regardless of the other filters.
        "interleaved": true,
    });
    // `--since` is resolved lazily by the reference (inside a generator), so an
    // unparseable value surfaces as a general error at 255 rather than as parameter
    // validation. Resolving it here would change the exit code.
    request["startTime"] = json!(to_epoch_millis(&options.since)?);
    if let Some(pattern) = &options.filter_pattern {
        request["filterPattern"] = json!(pattern);
    }
    if let Some(names) = &options.log_stream_names {
        request["logStreamNames"] = json!(names);
    }
    if let Some(prefix) = &options.log_stream_name_prefix {
        request["logStreamNamePrefix"] = json!(prefix);
    }

    let colorize = match options.color.as_deref() {
        Some("on") => true,
        Some("off") => false,
        // `auto`, and the default, follow whether stdout is a terminal.
        _ => std::io::stdout().is_terminal(),
    };
    let formatter = Formatter { format: options.format, colorize };

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    if options.follow {
        follow(&client, &mut request, &formatter, &mut out)
    } else {
        // Plain pagination: events are printed as pages arrive, not collected first.
        loop {
            let page = client.call("filter-log-events", Some(&request))?;
            for event in page.get("events").and_then(Value::as_array).unwrap_or(&Vec::new()) {
                formatter.write(&mut out, event);
            }
            match page.get("nextToken").and_then(Value::as_str) {
                Some(token) => request["nextToken"] = json!(token),
                None => break,
            }
        }
        Ok(exit::code(exit::SUCCESS))
    }
}

/// The `--follow` loop.
///
/// Dedup is keyed by raw epoch millis, and the map is pruned after each response to only
/// the newest timestamp — so it protects against re-delivery at exactly the newest
/// millisecond and nothing more. That is deliberate: `startTime` is advanced to that same
/// millisecond and the bound is inclusive, so those events would otherwise repeat forever.
fn follow(
    client: &Client<'_>,
    request: &mut Value,
    formatter: &Formatter,
    out: &mut impl Write,
) -> Result<ExitCode, Failure> {
    let mut seen: BTreeMap<i64, BTreeSet<String>> = BTreeMap::new();
    loop {
        let response = client.call("filter-log-events", Some(request))?;
        for event in response.get("events").and_then(Value::as_array).unwrap_or(&Vec::new()) {
            let timestamp = event.get("timestamp").and_then(Value::as_i64).unwrap_or_default();
            let id = event.get("eventId").and_then(Value::as_str).unwrap_or_default().to_string();
            if seen.entry(timestamp).or_default().insert(id) {
                formatter.write(out, event);
            }
        }
        // Keep only the newest timestamp's ids.
        if let Some(&newest) = seen.keys().next_back() {
            let keep = seen.remove(&newest).unwrap_or_default();
            seen.clear();
            seen.insert(newest, keep);
        }

        match response.get("nextToken").and_then(Value::as_str) {
            // More pages available: fetch immediately, without sleeping.
            Some(token) => {
                request["nextToken"] = json!(token);
            }
            None => {
                if let Some(&newest) = seen.keys().next_back() {
                    request["startTime"] = json!(newest);
                }
                if let Some(object) = request.as_object_mut() {
                    object.remove("nextToken");
                }
                let _ = out.flush();
                std::thread::sleep(FOLLOW_SLEEP);
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LogFormat {
    Detailed,
    Short,
    Json,
}

struct Options {
    group_name: String,
    since: String,
    follow: bool,
    format: LogFormat,
    filter_pattern: Option<String>,
    log_stream_names: Option<Vec<String>>,
    log_stream_name_prefix: Option<String>,
    color: Option<String>,
}

impl Options {
    /// Parsed from the raw argv tokens rather than the flag map, because
    /// `--log-stream-names` is `nargs='+'` and takes every following non-flag token.
    fn parse(parsed: &Parsed) -> Result<Options, Failure> {
        let mut options = Options {
            group_name: String::new(),
            since: "10m".to_string(),
            follow: false,
            format: LogFormat::Detailed,
            filter_pattern: None,
            log_stream_names: None,
            log_stream_name_prefix: None,
            color: parsed.color.clone(),
        };

        let tokens = &parsed.extras;
        let mut leftover: Vec<String> = Vec::new();
        let mut i = 0;
        while i < tokens.len() {
            let token = &tokens[i];
            let (name, inline) = match token.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (token.as_str(), None),
            };
            let next_value = |i: &mut usize| -> Option<String> {
                if let Some(v) = inline.clone() {
                    return Some(v);
                }
                let candidate = tokens.get(*i + 1)?;
                if candidate.starts_with("--") {
                    return None;
                }
                *i += 1;
                Some(candidate.clone())
            };
            match name {
                "--since" => options.since = next_value(&mut i).unwrap_or_default(),
                "--follow" => options.follow = true,
                "--format" => {
                    let value = next_value(&mut i).unwrap_or_default();
                    options.format = match value.as_str() {
                        "detailed" => LogFormat::Detailed,
                        "short" => LogFormat::Short,
                        "json" => LogFormat::Json,
                        other => {
                            return Err(Failure::new(
                                exit::PARAM_VALIDATION,
                                format!(
                                    "argument --format: Invalid choice, valid choices are:\n\
                                     \n detailed                                 | short  \
                                     \n json                                     \n\n\
                                     Invalid choice: '{other}'"
                                ),
                            ))
                        }
                    };
                }
                "--filter-pattern" => options.filter_pattern = next_value(&mut i),
                "--log-stream-name-prefix" => options.log_stream_name_prefix = next_value(&mut i),
                "--log-stream-names" => {
                    // nargs='+': consume every following token that is not a flag.
                    let mut names = Vec::new();
                    if let Some(v) = inline.clone() {
                        names.push(v);
                    }
                    while let Some(candidate) = tokens.get(i + 1) {
                        if candidate.starts_with("--") {
                            break;
                        }
                        names.push(candidate.clone());
                        i += 1;
                    }
                    options.log_stream_names = Some(names);
                }
                other => leftover.push(other.to_string()),
            }
            i += 1;
        }

        if !leftover.is_empty() {
            return Err(Failure::new(
                exit::PARAM_VALIDATION,
                aws_cli_runtime::RuntimeError::ParamValidation(format!(
                    "Unknown options: {}",
                    leftover.join(",")
                )),
            ));
        }

        match parsed.positionals.first() {
            Some(group) => options.group_name = group.clone(),
            None => {
                return Err(Failure::new(
                    exit::PARAM_VALIDATION,
                    format!(
                        "{}\n\n{}",
                        aws_cli_runtime::RuntimeError::ParamValidation(
                            "the following arguments are required: group_name".to_string()
                        ),
                        crate::USAGE_HINT
                    ),
                ))
            }
        }
        Ok(options)
    }
}

struct Formatter {
    format: LogFormat,
    colorize: bool,
}

impl Formatter {
    fn paint(&self, text: &str, color: &str) -> String {
        if self.colorize {
            format!("{color}{text}{RESET}")
        } else {
            text.to_string()
        }
    }

    fn write(&self, out: &mut impl Write, event: &Value) {
        let millis = event.get("timestamp").and_then(Value::as_i64).unwrap_or_default();
        let message = event.get("message").and_then(Value::as_str).unwrap_or_default();
        let stream = event.get("logStreamName").and_then(Value::as_str).unwrap_or_default();

        let line = match self.format {
            LogFormat::Short => {
                // `%Y-%m-%dT%H:%M:%S`: no fraction, no offset.
                format!("{} {message}", self.paint(&iso_seconds(millis), TIMESTAMP_COLOR))
            }
            LogFormat::Detailed => {
                // `isoformat(timespec='microseconds')`: always six fractional digits.
                format!(
                    "{} {} {message}",
                    self.paint(&iso_micros(millis), TIMESTAMP_COLOR),
                    self.paint(stream, STREAM_NAME_COLOR)
                )
            }
            LogFormat::Json => {
                // Plain `isoformat()`: the fraction disappears entirely at a whole second.
                format!(
                    "{} {} {}",
                    self.paint(&iso_auto(millis), TIMESTAMP_COLOR),
                    self.paint(stream, STREAM_NAME_COLOR),
                    pretty_json(message)
                )
            }
        };
        // `rstrip()` then one newline: strips ALL trailing whitespace from the assembled
        // line, so a message ending in spaces loses them. Interior newlines survive, which
        // is how multi-line messages stay multi-line.
        let _ = writeln!(out, "{}", line.trim_end());
    }
}

/// A JSON message is re-indented and pushed onto its own line; anything else is verbatim.
fn pretty_json(message: &str) -> String {
    match serde_json::from_str::<Value>(message) {
        Ok(value) => format!("\n{}", crate::custom::python_json(&value)),
        Err(_) => message.to_string(),
    }
}

/// Split epoch millis into the UTC date-time fields the three formats need.
fn parts(millis: i64) -> (String, i64) {
    let seconds = millis.div_euclid(1000);
    let micros = millis.rem_euclid(1000) * 1000;
    // `YYYYMMDDTHHMMSSZ` from the one date routine the workspace has.
    let compact = aws_cli_runtime::sigv4::format_timestamp(seconds);
    let formatted = format!(
        "{}-{}-{}T{}:{}:{}",
        &compact[0..4],
        &compact[4..6],
        &compact[6..8],
        &compact[9..11],
        &compact[11..13],
        &compact[13..15]
    );
    (formatted, micros)
}

fn iso_seconds(millis: i64) -> String {
    parts(millis).0
}

fn iso_micros(millis: i64) -> String {
    let (formatted, micros) = parts(millis);
    format!("{formatted}.{micros:06}+00:00")
}

fn iso_auto(millis: i64) -> String {
    let (formatted, micros) = parts(millis);
    if micros == 0 {
        format!("{formatted}+00:00")
    } else {
        format!("{formatted}.{micros:06}+00:00")
    }
}

/// `--since`: a relative offset, or an absolute timestamp.
///
/// The relative form is `<digits><s|m|h|d|w>` and nothing else — `5h30m`, `5M` and `5 m`
/// all fall through to absolute parsing.
fn to_epoch_millis(value: &str) -> Result<i64, Failure> {
    if let Some(millis) = relative_millis(value) {
        return Ok(millis);
    }
    absolute_millis(value)
}

fn relative_millis(value: &str) -> Option<i64> {
    let (digits, unit) = value.split_at(value.len().checked_sub(1)?);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let seconds_per = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 24 * 3600,
        "w" => 7 * 24 * 3600,
        _ => return None,
    };
    let amount: i64 = digits.parse().ok()?;
    Some((crate::now_unix() - amount * seconds_per) * 1000)
}

/// Absolute timestamps: epoch seconds, or ISO 8601 carrying an explicit offset.
///
/// The reference hands anything non-relative to `dateutil`, which interprets a naive
/// timestamp such as `2026-08-01T10:00:00` in the machine's **local** timezone — while
/// relative offsets are computed in UTC. Reproducing that needs a timezone database we do
/// not carry, and guessing UTC would silently shift the window by the local offset, so a
/// naive timestamp is refused rather than misread.
fn absolute_millis(value: &str) -> Result<i64, Failure> {
    if let Ok(seconds) = value.parse::<f64>() {
        return Ok((seconds * 1000.0) as i64);
    }
    if let Some(millis) = parse_iso8601_with_offset(value) {
        return Ok(millis);
    }
    // A well-formed timestamp carrying no offset is the one case we cannot follow: the
    // reference reads it in the machine's LOCAL timezone (while relative offsets are
    // computed in UTC), and reproducing that needs a timezone database we do not carry.
    // Assuming UTC would silently shift the window by the local offset, so it is refused.
    if looks_like_naive_timestamp(value) {
        return Err(Failure::new(
            exit::GENERAL_ERROR,
            format!(
                "Invalid timestamp \"{value}\": a timestamp without a UTC offset is read \
                 in local time by the reference, which needs a timezone database this \
                 build does not carry. Add an explicit offset (for example \
                 \"{value}Z\"), or use a relative offset such as 5m."
            ),
        ));
    }
    // Otherwise the value is simply unparseable, and dateutil's wording applies.
    Err(Failure::new(
        exit::GENERAL_ERROR,
        format!("Invalid timestamp \"{value}\": Unknown string format: {value}"),
    ))
}

/// `YYYY-MM-DD` optionally followed by a time, with no trailing offset.
fn looks_like_naive_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 10 {
        return false;
    }
    let digits = |range: std::ops::Range<usize>| {
        value.get(range).is_some_and(|s| s.bytes().all(|b| b.is_ascii_digit()))
    };
    digits(0..4)
        && bytes[4] == b'-'
        && digits(5..7)
        && bytes[7] == b'-'
        && digits(8..10)
        && !value.ends_with('Z')
        && !value.ends_with('z')
}

fn parse_iso8601_with_offset(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<i64> { value.get(range)?.parse().ok() };
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    if bytes[4] != b'-' || bytes[7] != b'-' || !(bytes[10] == b'T' || bytes[10] == b' ') {
        return None;
    }
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    let rest = &value[19..];
    // An optional fractional part, then a mandatory offset.
    let rest = match rest.strip_prefix('.') {
        Some(after) => {
            let digits = after.bytes().take_while(u8::is_ascii_digit).count();
            &after[digits..]
        }
        None => rest,
    };
    let offset_seconds = if rest == "Z" || rest == "z" {
        0
    } else {
        let sign = match rest.as_bytes().first()? {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        let body = &rest[1..];
        let (oh, om) = match body.split_once(':') {
            Some((h, m)) => (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?),
            None if body.len() == 4 => {
                (body[0..2].parse::<i64>().ok()?, body[2..4].parse::<i64>().ok()?)
            }
            _ => return None,
        };
        sign * (oh * 3600 + om * 60)
    };
    Some((days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second
        - offset_seconds)
        * 1000)
}

/// Howard Hinnant's `days_from_civil`, the inverse of the routine in `sigv4`.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three formats differ in exactly how the timestamp is rendered, which is easy to
    /// get wrong: `short` has no offset, `detailed` always shows six digits, and `json`
    /// drops the fraction entirely at a whole second.
    #[test]
    fn renders_each_timestamp_format() {
        // 2026-08-13T21:48:16.456Z
        let millis = 1_786_657_696_456;
        assert_eq!(iso_seconds(millis), "2026-08-13T21:48:16");
        assert_eq!(iso_micros(millis), "2026-08-13T21:48:16.456000+00:00");
        assert_eq!(iso_auto(millis), "2026-08-13T21:48:16.456000+00:00");

        let whole = 1_786_657_696_000;
        assert_eq!(iso_auto(whole), "2026-08-13T21:48:16+00:00");
        assert_eq!(iso_micros(whole), "2026-08-13T21:48:16.000000+00:00");
    }

    /// Only `<digits><unit>` is relative; everything else falls through.
    #[test]
    fn recognises_only_the_exact_relative_form() {
        assert!(relative_millis("5m").is_some());
        assert!(relative_millis("90s").is_some());
        assert!(relative_millis("1w").is_some());
        assert!(relative_millis("5h30m").is_none());
        assert!(relative_millis("5M").is_none());
        assert!(relative_millis("-5m").is_none());
        assert!(relative_millis("5 m").is_none());
        assert!(relative_millis("m").is_none());
    }

    #[test]
    fn parses_absolute_timestamps_that_carry_an_offset() {
        assert_eq!(parse_iso8601_with_offset("2026-08-13T21:48:16Z"), Some(1_786_657_696_000));
        assert_eq!(
            parse_iso8601_with_offset("2026-08-13T21:48:16.456Z"),
            Some(1_786_657_696_000)
        );
        // +01:00 means the instant is an hour earlier in UTC.
        assert_eq!(
            parse_iso8601_with_offset("2026-08-13T22:48:16+01:00"),
            Some(1_786_657_696_000)
        );
        assert_eq!(parse_iso8601_with_offset("2026-08-13T22:48:16+0100"), Some(1_786_657_696_000));
        // No offset: refused rather than assumed to be UTC.
        assert_eq!(parse_iso8601_with_offset("2026-08-13T21:48:16"), None);
    }

    /// Bare numbers are epoch seconds, which is dateutil's first attempt too.
    #[test]
    fn treats_bare_numbers_as_epoch_seconds() {
        assert_eq!(to_epoch_millis("1786657696").unwrap(), 1_786_657_696_000);
    }

    #[test]
    fn round_trips_the_civil_date_conversion() {
        for seconds in [0_i64, 1_786_657_696, 1_709_164_800] {
            let compact = aws_cli_runtime::sigv4::format_timestamp(seconds);
            let iso = format!(
                "{}-{}-{}T{}:{}:{}Z",
                &compact[0..4],
                &compact[4..6],
                &compact[6..8],
                &compact[9..11],
                &compact[11..13],
                &compact[13..15]
            );
            assert_eq!(parse_iso8601_with_offset(&iso), Some(seconds * 1000), "{iso}");
        }
    }

    fn event() -> Value {
        json!({
            "timestamp": 1_786_657_696_456_i64,
            "logStreamName": "my-stream",
            "message": "hello   ",
            "eventId": "1",
        })
    }

    /// Trailing whitespace is stripped from the assembled line; colour is opt-in.
    #[test]
    fn formats_a_detailed_line() {
        let mut out = Vec::new();
        Formatter { format: LogFormat::Detailed, colorize: false }.write(&mut out, &event());
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "2026-08-13T21:48:16.456000+00:00 my-stream hello\n"
        );
    }

    #[test]
    fn formats_a_short_line_with_colour() {
        let mut out = Vec::new();
        Formatter { format: LogFormat::Short, colorize: true }.write(&mut out, &event());
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b[32m2026-08-13T21:48:16\x1b[0m hello\n"
        );
    }

    /// A JSON message is re-indented onto its own line, leaving a trailing space after the
    /// stream name — the reference emits that space too.
    #[test]
    fn formats_a_json_message() {
        let mut out = Vec::new();
        let mut e = event();
        e["message"] = json!(r#"{"a":1}"#);
        Formatter { format: LogFormat::Json, colorize: false }.write(&mut out, &e);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "2026-08-13T21:48:16.456000+00:00 my-stream \n{\n    \"a\": 1\n}\n"
        );
    }

    /// A message that is not JSON is passed through untouched.
    #[test]
    fn leaves_non_json_messages_alone() {
        let mut out = Vec::new();
        let mut e = event();
        e["message"] = json!("not json {");
        Formatter { format: LogFormat::Json, colorize: false }.write(&mut out, &e);
        assert!(String::from_utf8(out).unwrap().ends_with(" my-stream not json {\n"));
    }

    /// Interior newlines survive, so a multi-line event stays multi-line with no prefix on
    /// the continuation lines.
    #[test]
    fn preserves_interior_newlines() {
        let mut out = Vec::new();
        let mut e = event();
        e["message"] = json!("line one\nline two\n\n");
        Formatter { format: LogFormat::Short, colorize: false }.write(&mut out, &e);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "2026-08-13T21:48:16 line one\nline two\n"
        );
    }
}
