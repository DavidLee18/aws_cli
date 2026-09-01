//! The config-file writer, ported from `customizations/configure/writer.py`.
//!
//! Its defining property is that it is **not** an INI serialiser: it edits the file as
//! lines of text. Comments, blank lines, unusual spacing and the ordering of everything
//! it was not asked to change all survive, because nothing is ever re-rendered from a
//! parsed model. `aws configure set region X` on a hand-maintained config must give the
//! file back with one line different, and a round-tripping parser cannot promise that.
//!
//! Verified against the reference by writing the same four values into the same starting
//! file: an updated key keeps its position, a new key lands after the last option of its
//! section, and a new section is appended with no blank line before it.

use std::collections::BTreeMap;
use std::path::Path;

/// A value to write: either a scalar, or the indented sub-block that `sso-session` and
/// nested `services` entries use.
#[derive(Debug, Clone)]
pub enum Setting {
    /// `key = value`.
    Value(String),
    /// `key =` followed by `    subkey = subvalue` lines.
    Nested(BTreeMap<String, String>),
    /// Remove the line entirely -- the writer's answer to a `None` value.
    ///
    /// Nothing constructs this yet: its only caller in the reference is the interactive
    /// `aws configure` prompt, which drops `aws_session_token` when the new access key is
    /// a long-term one. Kept and tested rather than left out, because it is part of the
    /// writer's contract and the prompt is the next thing to land here.
    #[allow(dead_code)]
    Remove,
}

/// The edit to apply: which section, and which keys within it.
pub struct Update {
    pub section: String,
    pub values: Vec<(String, Setting)>,
}

#[derive(Debug)]
pub enum WriteError {
    Invalid(String),
    Io { path: String, source: std::io::Error },
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::Invalid(message) => f.write_str(message),
            WriteError::Io { path, source } => write!(f, "Unable to write to {path}: {source}"),
        }
    }
}

/// Neither a key, a value nor a section name may contain a line break: a newline would
/// silently split one setting into two lines, and the second would be read back as a
/// different key -- or as a section header. The reference rejects it for the same reason,
/// and deliberately does NOT echo the offending value, so a secret cannot leak to stderr
/// through an error message.
fn reject_line_breaks(value: &str, label: &str, echo: bool) -> Result<(), WriteError> {
    if !value.contains('\n') && !value.contains('\r') {
        return Ok(());
    }
    Err(WriteError::Invalid(if echo {
        format!("Invalid {label}: newline characters and carriage returns are not allowed: {value:?}")
    } else {
        format!("Invalid value for key {label}: newline characters and carriage returns are not allowed.")
    }))
}

impl Update {
    fn validate(&self) -> Result<(), WriteError> {
        reject_line_breaks(&self.section, "section name", true)?;
        for (key, setting) in &self.values {
            reject_line_breaks(key, "key", true)?;
            match setting {
                Setting::Value(v) => reject_line_breaks(v, key, false)?,
                Setting::Nested(map) => {
                    for (sub_key, sub_value) in map {
                        reject_line_breaks(sub_key, "key", true)?;
                        reject_line_breaks(sub_value, key, false)?;
                    }
                }
                Setting::Remove => {}
            }
        }
        Ok(())
    }
}

/// Apply an update to a config file, creating it (and its directory) if absent.
pub fn update_config(update: &Update, path: &Path) -> Result<(), WriteError> {
    update.validate()?;

    let io = |source| WriteError::Io { path: path.display().to_string(), source };

    if !path.is_file() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        let text = render_new_section(update, false);
        write_private(path, &text)?;
        return Ok(());
    }

    let existing = std::fs::read_to_string(path).map_err(io)?;
    let mut lines: Vec<String> = split_keeping_newlines(&existing);

    match update_section(&mut lines, update) {
        true => std::fs::write(path, lines.concat()).map_err(io),
        // No such section: append it, preceded by a newline only when the file does not
        // already end with one.
        false => {
            let needs_newline = !existing.is_empty() && !existing.ends_with('\n');
            let appended = format!("{existing}{}", render_new_section(update, needs_newline));
            std::fs::write(path, appended).map_err(io)
        }
    }
}

/// Create with mode 0600, since this is where credentials go.
fn write_private(path: &Path, text: &str) -> Result<(), WriteError> {
    let io = |source| WriteError::Io { path: path.display().to_string(), source };
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(io)?;
        file.write_all(text.as_bytes()).map_err(io)
    }
    #[cfg(not(unix))]
    std::fs::write(path, text).map_err(io)
}

fn split_keeping_newlines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text.split_inclusive('\n').map(str::to_string).collect();
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn render_new_section(update: &Update, leading_newline: bool) -> String {
    let mut out = String::new();
    if leading_newline {
        out.push('\n');
    }
    out.push_str(&format!("[{}]\n", update.section));
    out.push_str(&render_values(&update.values, ""));
    out
}

fn render_values(values: &[(String, Setting)], indent: &str) -> String {
    let mut out = String::new();
    for (key, setting) in values {
        match setting {
            Setting::Value(v) => out.push_str(&format!("{indent}{key} = {v}\n")),
            Setting::Nested(map) => {
                out.push_str(&format!("{indent}{key} =\n"));
                for (sub_key, sub_value) in map {
                    out.push_str(&format!("{indent}    {sub_key} = {sub_value}\n"));
                }
            }
            // A removal has no line to write when the key was not already there.
            Setting::Remove => {}
        }
    }
    out
}

/// `[header]`, ignoring anything after it on the line, as the reference's regex does.
fn section_header(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix('[')?;
    rest.split(']').next()
}

/// `key = value` or `key : value`. A line whose first character is a separator is not an
/// option, which is what stops `= x` being read as an empty key.
fn option_key(line: &str) -> Option<&str> {
    if line.starts_with(['=', ':']) {
        return None;
    }
    let index = line.find(['=', ':'])?;
    Some(&line[..index])
}

/// Does this header line name the section? A section with a space in its name may be
/// written either quoted or bare -- `[profile my dev]` and `[profile "my dev"]` are the
/// same section, and the reference accepts both.
fn matches_section(header: &str, section: &str) -> bool {
    if header == section {
        return true;
    }
    match section.split_once(' ') {
        Some((kind, name)) => header == format!("{kind} \"{name}\""),
        None => false,
    }
}

/// Edit the named section in place. Returns false when the section is not in the file.
fn update_section(lines: &mut Vec<String>, update: &Update) -> bool {
    let Some(start) = lines.iter().position(|line| {
        // A commented-out header is text, not a section.
        !line.trim_start().starts_with(['#', ';'])
            && section_header(line).is_some_and(|h| matches_section(h, &update.section))
    }) else {
        return false;
    };

    let mut remaining: Vec<(String, Setting)> = update.values.clone();
    // Where a new key would go: after the last option line seen so far, so additions land
    // at the end of the section rather than jumping above existing settings.
    let mut last_option = start;

    let mut i = start + 1;
    while i < lines.len() {
        if section_header(&lines[i]).is_some() {
            break;
        }
        if let Some(key) = option_key(&lines[i]) {
            let key = key.trim().to_string();
            last_option = i;
            if let Some(position) = remaining.iter().position(|(k, _)| *k == key) {
                let (_, setting) = remaining.remove(position);
                match setting {
                    Setting::Value(v) => lines[i] = format!("{key} = {v}\n"),
                    Setting::Remove => lines[i] = String::new(),
                    // A nested value rewrites the sub-block that follows it.
                    Setting::Nested(map) => {
                        let indent = lines[i].len() - lines[i].trim_start().len();
                        i = update_subattributes(lines, i, &key, map, indent);
                        continue;
                    }
                }
            }
        }
        i += 1;
    }

    if !remaining.is_empty() {
        // A file whose last line has no newline would otherwise splice the new key onto
        // the end of it.
        if lines.last().is_some_and(|l| !l.is_empty() && !l.ends_with('\n')) {
            lines.push("\n".to_string());
        }
        lines.insert(last_option + 1, render_values(&remaining, ""));
    }
    true
}

/// Rewrite the indented block under `key =`, updating the sub-keys present and appending
/// the ones that are not.
fn update_subattributes(
    lines: &mut Vec<String>,
    key_line: usize,
    key: &str,
    mut values: BTreeMap<String, String>,
    parent_indent: usize,
) -> usize {
    lines[key_line] = format!("{}{key} =\n", " ".repeat(parent_indent));

    let mut i = key_line + 1;
    while i < lines.len() {
        if section_header(&lines[i]).is_some() {
            break;
        }
        let Some(raw_key) = option_key(&lines[i]) else { break };
        let indent = raw_key.len() - raw_key.trim_start().len();
        // Back at the parent's own indent: the sub-block has ended.
        if indent <= parent_indent {
            break;
        }
        let sub_key = raw_key.trim().to_string();
        if let Some(value) = values.remove(&sub_key) {
            lines[i] = format!("{}{sub_key} = {value}\n", " ".repeat(indent));
        }
        i += 1;
    }

    if !values.is_empty() {
        let block: String =
            values.iter().map(|(k, v)| format!("    {k} = {v}\n")).collect();
        lines.insert(i, block);
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scalar(section: &str, pairs: &[(&str, &str)]) -> Update {
        Update {
            section: section.to_string(),
            values: pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), Setting::Value((*v).to_string())))
                .collect(),
        }
    }

    /// A unique file per call. Tests run in parallel in one process, so a shared or
    /// time-derived name races: two tests seed the same path and each reads the other's
    /// content back. That produced failures that moved between runs.
    fn temp_path() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!("awsc-writer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join(format!("cfg-{}", COUNTER.fetch_add(1, Ordering::Relaxed)))
    }

    fn apply(start: &str, update: &Update) -> String {
        let path = temp_path();
        std::fs::write(&path, start).expect("seed");
        update_config(update, &path).expect("update");
        let out = std::fs::read_to_string(&path).expect("read back");
        let _ = std::fs::remove_file(&path);
        out
    }

    /// The whole point of editing lines rather than re-serialising: a comment and an
    /// oddly-spaced key that were not asked about come back untouched.
    #[test]
    fn everything_not_asked_about_survives() {
        let start = "[default]\n# keep me\nregion = us-west-2\noutput   = json\n";
        let out = apply(start, &scalar("default", &[("region", "ap-northeast-1")]));
        assert_eq!(out, "[default]\n# keep me\nregion = ap-northeast-1\noutput   = json\n");
    }

    /// A new key lands after the last option of its section, not at the top and not in
    /// the section after it.
    #[test]
    fn a_new_key_lands_at_the_end_of_its_section() {
        let start = "[default]\nregion = us-west-2\n\n[profile dev]\nregion = eu-west-1\n";
        let out = apply(start, &scalar("default", &[("output", "table")]));
        assert_eq!(
            out,
            "[default]\nregion = us-west-2\noutput = table\n\n[profile dev]\nregion = eu-west-1\n"
        );
    }

    /// An absent section is appended, with no blank line inserted before it -- matching
    /// the reference, which adds one only when the file does not end in a newline.
    #[test]
    fn a_missing_section_is_appended() {
        let out = apply("[default]\nregion = us-west-2\n", &scalar("profile other", &[("region", "sa-east-1")]));
        assert_eq!(out, "[default]\nregion = us-west-2\n[profile other]\nregion = sa-east-1\n");
    }

    #[test]
    fn a_file_not_ending_in_a_newline_gains_one() {
        let out = apply("[default]\nregion = us-west-2", &scalar("profile other", &[("region", "sa-east-1")]));
        assert_eq!(out, "[default]\nregion = us-west-2\n[profile other]\nregion = sa-east-1\n");
    }

    /// A quoted header names the same section as a bare one, so setting a value in
    /// `profile my dev` must not append a second, duplicate section.
    #[test]
    fn a_quoted_section_header_matches_the_bare_name() {
        let start = "[profile \"my dev\"]\nregion = eu-west-1\n";
        let out = apply(start, &scalar("profile my dev", &[("region", "us-east-1")]));
        assert_eq!(out, "[profile \"my dev\"]\nregion = us-east-1\n");
    }

    /// A commented-out header is text. Treating it as a section would write the value
    /// into a block the config parser never sees.
    #[test]
    fn a_commented_header_is_not_a_section() {
        let start = "#[default]\n[profile dev]\nregion = eu-west-1\n";
        let out = apply(start, &scalar("default", &[("region", "us-east-1")]));
        assert_eq!(out, "#[default]\n[profile dev]\nregion = eu-west-1\n[default]\nregion = us-east-1\n");
    }

    #[test]
    fn a_removal_deletes_the_line() {
        let start = "[default]\nregion = us-west-2\naws_session_token = stale\n";
        let update = Update {
            section: "default".into(),
            values: vec![("aws_session_token".into(), Setting::Remove)],
        };
        assert_eq!(apply(start, &update), "[default]\nregion = us-west-2\n");
    }

    /// A newline in a value would split it into two settings, and the second could be
    /// read back as a section header. The message must not echo the value, which may be
    /// a secret.
    /// A newline in a value would split it into two settings, and the second could be
    /// read back as a section header -- so it is refused. The message names the key but
    /// must NOT echo the value, which is very often a secret.
    #[test]
    fn a_value_containing_a_newline_is_refused_without_echoing_it() {
        let smuggled = "hunter2\n[default]\nregion = attacker";
        let update = scalar("default", &[("aws_secret_access_key", smuggled)]);
        let err = update.validate().expect_err("must refuse");
        let text = err.to_string();
        assert!(text.contains("aws_secret_access_key"), "the key should be named: {text}");
        assert!(!text.contains("hunter2"), "the value must not be echoed: {text}");
        assert!(!text.contains("attacker"), "the value must not be echoed: {text}");
    }
}
