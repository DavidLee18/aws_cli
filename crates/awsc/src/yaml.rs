//! A YAML reader for `--cli-input-yaml`.
//!
//! The reference loads this argument with `ruamel.yaml`'s `YAML(typ='safe', pure=True)`
//! (`customizations/cliinput.py:150-155`), which is **YAML 1.2**, not the YAML 1.1 that
//! PyYAML's `safe_load` implements. The difference is not cosmetic and decides what a
//! document *means*:
//!
//! | text | YAML 1.1 (PyYAML) | YAML 1.2 (here, and the reference) |
//! |---|---|---|
//! | `yes` / `no` / `on` / `off` | booleans | strings |
//! | `017` | octal 15 | decimal 17 |
//! | `1:30` | sexagesimal 90 | string |
//!
//! So `--cli-input-yaml 'DryRun: no'` sends the string `"no"`, not `false`. Implementing
//! the 1.1 rules from memory would have produced a parser that reads plausible documents
//! and quietly sends different values than `aws` does.
//!
//! The scalar rules below are transcribed from ruamel's own resolver table for version
//! 1.2 plus its constructors, which disagree in one place worth knowing: `017` matches
//! the *octal* branch of the integer regex, and is then constructed by Python's `int()`,
//! which reads it as decimal. Construction wins.
//!
//! **What is deliberately not implemented** is refused by name rather than approximated,
//! because a reader that silently ignores a construct is worse than one that stops:
//! explicit keys (`? key`), `%YAML` directives (which can switch the document back to
//! 1.1 and change every scalar under it), and the `!!set` / `!!omap` tags.
//!
//! One divergence is unavoidable: ruamel resolves `2020-01-01` to a `datetime.date`, and
//! JSON has no date. Timestamps stay strings here, which is what every AWS timestamp
//! member accepts anyway — see `docs/divergences.md`.

use serde_json::{Map, Value};
use std::collections::HashMap;

/// Why a document could not be read.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// Not valid YAML. The caller reports this exactly as the reference does, with no
    /// detail, so the wording cannot drift: `Invalid YAML received.`
    Invalid(String),
    /// Valid YAML that this reader does not implement. Reported in full, because the
    /// reference *would* have accepted it and the user needs to know why we did not.
    Unsupported(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Invalid(m) => write!(f, "{m}"),
            Error::Unsupported(m) => write!(f, "{m}"),
        }
    }
}

fn invalid<T>(message: impl Into<String>) -> Result<T, Error> {
    Err(Error::Invalid(message.into()))
}

fn unsupported<T>(message: impl Into<String>) -> Result<T, Error> {
    Err(Error::Unsupported(message.into()))
}

/// Read one YAML document.
///
/// An empty document is `null`, as it is in the reference — not an error and not an empty
/// map, which matters because the caller then reports `Invalid type: expecting map`.
pub fn parse(text: &str) -> Result<Value, Error> {
    Parser::new(text)?.document()
}

/// One physical line, split once so indentation is not re-measured at every decision.
#[derive(Clone, Debug)]
struct Line<'a> {
    indent: usize,
    /// Everything after the indentation, trailing whitespace removed. Comments are
    /// **not** stripped here: inside a block scalar a `#` is content.
    text: &'a str,
    /// Did this line actually end with a newline in the source? Only the last one can
    /// fail to, and it decides whether a block scalar keeps a trailing newline:
    /// `a: |\n  x` is `"x"`, while `a: |\n  x\n` is `"x\n"`.
    terminated: bool,
}

impl Line<'_> {
    fn is_blank(&self) -> bool {
        self.text.is_empty()
    }

    fn is_comment(&self) -> bool {
        self.text.starts_with('#')
    }

    fn ignorable(&self) -> bool {
        self.is_blank() || self.is_comment()
    }
}

struct Parser<'a> {
    lines: Vec<Line<'a>>,
    pos: usize,
    /// `&name` targets, by name. A YAML anchor is a value, not a reference: an alias
    /// yields a copy, so mutating one does not disturb the other.
    anchors: HashMap<String, Value>,
}

impl<'a> Parser<'a> {
    fn new(text: &'a str) -> Result<Self, Error> {
        let mut lines = Vec::new();
        // `split_inclusive` rather than `lines`, which discards the one bit of
        // information a block scalar's trailing newline depends on.
        for raw in text.split_inclusive('\n') {
            let terminated = raw.ends_with('\n');
            let trimmed_end = raw.trim_end();
            let indent = trimmed_end.len() - trimmed_end.trim_start_matches(' ').len();
            let body = &trimmed_end[indent..];
            // A tab in the indentation is an error in every YAML implementation, and a
            // silently accepted one would shift a whole block into the wrong parent.
            if body.starts_with('\t') {
                return invalid("found character '\\t' that cannot start any token");
            }
            lines.push(Line { indent, text: body, terminated });
        }
        Ok(Parser { lines, pos: 0, anchors: HashMap::new() })
    }

    fn peek(&self) -> Option<&Line<'a>> {
        self.lines.get(self.pos)
    }

    fn advance(&mut self) {
        self.pos += 1;
    }

    /// Skip blank and comment lines. Only ever called in block context — inside a block
    /// scalar or a quoted scalar these lines are content.
    fn skip_ignorable(&mut self) {
        while self.peek().is_some_and(Line::ignorable) {
            self.advance();
        }
    }

    /// The whole stream: directives, one document, and nothing after it.
    fn document(&mut self) -> Result<Value, Error> {
        self.skip_ignorable();
        if let Some(line) = self.peek() {
            if line.text.starts_with('%') {
                // `%YAML 1.1` switches the version back, and with it the meaning of every
                // `yes`, `no` and `017` below. Guessing here is exactly the silent-wrong
                // answer this module exists to avoid.
                return unsupported(format!(
                    "a YAML directive ({}) is not supported by --cli-input-yaml",
                    line.text
                ));
            }
        }
        // A leading `---` is optional and carries no meaning on its own.
        if self.peek().is_some_and(|l| l.indent == 0 && (l.text == "---" || l.text.starts_with("--- ")))
        {
            let rest = self.lines[self.pos].text[3..].trim();
            if rest.is_empty() {
                self.advance();
            } else {
                // `--- a: 1` puts the document's first node on the marker line.
                let terminated = self.lines[self.pos].terminated;
                self.lines[self.pos] = Line { indent: 4, text: rest, terminated };
            }
        }

        let value = self.node(self.current_indent())?;

        // Anything left is either a second document or content the parser did not
        // understand; both are errors, and `...` ends the stream.
        self.skip_ignorable();
        if let Some(line) = self.peek() {
            if line.text == "..." {
                self.advance();
                self.skip_ignorable();
                if self.peek().is_some() {
                    return invalid("expected a single document in the stream");
                }
                return Ok(value);
            }
            return invalid("expected a single document in the stream");
        }
        Ok(value)
    }

    fn current_indent(&self) -> usize {
        self.peek().map(|l| l.indent).unwrap_or(0)
    }

    /// A block node at `indent`: a sequence, a mapping, or a bare scalar.
    fn node(&mut self, indent: usize) -> Result<Value, Error> {
        self.skip_ignorable();
        let Some(line) = self.peek() else { return Ok(Value::Null) };
        if line.indent != indent {
            return Ok(Value::Null);
        }
        if is_sequence_entry(line.text) {
            return self.sequence(indent);
        }
        if split_key(line.text)?.is_some() {
            return self.mapping(indent);
        }
        // A document whose whole content is one scalar: `--cli-input-yaml 'hello'`.
        let text = line.text;
        self.advance();
        self.inline(text, indent)
    }

    /// A block mapping: entries at exactly `indent`, ending at the first shallower line.
    fn mapping(&mut self, indent: usize) -> Result<Value, Error> {
        let mut map = Map::new();
        let mut merges: Vec<Value> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        loop {
            self.skip_ignorable();
            let Some(line) = self.peek() else { break };
            if line.indent < indent {
                break;
            }
            if line.indent > indent {
                return invalid("mapping values are not allowed here");
            }
            let Some((key_text, rest)) = split_key(line.text)? else { break };
            let value = self.entry_value(rest, indent)?;

            if key_text == "<<" {
                merges.push(value);
                continue;
            }
            let resolved = scalar(key_text)?;
            let key = key_to_string(&resolved);
            // ruamel's safe loader raises DuplicateKeyError rather than letting the last
            // one win, so a typo is reported instead of silently dropping a parameter.
            if !seen.insert(python_key(&resolved)) {
                return invalid(format!("found duplicate key {key}"));
            }
            map.insert(key, value);
        }

        // `<<` fills in keys the mapping does not already set, in the order merged.
        for merge in merges {
            let sources = match merge {
                Value::Array(items) => items,
                other => vec![other],
            };
            for source in sources {
                let Value::Object(fields) = source else {
                    return invalid("merge key expects a mapping");
                };
                for (key, value) in fields {
                    map.entry(key).or_insert(value);
                }
            }
        }
        Ok(Value::Object(map))
    }

    /// The value of `key:`, which is either on the same line or in the block below it.
    fn entry_value(&mut self, rest: &'a str, indent: usize) -> Result<Value, Error> {
        let rest = rest.trim_start();
        if rest.is_empty() || rest.starts_with('#') {
            self.advance();
            self.skip_ignorable();
            return match self.peek() {
                Some(line) if line.indent > indent => {
                    let child = line.indent;
                    self.node(child)
                }
                // `a:` followed by `- x` at the *same* indentation is still a's value:
                // a block sequence may sit at its parent key's column.
                Some(line) if line.indent == indent && is_sequence_entry(line.text) => {
                    self.sequence(indent)
                }
                _ => Ok(Value::Null),
            };
        }
        self.advance();
        self.inline(rest, indent)
    }

    /// A block sequence: `- item` entries at exactly `indent`.
    fn sequence(&mut self, indent: usize) -> Result<Value, Error> {
        let mut items = Vec::new();
        loop {
            self.skip_ignorable();
            let Some(line) = self.peek() else { break };
            if line.indent != indent || !is_sequence_entry(line.text) {
                break;
            }
            let after_dash = &line.text[1..];
            let lead = after_dash.len() - after_dash.trim_start_matches(' ').len();
            let rest = &after_dash[lead..];
            // The column the item's own content starts at, which is the indentation its
            // continuation lines must use: `- a: 1` / `  b: 2`.
            let item_indent = indent + 1 + lead;

            if rest.is_empty() || rest.starts_with('#') {
                self.advance();
                self.skip_ignorable();
                let value = match self.peek() {
                    Some(next) if next.indent > indent => {
                        let child = next.indent;
                        self.node(child)?
                    }
                    _ => Value::Null,
                };
                items.push(value);
                continue;
            }

            // A nested collection whose first line shares the dash line. Rewriting the
            // line as if it stood on its own is what lets one parser handle `- - 1` and
            // `- a: 1` without a second, subtly different code path.
            if is_sequence_entry(rest) {
                let terminated = self.lines[self.pos].terminated;
                self.lines[self.pos] = Line { indent: item_indent, text: rest, terminated };
                items.push(self.sequence(item_indent)?);
                continue;
            }
            if split_key(rest)?.is_some() {
                let terminated = self.lines[self.pos].terminated;
                self.lines[self.pos] = Line { indent: item_indent, text: rest, terminated };
                items.push(self.mapping(item_indent)?);
                continue;
            }

            self.advance();
            items.push(self.inline(rest, indent)?);
        }
        Ok(Value::Array(items))
    }

    /// A value written on the line that introduced it. The line has already been
    /// consumed; multi-line forms (flow collections, quoted and plain scalars that carry
    /// on, block scalars) take the lines that follow.
    fn inline(&mut self, text: &'a str, parent_indent: usize) -> Result<Value, Error> {
        let text = text.trim();

        // `&anchor`, `!tag`, or both, in either order, ahead of the value proper.
        let mut anchor: Option<String> = None;
        let mut tag: Option<String> = None;
        let mut rest = text;
        loop {
            if let Some(body) = rest.strip_prefix('&') {
                let (name, tail) = split_token(body);
                if name.is_empty() {
                    return invalid("expected an anchor name");
                }
                anchor = Some(name.to_string());
                rest = tail.trim_start();
                continue;
            }
            if rest.starts_with('!') {
                let (name, tail) = split_token(rest);
                tag = Some(name.to_string());
                rest = tail.trim_start();
                continue;
            }
            break;
        }

        let value = if rest.is_empty() {
            // `key: &anchor` with the value in the block below.
            self.skip_ignorable();
            match self.peek() {
                Some(line) if line.indent > parent_indent => {
                    let child = line.indent;
                    self.node(child)?
                }
                _ => Value::Null,
            }
        } else if let Some(name) = rest.strip_prefix('*') {
            let (name, tail) = split_token(name);
            if !tail.trim().is_empty() {
                return invalid("unexpected content after an alias");
            }
            match self.anchors.get(name) {
                Some(value) => value.clone(),
                None => return invalid(format!("found undefined alias {name}")),
            }
        } else if is_sequence_entry(rest) {
            // `key: - 1`. A sequence has to start on its own line, and reading this as
            // the string "- 1" would turn a malformed document into a plausible value.
            return invalid("sequence entries are not allowed here");
        } else if rest.starts_with('|') || rest.starts_with('>') {
            self.block_scalar(rest, parent_indent)?
        } else if rest.starts_with('[') || rest.starts_with('{') {
            let joined = self.gather_flow(rest)?;
            flow(&joined)?
        } else if rest.starts_with('\'') || rest.starts_with('"') {
            let joined = self.gather_quoted(rest)?;
            let (value, tail) = read_quoted(&joined)?;
            if !comment_stripped(tail).trim().is_empty() {
                return invalid("unexpected content after a quoted scalar");
            }
            Value::String(value)
        } else {
            let folded = self.gather_plain(rest, parent_indent);
            scalar(&folded)?
        };

        let value = match &tag {
            Some(tag) => apply_tag(tag, value)?,
            None => value,
        };
        if let Some(name) = anchor {
            self.anchors.insert(name, value.clone());
        }
        Ok(value)
    }

    /// Collect a flow collection that may run past the end of its first line.
    fn gather_flow(&mut self, first: &str) -> Result<String, Error> {
        let mut joined = String::new();
        let mut depth = 0usize;
        let mut chunk = comment_stripped(first);
        loop {
            depth = flow_depth(&chunk, depth)?;
            if !joined.is_empty() {
                joined.push(' ');
            }
            joined.push_str(chunk.trim());
            if depth == 0 {
                return Ok(joined);
            }
            let Some(line) = self.peek() else {
                return invalid("expected the end of a flow collection");
            };
            if line.ignorable() {
                self.advance();
                chunk = String::new();
                continue;
            }
            chunk = comment_stripped(line.text);
            self.advance();
        }
    }

    /// Collect a quoted scalar that may run past the end of its first line.
    fn gather_quoted(&mut self, first: &str) -> Result<String, Error> {
        let mut joined = String::from(first);
        while !quoted_is_closed(&joined) {
            let Some(line) = self.peek() else {
                return invalid("expected the end of a quoted scalar");
            };
            // A line break inside a quoted scalar folds to a space, and a blank line to a
            // newline — the same folding a plain scalar gets.
            joined.push('\n');
            joined.push_str(line.text);
            self.advance();
        }
        Ok(joined)
    }

    /// A plain scalar plus any continuation lines.
    ///
    /// A more-indented line carries on the scalar even when it looks like structure:
    /// `a:\n- x\n  - y` is the one-item sequence `["x - y"]`, not a nested list. Only a
    /// mapping separator ends it — and ends the document, since that is an error.
    /// One line break folds to a space; a blank line between two continuation lines
    /// becomes a newline.
    fn gather_plain(&mut self, first: &str, parent_indent: usize) -> String {
        let mut text = comment_stripped(first).trim().to_string();
        loop {
            // Blank lines only count once something follows them at a deeper indent.
            let mut lookahead = self.pos;
            let mut breaks = 1usize;
            while self.lines.get(lookahead).is_some_and(Line::is_blank) {
                breaks += 1;
                lookahead += 1;
            }
            let Some(line) = self.lines.get(lookahead) else { break };
            if line.is_comment() || line.indent <= parent_indent {
                break;
            }
            // A `: ` anywhere in a continuation line ends the document, not just the
            // scalar — even inside what looks like a flow collection, because the scanner
            // has not entered one: `a:\n- x\n  - {k: v}` is "mapping values are not
            // allowed here", while `a:\n- x\n  - [1]` folds happily.
            if has_mapping_separator(line.text) {
                break;
            }
            if breaks == 1 {
                text.push(' ');
            } else {
                text.push_str(&"\n".repeat(breaks - 1));
            }
            text.push_str(comment_stripped(line.text).trim());
            self.pos = lookahead + 1;
        }
        text
    }

    /// A `|` or `>` block scalar, with its indentation and chomping indicators.
    fn block_scalar(&mut self, header: &str, parent_indent: usize) -> Result<Value, Error> {
        let literal = header.starts_with('|');
        let mut explicit_indent: Option<usize> = None;
        let mut chomp = Chomp::Clip;
        for ch in header[1..].chars() {
            match ch {
                '-' => chomp = Chomp::Strip,
                '+' => chomp = Chomp::Keep,
                '1'..='9' => explicit_indent = Some(parent_indent + (ch as usize - '0' as usize)),
                '#' => break,
                ' ' | '\t' => continue,
                _ => return invalid(format!("unexpected block scalar indicator {ch:?}")),
            }
        }

        // Take every line that belongs to the block before deciding its indentation:
        // the first non-empty one sets it when no indicator was given.
        let mut raw: Vec<(usize, &str, bool)> = Vec::new();
        let content_indent = match explicit_indent {
            Some(n) => n,
            None => {
                let mut probe = self.pos;
                loop {
                    match self.lines.get(probe) {
                        Some(line) if line.is_blank() => probe += 1,
                        Some(line) if line.indent > parent_indent => break line.indent,
                        // No content line at all. The blank lines are still the scalar's,
                        // and `|+` keeps them: `a: |+\n  \n` is "\n", not "".
                        _ => break parent_indent + 1,
                    }
                }
            }
        };
        while let Some(line) = self.peek() {
            if line.is_blank() {
                raw.push((0, "", line.terminated));
                self.advance();
                continue;
            }
            if line.indent < content_indent {
                break;
            }
            raw.push((line.indent - content_indent, line.text, line.terminated));
            self.advance();
        }
        // Blank lines after the block belong to whatever follows, not to the scalar —
        // except that `+` keeps them, so they are trimmed only from the collected tail.
        while raw.last().is_some_and(|(_, text, _)| text.is_empty()) && chomp != Chomp::Keep {
            raw.pop();
        }
        // A document that simply stops has no final line break, and neither does its
        // block scalar: `a: |\n  x` is `"x"` where `a: |\n  x\n` is `"x\n"`.
        let last_terminated = raw.last().map(|(_, _, t)| *t).unwrap_or(true);

        let mut body = String::new();
        if literal {
            for (i, (extra, text, _)) in raw.iter().enumerate() {
                body.push_str(&" ".repeat(*extra));
                body.push_str(text);
                if i + 1 < raw.len() || last_terminated {
                    body.push('\n');
                }
            }
        } else {
            // Folding: a single line break becomes a space, a blank line becomes a
            // newline, and a more-indented line keeps its breaks verbatim.
            let mut previous_folded = false;
            for (i, (extra, text, _)) in raw.iter().enumerate() {
                let last = i + 1 == raw.len();
                if text.is_empty() {
                    body.push('\n');
                    previous_folded = false;
                    continue;
                }
                if *extra > 0 {
                    if i > 0 && previous_folded {
                        body.push('\n');
                    }
                    body.push_str(&" ".repeat(*extra));
                    body.push_str(text);
                    if !last || last_terminated {
                        body.push('\n');
                    }
                    previous_folded = false;
                    continue;
                }
                if previous_folded {
                    body.push(' ');
                }
                body.push_str(text);
                previous_folded = true;
            }
            if previous_folded && last_terminated {
                body.push('\n');
            }
        }

        match chomp {
            Chomp::Strip => {
                while body.ends_with('\n') {
                    body.pop();
                }
            }
            Chomp::Clip => {
                while body.ends_with("\n\n") {
                    body.pop();
                }
            }
            // `+` keeps every trailing newline the source actually had, which is what the
            // collected lines already carry.
            Chomp::Keep => {}
        }
        Ok(Value::String(body))
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Chomp {
    Strip,
    Clip,
    Keep,
}

/// `- item`, `-` alone, but not `-1` or `--flag`.
fn is_sequence_entry(text: &str) -> bool {
    text == "-" || text.starts_with("- ")
}

/// The first whitespace-delimited token and the rest.
fn split_token(text: &str) -> (&str, &str) {
    match text.find(char::is_whitespace) {
        Some(i) => (&text[..i], &text[i..]),
        None => (text, ""),
    }
}

/// Split `key: value` at the separator, or return `None` when the line is not a mapping
/// entry. The separator is a `:` at flow depth zero, outside quotes, followed by a space
/// or the end of the line — `a: b:c` has exactly one.
fn split_key(text: &str) -> Result<Option<(&str, &str)>, Error> {
    if text.starts_with("? ") || text == "?" {
        return unsupported("an explicit key (`? key`) is not supported by --cli-input-yaml");
    }
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            // A quoted key: skip its body so a `:` inside it is not a separator.
            b'\'' | b'"' if depth == 0 && i == 0 => {
                let (_, tail) = read_quoted(&text[i..])?;
                i = text.len() - tail.len();
                continue;
            }
            b'[' | b'{' => depth += 1,
            b']' | b'}' => depth = depth.saturating_sub(1),
            b'#' if i > 0 && bytes[i - 1] == b' ' => return Ok(None),
            b':' if depth == 0 => {
                let next = bytes.get(i + 1);
                if next.is_none() || next == Some(&b' ') {
                    return Ok(Some((text[..i].trim_end(), &text[i + 1..])));
                }
            }
            _ => {}
        }
        i += 1;
    }
    Ok(None)
}

/// Does this line carry a `key:` separator anywhere outside quotes, regardless of
/// brackets? Used only for plain-scalar continuation lines, where the scanner has not
/// entered any flow collection and so a `: ` inside `{...}` still ends the scalar.
fn has_mapping_separator(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => match read_quoted(&text[i..]) {
                Ok((_, tail)) => {
                    i = text.len() - tail.len();
                    continue;
                }
                Err(_) => return false,
            },
            b'#' if i > 0 && bytes[i - 1] == b' ' => return false,
            b':' if bytes.get(i + 1).is_none_or(|next| *next == b' ') => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

/// Everything before an unquoted ` #` comment.
fn comment_stripped(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => {
                let quote = bytes[i];
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == quote {
                        // `''` inside a single-quoted scalar is an escaped quote.
                        if quote == b'\'' && bytes.get(i + 1) == Some(&b'\'') {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    if quote == b'"' && bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'#' if i == 0 || bytes[i - 1] == b' ' => return text[..i].trim_end().to_string(),
            _ => {}
        }
        i += 1;
    }
    text.trim_end().to_string()
}

/// Bracket depth after `text`, starting from `depth`, ignoring quoted sections.
fn flow_depth(text: &str, depth: usize) -> Result<usize, Error> {
    let bytes = text.as_bytes();
    let mut depth = depth;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => {
                let (_, tail) = read_quoted(&text[i..])?;
                i = text.len() - tail.len();
                continue;
            }
            b'[' | b'{' => depth += 1,
            b']' | b'}' => {
                if depth == 0 {
                    return invalid("unexpected closing bracket");
                }
                depth -= 1;
            }
            _ => {}
        }
        i += 1;
    }
    Ok(depth)
}

/// Is the quoted scalar starting `text` complete? `read_quoted` fails on an unterminated
/// one, which is the same question asked from the other side.
fn quoted_is_closed(text: &str) -> bool {
    read_quoted(text).is_ok()
}

/// Read a quoted scalar at the start of `text`, returning its value and the rest.
fn read_quoted(text: &str) -> Result<(String, &str), Error> {
    let bytes = text.as_bytes();
    let quote = bytes[0];
    let mut out = String::new();
    let mut i = 1usize;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte == quote {
            if quote == b'\'' && bytes.get(i + 1) == Some(&b'\'') {
                out.push('\'');
                i += 2;
                continue;
            }
            return Ok((out, &text[i + 1..]));
        }
        // A *physical* line break folds: one becomes a space, several become newlines,
        // and the indentation of the continuation line is not content. This has to
        // happen here rather than over the finished string, because `"a\nb"` contains an
        // escape that must survive — folding the decoded text would erase the difference
        // between a break the user typed and one they escaped.
        if byte == b'\n' {
            while out.ends_with(' ') {
                out.pop();
            }
            let mut breaks = 1usize;
            i += 1;
            loop {
                while bytes.get(i) == Some(&b' ') {
                    i += 1;
                }
                if bytes.get(i) == Some(&b'\n') {
                    breaks += 1;
                    i += 1;
                    continue;
                }
                break;
            }
            if breaks == 1 {
                out.push(' ');
            } else {
                out.push_str(&"\n".repeat(breaks - 1));
            }
            continue;
        }
        if quote == b'"' && byte == b'\\' {
            i += 1;
            let Some(esc) = bytes.get(i) else { break };
            match esc {
                b'n' => out.push('\n'),
                b't' => out.push('\t'),
                b'r' => out.push('\r'),
                b'0' => out.push('\0'),
                b'b' => out.push('\u{8}'),
                b'f' => out.push('\u{c}'),
                b'a' => out.push('\u{7}'),
                b'v' => out.push('\u{b}'),
                b'e' => out.push('\u{1b}'),
                b'/' => out.push('/'),
                b'\\' => out.push('\\'),
                b'"' => out.push('"'),
                b'\n' => {}
                b'x' | b'u' | b'U' => {
                    let width = match esc {
                        b'x' => 2,
                        b'u' => 4,
                        _ => 8,
                    };
                    let start = i + 1;
                    let end = start + width;
                    if end > bytes.len() {
                        return invalid("truncated escape sequence");
                    }
                    let digits = &text[start..end];
                    let code = u32::from_str_radix(digits, 16)
                        .map_err(|_| Error::Invalid(format!("invalid escape \\{digits}")))?;
                    match char::from_u32(code) {
                        Some(ch) => out.push(ch),
                        None => return invalid(format!("invalid escape \\{digits}")),
                    }
                    i = end;
                    continue;
                }
                other => {
                    return invalid(format!("unknown escape \\{}", *other as char));
                }
            }
            i += 1;
            continue;
        }
        // Multi-byte characters are copied whole.
        let ch = text[i..].chars().next().expect("index is on a char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    invalid("found unexpected end of stream while scanning a quoted scalar")
}

/// Apply a `!!tag` to a value.
fn apply_tag(tag: &str, value: Value) -> Result<Value, Error> {
    let name = tag.trim_start_matches('!');
    match name {
        "str" => Ok(Value::String(match value {
            Value::String(s) => s,
            Value::Null => String::new(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => n.to_string(),
            other => return invalid(format!("cannot read {other} as a string")),
        })),
        "null" => Ok(Value::Null),
        "bool" | "int" | "float" => {
            let text = match &value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let resolved = scalar(&text)?;
            let ok = match name {
                "bool" => resolved.is_boolean(),
                "int" => resolved.is_i64() || resolved.is_u64(),
                _ => resolved.is_number(),
            };
            if ok {
                Ok(resolved)
            } else {
                invalid(format!("cannot read {text:?} as {name}"))
            }
        }
        "map" | "seq" => Ok(value),
        "" => Ok(value),
        other => unsupported(format!("the !!{other} tag is not supported by --cli-input-yaml")),
    }
}

/// A mapping key's identity *as Python sees it*, for duplicate detection.
///
/// The loader builds a `dict`, where `True == 1` and `1 == 1.0`, so `{true: a, 1: b}` is
/// a duplicate key and refused — even though nothing about the text says so, and even
/// though JSON would keep them apart as `"true"` and `"1"`. Comparing the JSON spellings
/// would accept a document the reference rejects.
fn python_key(value: &Value) -> String {
    match value {
        Value::Bool(true) => "n:1".to_string(),
        Value::Bool(false) => "n:0".to_string(),
        Value::Number(n) => format!("n:{}", n.as_f64().unwrap_or(f64::NAN)),
        Value::Null => "null".to_string(),
        other => format!("s:{}", key_to_string(other)),
    }
}

/// A mapping key as JSON spells it. YAML allows any scalar as a key; JSON objects are
/// keyed by strings, and this is the spelling `json.dumps` would have produced.
fn key_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "null".to_string(),
        Value::Bool(true) => "true".to_string(),
        Value::Bool(false) => "false".to_string(),
        other => other.to_string(),
    }
}

/// Resolve a plain (unquoted) scalar to its type, by ruamel's YAML 1.2 rules.
fn scalar(text: &str) -> Result<Value, Error> {
    let text = text.trim();
    if text.is_empty() || text == "~" || text == "null" || text == "Null" || text == "NULL" {
        return Ok(Value::Null);
    }
    if matches!(text, "true" | "True" | "TRUE") {
        return Ok(Value::Bool(true));
    }
    if matches!(text, "false" | "False" | "FALSE") {
        return Ok(Value::Bool(false));
    }
    if text.starts_with('\'') || text.starts_with('"') {
        let (value, tail) = read_quoted(text)?;
        if !tail.trim().is_empty() {
            return invalid("unexpected content after a quoted scalar");
        }
        return Ok(Value::String(value));
    }
    // `@` and `` ` `` are reserved indicators and `%` opens a directive, so none of them
    // may *start* a plain scalar — though all three are ordinary text further in, which
    // is why `50%` and `x@y` are fine. Accepting them would turn a typo into a value.
    if text.starts_with(['@', '`', '%']) {
        return invalid(format!(
            "found character {:?} that cannot start any token",
            text.chars().next().unwrap_or_default()
        ));
    }
    if let Some(value) = number(text) {
        return Ok(value);
    }
    Ok(Value::String(text.to_string()))
}

/// The integer and float branches of the 1.2 resolver, constructed the way ruamel's
/// constructors do — which is not always the way the regex that matched them suggests.
fn number(text: &str) -> Option<Value> {
    let (sign, digits) = match text.as_bytes().first() {
        Some(b'-') => (-1i64, &text[1..]),
        Some(b'+') => (1, &text[1..]),
        _ => (1, text),
    };
    if digits.is_empty() {
        return None;
    }

    // `.inf` / `.nan`, which JSON cannot hold. The AWS JSON protocols spell them as
    // strings, so that is what the request carries.
    match digits {
        ".inf" | ".Inf" | ".INF" => {
            return Some(Value::String(
                if sign < 0 { "-Infinity" } else { "Infinity" }.to_string(),
            ))
        }
        ".nan" | ".NaN" | ".NAN" => return Some(Value::String("NaN".to_string())),
        _ => {}
    }

    let cleaned = digits.replace('_', "");
    if cleaned.is_empty() {
        return None;
    }
    let radix_value = |body: &str, radix: u32| -> Option<Value> {
        if body.is_empty() || !body.chars().all(|c| c.is_digit(radix)) {
            return None;
        }
        i64::from_str_radix(body, radix).ok().map(|n| Value::from(sign * n))
    };
    if let Some(body) = cleaned.strip_prefix("0b").or_else(|| cleaned.strip_prefix("0B")) {
        return radix_value(body, 2);
    }
    if let Some(body) = cleaned.strip_prefix("0x").or_else(|| cleaned.strip_prefix("0X")) {
        return radix_value(body, 16);
    }
    if let Some(body) = cleaned.strip_prefix("0o").or_else(|| cleaned.strip_prefix("0O")) {
        return radix_value(body, 8);
    }
    // Plain digits. `017` matches the resolver's octal branch but Python's `int()`
    // constructs it, and that reads decimal — so 17, not 15.
    if cleaned.chars().all(|c| c.is_ascii_digit()) {
        return match cleaned.parse::<i64>() {
            Ok(n) => Some(Value::from(sign * n)),
            // Beyond i64 there is no AWS numeric type left to hold it; a float at least
            // keeps the magnitude rather than failing the whole document.
            Err(_) => cleaned.parse::<f64>().ok().map(|n| Value::from(sign as f64 * n)),
        };
    }

    // Floats: `1.5`, `.5`, `5.`, `1e3`, `1.0e+3`.
    if !cleaned.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-')) {
        return None;
    }
    let dots = cleaned.matches('.').count();
    let has_exponent = cleaned.contains(['e', 'E']);
    if dots > 1 || (dots == 0 && !has_exponent) {
        return None;
    }
    let parsed: f64 = cleaned.parse().ok()?;
    serde_json::Number::from_f64(sign as f64 * parsed).map(Value::Number)
}

/// Parse a flow collection or flow scalar from one joined string.
fn flow(text: &str) -> Result<Value, Error> {
    let chars: Vec<char> = text.chars().collect();
    let mut cursor = Flow { chars: &chars, i: 0 };
    let value = cursor.value()?;
    cursor.skip_space();
    if cursor.i < chars.len() {
        return invalid("unexpected content after a flow collection");
    }
    Ok(value)
}

struct Flow<'a> {
    chars: &'a [char],
    i: usize,
}

impl Flow<'_> {
    fn skip_space(&mut self) {
        while self.chars.get(self.i).is_some_and(|c| c.is_whitespace()) {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.i).copied()
    }

    fn value(&mut self) -> Result<Value, Error> {
        self.skip_space();
        match self.peek() {
            None => Ok(Value::Null),
            Some('[') => self.sequence(),
            Some('{') => self.mapping(),
            Some('\'') | Some('"') => {
                let rest: String = self.chars[self.i..].iter().collect();
                let (value, tail) = read_quoted(&rest)?;
                self.i = self.chars.len() - tail.chars().count();
                Ok(Value::String(value))
            }
            Some(_) => {
                let text = self.plain();
                scalar(&text)
            }
        }
    }

    /// A plain scalar inside a flow collection, which ends at a delimiter rather than at
    /// the end of the line.
    fn plain(&mut self) -> String {
        let start = self.i;
        while let Some(ch) = self.peek() {
            if matches!(ch, ',' | ']' | '}') {
                break;
            }
            if ch == ':' {
                let next = self.chars.get(self.i + 1);
                if next.is_none() || next.is_some_and(|c| c.is_whitespace() || matches!(c, ',' | ']' | '}')) {
                    break;
                }
            }
            self.i += 1;
        }
        self.chars[start..self.i].iter().collect::<String>().trim().to_string()
    }

    fn sequence(&mut self) -> Result<Value, Error> {
        self.i += 1; // `[`
        let mut items = Vec::new();
        loop {
            self.skip_space();
            match self.peek() {
                None => return invalid("expected the end of a flow sequence"),
                Some(']') => {
                    self.i += 1;
                    return Ok(Value::Array(items));
                }
                Some(',') => {
                    self.i += 1;
                    continue;
                }
                _ => items.push(self.value()?),
            }
        }
    }

    fn mapping(&mut self) -> Result<Value, Error> {
        self.i += 1; // `{`
        let mut map = Map::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        loop {
            self.skip_space();
            match self.peek() {
                None => return invalid("expected the end of a flow mapping"),
                Some('}') => {
                    self.i += 1;
                    return Ok(Value::Object(map));
                }
                Some(',') => {
                    self.i += 1;
                    continue;
                }
                _ => {}
            }
            let key = self.value()?;
            self.skip_space();
            // A key with no `:` is a key with a null value — `{a, b}` is legal YAML.
            let value = if self.peek() == Some(':') {
                self.i += 1;
                self.skip_space();
                if matches!(self.peek(), Some(',') | Some('}')) {
                    Value::Null
                } else {
                    self.value()?
                }
            } else {
                Value::Null
            };
            let identity = python_key(&key);
            let key = key_to_string(&key);
            if !seen.insert(identity) {
                return invalid(format!("found duplicate key {key}"));
            }
            map.insert(key, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read(text: &str) -> Value {
        parse(text).unwrap_or_else(|e| panic!("{text:?} should parse: {e}"))
    }

    #[test]
    fn reads_a_block_mapping() {
        assert_eq!(read("a: 1\nb: two"), json!({"a": 1, "b": "two"}));
    }

    #[test]
    fn nests_by_indentation() {
        assert_eq!(read("a:\n  b:\n    c: 1"), json!({"a": {"b": {"c": 1}}}));
    }

    /// A block sequence may sit at its key's own column, which looks wrong and is legal.
    #[test]
    fn a_sequence_may_share_its_keys_column() {
        assert_eq!(read("a:\n- 1\n- 2"), json!({"a": [1, 2]}));
        assert_eq!(read("a:\n  - 1\n  - 2"), json!({"a": [1, 2]}));
    }

    #[test]
    fn reads_a_mapping_that_starts_on_the_dash_line() {
        assert_eq!(read("- a: 1\n  b: 2"), json!([{"a": 1, "b": 2}]));
    }

    #[test]
    fn reads_nested_sequences() {
        assert_eq!(read("- - 1\n  - 2"), json!([[1, 2]]));
    }

    #[test]
    fn reads_flow_collections() {
        assert_eq!(read("a: [1, two, {b: 3}]"), json!({"a": [1, "two", {"b": 3}]}));
        assert_eq!(read(r#"{"a": 1, "b": [2]}"#), json!({"a": 1, "b": [2]}));
    }

    /// A flow collection may run past the end of its line.
    #[test]
    fn a_flow_collection_may_span_lines() {
        assert_eq!(read("a: [1,\n  2,\n  3]"), json!({"a": [1, 2, 3]}));
    }

    /// `{a:1}` is one scalar `a:1`, not a pair: a plain key's colon needs a space.
    #[test]
    fn a_colon_without_a_space_is_not_a_separator() {
        assert_eq!(read("{a:1}"), json!({"a:1": null}));
        assert_eq!(read("a: b:c"), json!({"a": "b:c"}));
    }

    /// YAML 1.2, which is what the reference's loader is. Under 1.1 these would be bools.
    #[test]
    fn yes_and_no_are_strings() {
        assert_eq!(read("a: yes\nb: no\nc: on\nd: off"), json!({"a": "yes", "b": "no", "c": "on", "d": "off"}));
        assert_eq!(read("a: true\nb: False"), json!({"a": true, "b": false}));
    }

    /// `017` matches the resolver's octal branch but is constructed by `int()`, decimal.
    #[test]
    fn leading_zeros_are_decimal_but_0o_is_octal() {
        assert_eq!(read("a: 017\nb: 0o17\nc: 0x1f\nd: 0b101"), json!({"a": 17, "b": 15, "c": 31, "d": 5}));
    }

    #[test]
    fn reads_floats_in_every_spelling() {
        assert_eq!(read("a: 0.5\nb: .5\nc: 5.\nd: 1e3\ne: 1.0e+3"), json!({"a": 0.5, "b": 0.5, "c": 5.0, "d": 1000.0, "e": 1000.0}));
    }

    #[test]
    fn underscores_are_digit_separators() {
        assert_eq!(read("a: 1_000\nb: 1__0"), json!({"a": 1000, "b": 10}));
    }

    /// Sexagesimal is a 1.1 rule; under 1.2 this is a string.
    #[test]
    fn a_clock_time_is_a_string() {
        assert_eq!(read("a: 1:30"), json!({"a": "1:30"}));
    }

    #[test]
    fn reads_nulls() {
        assert_eq!(read("a:\nb: ~\nc: null\nd: NULL"), json!({"a": null, "b": null, "c": null, "d": null}));
    }

    #[test]
    fn strips_comments_outside_quotes() {
        assert_eq!(read("a: 1 # trailing\n# whole line\nb: '# not a comment'"), json!({"a": 1, "b": "# not a comment"}));
        assert_eq!(read("a: x#y"), json!({"a": "x#y"}));
    }

    #[test]
    fn reads_quoted_scalars() {
        assert_eq!(read("a: 'it''s'"), json!({"a": "it's"}));
        assert_eq!(read(r#"a: "esc\n\u0041""#), json!({"a": "esc\nA"}));
        assert_eq!(read("a: ''"), json!({"a": ""}));
    }

    /// Quoting is how a number stays a string, which matters for AWS ids like `012345`.
    #[test]
    fn a_quoted_number_stays_a_string() {
        assert_eq!(read("Account: '012345678901'"), json!({"Account": "012345678901"}));
    }

    #[test]
    fn folds_multi_line_plain_scalars() {
        assert_eq!(read("a: multi\n  line plain"), json!({"a": "multi line plain"}));
    }

    #[test]
    fn folds_multi_line_quoted_scalars() {
        assert_eq!(read("a: 'x\n  y'"), json!({"a": "x y"}));
    }

    #[test]
    fn reads_literal_block_scalars() {
        assert_eq!(read("a: |\n  line1\n  line2\n"), json!({"a": "line1\nline2\n"}));
        assert_eq!(read("a: |-\n  x\n"), json!({"a": "x"}));
        assert_eq!(read("a: |+\n  x\n\n"), json!({"a": "x\n\n"}));
    }

    #[test]
    fn reads_folded_block_scalars() {
        assert_eq!(read("a: >\n  fold1\n  fold2\n"), json!({"a": "fold1 fold2\n"}));
        assert_eq!(read("a: >-\n  x\n  y\n"), json!({"a": "x y"}));
    }

    /// An explicit indentation indicator keeps leading spaces that would otherwise be
    /// mistaken for the block's own indentation.
    #[test]
    fn honours_a_block_indentation_indicator() {
        assert_eq!(read("a: |2\n    x\n"), json!({"a": "  x\n"}));
    }

    #[test]
    fn resolves_anchors_and_aliases() {
        assert_eq!(read("a: &anc 1\nb: *anc"), json!({"a": 1, "b": 1}));
        assert_eq!(
            read("base: &b {x: 1}\nchild:\n  <<: *b\n  y: 2"),
            json!({"base": {"x": 1}, "child": {"x": 1, "y": 2}})
        );
    }

    /// A merge fills in only what the mapping does not already say.
    #[test]
    fn an_explicit_key_wins_over_a_merge() {
        assert_eq!(
            read("base: &b {x: 1}\nchild:\n  <<: *b\n  x: 2"),
            json!({"base": {"x": 1}, "child": {"x": 2}})
        );
    }

    #[test]
    fn applies_the_str_tag() {
        assert_eq!(read("a: !!str 123"), json!({"a": "123"}));
    }

    #[test]
    fn reads_a_bare_document_scalar() {
        assert_eq!(read("plain scalar"), json!("plain scalar"));
        assert_eq!(read("123"), json!(123));
        assert_eq!(read("[1, 2]"), json!([1, 2]));
        assert_eq!(read(""), json!(null));
    }

    #[test]
    fn accepts_a_document_marker() {
        assert_eq!(read("---\na: 1"), json!({"a": 1}));
        assert_eq!(read("a: 1\n...\n"), json!({"a": 1}));
    }

    /// The reference's loader refuses a second document rather than taking the first.
    #[test]
    fn refuses_a_second_document() {
        assert!(matches!(parse("a: 1\n---\nb: 2"), Err(Error::Invalid(_))));
    }

    /// A repeated key is a mistake worth reporting, not a last-one-wins overwrite.
    #[test]
    fn refuses_a_duplicate_key() {
        assert!(matches!(parse("a: 1\na: 2"), Err(Error::Invalid(_))));
        assert!(matches!(parse("{a: 1, a: 2}"), Err(Error::Invalid(_))));
    }

    #[test]
    fn refuses_an_unterminated_quote() {
        assert!(matches!(parse("a: 'unterminated"), Err(Error::Invalid(_))));
    }

    #[test]
    fn refuses_an_undefined_alias() {
        assert!(matches!(parse("a: *nope"), Err(Error::Invalid(_))));
    }

    #[test]
    fn refuses_a_tab_in_the_indentation() {
        assert!(matches!(parse("a:\n\tb: 1"), Err(Error::Invalid(_))));
    }

    /// A block scalar's trailing newline follows the document's: `|` clips to one
    /// newline only when the source actually ended with one.
    #[test]
    fn a_block_scalar_ends_where_the_document_does() {
        assert_eq!(read("a: |\n  x\n"), json!({"a": "x\n"}));
        assert_eq!(read("a: |\n  x"), json!({"a": "x"}));
        assert_eq!(read("a: >\n  more text"), json!({"a": "more text"}));
        assert_eq!(read("a: |+\n  \n"), json!({"a": "\n"}));
    }

    /// A more-indented line carries on a plain scalar even when it looks like a nested
    /// sequence — the dash is text, not an indicator, once the scalar has started.
    #[test]
    fn a_plain_scalar_swallows_more_indented_lines() {
        assert_eq!(read("a:\n- x\n  - y"), json!({"a": ["x - y"]}));
        assert_eq!(read("a: text\n\n  more"), json!({"a": "text\nmore"}));
        assert_eq!(read("k: ~\n  more"), json!({"k": "~ more"}));
    }

    /// `@` and `` ` `` are reserved and `%` opens a directive, so none may start a plain
    /// scalar — though they are ordinary text anywhere else.
    #[test]
    fn refuses_a_reserved_indicator_at_the_start_of_a_scalar() {
        assert!(matches!(parse("a: @at"), Err(Error::Invalid(_))));
        assert!(matches!(parse("a: `tick`"), Err(Error::Invalid(_))));
        assert_eq!(read("k: x@y"), json!({"k": "x@y"}));
        assert_eq!(read("k: 50%"), json!({"k": "50%"}));
    }

    /// The loader builds a Python `dict`, where `True == 1`, so these collide even though
    /// JSON would keep `"true"` and `"1"` apart.
    #[test]
    fn a_bool_and_the_matching_number_are_one_key() {
        assert!(matches!(parse("true: 1\n1: 2"), Err(Error::Invalid(_))));
        assert!(matches!(parse("1: a\n1.0: b"), Err(Error::Invalid(_))));
        assert_eq!(read("true: 1\nfalse: 2"), json!({"true": 1, "false": 2}));
    }

    /// Constructs the reference would accept are named, not silently mis-read.
    #[test]
    fn names_what_it_does_not_implement() {
        assert!(matches!(parse("? complex\n: 1"), Err(Error::Unsupported(_))));
        assert!(matches!(parse("%YAML 1.1\n---\na: 1"), Err(Error::Unsupported(_))));
        assert!(matches!(parse("a: !!set\n  ? x"), Err(Error::Unsupported(_))));
    }

    /// JSON is a subset of YAML, so a skeleton pasted as-is must still read.
    #[test]
    fn reads_json_unchanged() {
        let document = r#"{"Filters": [{"Name": "tag:Env", "Values": ["prod"]}], "MaxResults": 5}"#;
        assert_eq!(read(document), serde_json::from_str::<Value>(document).expect("json"));
    }

    /// A timestamp stays a string: JSON has no date, and every AWS timestamp member
    /// accepts the ISO spelling.
    #[test]
    fn keeps_timestamps_as_strings() {
        assert_eq!(read("a: 2020-01-01"), json!({"a": "2020-01-01"}));
        assert_eq!(read("a: 2020-01-01T10:00:00Z"), json!({"a": "2020-01-01T10:00:00Z"}));
    }

    /// Every case answered by the loader the reference actually uses.
    ///
    /// The tests above say what this reader does; this one says what `aws` does. It is
    /// the only check that can catch a rule transcribed from the wrong YAML version,
    /// because such a rule looks perfectly reasonable from inside. Regenerate with
    /// `scripts/extract-yaml-input-cases.py` after changing the corpus.
    #[test]
    fn agrees_with_the_reference_loader() {
        #[derive(serde::Deserialize)]
        struct Corpus {
            cases: Vec<Case>,
        }
        #[derive(serde::Deserialize)]
        struct Case {
            yaml: String,
            value: Value,
        }

        let corpus: Corpus =
            serde_json::from_str(include_str!("../../../tests/golden/yaml-input-cases.json"))
                .expect("the golden corpus should parse");
        let mut divergences = Vec::new();
        for case in &corpus.cases {
            let tagged = case.value.as_object().and_then(|o| o.keys().next().map(String::as_str));
            let ours = parse(&case.yaml);
            match (tagged, &ours) {
                // The reference refused the document, so we must too — for whatever
                // reason. Reproducing ruamel's exception names is not the point; the
                // point is that neither of us invents a value for a broken document.
                (Some("__error__"), Err(_)) => continue,
                (Some("__error__"), Ok(value)) => {
                    divergences.push(format!("{:?}: reference refused, we read {value}", case.yaml));
                }
                _ => match ours {
                    Err(error) => {
                        divergences.push(format!("{:?}: we refused ({error})", case.yaml));
                    }
                    Ok(value) => {
                        let expected = untag(&case.value, &value);
                        if value != expected {
                            divergences.push(format!(
                                "{:?}: reference {}, ours {value}",
                                case.yaml, case.value
                            ));
                        }
                    }
                },
            }
        }
        assert!(
            divergences.is_empty(),
            "{} of {} cases diverge from the reference loader:\n{}",
            divergences.len(),
            corpus.cases.len(),
            divergences.join("\n")
        );
    }

    /// The corpus value to compare against, walked alongside ours so the one documented
    /// divergence can be allowed for at the exact position it occurs.
    fn untag(expected: &Value, ours: &Value) -> Value {
        match expected {
            // ruamel resolves a timestamp to a `datetime` and normalises its spelling
            // (`Z` becomes `+00:00`); this reader keeps the source text, because JSON has
            // no date type. What must agree is *which* member is a timestamp, so any
            // string here is accepted — the wire format is decided later from the model,
            // and both spellings parse there.
            Value::Object(fields) if fields.len() == 1 && fields.contains_key("__timestamp__") => {
                match ours {
                    Value::String(text) => Value::String(text.clone()),
                    _ => Value::String(
                        fields["__timestamp__"].as_str().unwrap_or_default().to_string(),
                    ),
                }
            }
            Value::Object(fields) => Value::Object(
                fields
                    .iter()
                    .map(|(key, value)| {
                        let ours = ours.get(key).unwrap_or(&Value::Null);
                        (key.clone(), untag(value, ours))
                    })
                    .collect(),
            ),
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .enumerate()
                    .map(|(i, value)| untag(value, ours.get(i).unwrap_or(&Value::Null)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}
