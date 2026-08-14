//! A resolved client for one service: endpoint, credentials, transport, retry policy.
//!
//! `main` used to hold all of this inline, which was fine while every invocation was
//! exactly one modeled operation. Custom commands break that assumption — `ecr
//! get-login-password` calls `GetAuthorizationToken` and prints one field of it, and
//! `configservice get-status` makes several calls — so the machinery is factored out here
//! and both paths share it.
//!
//! A `Client` is bound to one service. Commands that need a second service (`eks
//! get-token` reaches for STS) build a second client.

use crate::dispatch;
use crate::exit;
use crate::Failure;
use aws_cli_model::{Model, Protocol};
use aws_cli_runtime::{credentials, endpoint, http, retry, sigv4};
use serde_json::Value;
use std::cell::RefCell;

/// The global arguments a client needs, independent of which command is running.
///
/// Custom commands construct one of these directly, since they do not go through the
/// modeled-operation argument binding.
#[derive(Debug, Clone, Default)]
pub struct Globals {
    pub region: Option<String>,
    pub profile: Option<String>,
    pub endpoint_url: Option<String>,
    pub debug: bool,
    pub no_sign_request: bool,
    pub verify_ssl: bool,
    pub ca_bundle: Option<String>,
    pub read_timeout: Option<u64>,
    pub connect_timeout: Option<u64>,
}

impl Globals {
    pub fn from_parsed(p: &crate::args::Parsed) -> Self {
        Globals {
            region: p.region.clone(),
            profile: p.profile.clone(),
            endpoint_url: p.endpoint_url.clone(),
            debug: p.debug,
            no_sign_request: p.no_sign_request,
            verify_ssl: p.verify_ssl,
            ca_bundle: p.ca_bundle.clone(),
            read_timeout: p.read_timeout,
            connect_timeout: p.connect_timeout,
        }
    }

    /// The same globals aimed at a different service.
    ///
    /// `--endpoint-url` is deliberately dropped: the reference's
    /// `create_client_from_parsed_globals` passes the endpoint override only to the
    /// service the user named, so `ecr get-login-password --endpoint-url ...` must not
    /// redirect an unrelated call.
    pub fn for_other_service(&self) -> Self {
        Globals { endpoint_url: None, ..self.clone() }
    }
}

pub struct Client<'a> {
    pub model: &'a Model,
    pub protocol: Protocol,
    pub endpoint: endpoint::Endpoint,
    pub credentials: credentials::Credentials,
    transport: http::Transport,
    retry: RefCell<retry::RetryPolicy>,
    debug: bool,
    no_sign_request: bool,
}

impl<'a> Client<'a> {
    /// Resolve the endpoint and credentials for an already-loaded model.
    ///
    /// The model is borrowed rather than owned because `main` resolves the operation's
    /// shapes out of it before deciding whether a client is needed at all —
    /// `--generate-cli-skeleton` never builds one.
    pub fn new(model: &'a Model, globals: &Globals) -> Result<Client<'a>, Failure> {
        Client::for_bucket(model, globals, None)
    }

    /// As [`Client::new`], but supplying S3's `Bucket` endpoint parameter.
    ///
    /// S3's ruleset resolves a *different host* per bucket (the virtual-host form), and
    /// the host is signed, so the bucket has to be known before the endpoint is resolved.
    pub fn for_bucket(
        model: &'a Model,
        globals: &Globals,
        bucket: Option<&str>,
    ) -> Result<Client<'a>, Failure> {
        let protocol = model.protocol().map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;
        // The profile's `region` key is the last step of botocore's precedence, and
        // skipping it silently sent S3 and STS to their legacy global endpoints.
        let profile_region =
            credentials::profile::profile_region(globals.profile.as_deref());
        let region =
            endpoint::resolve_region(globals.region.as_deref(), profile_region.as_deref());
        let ep_params = endpoint::EndpointParams {
            region,
            endpoint_url: globals.endpoint_url.clone(),
            bucket: bucket.map(str::to_string),
            ..Default::default()
        };
        // A ruleset that rejects the inputs, or a missing region, is a configuration
        // problem (253). The ruleset's own wording beats anything we would substitute.
        let ep = endpoint::resolve(&model, &ep_params).map_err(|e| match e {
            endpoint::EndpointError::Rules(_) | endpoint::EndpointError::NoRegion => {
                Failure::new(exit::CONFIGURATION, e)
            }
            other => Failure::new(exit::GENERAL_ERROR, other),
        })?;

        let creds = credentials::resolve(globals.profile.as_deref(), Some(&ep.signing_region))
            .map_err(|e| {
                // Only "no credentials found at all" is a configuration error; an unknown
                // profile or an expired SSO token is general (255). Matches the reference.
                let code = if e.is_configuration_error() {
                    exit::CONFIGURATION
                } else if e.is_client_error() {
                    exit::CLIENT_ERROR
                } else {
                    exit::GENERAL_ERROR
                };
                Failure::new(code, e)
            })?;

        Ok(Client {
            model,
            protocol,
            endpoint: ep,
            credentials: creds,
            transport: http::Transport {
                verify_ssl: globals.verify_ssl,
                ca_bundle: globals.ca_bundle.clone(),
                read_timeout: globals.read_timeout,
                connect_timeout: globals.connect_timeout,
            },
            retry: RefCell::new(retry::RetryPolicy::from_environment()),
            debug: globals.debug,
            no_sign_request: globals.no_sign_request,
        })
    }

    /// Call an operation by its CLI name (`get-authorization-token`), returning the
    /// parsed response. This is the entry point custom commands use.
    pub fn call(&self, operation: &str, input: Option<&Value>) -> Result<Value, Failure> {
        let (op_id, op) = self
            .model
            .operation(operation)
            .map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))?;
        let input_shape =
            self.model.operation_input(op).map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;
        let output_shape =
            self.model.operation_output(op).map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;
        self.call_operation(
            op_id.name(),
            op,
            input_shape,
            output_shape,
            input,
        )
    }

    /// A hand-built request, signed and retried but not routed through the protocol layer.
    ///
    /// The `s3` tree uses this rather than `call`: it moves arbitrary binary payloads, and
    /// the modelled path would base64 a blob member and decode every response as UTF-8.
    /// Building the handful of S3 requests directly is both safer and clearer than bending
    /// the generic serializer around them.
    pub fn send_raw(
        &self,
        method: &str,
        path: &str,
        query: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<http::Response, Failure> {
        let invocation_id = retry::new_invocation_id();
        let max_attempts = self.retry.borrow().max_attempts;
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
                body_bytes: body.clone(),
            };

            let timestamp = sigv4::format_timestamp(crate::now_unix());
            let (signed, _) = if self.no_sign_request {
                (http::unsigned_headers(&request), None)
            } else {
                let (h, s) = http::sign_request(&request, &self.credentials, &timestamp);
                (h, Some(s))
            };

            if self.debug {
                eprintln!("{method} {}{path}?{query}", request.endpoint.url);
            }

            let sent = http::send(&request, &signed, &self.transport);
            let delay = match &sent {
                Err(e) => {
                    let message = e.to_string();
                    let timeout = message.contains("timed out") || message.contains("timeout");
                    self.retry.borrow_mut().next_delay(
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
                    self.retry.borrow_mut().next_delay(
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
                    std::thread::sleep(delay);
                    attempt += 1;
                }
                None => {
                    let response = sent.map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;
                    self.retry.borrow_mut().record_success(response.status);
                    return Ok(response);
                }
            }
        }
    }

    /// One round trip, retried in place.
    ///
    /// Takes the already-resolved shapes so the paginating path can issue the same
    /// operation repeatedly without re-resolving them.
    pub fn call_operation(
        &self,
        operation_wire_name: &str,
        op: &aws_cli_model::shape::OperationShape,
        input_shape: Option<&aws_cli_model::shape::StructureShape>,
        output_shape: Option<&aws_cli_model::shape::StructureShape>,
        input: Option<&Value>,
    ) -> Result<Value, Failure> {
        let wire = dispatch::serialize(
            &self.model,
            self.protocol,
            operation_wire_name,
            op,
            input_shape,
            input,
        )
        .map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;

        // One invocation id for the whole call, stable across its retries.
        let invocation_id = retry::new_invocation_id();
        let max_attempts = self.retry.borrow().max_attempts;
        let mut attempt: u32 = 1;

        let response = loop {
            let mut extra_headers = wire.headers.clone();
            extra_headers.extend(retry::retry_headers(&invocation_id, attempt, max_attempts));

            let request = http::PreparedRequest {
                method: wire.method.clone(),
                endpoint: self.endpoint.clone(),
                path: wire.path.clone(),
                query: wire.query.clone(),
                content_type: wire.content_type.clone(),
                extra_headers,
                body_bytes: wire.body.clone().into_bytes(),
            };

            // Re-signed each attempt, since the timestamp changes.
            let timestamp = sigv4::format_timestamp(crate::now_unix());
            let (headers, signature) = if self.no_sign_request {
                (http::unsigned_headers(&request), None)
            } else {
                let (h, s) = http::sign_request(&request, &self.credentials, &timestamp);
                (h, Some(s))
            };

            if self.debug {
                eprintln!("endpoint: {}", request.endpoint.url);
                eprintln!("body: {}", String::from_utf8_lossy(&request.body_bytes));
                if let Some(signature) = &signature {
                    eprintln!("CanonicalRequest:\n{}", signature.canonical_request);
                    eprintln!("StringToSign:\n{}", signature.string_to_sign);
                    eprintln!("Signature:\n{}", signature.signature);
                }
            }

            let sent = http::send(&request, &headers, &self.transport);

            // Classify this attempt, then ask the policy whether to go again.
            let outcome_delay = match &sent {
                Err(e) => {
                    let message = e.to_string();
                    let timeout = message.contains("timed out") || message.contains("timeout");
                    self.retry.borrow_mut().next_delay(
                        attempt,
                        &retry::Outcome::Transport { timeout },
                        &self.endpoint.signing_name,
                    )
                }
                Ok(response) => {
                    let code = if response.status >= 400 {
                        Some(
                            dispatch::parse_error(
                                self.protocol,
                                &response.text(),
                                response.header("x-amzn-errortype").as_deref(),
                            )
                            .0,
                        )
                    } else {
                        None
                    };
                    self.retry.borrow_mut().next_delay(
                        attempt,
                        &retry::Outcome::Response {
                            status: response.status,
                            error_code: code.as_deref(),
                        },
                        &self.endpoint.signing_name,
                    )
                }
            };

            match outcome_delay {
                Some(delay) => {
                    std::thread::sleep(delay);
                    attempt += 1;
                }
                None => {
                    let response = sent.map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;
                    self.retry.borrow_mut().record_success(response.status);
                    break response;
                }
            }
        };

        if response.status >= 400 {
            let (code, message) = dispatch::parse_error(
                self.protocol,
                &response.text(),
                response.header("x-amzn-errortype").as_deref(),
            );
            // The reference appends the retry count only when the attempt limit was
            // actually reached, and only when a response was parsed.
            let suffix = retry::max_retries_suffix(attempt, max_attempts);
            let mut failure = Failure::new(
                exit::CLIENT_ERROR,
                format!(
                    "An error occurred ({code}) when calling the \
                     {operation_wire_name} operation{suffix}: {message}"
                ),
            );
            failure.service_error_code = Some(code);
            return Err(failure);
        }

        dispatch::parse_response(
            &self.model,
            self.protocol,
            operation_wire_name,
            output_shape,
            &response.text(),
        )
        .map_err(|e| Failure::new(exit::GENERAL_ERROR, e))
    }
}
