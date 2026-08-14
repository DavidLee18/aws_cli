//! Pagination: the pure parts — evaluating token/result paths and merging pages.
//!
//! The paginator configs use a small JMESPath subset. Measured across all 3,279 shipped
//! paginators: plain member names dominate (3,264 output tokens, 3,296 result keys),
//! with 38 dotted paths, 2 negative indexes (`Jobs[-1].JobId`) and exactly one
//! alternation (`s3api list-objects`: `NextMarker || Contents[-1].Key`). Anything outside
//! that subset evaluates to "absent", which terminates pagination rather than looping.

use serde_json::{Map, Value};

/// Evaluate a paginator path against a response.
///
/// Supports `A`, `A.B`, `A[0]`, `A[-1]`, and `X || Y` alternation (first non-null wins).
pub fn resolve_path(value: &Value, path: &str) -> Option<Value> {
    for alternative in path.split("||") {
        if let Some(found) = resolve_single(value, alternative.trim()) {
            if !found.is_null() {
                return Some(found);
            }
        }
    }
    None
}

fn resolve_single(value: &Value, path: &str) -> Option<Value> {
    let mut current = value.clone();
    for segment in path.split('.') {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        let (name, index) = match segment.split_once('[') {
            Some((name, rest)) => {
                let idx: i64 = rest.trim_end_matches(']').parse().ok()?;
                (name, Some(idx))
            }
            None => (segment, None),
        };

        if !name.is_empty() {
            current = current.get(name)?.clone();
        }
        if let Some(idx) = index {
            let items = current.as_array()?;
            // A negative index counts back from the end, as JMESPath does.
            let resolved = if idx < 0 { items.len() as i64 + idx } else { idx };
            current = items.get(usize::try_from(resolved).ok()?)?.clone();
        }
    }
    Some(current)
}

/// A paginator's configuration, reduced to what the runtime needs.
#[derive(Debug, Clone, Default)]
pub struct PaginationConfig {
    pub input_tokens: Vec<String>,
    pub output_tokens: Vec<String>,
    pub result_keys: Vec<String>,
    pub more_results: Option<String>,
    pub limit_key: Option<String>,
    pub non_aggregate_keys: Vec<String>,
}

/// Accumulates pages into the single merged object the CLI prints.
pub struct Accumulator {
    config: PaginationConfig,
    /// Result-key path -> accumulated value, in config order.
    results: Vec<(String, Value)>,
    /// Captured from the FIRST page only.
    non_aggregate: Map<String, Value>,
    first_page_seen: bool,
    pages_seen: usize,
    pub items_collected: usize,
    /// From a resume token: items of the primary key to drop from the FIRST page,
    /// because a previous run already returned them.
    starting_truncation: usize,
}

impl Accumulator {
    pub fn new(config: PaginationConfig) -> Self {
        Self::resuming(config, 0)
    }

    /// An accumulator that skips `starting_truncation` items of the primary result key
    /// on the first page, resuming a previously truncated run.
    pub fn resuming(config: PaginationConfig, starting_truncation: usize) -> Self {
        let results = config.result_keys.iter().map(|k| (k.clone(), Value::Null)).collect();
        Accumulator {
            config,
            results,
            non_aggregate: Map::new(),
            first_page_seen: false,
            pages_seen: 0,
            items_collected: 0,
            starting_truncation,
        }
    }

    /// The offset to record in a resume token emitted from the current page.
    pub fn truncation_offset(&self, kept: usize) -> usize {
        // Offsets compose across successive resumes.
        kept + if self.first_page_only() { self.starting_truncation } else { 0 }
    }

    fn first_page_only(&self) -> bool {
        self.pages_seen == 1
    }

    /// Fold one page in. `max_items` truncates the final page when supplied.
    pub fn add_page(&mut self, page: &Value, max_items: Option<usize>) -> PageOutcome {
        self.pages_seen += 1;
        let skip = if self.pages_seen == 1 { self.starting_truncation } else { 0 };
        // Non-aggregate keys come from the first page and are never overwritten: they
        // describe the query, not the results.
        if !self.first_page_seen {
            for key in &self.config.non_aggregate_keys {
                // Recorded even when absent, which is why `s3api list-buckets` prints
                // `"Prefix": null` where a plain call prints nothing.
                let value = resolve_path(page, key).unwrap_or(Value::Null);
                self.non_aggregate.insert(key.clone(), value);
            }
            self.first_page_seen = true;
        }

        let mut truncated: Option<usize> = None;
        for (index, (key, accumulated)) in self.results.iter_mut().enumerate() {
            let Some(page_value) = resolve_path(page, key) else { continue };
            // `result_keys[0]` is the PRIMARY key: it alone is counted against
            // --max-items and truncated. Secondary keys accumulate in full.
            let is_primary = index == 0;
            match page_value {
                Value::Array(mut items) => {
                    // Drop what a previous run already returned from this page.
                    if is_primary && skip > 0 {
                        items = items.into_iter().skip(skip).collect();
                    }
                    if let Some(limit) = max_items.filter(|_| is_primary) {
                        let remaining = limit.saturating_sub(self.items_collected);
                        if items.len() > remaining {
                            items.truncate(remaining);
                            truncated = Some(items.len());
                        }
                    }
                    if is_primary {
                        self.items_collected += items.len();
                    }
                    match accumulated {
                        Value::Array(existing) => existing.extend(items),
                        _ => *accumulated = Value::Array(items),
                    }
                }
                // Non-list result keys accumulate BY TYPE, and the rules are not
                // uniform (botocore paginate.py:506-520):
                //   int/float  -> summed   (dynamodb Query/Scan `Count`)
                //   string     -> concatenated (rds DownloadDBLogFilePortion)
                //   map/struct -> first page wins, later pages are DROPPED
                Value::Object(entries) => {
                    if accumulated.is_null() {
                        *accumulated = Value::Object(entries);
                    }
                }
                Value::Number(n) => match accumulated.as_f64() {
                    Some(existing) => {
                        let sum = existing + n.as_f64().unwrap_or(0.0);
                        *accumulated = if sum.fract() == 0.0 {
                            Value::from(sum as i64)
                        } else {
                            Value::from(sum)
                        };
                    }
                    None => *accumulated = Value::Number(n),
                },
                Value::String(s) => match accumulated.as_str() {
                    Some(existing) => *accumulated = Value::String(format!("{existing}{s}")),
                    None => *accumulated = Value::String(s),
                },
                other => {
                    if accumulated.is_null() {
                        *accumulated = other;
                    }
                }
            }
        }

        match truncated {
            Some(kept) => PageOutcome::Truncated { kept },
            None => PageOutcome::Continue,
        }
    }

    /// Whether `--max-items` has been reached.
    pub fn reached_limit(&self, max_items: Option<usize>) -> bool {
        max_items.is_some_and(|limit| self.items_collected >= limit)
    }

    /// Produce the merged object.
    ///
    /// Key order is: result keys (in config order), then non-aggregate keys, then
    /// `NextToken` when the run stopped early. A result key that never produced a value
    /// is omitted, matching a plain call's output.
    pub fn finish(self, next_token: Option<String>) -> Value {
        let mut out = Map::new();
        for (key, value) in self.results {
            if value.is_null() {
                continue;
            }
            // Only the leaf name is used: `ResultSet.Rows` prints as `Rows`.
            out.insert(leaf_name(&key).to_string(), value);
        }
        for (key, value) in self.non_aggregate {
            out.insert(leaf_name(&key).to_string(), value);
        }
        if let Some(token) = next_token {
            out.insert("NextToken".to_string(), Value::String(token));
        }
        Value::Object(out)
    }
}

/// Encode a resume token the way botocore does.
///
/// The emitted `NextToken` is **not** the raw service token: it is
/// `base64(json.dumps(token_dict))`, where the dict maps each `input_token` name to its
/// value, plus `boto_truncate_amount` when a page was cut mid-way. Emitting the raw token
/// would produce something the reference cannot resume from, and vice versa.
///
/// The JSON uses Python's `", "`/`": "` separators and insertion order (input-token
/// config order), and the base64 is standard with `=` padding.
pub fn encode_resume_token(
    input_tokens: &[String],
    values: &[Value],
    truncate_amount: Option<usize>,
) -> String {
    let mut entries: Vec<(String, Value)> = input_tokens
        .iter()
        .cloned()
        .zip(values.iter().cloned().chain(std::iter::repeat(Value::Null)))
        .collect();
    if let Some(amount) = truncate_amount {
        entries.push(("boto_truncate_amount".to_string(), Value::from(amount)));
    }
    let object: Map<String, Value> = entries.into_iter().collect();
    crate::shapes::base64_encode(crate::json::to_python_json(&Value::Object(object)).as_bytes())
}

/// Decode a `--starting-token` produced by [`encode_resume_token`].
///
/// Returns the token values keyed by input-token name, plus the `boto_truncate_amount`
/// offset — the number of items already returned from the page this token points at, so
/// the resumed run must skip them. Dropping that offset silently re-returns items.
///
/// A token that is not valid base64-JSON is treated as an opaque service token for the
/// first input token, which is what botocore's deprecated fallback path does.
pub fn decode_resume_token(token: &str, input_tokens: &[String]) -> (Map<String, Value>, usize) {
    let decoded = crate::shapes::base64_decode(token)
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());

    match decoded {
        Some(Value::Object(mut map)) => {
            let skip = map
                .remove("boto_truncate_amount")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            (map, skip)
        }
        _ => {
            let mut map = Map::new();
            if let Some(first) = input_tokens.first() {
                map.insert(first.clone(), Value::String(token.to_string()));
            }
            (map, 0)
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PageOutcome {
    Continue,
    /// `--max-items` cut this page short. `kept` is how many items of the primary result
    /// key survived, which becomes `boto_truncate_amount` in the resume token so a
    /// subsequent run can skip them.
    Truncated { kept: usize },
}

fn leaf_name(path: &str) -> &str {
    let without_index = path.split('[').next().unwrap_or(path);
    without_index.rsplit('.').next().unwrap_or(without_index)
}

/// Extract the next-page token(s) from a response.
///
/// Returns `None` when pagination should stop: no token, an empty token, or
/// `more_results` explicitly false.
pub fn next_token(page: &Value, config: &PaginationConfig) -> Option<Vec<Value>> {
    // `more_results` is authoritative when present.
    if let Some(flag) = &config.more_results {
        match resolve_path(page, flag) {
            Some(Value::Bool(true)) => {}
            // Anything other than an explicit `true` ends the run.
            _ => return None,
        }
    }

    let tokens: Vec<Value> = config
        .output_tokens
        .iter()
        .map(|path| resolve_path(page, path).unwrap_or(Value::Null))
        .collect();

    // All-absent or all-empty means there is no next page.
    let usable = tokens.iter().any(|t| match t {
        Value::Null => false,
        Value::String(s) => !s.is_empty(),
        _ => true,
    });
    usable.then_some(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolves_plain_dotted_and_indexed_paths() {
        let page = json!({
            "NextMarker": "m1",
            "Contents": [{"Key": "a"}, {"Key": "b"}],
            "ResultSet": {"Rows": [1, 2]},
            "Jobs": [{"JobId": "j1"}, {"JobId": "j2"}]
        });
        assert_eq!(resolve_path(&page, "NextMarker"), Some(json!("m1")));
        assert_eq!(resolve_path(&page, "ResultSet.Rows"), Some(json!([1, 2])));
        // Negative indexing counts from the end.
        assert_eq!(resolve_path(&page, "Jobs[-1].JobId"), Some(json!("j2")));
        assert_eq!(resolve_path(&page, "Contents[0].Key"), Some(json!("a")));
        assert_eq!(resolve_path(&page, "Missing"), None);
        assert_eq!(resolve_path(&page, "ResultSet.Missing"), None);
    }

    /// The one alternation in the corpus: `s3api list-objects`.
    #[test]
    fn alternation_takes_the_first_non_null() {
        let with_marker = json!({"NextMarker": "m", "Contents": [{"Key": "k"}]});
        assert_eq!(
            resolve_path(&with_marker, "NextMarker || Contents[-1].Key"),
            Some(json!("m"))
        );
        let without = json!({"Contents": [{"Key": "last"}]});
        assert_eq!(
            resolve_path(&without, "NextMarker || Contents[-1].Key"),
            Some(json!("last"))
        );
    }

    fn config() -> PaginationConfig {
        PaginationConfig {
            input_tokens: vec!["Marker".into()],
            output_tokens: vec!["NextMarker".into()],
            result_keys: vec!["Items".into()],
            non_aggregate_keys: vec!["Owner".into(), "Prefix".into()],
            ..Default::default()
        }
    }

    #[test]
    fn accumulates_lists_across_pages() {
        let mut acc = Accumulator::new(config());
        acc.add_page(&json!({"Items": [1, 2], "Owner": "me"}), None);
        acc.add_page(&json!({"Items": [3], "Owner": "someone-else"}), None);
        let result = acc.finish(None);

        assert_eq!(result["Items"], json!([1, 2, 3]));
        // Non-aggregate keys come from the FIRST page only.
        assert_eq!(result["Owner"], json!("me"));
        // ...and absent ones are recorded as null, which is why `s3api list-buckets`
        // prints `"Prefix": null`.
        assert_eq!(result["Prefix"], Value::Null);
    }

    #[test]
    fn truncates_at_max_items_and_reports_it() {
        let mut acc = Accumulator::new(config());
        assert_eq!(acc.add_page(&json!({"Items": [1, 2]}), Some(3)), PageOutcome::Continue);
        // One item of the second page survives, and that count rides in the token.
        assert_eq!(
            acc.add_page(&json!({"Items": [3, 4, 5]}), Some(3)),
            PageOutcome::Truncated { kept: 1 }
        );
        assert!(acc.reached_limit(Some(3)));
        assert_eq!(acc.finish(Some("tok".into()))["Items"], json!([1, 2, 3]));
    }

    #[test]
    fn result_key_leaf_name_is_printed() {
        let mut acc = Accumulator::new(PaginationConfig {
            result_keys: vec!["ResultSet.Rows".into()],
            ..Default::default()
        });
        acc.add_page(&json!({"ResultSet": {"Rows": [1]}}), None);
        let out = acc.finish(None);
        assert_eq!(out["Rows"], json!([1]));
        assert!(out.get("ResultSet.Rows").is_none());
    }

    #[test]
    fn stops_on_absent_empty_or_more_results_false() {
        let c = config();
        assert!(next_token(&json!({"NextMarker": "m"}), &c).is_some());
        assert!(next_token(&json!({}), &c).is_none());
        assert!(next_token(&json!({"NextMarker": ""}), &c).is_none());

        let with_flag = PaginationConfig { more_results: Some("IsTruncated".into()), ..config() };
        assert!(next_token(&json!({"IsTruncated": true, "NextMarker": "m"}), &with_flag).is_some());
        // An explicit false ends the run even though a token is present.
        assert!(next_token(&json!({"IsTruncated": false, "NextMarker": "m"}), &with_flag).is_none());
    }

    /// The emitted token must round-trip, and must be botocore-shaped rather than the
    /// raw service token — otherwise `--starting-token` cannot be handed between the two
    /// CLIs.
    #[test]
    fn resume_tokens_round_trip() {
        let inputs = vec!["Marker".to_string()];
        let encoded = encode_resume_token(&inputs, &[json!("svc-token")], None);
        // base64 of Python-style JSON, not the raw token.
        assert_ne!(encoded, "svc-token");
        assert_eq!(
            String::from_utf8(crate::shapes::base64_decode(&encoded).unwrap()).unwrap(),
            r#"{"Marker": "svc-token"}"#
        );
        let (decoded, skip) = decode_resume_token(&encoded, &inputs);
        assert_eq!(decoded["Marker"], json!("svc-token"));
        assert_eq!(skip, 0);
    }

    #[test]
    fn truncation_amount_rides_along_and_is_stripped_on_decode() {
        let inputs = vec!["Marker".to_string()];
        let encoded = encode_resume_token(&inputs, &[json!("t")], Some(7));
        assert!(String::from_utf8(crate::shapes::base64_decode(&encoded).unwrap())
            .unwrap()
            .contains("boto_truncate_amount"));
        let (decoded, skip) = decode_resume_token(&encoded, &inputs);
        assert!(!decoded.contains_key("boto_truncate_amount"));
        // The offset is returned separately, not silently dropped.
        assert_eq!(skip, 7);
    }

    #[test]
    fn an_opaque_token_falls_back_to_the_first_input_token() {
        let inputs = vec!["Marker".to_string()];
        let (decoded, skip) = decode_resume_token("not-base64-json!!", &inputs);
        assert_eq!(decoded["Marker"], json!("not-base64-json!!"));
        assert_eq!(skip, 0);
    }

    /// Non-list result keys do not all accumulate the same way.
    #[test]
    fn accumulates_non_list_result_keys_by_type() {
        let mut acc = Accumulator::new(PaginationConfig {
            result_keys: vec!["Items".into(), "Count".into(), "Log".into(), "Facets".into()],
            ..Default::default()
        });
        acc.add_page(&json!({"Items": [1], "Count": 2, "Log": "a", "Facets": {"x": 1}}), None);
        acc.add_page(&json!({"Items": [2], "Count": 3, "Log": "b", "Facets": {"y": 2}}), None);
        let out = acc.finish(None);

        assert_eq!(out["Items"], json!([1, 2]), "lists concatenate");
        assert_eq!(out["Count"], json!(5), "integers sum");
        assert_eq!(out["Log"], json!("ab"), "strings concatenate");
        assert_eq!(out["Facets"], json!({"x": 1}), "maps keep the first page only");
    }

    /// Only the primary result key counts against --max-items.
    #[test]
    fn secondary_result_keys_are_not_truncated() {
        let mut acc = Accumulator::new(PaginationConfig {
            result_keys: vec!["Contents".into(), "CommonPrefixes".into()],
            ..Default::default()
        });
        acc.add_page(&json!({"Contents": [1, 2, 3], "CommonPrefixes": [9, 9, 9]}), Some(2));
        let out = acc.finish(None);
        assert_eq!(out["Contents"], json!([1, 2]));
        assert_eq!(out["CommonPrefixes"], json!([9, 9, 9]));
    }

    /// Resuming skips what the previous run already returned.
    #[test]
    fn starting_truncation_skips_items_of_the_first_page_only() {
        let mut acc = Accumulator::resuming(
            PaginationConfig { result_keys: vec!["Items".into()], ..Default::default() },
            2,
        );
        acc.add_page(&json!({"Items": [1, 2, 3]}), None);
        // The offset applies to the first page only.
        acc.add_page(&json!({"Items": [4, 5]}), None);
        assert_eq!(acc.finish(None)["Items"], json!([3, 4, 5]));
    }

    #[test]
    fn omits_result_keys_that_never_appeared() {
        let mut acc = Accumulator::new(PaginationConfig {
            result_keys: vec!["Items".into(), "Other".into()],
            ..Default::default()
        });
        acc.add_page(&json!({"Items": [1]}), None);
        let out = acc.finish(None);
        assert!(out.get("Other").is_none());
    }
}
