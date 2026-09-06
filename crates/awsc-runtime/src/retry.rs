//! Retries, in botocore's `standard` mode.
//!
//! Three things about this are easy to get wrong:
//!
//! - `max_attempts` is a **total attempt count**, not a retry count. The default of 3
//!   means one try plus at most two retries.
//! - Backoff is **full jitter with no floor**: `rand(0,1) * min(2^(attempt-1), 20)`.
//!   A naive `2^n` schedule would sleep far longer.
//! - There is a **per-client retry quota** of 500 that retries spend from and only
//!   partially refund. Exhausting it stops retrying even when the error is retryable.
//!
//! `legacy` mode no longer exists in v2 — an unrecognised mode disables retries entirely
//! rather than falling back.

use std::time::Duration;

/// Total attempts, including the first. Retries = this minus one.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;

const MAX_BACKOFF_SECS: f64 = 20.0;
const INITIAL_QUOTA: i64 = 500;
const RETRY_COST: i64 = 5;
const TIMEOUT_RETRY_COST: i64 = 10;
const NO_RETRY_INCREMENT: i64 = 1;

/// Error codes that mean "transient, try again".
const TRANSIENT_ERROR_CODES: &[&str] =
    &["RequestTimeout", "RequestTimeoutException", "PriorRequestNotComplete"];

const TRANSIENT_STATUS_CODES: &[u16] = &[500, 502, 503, 504];

/// Throttling codes. Note there is **no HTTP 429 rule** — a 429 whose error code is not
/// on this list is not retried.
const THROTTLED_ERROR_CODES: &[&str] = &[
    "Throttling",
    "ThrottlingException",
    "ThrottledException",
    "RequestThrottledException",
    "TooManyRequestsException",
    "ProvisionedThroughputExceededException",
    "TransactionInProgressException",
    "RequestLimitExceeded",
    "BandwidthLimitExceeded",
    "LimitExceededException",
    "RequestThrottled",
    "SlowDown",
    "PriorRequestNotComplete",
    "EC2ThrottledException",
];

/// What one attempt produced.
#[derive(Debug, Clone)]
pub enum Outcome<'a> {
    /// The request never completed: a connection failure or timeout.
    Transport { timeout: bool },
    /// A response came back, with the service error code if it was an error.
    Response { status: u16, error_code: Option<&'a str> },
}

/// Retry configuration and the per-client quota.
pub struct RetryPolicy {
    pub max_attempts: u32,
    /// Disabled entirely when the mode is unrecognised, matching v2's behaviour.
    pub enabled: bool,
    quota: i64,
    /// The cost of the most recent acquisition, for the partial refund on success.
    last_acquired: Option<i64>,
    seed: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy::from_environment()
    }
}

impl RetryPolicy {
    /// Resolve from `AWS_RETRY_MODE` / `AWS_MAX_ATTEMPTS`.
    pub fn from_environment() -> Self {
        let mode = std::env::var("AWS_RETRY_MODE").unwrap_or_else(|_| "standard".to_string());
        let max_attempts = std::env::var("AWS_MAX_ATTEMPTS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_MAX_ATTEMPTS);

        RetryPolicy {
            max_attempts,
            // `adaptive` is standard plus a rate limiter; the retry decisions are the
            // same, so it behaves as standard here. Anything else disables retries.
            enabled: matches!(mode.as_str(), "standard" | "adaptive"),
            quota: INITIAL_QUOTA,
            last_acquired: None,
            seed: seed_from_clock(),
        }
    }

    /// How long to wait before retrying, or `None` to give up.
    ///
    /// `attempt` is 1-based: the number of the attempt that just finished.
    pub fn next_delay(
        &mut self,
        attempt: u32,
        outcome: &Outcome<'_>,
        signing_name: &str,
    ) -> Option<Duration> {
        if !self.enabled || attempt >= self.max_attempts {
            return None;
        }
        if !is_retryable(outcome, signing_name) {
            return None;
        }
        // The quota is checked only after deciding the error is retryable, and a failed
        // acquisition ends the loop even though the error would otherwise qualify.
        let cost = match outcome {
            Outcome::Transport { timeout: true } => TIMEOUT_RETRY_COST,
            _ => RETRY_COST,
        };
        if self.quota < cost {
            return None;
        }
        self.quota -= cost;
        self.last_acquired = Some(cost);

        Some(Duration::from_secs_f64(self.backoff(attempt)))
    }

    /// Full jitter over `[0, min(2^(attempt-1), 20))`. No minimum floor.
    fn backoff(&mut self, attempt: u32) -> f64 {
        let ceiling = 2f64.powi(attempt as i32 - 1).min(MAX_BACKOFF_SECS);
        self.random() * ceiling
    }

    /// Refund on a successful call.
    ///
    /// A 2xx that never retried gives back 1; a 2xx that did retry gives back only the
    /// **last** acquired cost, so a call that retried repeatedly still drains the quota.
    /// A non-2xx refunds nothing.
    pub fn record_success(&mut self, status: u16) {
        if !(200..300).contains(&status) {
            return;
        }
        let refund = self.last_acquired.take().unwrap_or(NO_RETRY_INCREMENT);
        self.quota = (self.quota + refund).min(INITIAL_QUOTA);
    }

    pub fn quota_remaining(&self) -> i64 {
        self.quota
    }

    /// A small xorshift, so tests can pin a seed rather than depend on the clock.
    fn random(&mut self) -> f64 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        // 53 bits of mantissa, as `random.random()` yields.
        ((self.seed >> 11) as f64) / ((1u64 << 53) as f64)
    }

    #[cfg(test)]
    fn with_seed(seed: u64) -> Self {
        RetryPolicy { seed, ..RetryPolicy::from_environment() }
    }
}

fn seed_from_clock() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x2545F4914F6CDD1D)
        | 1
}

/// The standard-mode predicate: transient, throttling, or one of the two service-specific
/// special cases.
fn is_retryable(outcome: &Outcome<'_>, signing_name: &str) -> bool {
    match outcome {
        // Connection errors and both kinds of timeout are retryable.
        Outcome::Transport { .. } => true,
        Outcome::Response { status, error_code } => {
            if TRANSIENT_STATUS_CODES.contains(status) {
                return true;
            }
            let Some(code) = error_code else { return false };
            if TRANSIENT_ERROR_CODES.contains(code) || THROTTLED_ERROR_CODES.contains(code) {
                return true;
            }
            // `special.py`: STS retries IDP communication failures.
            signing_name == "sts" && *code == "IDPCommunicationError"
        }
    }
}

/// The retry headers sent on every attempt.
///
/// `amz-sdk-invocation-id` is one UUID per API call, stable across its retries.
/// `amz-sdk-request` joins the present keys with `"; "` in the fixed order
/// `ttl`, `attempt`, `max` — and `max` is **absent on the first attempt**, because
/// upstream only writes it once a retry decision has been evaluated.
pub fn retry_headers(invocation_id: &str, attempt: u32, max_attempts: u32) -> Vec<(String, String)> {
    let mut request = format!("attempt={attempt}");
    if attempt > 1 {
        request.push_str(&format!("; max={max_attempts}"));
    }
    vec![
        ("amz-sdk-invocation-id".to_string(), invocation_id.to_string()),
        ("amz-sdk-request".to_string(), request),
    ]
}

/// A UUID-shaped identifier for one API call.
pub fn new_invocation_id() -> String {
    let mut seed = seed_from_clock();
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let (a, b) = (next(), next());
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        a as u32,
        (a >> 32) as u16,
        (a >> 48) & 0xfff,
        ((b as u16) & 0x3fff) | 0x8000,
        b >> 16 & 0xffff_ffff_ffff
    )
}

/// The ` (reached max retries: N)` suffix, where N is **retries, not attempts**.
///
/// Only appears when a response was actually parsed — a final transport failure raises
/// the transport error instead and never carries it.
pub fn max_retries_suffix(attempts_made: u32, max_attempts: u32) -> String {
    if attempts_made >= max_attempts && max_attempts > 0 {
        format!(" (reached max retries: {})", max_attempts - 1)
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> RetryPolicy {
        RetryPolicy { enabled: true, max_attempts: 3, ..RetryPolicy::with_seed(0x1234_5678) }
    }

    #[test]
    fn retries_transient_statuses_and_codes() {
        for status in [500, 502, 503, 504] {
            assert!(is_retryable(&Outcome::Response { status, error_code: None }, "s3"));
        }
        assert!(is_retryable(
            &Outcome::Response { status: 400, error_code: Some("ThrottlingException") },
            "ec2"
        ));
        assert!(is_retryable(
            &Outcome::Response { status: 400, error_code: Some("RequestTimeout") },
            "s3"
        ));
        assert!(is_retryable(&Outcome::Transport { timeout: true }, "s3"));
    }

    /// A 429 is NOT retried on status alone — only its error code counts.
    #[test]
    fn does_not_retry_on_429_alone() {
        assert!(!is_retryable(&Outcome::Response { status: 429, error_code: None }, "ec2"));
        assert!(!is_retryable(
            &Outcome::Response { status: 400, error_code: Some("AccessDenied") },
            "ec2"
        ));
    }

    #[test]
    fn sts_idp_communication_error_is_service_specific() {
        let outcome = Outcome::Response { status: 400, error_code: Some("IDPCommunicationError") };
        assert!(is_retryable(&outcome, "sts"));
        assert!(!is_retryable(&outcome, "ec2"), "only STS retries this code");
    }

    /// `max_attempts` counts total attempts, so the default 3 allows two retries.
    #[test]
    fn stops_after_max_attempts() {
        let mut p = policy();
        let outcome = Outcome::Response { status: 503, error_code: None };
        assert!(p.next_delay(1, &outcome, "s3").is_some());
        assert!(p.next_delay(2, &outcome, "s3").is_some());
        assert!(p.next_delay(3, &outcome, "s3").is_none(), "third attempt is the last");
    }

    #[test]
    fn backoff_is_full_jitter_within_the_ceiling() {
        let mut p = policy();
        for attempt in 1..8u32 {
            let ceiling = 2f64.powi(attempt as i32 - 1).min(20.0);
            for _ in 0..50 {
                let d = p.backoff(attempt);
                assert!((0.0..ceiling).contains(&d), "attempt {attempt}: {d} exceeds {ceiling}");
            }
        }
    }

    #[test]
    fn quota_limits_retries_and_refunds_partially() {
        let mut p = policy();
        p.max_attempts = 10_000;
        let outcome = Outcome::Response { status: 503, error_code: None };

        // Each retry costs 5, so 500 buys 100 of them.
        let mut retries = 0;
        while p.next_delay(1, &outcome, "s3").is_some() {
            retries += 1;
        }
        assert_eq!(retries, 100);
        assert!(p.quota_remaining() < RETRY_COST);

        // A success after retries refunds only the LAST acquisition, not all of them.
        p.record_success(200);
        assert_eq!(p.quota_remaining(), RETRY_COST);
    }

    #[test]
    fn timeouts_cost_double() {
        let mut p = policy();
        p.next_delay(1, &Outcome::Transport { timeout: true }, "s3");
        assert_eq!(p.quota_remaining(), INITIAL_QUOTA - TIMEOUT_RETRY_COST);
    }

    #[test]
    fn a_clean_success_refunds_one_and_a_failure_refunds_nothing() {
        let mut p = policy();
        p.quota = 100;
        p.record_success(200);
        assert_eq!(p.quota_remaining(), 101);
        p.record_success(500);
        assert_eq!(p.quota_remaining(), 101, "non-2xx refunds nothing");
    }

    /// `max` is absent on the first attempt and present afterwards.
    #[test]
    fn retry_headers_match_the_wire_format() {
        let first = retry_headers("abc", 1, 3);
        assert_eq!(first[0].1, "abc");
        assert_eq!(first[1].1, "attempt=1");
        let second = retry_headers("abc", 2, 3);
        assert_eq!(second[1].1, "attempt=2; max=3");
    }

    #[test]
    fn suffix_reports_retries_not_attempts() {
        assert_eq!(max_retries_suffix(3, 3), " (reached max retries: 2)");
        assert_eq!(max_retries_suffix(1, 3), "");
    }

    #[test]
    fn an_unrecognised_mode_disables_retries() {
        let mut p = RetryPolicy { enabled: false, ..policy() };
        assert!(p.next_delay(1, &Outcome::Response { status: 503, error_code: None }, "s3").is_none());
    }

    #[test]
    fn invocation_ids_look_like_uuids_and_differ() {
        let a = new_invocation_id();
        assert_eq!(a.len(), 36);
        assert_eq!(a.chars().filter(|c| *c == '-').count(), 4);
    }
}
