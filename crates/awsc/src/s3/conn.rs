//! A share-able S3 connection.
//!
//! `Client` holds its retry policy in a `RefCell`, which is fine for the one-request-per-
//! invocation path but cannot cross threads. The transfer commands run many requests at
//! once, so this carries only plain data — endpoint, credentials, transport — and gives
//! each request its own retry policy.

use crate::exit;
use crate::Failure;
use aws_cli_runtime::{credentials::Credentials, endpoint::Endpoint, http, retry, sigv4};

/// The service error code of a response that is about to be retried, for the trace.
fn retried_error_code(response: &http::Response) -> Option<String> {
    if response.status >= 400 {
        aws_cli_protocol::xml::parse_error(&response.text()).map(|e| e.code)
    } else {
        None
    }
}

pub struct Conn {
    pub endpoint: Endpoint,
    pub credentials: Credentials,
    pub transport: http::Transport,
    pub debug: bool,
    pub no_sign_request: bool,
    /// Report every *retried* response on stderr, from `AWSC_RETRY_TRACE`.
    ///
    /// The pool only learns about throttling once the retry budget is exhausted, so a
    /// `SlowDown` that retry absorbs is otherwise invisible. Measuring whether a given
    /// concurrency draws throttling at all needs to see the absorbed ones too.
    pub retry_trace: bool,
}

impl Conn {
    pub fn from_client(client: &crate::client::Client<'_>, globals: &crate::client::Globals) -> Conn {
        Conn {
            endpoint: client.endpoint.clone(),
            credentials: client.credentials.clone(),
            transport: http::Transport {
                verify_ssl: globals.verify_ssl,
                ca_bundle: globals.ca_bundle.clone(),
                read_timeout: globals.read_timeout,
                connect_timeout: globals.connect_timeout,
            },
            debug: globals.debug,
            no_sign_request: globals.no_sign_request,
            retry_trace: std::env::var_os("AWSC_RETRY_TRACE").is_some(),
        }
    }

    /// Issue one request, retried in place. Safe to call from many threads at once.
    pub fn send(
        &self,
        method: &str,
        path: &str,
        query: &str,
        headers: &[(String, String)],
        body: http::Body,
    ) -> Result<http::Response, Failure> {
        let mut policy = retry::RetryPolicy::from_environment();
        let invocation_id = retry::new_invocation_id();
        let max_attempts = policy.max_attempts;
        let mut attempt: u32 = 1;

        loop {
            let mut extra_headers = headers.to_vec();
            extra_headers.extend(retry::retry_headers(&invocation_id, attempt, max_attempts));

            let request = http::PreparedRequest {
                method: method.to_string(),
                endpoint: self.endpoint.clone(),
                path: path.to_string(),
                query: query.to_string(),
                content_type: None,
                extra_headers,
                body: body.clone(),
            };

            let timestamp = sigv4::format_timestamp(crate::now_unix());
            let signed = if self.no_sign_request {
                http::unsigned_headers(&request)
            } else {
                http::sign_request(&request, &self.credentials, &timestamp).0
            };
            if self.debug {
                eprintln!("{method} {}{path}?{query}", request.endpoint.url);
            }

            let sent = http::send(&request, &signed, &self.transport);
            let delay = match &sent {
                Err(e) => {
                    let message = e.to_string();
                    let timeout = message.contains("timed out") || message.contains("timeout");
                    policy.next_delay(
                        attempt,
                        &retry::Outcome::Transport { timeout },
                        &self.endpoint.signing_name,
                    )
                }
                Ok(response) => {
                    let code = if response.status >= 400 {
                        aws_cli_protocol::xml::parse_error(&response.text()).map(|e| e.code)
                    } else {
                        None
                    };
                    policy.next_delay(
                        attempt,
                        &retry::Outcome::Response {
                            status: response.status,
                            error_code: code.as_deref(),
                        },
                        &self.endpoint.signing_name,
                    )
                }
            };

            match delay {
                Some(delay) => {
                    if self.retry_trace {
                        let (status, code) = match &sent {
                            Ok(r) => (r.status, retried_error_code(r)),
                            Err(_) => (0, Some("transport".to_string())),
                        };
                        eprintln!(
                            "retry: {method} {path} attempt {attempt}/{max_attempts} status {status} code {} after {:?}",
                            code.as_deref().unwrap_or("-"),
                            delay
                        );
                    }
                    std::thread::sleep(delay);
                    attempt += 1;
                }
                None => {
                    let response = sent.map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;
                    policy.record_success(response.status);
                    return Ok(response);
                }
            }
        }
    }

    /// Send, and turn any non-2xx into the reference's error wording.
    pub fn send_checked(
        &self,
        operation: &str,
        method: &str,
        path: &str,
        query: &str,
        headers: &[(String, String)],
        body: http::Body,
    ) -> Result<http::Response, Failure> {
        let response = self.send(method, path, query, headers, body)?;
        if response.status >= 400 {
            return Err(super::service_error(operation, &response));
        }
        Ok(response)
    }

    /// The request path for an object key.
    ///
    /// Deliberately does NOT include `endpoint.path_prefix`: the transport builds the URL
    /// from `endpoint.url`, which already contains it, and the signer adds it separately.
    /// Including it here sends `/bucket/bucket/key`.
    pub fn object_path(&self, key: &str) -> String {
        format!("/{}", super::encode_key(key))
    }
}
