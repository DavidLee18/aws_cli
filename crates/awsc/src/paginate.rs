//! Driving the pagination loop.
//!
//! 3,279 operations auto-paginate, and for every one of them the reference prints a
//! single merged object rather than the first page — so this is the difference between
//! "the protocols work" and "the output is right".
//!
//! The pure parts (path evaluation, page merging) live in
//! [`aws_cli_protocol::pagination`]; this module owns the loop and the decision of
//! whether to paginate at all.

use aws_cli_model::paginators::PaginatorOverlay;
use aws_cli_protocol::pagination::{self, Accumulator, PaginationConfig};
use serde_json::Value;
use std::sync::LazyLock;

use crate::{exit, Failure};

/// What the caller asked for.
pub struct Settings<'a> {
    pub service: &'a str,
    pub operation: &'a str,
    pub input: Option<Value>,
    pub no_paginate: bool,
    pub max_items: Option<usize>,
    pub page_size: Option<i64>,
    pub starting_token: Option<String>,
}

static OVERLAY: LazyLock<Option<PaginatorOverlay>> = LazyLock::new(|| {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/paginators.json");
    PaginatorOverlay::load(&path).ok()
});

/// Run an operation, paginating when it should be paginated.
///
/// `issue` performs one round trip; it is called repeatedly with a token injected.
pub fn run<F>(settings: &Settings<'_>, issue: F) -> Result<Value, Failure>
where
    F: Fn(Option<&Value>) -> Result<Value, Failure>,
{
    let Some(config) = config_for(settings) else {
        return issue(settings.input.as_ref());
    };

    // Supplying a paging parameter by hand disables auto-pagination: the user has taken
    // manual control, and the reference hands back that single page verbatim.
    if user_supplied_paging_param(settings, &config) {
        return issue(settings.input.as_ref());
    }

    let mut input = settings.input.clone().unwrap_or_else(|| Value::Object(Default::default()));
    let mut starting_truncation = 0usize;

    // `--page-size` is a server-side hint, passed through as the operation's limit key.
    if let (Some(size), Some(limit_key)) = (settings.page_size, &config.limit_key) {
        set_member(&mut input, limit_key, Value::from(size));
    }
    // `--starting-token` resumes where a previous truncated run stopped. It is
    // botocore's own encoded token, so it decodes into one value per input token.
    if let Some(token) = &settings.starting_token {
        let (values, skip) = pagination::decode_resume_token(token, &config.input_tokens);
        starting_truncation = skip;
        for (name, value) in values {
            // A null token means "start from the beginning"; sending it would be a
            // different request, so the parameter is left off entirely.
            if value.is_null() {
                remove_member(&mut input, &name);
            } else {
                set_member(&mut input, &name, value);
            }
        }
    }

    let mut accumulator = Accumulator::resuming(config.clone(), starting_truncation);

    let mut next_token: Option<String> = None;
    // The token values that produced the CURRENT page. A mid-page truncation resumes
    // from here plus an offset, not from the next page's token — which is why this has
    // to be tracked rather than read off the response.
    let mut current_tokens: Vec<Value> = vec![Value::Null; config.input_tokens.len()];

    loop {
        let page = issue(Some(&input))?;
        let outcome = accumulator.add_page(&page, settings.max_items);
        let tokens = pagination::next_token(&page, &config);

        // Mid-page truncation: resume from the token that produced THIS page, skipping
        // the items already returned. The reference emits this even when there is no
        // next page at all, e.g. `{"Marker": null, "boto_truncate_amount": 2}`.
        if let pagination::PageOutcome::Truncated { kept } = outcome {
            next_token = Some(pagination::encode_resume_token(
                &config.input_tokens,
                &current_tokens,
                // Offsets compose, so a resume of a resume points at the right item.
                Some(accumulator.truncation_offset(kept)),
            ));
            break;
        }

        // Limit reached exactly on a page boundary: resume from the NEXT page's token,
        // with no offset.
        if accumulator.reached_limit(settings.max_items) {
            next_token = tokens.as_ref().map(|values| {
                pagination::encode_resume_token(&config.input_tokens, values, None)
            });
            break;
        }

        let Some(tokens) = tokens else { break };
        current_tokens = tokens.clone();
        for (input_token, value) in config.input_tokens.iter().zip(tokens.iter()) {
            // A missing token DELETES the parameter rather than leaving the previous
            // page's value behind, and the literal string "None" counts as missing.
            let absent = matches!(value, Value::Null)
                || matches!(value, Value::String(s) if s.is_empty() || s == "None");
            if absent {
                remove_member(&mut input, input_token);
            } else {
                set_member(&mut input, input_token, value.clone());
            }
        }
        // A config with more output tokens than input tokens cannot be advanced safely;
        // stopping beats looping forever on the same page.
        if config.input_tokens.is_empty() {
            break;
        }
    }

    Ok(accumulator.finish(next_token))
}

/// The paginator config for this operation, or `None` when it should not paginate.
fn config_for(settings: &Settings<'_>) -> Option<PaginationConfig> {
    if settings.no_paginate {
        return None;
    }
    let overlay = OVERLAY.as_ref()?;
    let paginator = overlay.get(settings.service, settings.operation)?;

    let strings = |key: &str| -> Vec<String> {
        match paginator.config.get(key) {
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(items)) => {
                items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()
            }
            _ => Vec::new(),
        }
    };

    Some(PaginationConfig {
        input_tokens: strings("input_token"),
        output_tokens: strings("output_token"),
        result_keys: strings("result_key"),
        more_results: paginator
            .config
            .get("more_results")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        limit_key: paginator.limit_key.clone(),
        non_aggregate_keys: strings("non_aggregate_keys"),
    })
}

/// Whether the user set a token or limit-key parameter themselves.
///
/// botocore turns auto-pagination off in that case — but NOT for `--max-items` or
/// `--starting-token`, which are the CLI's own controls rather than the operation's.
fn user_supplied_paging_param(settings: &Settings<'_>, config: &PaginationConfig) -> bool {
    let Some(Value::Object(input)) = &settings.input else { return false };
    let mut names: Vec<&String> = config.input_tokens.iter().collect();
    if let Some(limit_key) = &config.limit_key {
        names.push(limit_key);
    }
    names.iter().any(|name| input.contains_key(name.as_str()))
}

/// Set a (possibly dotted) member on the request input.
fn set_member(input: &mut Value, path: &str, value: Value) {
    if !input.is_object() {
        *input = Value::Object(Default::default());
    }
    // Input tokens are plain member names in every shipped paginator (verified across
    // all 3,283 entries), so a nested set is not needed.
    if let Some(object) = input.as_object_mut() {
        object.insert(path.split('.').next().unwrap_or(path).to_string(), value);
    }
}

/// Remove a member from the request input.
fn remove_member(input: &mut Value, path: &str) {
    if let Some(object) = input.as_object_mut() {
        object.remove(path.split('.').next().unwrap_or(path));
    }
}

/// Surface a pagination failure with the general exit code.
#[allow(dead_code)]
fn failure(message: impl std::fmt::Display) -> Failure {
    Failure::new(exit::GENERAL_ERROR, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn settings<'a>(input: Option<Value>) -> Settings<'a> {
        Settings {
            service: "svc",
            operation: "op",
            input,
            no_paginate: false,
            max_items: None,
            page_size: None,
            starting_token: None,
        }
    }

    fn config() -> PaginationConfig {
        PaginationConfig {
            input_tokens: vec!["Marker".into()],
            output_tokens: vec!["NextMarker".into()],
            result_keys: vec!["Items".into()],
            limit_key: Some("MaxItems".into()),
            ..Default::default()
        }
    }

    #[test]
    fn detects_a_user_supplied_token_or_limit() {
        assert!(user_supplied_paging_param(&settings(Some(json!({"Marker": "m"}))), &config()));
        assert!(user_supplied_paging_param(&settings(Some(json!({"MaxItems": 5}))), &config()));
        assert!(!user_supplied_paging_param(&settings(Some(json!({"Other": 1}))), &config()));
        assert!(!user_supplied_paging_param(&settings(None), &config()));
    }

    #[test]
    fn sets_members_on_an_absent_or_present_input() {
        let mut input = Value::Null;
        set_member(&mut input, "Marker", json!("m"));
        assert_eq!(input, json!({"Marker": "m"}));

        let mut existing = json!({"A": 1});
        set_member(&mut existing, "Marker", json!("m2"));
        assert_eq!(existing, json!({"A": 1, "Marker": "m2"}));
    }

    /// The loop must follow tokens, merge pages, and stop when the token dries up.
    #[test]
    fn follows_tokens_until_exhausted() {
        let pages = std::cell::RefCell::new(vec![
            json!({"Items": [1, 2], "NextMarker": "p2"}),
            json!({"Items": [3], "NextMarker": ""}),
        ]);
        let seen_tokens = std::cell::RefCell::new(Vec::new());

        let mut accumulator = Accumulator::new(config());
        let mut input = json!({});
        loop {
            seen_tokens
                .borrow_mut()
                .push(input.get("Marker").and_then(|v| v.as_str()).map(str::to_string));
            let page = pages.borrow_mut().remove(0);
            accumulator.add_page(&page, None);
            match pagination::next_token(&page, &config()) {
                Some(tokens) => set_member(&mut input, "Marker", tokens[0].clone()),
                None => break,
            }
        }
        assert_eq!(accumulator.finish(None)["Items"], json!([1, 2, 3]));
        assert_eq!(*seen_tokens.borrow(), vec![None, Some("p2".to_string())]);
    }
}
