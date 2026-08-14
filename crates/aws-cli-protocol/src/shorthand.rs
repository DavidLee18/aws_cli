//! The shorthand argument syntax: `Name=value,Values=a,b`, `A={B=c}`, `[a,b]`.
//!
//! A direct port of `awscli/shorthand.py`'s recursive-descent parser. The grammar is
//! purely syntactic — no model is consulted here; a second pass applies the model to
//! coerce scalars and wrap bare values into single-element lists.
//!
//! ```text
//! parameter       = keyval *("," keyval)
//! keyval          = key ["@"] "=" [values]
//! key             = 1*(ALPHA / DIGIT / "-" / "_" / "." / "#" / "/" / ":")
//! values          = explicit-list / hash-literal / csv-value
//! csv-value       = first-value [ "," second-value *("," second-value) ]
//! explicit-list   = "[" [ explicit-values *("," explicit-values) ] "]"
//! hash-literal    = "{" [ hashkeyval *("," hashkeyval) ] "}"
//! ```
//!
//! The subtle part is `csv-value`: on hitting a syntax error mid-list it **backtracks to
//! the previous comma**, which is how `foo=a,b,c=d` splits into `foo=[a,b]` and `c=d`
//! rather than failing.

use serde_json::{Map, Value};

#[derive(Debug, thiserror::Error)]
#[error("{message}\n{expression}\n{}^", " ".repeat(*.position))]
pub struct ShorthandError {
    pub message: String,
    pub expression: String,
    pub position: usize,
}

/// Parse a shorthand expression into JSON.
pub fn parse(input: &str) -> Result<Value, ShorthandError> {
    Parser::new(input).parameter()
}

struct Parser<'a> {
    input: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Parser { input, bytes: input.as_bytes(), pos: 0 }
    }

    fn error(&self, message: &str) -> ShorthandError {
        ShorthandError {
            message: format!("Error parsing parameter: {message}"),
            expression: self.input.to_string(),
            position: self.pos.min(self.input.len()),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_whitespace(&mut self) {
        while self.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, c: u8, skip_ws: bool) -> Result<(), ShorthandError> {
        if skip_ws {
            self.skip_whitespace();
        }
        if self.peek() == Some(c) {
            self.pos += 1;
            if skip_ws {
                self.skip_whitespace();
            }
            Ok(())
        } else {
            Err(self.error(&format!("Expected `{}`", c as char)))
        }
    }

    /// `parameter = keyval *("," keyval)`
    fn parameter(&mut self) -> Result<Value, ShorthandError> {
        let mut out = Map::new();
        let (key, value) = self.keyval()?;
        out.insert(key, value);

        while self.peek() == Some(b',') {
            self.pos += 1;
            let (key, value) = self.keyval()?;
            // Top-level duplicate keys are an error (hash literals do not check).
            if out.contains_key(&key) {
                return Err(self.error(&format!("Duplicate key in object: \"{key}\"")));
            }
            out.insert(key, value);
        }
        if self.pos < self.bytes.len() {
            return Err(self.error("Unexpected trailing input"));
        }
        Ok(Value::Object(out))
    }

    /// `keyval = key ["@"] "=" [values]`
    fn keyval(&mut self) -> Result<(String, Value), ShorthandError> {
        let key = self.key()?;
        // `@=` marks a leaf whose value is a paramfile reference; the resolution itself
        // happens in the caller, which owns filesystem access.
        let resolve_paramfile = if self.peek() == Some(b'@') {
            self.pos += 1;
            true
        } else {
            false
        };
        self.expect(b'=', true)?;
        let mut value = self.values()?;
        if resolve_paramfile {
            value = mark_paramfile(value);
        }
        Ok((key, value))
    }

    fn key(&mut self) -> Result<String, ShorthandError> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || b"-_.#/:".contains(&c) {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            return Err(self.error("Expected a key"));
        }
        Ok(self.input[start..self.pos].to_string())
    }

    /// `values = explicit-list / hash-literal / csv-value`
    fn values(&mut self) -> Result<Value, ShorthandError> {
        match self.peek() {
            // An empty value at end of input is the empty string: `Key=`.
            None => Ok(Value::String(String::new())),
            Some(b'[') => self.explicit_list(),
            Some(b'{') => self.hash_literal(),
            _ => self.csv_value(),
        }
    }

    /// `csv-value = first-value [ "," second-value *("," second-value) ]`
    ///
    /// An element is appended only after a following comma (or EOF) is confirmed. When
    /// the next token turns out to be `key=` instead, the parser **backtracks to the
    /// previous comma** and lets `parameter` pick it up — that is how `foo=a,b,c=d`
    /// becomes `foo=[a,b]` plus `c=d` rather than a parse error.
    fn csv_value(&mut self) -> Result<Value, ShorthandError> {
        let first = self.first_value()?;
        self.skip_whitespace();
        if self.peek() != Some(b',') {
            return Ok(Value::String(first));
        }
        self.expect(b',', true)?;

        let mut items = vec![Value::String(first.clone())];
        loop {
            let attempt = (|parser: &mut Self| -> Result<String, ShorthandError> {
                let current = parser.second_value()?;
                parser.skip_whitespace();
                if parser.pos >= parser.bytes.len() {
                    return Ok(current);
                }
                // The element only counts once a comma follows it.
                parser.expect(b',', true)?;
                Ok(current)
            })(self);

            match attempt {
                Ok(current) => {
                    let at_eof = self.pos >= self.bytes.len();
                    items.push(Value::String(current));
                    if at_eof {
                        break;
                    }
                }
                Err(e) => {
                    if self.pos >= self.bytes.len() {
                        return Err(e);
                    }
                    self.backtrack_to(b',');
                    break;
                }
            }
        }

        if items.len() == 1 {
            return Ok(Value::String(first));
        }
        Ok(Value::Array(items))
    }

    /// Rewind to the most recent occurrence of `c`, leaving the index ON it.
    fn backtrack_to(&mut self, c: u8) {
        while self.pos > 0 && self.bytes.get(self.pos) != Some(&c) {
            self.pos -= 1;
        }
    }

    fn explicit_list(&mut self) -> Result<Value, ShorthandError> {
        self.expect(b'[', true)?;
        let mut items = Vec::new();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Array(items));
        }
        loop {
            items.push(self.explicit_values()?);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                    self.skip_whitespace();
                }
                _ => break,
            }
        }
        self.expect(b']', true)?;
        Ok(Value::Array(items))
    }

    fn explicit_values(&mut self) -> Result<Value, ShorthandError> {
        match self.peek() {
            Some(b'[') => self.explicit_list(),
            Some(b'{') => self.hash_literal(),
            _ => Ok(Value::String(self.first_value()?)),
        }
    }

    fn hash_literal(&mut self) -> Result<Value, ShorthandError> {
        self.expect(b'{', true)?;
        let mut out = Map::new();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(out));
        }
        loop {
            let key = self.key()?;
            let resolve_paramfile = if self.peek() == Some(b'@') {
                self.pos += 1;
                true
            } else {
                false
            };
            self.expect(b'=', true)?;
            let mut value = self.explicit_values()?;
            if resolve_paramfile {
                value = mark_paramfile(value);
            }
            // Note: unlike the top level, a duplicate here silently keeps the last.
            out.insert(key, value);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                    self.skip_whitespace();
                }
                _ => break,
            }
        }
        self.expect(b'}', true)?;
        Ok(Value::Object(out))
    }

    fn first_value(&mut self) -> Result<String, ShorthandError> {
        match self.peek() {
            Some(b'\'') => self.quoted(b'\''),
            Some(b'"') => self.quoted(b'"'),
            _ => self.bare_value(FollowSet::First),
        }
    }

    fn second_value(&mut self) -> Result<String, ShorthandError> {
        match self.peek() {
            Some(b'\'') => self.quoted(b'\''),
            Some(b'"') => self.quoted(b'"'),
            _ => self.bare_value(FollowSet::Second),
        }
    }

    fn quoted(&mut self, quote: u8) -> Result<String, ShorthandError> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err(self.error("Unterminated quote")),
                Some(b'\\') => {
                    self.pos += 1;
                    match self.peek() {
                        Some(c) => {
                            // `\'` and `\"` yield the bare quote; other escapes keep the
                            // backslash.
                            if c != quote {
                                out.push('\\');
                            }
                            out.push(c as char);
                            self.pos += 1;
                        }
                        None => return Err(self.error("Unterminated escape")),
                    }
                }
                Some(c) if c == quote => {
                    self.pos += 1;
                    break;
                }
                Some(_) => {
                    let ch = self.input[self.pos..].chars().next().expect("in bounds");
                    out.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
        Ok(out)
    }

    /// A bare value. The first character may not be whitespace or a delimiter; the rest
    /// may contain interior whitespace, which is then trimmed from the right.
    fn bare_value(&mut self, follow: FollowSet) -> Result<String, ShorthandError> {
        let start = self.pos;
        let mut out = String::new();

        // First character.
        match self.peek() {
            Some(c) if is_start_word(c) => {}
            Some(b'\\') if self.bytes.get(self.pos + 1) == Some(&b',') => {}
            _ => return Err(self.error("Expected a value")),
        }

        while let Some(c) = self.peek() {
            if c == b'\\' && self.bytes.get(self.pos + 1) == Some(&b',') {
                out.push(',');
                self.pos += 2;
                continue;
            }
            if !follow.allows(c) {
                break;
            }
            let ch = self.input[self.pos..].chars().next().expect("in bounds");
            out.push(ch);
            self.pos += ch.len_utf8();
        }

        if self.pos == start {
            return Err(self.error("Expected a value"));
        }
        Ok(out.trim_end().to_string())
    }
}

#[derive(Clone, Copy)]
enum FollowSet {
    /// Top level and inside lists: `=`, `[`, `{` are allowed; `]`, `}` are not.
    First,
    /// After a top-level comma: `]`, `}` are allowed; `=` is not.
    Second,
}

impl FollowSet {
    fn allows(self, c: u8) -> bool {
        match self {
            FollowSet::First => !matches!(c, b'"' | b'\'' | b',' | b']' | b'}'),
            FollowSet::Second => !matches!(c, b'"' | b'\'' | b',' | b'='),
        }
    }
}

/// A bare value may not START with whitespace or any delimiter.
fn is_start_word(c: u8) -> bool {
    !c.is_ascii_whitespace() && !matches!(c, b'"' | b'\'' | b',' | b'=' | b'[' | b'{')
}

/// Marker prefix for a `@=` value, resolved by the caller which owns file access.
pub const PARAMFILE_MARKER: &str = "\u{0}paramfile\u{0}";

fn mark_paramfile(value: Value) -> Value {
    match value {
        Value::String(s) => Value::String(format!("{PARAMFILE_MARKER}{s}")),
        Value::Array(items) => Value::Array(items.into_iter().map(mark_paramfile).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_simple_key_values() {
        assert_eq!(parse("Name=value").unwrap(), json!({"Name": "value"}));
        assert_eq!(
            parse("Name=a,Other=b").unwrap(),
            json!({"Name": "a", "Other": "b"})
        );
        // An empty trailing value is the empty string.
        assert_eq!(parse("Key=").unwrap(), json!({"Key": ""}));
    }

    /// The documented backtracking case: the parser cannot know `c=d` starts a new pair
    /// until it fails, then rewinds to the comma.
    #[test]
    fn backtracks_out_of_a_csv_list_at_the_next_key() {
        assert_eq!(
            parse("foo=a,b,c=d,e=f").unwrap(),
            json!({"foo": ["a", "b"], "c": "d", "e": "f"})
        );
    }

    #[test]
    fn parses_the_filter_idiom() {
        assert_eq!(
            parse("Name=tag:Env,Values=prod,staging").unwrap(),
            json!({"Name": "tag:Env", "Values": ["prod", "staging"]})
        );
    }

    #[test]
    fn parses_explicit_lists_and_hash_literals() {
        assert_eq!(parse("A=[a,b]").unwrap(), json!({"A": ["a", "b"]}));
        assert_eq!(parse("A=[]").unwrap(), json!({"A": []}));
        assert_eq!(parse("A={B=c}").unwrap(), json!({"A": {"B": "c"}}));
        assert_eq!(
            parse("A=[{K=v},{K=w}]").unwrap(),
            json!({"A": [{"K": "v"}, {"K": "w"}]})
        );
        assert_eq!(
            parse("A={B=c,D={E=f}}").unwrap(),
            json!({"A": {"B": "c", "D": {"E": "f"}}})
        );
    }

    #[test]
    fn handles_quoting_and_escapes() {
        assert_eq!(parse(r#"A="has,comma""#).unwrap(), json!({"A": "has,comma"}));
        assert_eq!(parse("A='single quoted'").unwrap(), json!({"A": "single quoted"}));
        // A backslash-escaped comma is literal in a bare value.
        assert_eq!(parse(r"A=a\,b").unwrap(), json!({"A": "a,b"}));
        assert_eq!(parse(r#"A="say \"hi\"""#).unwrap(), json!({"A": r#"say "hi""#}));
    }

    /// Bare values may contain interior whitespace, and whitespace surrounds `=`.
    #[test]
    fn tolerates_whitespace() {
        assert_eq!(parse("A = b").unwrap(), json!({"A": "b"}));
        assert_eq!(parse("A=two words").unwrap(), json!({"A": "two words"}));
    }

    #[test]
    fn rejects_duplicate_top_level_keys() {
        assert!(parse("A=1,A=2").is_err());
        // ...but a hash literal keeps the last silently, as the reference does.
        assert_eq!(parse("X={A=1,A=2}").unwrap(), json!({"X": {"A": "2"}}));
    }

    #[test]
    fn reports_position_on_error() {
        let err = parse("A").unwrap_err();
        assert!(err.to_string().contains('^'), "error should point at the position");
        assert!(parse("A=[a,b").is_err(), "unterminated list");
        assert!(parse(r#"A="unterminated"#).is_err());
    }

    #[test]
    fn marks_paramfile_values() {
        let v = parse("Body@=file://data.txt").unwrap();
        assert_eq!(
            v["Body"].as_str().unwrap(),
            format!("{PARAMFILE_MARKER}file://data.txt")
        );
    }
}
