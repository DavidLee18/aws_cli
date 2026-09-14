//! The resource tree `ecs monitor-express-gateway-service` renders.
//!
//! A port of `customizations/ecs/expressgateway/managedresource.py` and
//! `managedresourcegroup.py`. Two node kinds — a leaf resource and a group of nodes —
//! with four operations between them: render, combine two trees, diff two trees as
//! *sets* (which is what the DEPLOYMENT view shows), and diff a tree against the previous
//! poll by *properties* (which is what TEXT-ONLY change detection shows).
//!
//! The ordering rules are the fiddly part and none of them are arbitrary:
//!
//! - A group keeps the **insertion order** of its children, and keeps a *duplicate* key
//!   in that order list while the map behind it dedupes — so a repeated key renders
//!   twice, both times showing the deduped node. That is Python's `list` plus `dict`
//!   behaviour and the reference depends on it.
//! - [`Group::combine`] **regroups children by resource type**, in order of each type's
//!   first appearance, rather than preserving the interleaved input order.

use std::collections::BTreeSet;

/// Statuses that mean a resource has stopped changing.
///
/// Ported because it is part of the model, and kept although nothing reads it: the
/// reference defines `is_terminal` on both classes and calls it from nowhere either.
#[allow(dead_code)]
const TERMINAL_STATUSES: [&str; 3] = ["ACTIVE", "DELETED", "FAILED"];

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Resource {
    pub resource_type: Option<String>,
    pub identifier: Option<String>,
    pub status: Option<String>,
    /// Unix seconds. The API sends a timestamp string, which is parsed on the way in.
    pub updated_at: Option<f64>,
    pub reason: Option<String>,
    pub additional_info: Option<String>,
}

/// One leaf as of the previous poll, keyed by type and identifier — which is how the
/// reference remembers what it has already reported.
pub type SeenResource = ((Option<String>, Option<String>), Resource);

#[derive(Clone, Debug)]
pub enum Node {
    Leaf(Resource),
    Group(Group),
}

#[derive(Clone, Debug, Default)]
pub struct Group {
    pub resource_type: Option<String>,
    pub identifier: Option<String>,
    pub status: Option<String>,
    pub reason: Option<String>,
    /// Every child key in insertion order, duplicates included.
    keys: Vec<String>,
    /// The deduped children, each at the position where its key first appeared.
    children: Vec<(String, Node)>,
}

impl Node {
    pub fn resource_type(&self) -> Option<&str> {
        match self {
            Node::Leaf(resource) => resource.resource_type.as_deref(),
            Node::Group(group) => group.resource_type.as_deref(),
        }
    }

    pub fn identifier(&self) -> Option<&str> {
        match self {
            Node::Leaf(resource) => resource.identifier.as_deref(),
            Node::Group(group) => group.identifier.as_deref(),
        }
    }

    /// `<type>/<identifier>`, with an absent half contributing an empty string.
    pub fn key(&self) -> String {
        format!(
            "{}/{}",
            self.resource_type().unwrap_or_default(),
            self.identifier().unwrap_or_default()
        )
    }

    /// See [`TERMINAL_STATUSES`]: reference parity, exercised only by the corpus test.
    #[allow(dead_code)]
    pub fn is_terminal(&self) -> bool {
        match self {
            Node::Leaf(resource) => resource.is_terminal(),
            Node::Group(group) => group.is_terminal(),
        }
    }

    pub fn status_string(&self, spinner: &str, depth: usize, color: bool) -> String {
        match self {
            Node::Leaf(resource) => resource.status_string(spinner, depth, color),
            Node::Group(group) => group.status_string(spinner, depth, color),
        }
    }

    /// The whole-tree stream rendering. TEXT-ONLY mode prints *changed leaves* rather
    /// than the tree, as the reference does, so this is reached only by the corpus test.
    #[allow(dead_code)]
    pub fn stream_string(&self, timestamp: &str, color: bool) -> String {
        match self {
            Node::Leaf(resource) => resource.stream_string(timestamp, color),
            Node::Group(group) => group.stream_string(timestamp, color),
        }
    }

    /// Keep whichever of two same-keyed nodes is more recent.
    pub fn combine(self, other: Node) -> Node {
        match (self, other) {
            (Node::Group(a), Node::Group(b)) => Node::Group(a.combine(b)),
            (Node::Leaf(a), Node::Leaf(b)) => Node::Leaf(a.combine(b)),
            // Mixed kinds cannot share a key in practice, since the key is built from the
            // same two fields either way. Keeping the first is the harmless answer.
            (a, _) => a,
        }
    }
}

impl Resource {
    pub fn new(resource_type: &str, identifier: Option<String>) -> Resource {
        Resource {
            resource_type: Some(resource_type.to_string()),
            identifier,
            ..Resource::default()
        }
    }

    #[allow(dead_code)]
    pub fn is_terminal(&self) -> bool {
        self.status.as_deref().is_some_and(|status| TERMINAL_STATUSES.contains(&status))
    }

    /// The nested, indented rendering used by both display modes' main view.
    pub fn status_string(&self, spinner: &str, depth: usize, color: bool) -> String {
        let mut lines = Vec::new();
        let indent = " ".repeat(depth);
        let mut header =
            format!("{indent}{}", cyan(self.resource_type.as_deref().unwrap_or(""), color));
        header.push_str(if self.identifier.is_some() { ": " } else { " " });
        header.push_str(&status_symbol(self.status.as_deref(), spinner, color));
        if let Some(identifier) = &self.identifier {
            header.push_str(&color_by_status(identifier, self.status.as_deref(), color));
            header.push(' ');
        }
        if let Some(status) = &self.status {
            header.push_str("- ");
            header.push_str(&color_by_status(status, Some(status), color));
        }
        lines.push(header);

        let inner = " ".repeat(depth + 1);
        if let Some(reason) = &self.reason {
            lines.push(format!("{inner}Reason: {reason}"));
        }
        if let Some(updated_at) = self.updated_at {
            lines.push(format!(
                "{inner}Last updated at: {}",
                // Local time, despite the trailing `Z`. See `docs/divergences.md`.
                format_timestamp(updated_at, true)
            ));
        }
        if let Some(info) = &self.additional_info {
            lines.push(format!("{inner}{info}"));
        }
        // The blank line that separates one resource from the next.
        lines.push(String::new());
        lines.join("\n")
    }

    /// The one-resource-per-change rendering used by TEXT-ONLY mode.
    pub fn stream_string(&self, timestamp: &str, color: bool) -> String {
        let mut parts = vec![format!("[{timestamp}]")];
        if self.resource_type.is_none() && self.identifier.is_none() {
            parts.push(cyan("Unknown Resource", color));
        } else {
            if let Some(resource_type) = &self.resource_type {
                parts.push(cyan(resource_type, color));
            }
            if let Some(identifier) = &self.identifier {
                parts.push(color_by_status(identifier, self.status.as_deref(), color));
            }
        }
        if let Some(status) = &self.status {
            parts.push(format!("[{}]", color_by_status(status, Some(status), color)));
        }
        let mut lines = vec![parts.join(" ")];
        if let Some(reason) = &self.reason {
            lines.push(format!("  Reason: {reason}"));
        }
        if let Some(updated_at) = self.updated_at {
            lines.push(format!("  Last Updated At: {}", format_timestamp(updated_at, false)));
        }
        if let Some(info) = &self.additional_info {
            lines.push(format!("  Info: {info}"));
        }
        lines.join("\n")
    }

    /// The more recently updated of two views of the same resource.
    ///
    /// **Divergence:** the reference compares `self.updated_at >= other.updated_at` after
    /// testing only *other* for `None`, so a `None` on this side against a timestamp on
    /// the other raises `TypeError`. Here an absent timestamp simply loses.
    pub fn combine(self, other: Resource) -> Resource {
        match (self.updated_at, other.updated_at) {
            (_, None) => self,
            (None, Some(_)) => other,
            (Some(mine), Some(theirs)) => {
                if mine >= theirs {
                    self
                } else {
                    other
                }
            }
        }
    }

    /// Did anything about this resource change since the previous poll?
    pub fn differs_from(&self, previous: Option<&Resource>) -> bool {
        match previous {
            None => true,
            Some(previous) => self != previous,
        }
    }
}

impl Group {
    pub fn new(resources: Vec<Node>) -> Group {
        Group::with(None, None, resources)
    }

    pub fn with(
        resource_type: Option<&str>,
        identifier: Option<String>,
        resources: Vec<Node>,
    ) -> Group {
        let mut group = Group {
            resource_type: resource_type.map(str::to_string),
            identifier,
            ..Group::default()
        };
        for resource in resources {
            group.push(resource);
        }
        group
    }

    /// Add a child. A repeated key keeps its original position in the map but is listed
    /// again in the key order, which is what a Python `list` plus `dict` does.
    fn push(&mut self, node: Node) {
        let key = node.key();
        self.keys.push(key.clone());
        match self.children.iter_mut().find(|(existing, _)| *existing == key) {
            Some((_, slot)) => *slot = node,
            None => self.children.push((key, node)),
        }
    }

    fn get(&self, key: &str) -> Option<&Node> {
        self.children.iter().find(|(existing, _)| existing == key).map(|(_, node)| node)
    }

    #[allow(dead_code)]
    pub fn is_terminal(&self) -> bool {
        self.children.iter().all(|(_, node)| node.is_terminal())
    }

    pub fn status_string(&self, spinner: &str, depth: usize, color: bool) -> String {
        let mut lines = Vec::new();
        if let Some(resource_type) = &self.resource_type {
            let mut header = format!("{}{}", " ".repeat(depth), cyan(resource_type, color));
            if let Some(identifier) = &self.identifier {
                header.push_str(": ");
                match &self.status {
                    Some(status) => {
                        header.push_str(&status_symbol(Some(status), spinner, color));
                        header.push_str(&color_by_status(identifier, Some(status), color));
                        header.push_str(" - ");
                        header.push_str(&color_by_status(status, Some(status), color));
                    }
                    None => header.push_str(identifier),
                }
            } else if let Some(status) = &self.status {
                header.push(' ');
                header.push_str(&status_symbol(Some(status), spinner, color));
                header.push_str("- ");
                header.push_str(&color_by_status(status, Some(status), color));
            }
            lines.push(header);

            if let (Some(_), Some(reason)) = (&self.status, &self.reason) {
                lines.push(format!("{}Reason: {reason}", " ".repeat(depth + 1)));
            }
            if !self.keys.is_empty() {
                lines.push(String::new());
            }
        }

        let offset = usize::from(self.resource_type.is_some());
        for key in &self.keys {
            if let Some(node) = self.get(key) {
                lines.push(node.status_string(spinner, depth + offset, color));
            }
        }
        if self.keys.is_empty() && self.resource_type.is_some() {
            lines.push(format!("{}<empty>", " ".repeat(depth + offset)));
        }
        lines.join("\n")
    }

    /// Every leaf under this group, rendered one per line.
    #[allow(dead_code)]
    pub fn stream_string(&self, timestamp: &str, color: bool) -> String {
        let rendered: Vec<String> = self
            .children
            .iter()
            .filter_map(|(_, node)| {
                let text = node.stream_string(timestamp, color);
                // A group with no leaves renders as nothing at all, and an empty entry
                // would otherwise become a blank line.
                if matches!(node, Node::Group(_)) && text.is_empty() {
                    None
                } else {
                    Some(text)
                }
            })
            .collect();
        rendered.join("\n")
    }

    pub fn flatten(&self) -> Vec<&Resource> {
        let mut out = Vec::new();
        for (_, node) in &self.children {
            match node {
                Node::Leaf(resource) => out.push(resource),
                Node::Group(group) => out.extend(group.flatten()),
            }
        }
        out
    }

    /// Which leaves changed since the previous poll, and the state to compare against
    /// next time.
    pub fn changed_since(&self, previous: &[SeenResource]) -> (Vec<Resource>, Vec<SeenResource>) {
        let mut changed = Vec::new();
        let mut updated = Vec::new();
        for resource in self.flatten() {
            let key = (resource.resource_type.clone(), resource.identifier.clone());
            let before = previous.iter().find(|(k, _)| *k == key).map(|(_, r)| r);
            if resource.differs_from(before) {
                changed.push(resource.clone());
            }
            updated.push((key, resource.clone()));
        }
        (changed, updated)
    }

    /// Merge two trees, keeping the more recent view of anything they share.
    ///
    /// Children come out **grouped by resource type** in order of each type's first
    /// appearance, not in the interleaved order they went in. And when a type has both
    /// identified and unidentified members, the unidentified ones are dropped: they are
    /// the placeholder a revision emits before the real resource exists.
    pub fn combine(self, other: Group) -> Group {
        let mut all: Vec<Node> =
            self.children.into_iter().map(|(_, node)| node).collect();
        all.extend(other.children.into_iter().map(|(_, node)| node));

        let mut types: Vec<Option<String>> = Vec::new();
        for node in &all {
            let resource_type = node.resource_type().map(str::to_string);
            if !types.contains(&resource_type) {
                types.push(resource_type);
            }
        }

        let mut filtered: Vec<Node> = Vec::new();
        for resource_type in types {
            let of_type: Vec<&Node> = all
                .iter()
                .filter(|node| node.resource_type().map(str::to_string) == resource_type)
                .collect();
            let any_identified = of_type.iter().any(|node| node.identifier().is_some());
            let any_unidentified = of_type.iter().any(|node| node.identifier().is_none());
            for node in of_type {
                if any_identified && any_unidentified && node.identifier().is_none() {
                    continue;
                }
                filtered.push(node.clone());
            }
        }

        let mut combined = Group {
            resource_type: self.resource_type,
            identifier: self.identifier,
            ..Group::default()
        };
        for node in filtered {
            let key = node.key();
            match combined.children.iter().position(|(existing, _)| *existing == key) {
                Some(index) => {
                    let (_, existing) = combined.children.remove(index);
                    combined.children.insert(index, (key, existing.combine(node)));
                }
                None => {
                    combined.keys.push(key.clone());
                    combined.children.push((key, node));
                }
            }
        }
        combined
    }

    /// What is in this tree and not the other, and vice versa — the DEPLOYMENT view.
    ///
    /// The asymmetry in the middle is deliberate in the reference: a type this side has
    /// *without* an identifier suppresses every key of that type on the other side. An
    /// unidentified entry means "this type exists but is not resolved yet", so reporting
    /// the other side's resolved ones as disassociating would be wrong.
    pub fn compare_sets(&self, other: &Group) -> (Group, Group) {
        let self_keys: BTreeSet<&str> =
            self.children.iter().map(|(key, _)| key.as_str()).collect();
        let other_keys: BTreeSet<&str> =
            other.children.iter().map(|(key, _)| key.as_str()).collect();

        let types_without_id: Vec<&str> = self_keys
            .iter()
            .filter(|key| key.ends_with('/'))
            .map(|key| key.trim_end_matches('/'))
            .collect();

        let mut common_self: Vec<(String, Node)> = Vec::new();
        let mut common_other: Vec<(String, Node)> = Vec::new();
        for key in self_keys.intersection(&other_keys) {
            // Only groups recurse; a leaf present on both sides is unchanged by
            // definition, since the key is everything that identifies it.
            let (Some(Node::Group(mine)), Some(other_node)) = (self.get(key), other.get(key))
            else {
                continue;
            };
            let theirs = match other_node {
                Node::Group(group) => group.clone(),
                Node::Leaf(_) => Group::default(),
            };
            let (unique_mine, unique_theirs) = mine.compare_sets(&theirs);
            common_self.push((key.to_string(), Node::Group(unique_mine)));
            common_other.push((key.to_string(), Node::Group(unique_theirs)));
        }

        let mut self_resources = Vec::new();
        for key in &self.keys {
            if !other_keys.contains(key.as_str()) {
                if let Some(node) = self.get(key) {
                    self_resources.push(node.clone());
                }
            } else if let Some((_, node)) = common_self.iter().find(|(k, _)| k == key) {
                self_resources.push(node.clone());
            }
        }

        let mut other_resources = Vec::new();
        for key in &other.keys {
            let suppressed = types_without_id
                .iter()
                .any(|resource_type| key.starts_with(&format!("{resource_type}/")));
            if !self_keys.contains(key.as_str()) {
                if !suppressed {
                    if let Some(node) = other.get(key) {
                        other_resources.push(node.clone());
                    }
                }
            } else if let Some((_, node)) = common_other.iter().find(|(k, _)| k == key) {
                other_resources.push(node.clone());
            }
        }

        (
            Group::with(self.resource_type.as_deref(), self.identifier.clone(), self_resources),
            Group::with(self.resource_type.as_deref(), self.identifier.clone(), other_resources),
        )
    }
}

const CHECK_MARK: &str = "✓";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const MAGENTA: &str = "\x1b[35m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const RESET: &str = "\x1b[0m";

fn paint(text: &str, code: &str, color: bool) -> String {
    if color {
        format!("{code}{text}{RESET}")
    } else {
        text.to_string()
    }
}

fn cyan(text: &str, color: bool) -> String {
    paint(text, CYAN, color)
}

/// Green for healthy or unknown, red for failed, yellow for gone, magenta for in flight.
pub fn color_by_status(text: &str, status: Option<&str>, color: bool) -> String {
    match status {
        None | Some("ACTIVE") | Some("SUCCESSFUL") => paint(text, GREEN, color),
        Some("FAILED") => paint(text, RED, color),
        Some("DELETED") => paint(text, YELLOW, color),
        _ => paint(text, MAGENTA, color),
    }
}

/// The glyph in front of a resource. Note the trailing space is part of it.
fn status_symbol(status: Option<&str>, spinner: &str, color: bool) -> String {
    match status {
        None | Some("ACTIVE") | Some("SUCCESSFUL") => {
            paint(&format!("{CHECK_MARK} "), GREEN, color)
        }
        Some("FAILED") | Some("ROLLBACK_FAILED") => paint("X ", RED, color),
        Some("DELETED") | Some("STOPPED") | Some("ROLLBACK_SUCCESSFUL") => {
            paint("— ", YELLOW, color)
        }
        _ => paint(&format!("{spinner} "), MAGENTA, color),
    }
}

/// `%Y-%m-%dT%H:%M:%SZ` for the nested view, `%Y-%m-%d %H:%M:%SZ` for the stream one.
///
/// **The nested view renders local time under a `Z` suffix**, because the reference calls
/// `datetime.fromtimestamp()` with no timezone there and `tz=timezone.utc` in the other.
/// Reproduced rather than corrected — see `docs/divergences.md`.
fn format_timestamp(unix: f64, local: bool) -> String {
    let seconds = unix as i64;
    let seconds = if local { seconds + local_offset(seconds) } else { seconds };
    let (year, month, day, hour, minute, second) = civil(seconds);
    let separator = if local { 'T' } else { ' ' };
    format!("{year:04}-{month:02}-{day:02}{separator}{hour:02}:{minute:02}:{second:02}Z")
}

fn local_offset(unix: i64) -> i64 {
    awsc_runtime::localtime::offset_seconds(unix)
}

/// Split a unix timestamp into civil components.
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// Cases rendered by the reference's own classes.
    ///
    /// `scripts/extract-ecs-express-cases.py` regenerates them: it runs
    /// `managedresource.py` and `managedresourcegroup.py` unmodified, against stub
    /// `colorama` and `dateutil` modules, so the expectations are the reference's output
    /// rather than a reading of its source.
    ///
    /// Generated under `TZ=Asia/Seoul`, which matters: the nested view renders *local*
    /// time, so a UTC-only corpus would have hidden that. The offset it was generated at
    /// is recorded in the file, and on a machine at a different one the `Last updated at`
    /// lines are compared for shape rather than for value — everything else still is.
    fn corpus() -> Value {
        serde_json::from_str(include_str!("../../../../tests/golden/ecs-express-cases.json"))
            .expect("the corpus parses")
    }

    /// True when this machine renders local time the same way the corpus was generated.
    fn same_zone(corpus: &Value) -> bool {
        let recorded = corpus["tz_offset_seconds"].as_i64().expect("an offset");
        awsc_runtime::localtime::offset_seconds(1_789_380_000) == recorded
    }

    /// Drop the one line whose value depends on the machine's timezone.
    fn without_local_timestamps(text: &str) -> String {
        text.lines()
            .filter(|line| !line.trim_start().starts_with("Last updated at: "))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn build(spec: &Value) -> Node {
        let text = |key: &str| spec.get(key).and_then(Value::as_str).map(str::to_string);
        if spec["kind"] == "leaf" {
            return Node::Leaf(Resource {
                resource_type: text("type"),
                identifier: text("id"),
                status: text("status"),
                updated_at: text("updated_at").map(|stamp| parse_timestamp(&stamp)),
                reason: text("reason"),
                additional_info: text("info"),
            });
        }
        let children = spec
            .get("children")
            .and_then(Value::as_array)
            .map(|items| items.iter().map(build).collect())
            .unwrap_or_default();
        let mut group = Group::with(text("type").as_deref(), text("id"), children);
        group.status = text("status");
        group.reason = text("reason");
        Node::Group(group)
    }

    /// `YYYY-MM-DDTHH:MM:SSZ` to unix seconds, which is all the corpus uses.
    fn parse_timestamp(text: &str) -> f64 {
        let number = |range: std::ops::Range<usize>| text[range].parse::<i64>().expect("digits");
        let (year, month, day) = (number(0..4), number(5..7), number(8..10));
        let (hour, minute, second) = (number(11..13), number(14..16), number(17..19));
        let m = if month <= 2 { month + 12 } else { month };
        let y = if month <= 2 { year - 1 } else { year };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * (m - 3) + 2) / 5 + day - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146_097 + doe - 719_468;
        (days * 86_400 + hour * 3600 + minute * 60 + second) as f64
    }

    #[test]
    fn the_timestamp_parser_round_trips_through_the_formatter() {
        let unix = parse_timestamp("2026-09-14T10:00:00Z");
        assert_eq!(unix, 1_789_380_000.0);
        assert_eq!(format_timestamp(unix, false), "2026-09-14 10:00:00Z");
    }

    #[test]
    fn every_tree_renders_the_way_the_reference_renders_it() {
        let corpus = corpus();
        let zoned = same_zone(&corpus);
        let compare = |ours: String, theirs: &str| {
            if zoned {
                (ours, theirs.to_string())
            } else {
                (without_local_timestamps(&ours), without_local_timestamps(theirs))
            }
        };
        for case in corpus["render"].as_array().expect("an array") {
            let name = case["name"].as_str().expect("a name");
            let node = build(&case["tree"]);
            assert_eq!(
                node.stream_string("TS", false),
                case["stream_plain"].as_str().expect("plain stream"),
                "{name} stream, plain"
            );
            assert_eq!(
                node.stream_string("TS", true),
                case["stream_color"].as_str().expect("coloured stream"),
                "{name} stream, coloured"
            );
            assert_eq!(
                node.is_terminal(),
                case["is_terminal"].as_bool().expect("terminal"),
                "{name} is_terminal"
            );
            // The reference crashes rendering a resource with no type in the nested view,
            // so there is nothing to compare against for that one.
            if case.get("status_error").is_some() {
                continue;
            }
            let (ours, theirs) = compare(
                node.status_string("*", 0, false),
                case["status_plain"].as_str().expect("plain status"),
            );
            assert_eq!(ours, theirs, "{name} status, plain");
            let (ours, theirs) = compare(
                node.status_string("*", 0, true),
                case["status_color"].as_str().expect("coloured status"),
            );
            assert_eq!(ours, theirs, "{name} status, coloured");
            let (ours, theirs) = compare(
                node.status_string("*", 2, false),
                case["status_depth2"].as_str().expect("indented status"),
            );
            assert_eq!(ours, theirs, "{name} status, indented");
        }
    }

    #[test]
    fn combining_two_trees_matches_the_reference() {
        let corpus = corpus();
        let zoned = same_zone(&corpus);
        for case in corpus["combine"].as_array().expect("an array") {
            let name = case["name"].as_str().expect("a name");
            let (Node::Group(left), Node::Group(right)) =
                (build(&case["left"]), build(&case["right"]))
            else {
                panic!("{name}: both sides are groups");
            };
            let ours = left.combine(right).status_string("*", 0, false);
            let theirs = case["status_plain"].as_str().expect("plain status");
            if zoned {
                assert_eq!(ours, theirs, "{name}");
            } else {
                assert_eq!(
                    without_local_timestamps(&ours),
                    without_local_timestamps(theirs),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn set_comparison_matches_the_reference() {
        for case in corpus()["compare"].as_array().expect("an array") {
            let name = case["name"].as_str().expect("a name");
            let (Node::Group(left), Node::Group(right)) =
                (build(&case["left"]), build(&case["right"]))
            else {
                panic!("{name}: both sides are groups");
            };
            let (unique_left, unique_right) = left.compare_sets(&right);
            assert_eq!(
                unique_left.status_string("*", 0, false),
                case["unique_left"].as_str().expect("left"),
                "{name} left"
            );
            assert_eq!(
                unique_right.status_string("*", 0, false),
                case["unique_right"].as_str().expect("right"),
                "{name} right"
            );
        }
    }

    /// Change detection is by *properties*, not by key: a resource whose status moved on
    /// is reported again, one that did not is silent.
    #[test]
    fn only_changed_resources_are_reported_between_polls() {
        let tree = Group::new(vec![
            Node::Leaf(Resource {
                status: Some("ACTIVE".to_string()),
                ..Resource::new("A", Some("1".to_string()))
            }),
            Node::Leaf(Resource::new("B", Some("2".to_string()))),
        ]);
        let (changed, state) = tree.changed_since(&[]);
        assert_eq!(changed.len(), 2, "everything is new on the first poll");

        let (changed, state) = tree.changed_since(&state);
        assert!(changed.is_empty(), "nothing changed on an identical poll");

        let moved = Group::new(vec![
            Node::Leaf(Resource {
                status: Some("DELETED".to_string()),
                ..Resource::new("A", Some("1".to_string()))
            }),
            Node::Leaf(Resource::new("B", Some("2".to_string()))),
        ]);
        let (changed, _) = moved.changed_since(&state);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].status.as_deref(), Some("DELETED"));
    }
}
