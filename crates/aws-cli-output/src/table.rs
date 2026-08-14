//! The `table` output format.
//!
//! ASCII box drawing, not Unicode. The parts that are easy to get wrong:
//!
//! - Column widths are **scaled to a common total** across every section, so all
//!   sections line up. The total is the widest section's natural width and is NOT capped
//!   by the terminal — the terminal width (80 when stdout is not a tty) only decides
//!   whether wide single-row sections are reformatted vertically. Content is never
//!   clipped, so a wide table can still overflow.
//! - Nested sections are wrapped in extra `|` on both sides, one pair per level.
//! - Keys are **sorted alphabetically**, both for headers and for the scalar/container
//!   split.
//! - A dict with exactly one scalar key renders as a two-column `| Key | value |` row
//!   with **no header**.
//! - Falsy values (`{}`, `[]`, `null`, `false`, `0`, `""`) render **nothing at all**.

use serde_json::{Map, Value};

/// A section: an optional title, an optional header row, and data rows.
#[derive(Debug, Default)]
struct Section {
    title: Option<String>,
    indent: usize,
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Section {
    /// Natural width: every column padded by 4, plus the borders and the indent, but at
    /// least wide enough for the title.
    fn natural_width(&self) -> usize {
        let content: usize = self.max_widths().iter().map(|w| w + 4).sum::<usize>() + 2
            + 2 * self.indent;
        let title = self.title.as_ref().map_or(0, |t| display_width(t) + 2 + 2 * self.indent);
        content.max(title)
    }

    fn max_widths(&self) -> Vec<usize> {
        let mut widths: Vec<usize> = self.headers.iter().map(|h| display_width(h)).collect();
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                let w = display_width(cell);
                match widths.get_mut(i) {
                    Some(existing) => *existing = (*existing).max(w),
                    None => widths.push(w),
                }
            }
        }
        widths
    }

    /// Turn a single-row section into N two-column `[header, value]` rows, dropping the
    /// header row. This is what makes a wide result print vertically.
    fn reformat_vertically(&mut self) {
        if self.rows.len() != 1 || self.headers.is_empty() {
            return;
        }
        let row = self.rows.remove(0);
        self.rows = self
            .headers
            .iter()
            .cloned()
            .zip(row.into_iter().chain(std::iter::repeat(String::new())))
            .map(|(h, v)| vec![h, v])
            .collect();
        self.headers.clear();
    }

    /// Scale the natural widths so they sum to exactly `total`.
    fn column_widths(&self, total: usize) -> Vec<usize> {
        let unscaled: Vec<usize> = self.max_widths().iter().map(|w| w + 4).collect();
        if unscaled.is_empty() {
            return Vec::new();
        }
        let sum: usize = unscaled.iter().sum();
        if sum == 0 {
            return unscaled;
        }
        let scale = total as f64 / sum as f64;
        let mut scaled: Vec<usize> =
            unscaled.iter().map(|w| (scale * *w as f64).round() as usize).collect();

        // Rounding leaves a residual; walk forward removing, or backward adding, until
        // the widths sum to exactly `total`.
        let mut off_by = scaled.iter().sum::<usize>() as i64 - total as i64;
        let mut i = 0;
        while off_by > 0 && !scaled.is_empty() {
            let idx = i % scaled.len();
            if scaled[idx] > 1 {
                scaled[idx] -= 1;
                off_by -= 1;
            }
            i += 1;
        }
        let mut i = 0;
        while off_by < 0 && !scaled.is_empty() {
            let idx = scaled.len() - 1 - (i % scaled.len());
            scaled[idx] += 1;
            off_by += 1;
            i += 1;
        }
        scaled
    }
}

/// Render a value as a table. `title` is the **API** operation name (`GetCallerIdentity`,
/// not `get-caller-identity`) — the reference passes `operation_model.name`.
pub fn render(title: &str, value: &Value) -> Option<String> {
    let mut sections = Vec::new();
    if !build(Some(title), value, 0, &mut sections) {
        return None;
    }

    let mut max_width = sections.iter().map(|s| s.natural_width()).max().unwrap_or(0);

    // The terminal width does NOT cap the table — it only decides whether to reformat
    // wide single-row sections vertically. After the reformat the table renders at its
    // natural width even if that still exceeds the terminal; content is never clipped.
    if max_width > terminal_width() {
        for section in sections.iter_mut() {
            section.reformat_vertically();
        }
        max_width = sections.iter().map(|s| s.natural_width()).max().unwrap_or(0);
    }

    let mut out = String::new();
    out.push_str(&"-".repeat(max_width));
    out.push('\n');
    for section in &sections {
        render_section(section, max_width, &mut out);
    }
    Some(out)
}

fn terminal_width() -> usize {
    // The reference uses an ioctl and falls back to 80 on any failure — which is every
    // piped invocation, and therefore the case that matters for reproducibility.
    80
}

fn render_section(section: &Section, max_width: usize, out: &mut String) {
    let indent = section.indent;
    let inner = max_width.saturating_sub(2 * indent);
    let bar = "|".repeat(indent);

    let line = |text: &str, out: &mut String| {
        out.push_str(&bar);
        out.push_str(text);
        out.push_str(&bar);
        out.push('\n');
    };

    if let Some(title) = &section.title {
        line(&center_text(title, inner, "|", "|"), out);
    }

    if section.headers.is_empty() && section.rows.is_empty() {
        // A title with nothing under it still gets its closing rule.
        line(&format!("+{}+", "-".repeat(inner.saturating_sub(2))), out);
        return;
    }

    let widths = section.column_widths(inner);

    if !section.headers.is_empty() {
        line(&rule(&widths), out);
        let mut row = String::new();
        for (i, header) in section.headers.iter().enumerate() {
            let left = if i == 0 { "|" } else { "" };
            row.push_str(&center_text(header, widths[i], left, "|"));
        }
        line(&row, out);
    }

    line(&rule(&widths), out);
    for data in &section.rows {
        let mut row = String::new();
        for (i, cell) in data.iter().enumerate() {
            let left = if i == 0 { "|" } else { "" };
            row.push_str(&align_left(cell, widths[i], left, "|"));
        }
        line(&row, out);
    }
    line(&rule(&widths), out);
}

fn rule(widths: &[usize]) -> String {
    let mut out = String::new();
    for (i, w) in widths.iter().enumerate() {
        if i == 0 {
            out.push('+');
            out.push_str(&"-".repeat(w.saturating_sub(2)));
            out.push('+');
        } else {
            out.push_str(&"-".repeat(w.saturating_sub(1)));
            out.push('+');
        }
    }
    out
}

/// Centre `text` in `length` columns. Repeat counts are clamped at zero — Python renders
/// a negative repeat as empty rather than erroring, and a long title would otherwise
/// underflow.
fn center_text(text: &str, length: usize, left_edge: &str, right_edge: &str) -> String {
    let text_len = display_width(text);
    let start = (length / 2).saturating_sub(text_len / 2).saturating_sub(1);
    let used = left_edge.len() + start + text_len;
    let trailing = length.saturating_sub(right_edge.len()).saturating_sub(used);
    format!("{left_edge}{}{text}{}{right_edge}", " ".repeat(start), " ".repeat(trailing))
}

/// Left-align with two spaces of padding, dropping the padding when it would not fit.
fn align_left(text: &str, length: usize, left_edge: &str, right_edge: &str) -> String {
    let text_len = display_width(text);
    let padding =
        if length >= text_len + 2 + left_edge.len() + right_edge.len() { 2 } else { 0 };
    let used = left_edge.len() + padding + text_len;
    let trailing = length.saturating_sub(used).saturating_sub(right_edge.len());
    format!("{left_edge}{}{text}{}{right_edge}", " ".repeat(padding), " ".repeat(trailing))
}

/// East-Asian wide and ambiguous characters occupy two columns.
fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| {
            let cp = c as u32;
            let wide = (0x1100..=0x115F).contains(&cp)
                || (0x2E80..=0xA4CF).contains(&cp)
                || (0xAC00..=0xD7A3).contains(&cp)
                || (0xF900..=0xFAFF).contains(&cp)
                || (0xFF00..=0xFF60).contains(&cp)
                || (0xFFE0..=0xFFE6).contains(&cp);
            if wide { 2 } else { 1 }
        })
        .sum()
}

/// Python `str()` of a scalar, as the table renderer uses.
fn cell(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

fn is_scalar(value: &Value) -> bool {
    !matches!(value, Value::Object(_) | Value::Array(_))
}

/// Falsy in Python's sense — these render nothing at all.
fn is_falsy(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
    }
}

/// Build the section list. Returns false when nothing should be rendered.
fn build(title: Option<&str>, value: &Value, indent: usize, sections: &mut Vec<Section>) -> bool {
    if is_falsy(value) {
        return false;
    }
    if let Some(title) = title {
        sections.push(Section {
            title: Some(title.to_string()),
            indent,
            ..Default::default()
        });
    }

    match value {
        Value::Array(items) if items.iter().any(|i| matches!(i, Value::Object(_))) => {
            build_from_list(items, title, indent, sections);
        }
        Value::Array(items) => {
            for item in items {
                if is_scalar(item) {
                    current(sections).rows.push(vec![cell(item)]);
                } else if let Value::Array(inner) = item {
                    if inner.iter().all(is_scalar) {
                        current(sections).rows.push(inner.iter().map(cell).collect());
                    } else {
                        build(None, item, indent, sections);
                    }
                }
            }
        }
        Value::Object(map) => build_from_dict(map, indent, sections),
        _ => {}
    }
    true
}

fn current(sections: &mut Vec<Section>) -> &mut Section {
    if sections.is_empty() {
        sections.push(Section::default());
    }
    sections.last_mut().expect("just ensured non-empty")
}

fn sorted_split(map: &Map<String, Value>) -> (Vec<String>, Vec<String>) {
    let mut scalars: Vec<String> =
        map.iter().filter(|(_, v)| is_scalar(v)).map(|(k, _)| k.clone()).collect();
    let mut containers: Vec<String> =
        map.iter().filter(|(_, v)| !is_scalar(v)).map(|(k, _)| k.clone()).collect();
    scalars.sort();
    containers.sort();
    (scalars, containers)
}

fn build_from_dict(map: &Map<String, Value>, indent: usize, sections: &mut Vec<Section>) {
    let (scalars, containers) = sorted_split(map);

    if scalars.len() == 1 {
        // The vertical two-column form, with no header row.
        let key = &scalars[0];
        current(sections).rows.push(vec![key.clone(), cell(&map[key])]);
    } else if !scalars.is_empty() {
        let section = current(sections);
        section.headers = scalars.clone();
        section.rows.push(scalars.iter().map(|k| cell(&map[k])).collect());
    }

    for key in containers {
        build(Some(&key), &map[&key], indent + 1, sections);
    }
}

fn build_from_list(
    items: &[Value],
    title: Option<&str>,
    indent: usize,
    sections: &mut Vec<Section>,
) {
    // The header is the sorted union of scalar keys across every element.
    let mut headers: Vec<String> = Vec::new();
    for item in items {
        if let Value::Object(map) = item {
            for (k, v) in map.iter().filter(|(_, v)| is_scalar(v)) {
                let _ = v;
                if !headers.contains(k) {
                    headers.push(k.clone());
                }
            }
        }
    }
    headers.sort();

    let has_containers = items.iter().any(|i| {
        matches!(i, Value::Object(map) if map.values().any(|v| !is_scalar(v)))
    });

    for (index, item) in items.iter().enumerate() {
        let Value::Object(map) = item else { continue };

        // When elements carry nested containers, each element after the first restarts
        // a section with the same title and header — which is why repeated blocks appear.
        if index > 0 && has_containers {
            sections.push(Section {
                title: title.map(str::to_string),
                indent,
                ..Default::default()
            });
        }
        {
            let section = current(sections);
            if section.headers.is_empty() {
                section.headers = headers.clone();
            }
            section
                .rows
                .push(headers.iter().map(|h| map.get(h).map(cell).unwrap_or_default()).collect());
        }

        let (_, containers) = sorted_split(map);
        for key in containers {
            build(Some(&key), &map[&key], indent + 1, sections);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn falsy_values_render_nothing() {
        for v in [json!({}), json!([]), json!(null), json!(false), json!(0), json!("")] {
            assert_eq!(render("op", &v), None, "{v:?}");
        }
    }

    /// The canonical worked example, verified against the reference.
    #[test]
    fn renders_a_list_of_dicts() {
        let out = render("list-users", &json!({"Users": [{"Name": "a", "Id": 1}]})).unwrap();
        assert_eq!(
            out,
            "------------------\n\
             |   list-users   |\n\
             +----------------+\n\
             ||     Users    ||\n\
             |+-----+--------+|\n\
             || Id  | Name   ||\n\
             |+-----+--------+|\n\
             ||  1  |  a     ||\n\
             |+-----+--------+|\n"
        );
    }

    /// A single scalar key uses the two-column vertical form with no header.
    #[test]
    fn single_scalar_key_is_vertical() {
        let out = render("op", &json!({"Key": "value"})).unwrap();
        assert_eq!(
            out,
            "------------------\n\
             |       op       |\n\
             +------+---------+\n\
             |  Key |  value  |\n\
             +------+---------+\n"
        );
    }

    #[test]
    fn two_scalar_keys_get_a_header_row() {
        let out = render("op", &json!({"A": 1, "B": 2})).unwrap();
        assert_eq!(
            out,
            "------------\n\
             |    op    |\n\
             +----+-----+\n\
             |  A |  B  |\n\
             +----+-----+\n\
             |  1 |  2  |\n\
             +----+-----+\n"
        );
    }

    #[test]
    fn a_list_of_scalars_is_one_column() {
        let out = render("op", &json!(["x", "y"])).unwrap();
        assert_eq!(
            out,
            "-------\n\
             | op  |\n\
             +-----+\n\
             |  x  |\n\
             |  y  |\n\
             +-----+\n"
        );
    }

    #[test]
    fn centring_and_alignment_clamp_rather_than_underflow() {
        // A title wider than the space available must not panic.
        assert_eq!(center_text("averylongtitle", 4, "|", "|"), "|averylongtitle|");
        assert_eq!(align_left("wide", 3, "|", "|"), "|wide|");
    }

    #[test]
    fn scalars_use_python_spellings() {
        assert_eq!(cell(&json!(null)), "None");
        assert_eq!(cell(&json!(true)), "True");
        assert_eq!(cell(&json!(1.5)), "1.5");
    }
}
