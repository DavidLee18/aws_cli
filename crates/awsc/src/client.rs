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
use aws_cli_protocol::eventstream;
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

/// Combine header-bound and body-bound members, keeping the model's member order.
///
/// Order is user-visible: the CLI prints members in the order the model declares them,
/// and a response that mixes header and body bindings must interleave them accordingly.
fn merge_in_model_order(
    output_shape: Option<&aws_cli_model::shape::StructureShape>,
    headers: Value,
    body: Value,
) -> Value {
    let (Some(shape), Value::Object(headers), Value::Object(mut body)) =
        (output_shape, headers, body)
    else {
        return Value::Object(Default::default());
    };
    let mut out = serde_json::Map::new();
    for name in shape.members.keys() {
        if let Some(value) = headers.get(name) {
            out.insert(name.clone(), value.clone());
        } else if let Some(value) = body.remove(name) {
            out.insert(name.clone(), value);
        }
    }
    // Anything the shape does not declare (ResponseMetadata and friends) keeps its place
    // at the end rather than being dropped.
    for (key, value) in body {
        out.entry(key).or_insert(value);
    }
    Value::Object(out)
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
    /// The bucket handed to the endpoint ruleset, if any.
    ///
    /// When it is set the endpoint has already accounted for the bucket — in the host
    /// for virtual-host addressing, in `path_prefix` for path-style — so the operation's
    /// own URI template must not repeat it. See [`Client::operation_path`].
    endpoint_bucket: Option<String>,
}

impl<'a> Client<'a> {

    /// The operation's path, with the bucket removed when the endpoint already carries it.
    ///
    /// S3's ruleset resolves the bucket into the endpoint: `my-bucket` becomes the host
    /// `my-bucket.s3.<region>.amazonaws.com`, and a name a virtual host cannot express
    /// (one containing a dot) becomes a path prefix instead. Either way the bucket is
    /// already in the URL, while the operation's `smithy.api#http` URI template still
    /// starts with `/{Bucket}` — so leaving both in place asks for
    /// `my-bucket.s3.../my-bucket`, which S3 answers with 404 or NoSuchKey.
    fn operation_path(&self, path: &str) -> String {
        let Some(bucket) = &self.endpoint_bucket else { return path.to_string() };
        let Some(rest) = path.strip_prefix('/').and_then(|p| p.strip_prefix(bucket.as_str()))
        else {
            return path.to_string();
        };
        // Only a whole segment counts: a bucket named `logs` must not eat the `/logs-2`
        // of a key that merely starts the same way.
        if !rest.is_empty() && !rest.starts_with('/') {
            return path.to_string();
        }
        if rest.is_empty() { "/".to_string() } else { rest.to_string() }
    }

    /// Resolve the endpoint and credentials for an already-loaded model.
    ///
    /// The model is borrowed rather than owned because `main` resolves the operation's
    /// shapes out of it before deciding whether a client is needed at all —
    /// `--generate-cli-skeleton` never builds one.
    pub fn new(model: &'a Model, globals: &Globals) -> Result<Client<'a>, Failure> {
        Client::build(model, globals, None, None)
    }

    /// As [`Client::new`], but letting the operation and its arguments contribute
    /// endpoint parameters. Some operations resolve to a different host than their
    /// siblings, and for S3 the bucket decides the host outright.
    pub fn for_operation(
        model: &'a Model,
        globals: &Globals,
        operation: &aws_cli_model::shape::OperationShape,
        input: Option<&Value>,
    ) -> Result<Client<'a>, Failure> {
        // S3's ruleset branches on the bucket name — a directory bucket
        // (`...--x-s3`) resolves to the S3 Express control endpoint and an ordinary one
        // does not. Without the bucket, `create-bucket`'s
        // `UseS3ExpressControlEndpoint` static parameter sends every bucket to Express.
        let bucket = input
            .and_then(|v| v.get("Bucket"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Client::build(model, globals, bucket.as_deref(), Some(operation))
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
        Client::build(model, globals, bucket, None)
    }

    fn build(
        model: &'a Model,
        globals: &Globals,
        bucket: Option<&str>,
        operation: Option<&aws_cli_model::shape::OperationShape>,
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
            static_context: operation.map(endpoint::static_context_params).unwrap_or_default(),
            ..Default::default()
        };
        // A ruleset that rejects the inputs, or a missing region, is a configuration
        // problem (253). The ruleset's own wording beats anything we would substitute.
        let endpoint_bucket = ep_params.bucket.clone();
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
            endpoint_bucket,
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
        body: http::Body,
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
                body: body.clone(),
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

    /// Issue an operation whose output is an event stream, reporting each event as it
    /// arrives.
    ///
    /// Deliberately not retried. A retry would replay the operation from the beginning,
    /// and by the time a stream fails the caller has already been handed events from the
    /// first attempt — re-running it would duplicate them. The `call_operation` path
    /// retries because nothing has been observed until it returns; here it has.
    ///
    /// The transport's read timeout applies per frame, so an idle stream (`StartLiveTail`
    /// between matches) ends once it exceeds it. `--cli-read-timeout 0` disables that.
    pub fn call_operation_events(
        &self,
        operation_wire_name: &str,
        op: &aws_cli_model::shape::OperationShape,
        input_shape: Option<&aws_cli_model::shape::StructureShape>,
        output_shape: &aws_cli_model::shape::StructureShape,
        input: Option<&Value>,
        on_event: &mut dyn FnMut(eventstream::Event) -> Result<(), Failure>,
    ) -> Result<(), Failure> {
        let Some((_, union_shape)) = eventstream::stream_member(self.model, output_shape) else {
            return Err(Failure::new(
                exit::GENERAL_ERROR,
                "operation output has no event stream member",
            ));
        };

        let wire = dispatch::serialize(
            self.model,
            self.protocol,
            operation_wire_name,
            op,
            input_shape,
            input,
        )
        .map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;

        let mut extra_headers = wire.headers.clone();
        extra_headers.extend(retry::retry_headers(&retry::new_invocation_id(), 1, 1));

        let request = http::PreparedRequest {
            method: wire.method.clone(),
            endpoint: self.endpoint.clone(),
            path: self.operation_path(&wire.path),
            query: wire.query.clone(),
            content_type: wire.content_type.clone(),
            extra_headers,
            body: http::Body::from_vec(wire.body.clone()),
        };

        let timestamp = sigv4::format_timestamp(crate::now_unix());
        let headers = if self.no_sign_request {
            http::unsigned_headers(&request)
        } else {
            http::sign_request(&request, &self.credentials, &timestamp).0
        };
        if self.debug {
            eprintln!("endpoint: {}", request.endpoint.url);
        }

        let mut sink = EventSink {
            model: self.model,
            protocol: self.protocol,
            union_shape,
            decoder: eventstream::Decoder::new(),
            on_event,
            failure: None,
        };

        let sent = http::send_to_writer(&request, &headers, &self.transport, &mut sink);

        // A failure raised by the callback or by frame decoding surfaces as an IO error
        // out of the sink; the real one was stashed on the way past.
        if let Some(failure) = sink.failure.take() {
            return Err(failure);
        }
        match sent {
            Err(aws_cli_runtime::RuntimeError::HttpStatus { status, body, headers }) => {
                let (code, message) = dispatch::parse_error(
                    self.protocol,
                    status,
                    body.as_bytes(),
                    headers
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("x-amzn-errortype"))
                        .map(|(_, v)| v.as_str()),
                );
                let mut failure = Failure::new(
                    exit::CLIENT_ERROR,
                    format!(
                        "An error occurred ({code}) when calling the \
                         {operation_wire_name} operation: {message}"
                    ),
                );
                failure.service_error_code = Some(code);
                Err(failure)
            }
            Err(e) => Err(Failure::new(exit::GENERAL_ERROR, e)),
            Ok(_) => {
                // Bytes left over mean the connection ended part-way through a frame.
                // Reporting success here would present a truncated stream as a complete
                // one, which is the whole failure mode the checksums exist to prevent.
                if !sink.decoder.is_empty() {
                    return Err(Failure::new(
                        exit::GENERAL_ERROR,
                        "event stream ended in the middle of a frame",
                    ));
                }
                Ok(())
            }
        }
    }

    /// Issue a duplex operation: send request events while reading response events.
    ///
    /// The threading is the interesting part. Both directions need the model — one to
    /// encode outgoing events, one to interpret incoming ones — and `Model` is not
    /// `Sync`, so neither job can be moved off this thread. Instead the two *thread*-side
    /// jobs are the ones that need nothing but bytes: one thread reads `lines`, another
    /// runs the HTTP call. Both report into a single channel, and this thread runs an
    /// event loop over it, holding the model and the signature chain.
    ///
    /// That the chain is advanced on one thread, in order, is a correctness requirement
    /// rather than a convenience: each frame signs over the previous frame's signature,
    /// so signing two frames concurrently would produce a pair the service rejects.
    pub fn call_operation_duplex(
        &self,
        operation_wire_name: &str,
        op: &aws_cli_model::shape::OperationShape,
        input_shape: Option<&aws_cli_model::shape::StructureShape>,
        output_shape: &aws_cli_model::shape::StructureShape,
        input: Option<&Value>,
        lines: Box<dyn Iterator<Item = std::io::Result<String>> + Send>,
        on_event: &mut dyn FnMut(eventstream::Event) -> Result<(), Failure>,
    ) -> Result<(), Failure> {
        use std::sync::mpsc;

        let Some(input_shape) = input_shape else {
            return Err(Failure::new(exit::GENERAL_ERROR, "operation has no input to stream"));
        };
        let Some((_, request_union)) = eventstream::stream_member(self.model, input_shape) else {
            return Err(Failure::new(
                exit::GENERAL_ERROR,
                "operation input has no event stream member",
            ));
        };
        let Some((_, response_union)) = eventstream::stream_member(self.model, output_shape) else {
            return Err(Failure::new(
                exit::GENERAL_ERROR,
                "operation output has no event stream member",
            ));
        };

        // The initial request carries no body: the stream member is not serialised into
        // it, only the members bound to the URI, query and headers.
        let wire = dispatch::serialize(
            self.model,
            self.protocol,
            operation_wire_name,
            op,
            Some(input_shape),
            input,
        )
        .map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;

        let mut extra_headers = wire.headers.clone();
        extra_headers.extend(retry::retry_headers(&retry::new_invocation_id(), 1, 1));
        extra_headers
            .push(("x-amz-content-sha256".into(), http::STREAMING_EVENTS_SHA256.to_string()));

        let request = http::PreparedRequest {
            method: wire.method.clone(),
            endpoint: self.endpoint.clone(),
            path: self.operation_path(&wire.path),
            query: wire.query.clone(),
            content_type: Some(EVENT_STREAM_CONTENT_TYPE.to_string()),
            extra_headers,
            body: http::Body::EventStream,
        };

        let timestamp = sigv4::format_timestamp(crate::now_unix());
        let (headers, seed) = if self.no_sign_request {
            (http::unsigned_headers(&request), String::new())
        } else {
            let (h, s) = http::sign_request(&request, &self.credentials, &timestamp);
            (h, s.signature)
        };
        if self.debug {
            eprintln!("endpoint: {}", request.endpoint.url);
        }

        let (wake_tx, wake_rx) = mpsc::channel::<Wake>();
        let (frame_tx, frame_rx) = mpsc::channel::<Vec<u8>>();

        let line_tx = wake_tx.clone();
        let reader = std::thread::spawn(move || {
            for line in lines {
                let wake = match line {
                    Ok(line) => Wake::Line(line),
                    Err(e) => Wake::InputFailed(e.to_string()),
                };
                let failed = matches!(wake, Wake::InputFailed(_));
                if line_tx.send(wake).is_err() || failed {
                    return;
                }
            }
            let _ = line_tx.send(Wake::InputDone);
        });

        let http_tx = wake_tx.clone();
        let transport = self.transport.clone();
        let caller = std::thread::spawn(move || {
            let mut sink = ChannelSink(http_tx.clone());
            let result = http::send_duplex(
                &request,
                &headers,
                &transport,
                move |sender| {
                    // Ends when the event loop drops its sender, which closes the
                    // request body and tells the service the stream is over.
                    while let Ok(bytes) = frame_rx.recv() {
                        if sender.send(bytes).is_err() {
                            return;
                        }
                    }
                },
                &mut sink,
            );
            let _ = http_tx.send(Wake::Finished(result.err()));
        });
        // The loop's own clone would otherwise keep the channel open forever.
        drop(wake_tx);

        let outcome = self.pump_duplex(
            operation_wire_name,
            request_union,
            response_union,
            &wake_rx,
            frame_tx,
            &seed,
            on_event,
        );
        let _ = reader.join();
        let _ = caller.join();
        outcome
    }

    /// The event loop: one thread, holding the model and the signature chain.
    #[allow(clippy::too_many_arguments)]
    fn pump_duplex(
        &self,
        operation_wire_name: &str,
        request_union: &aws_cli_model::shape::StructureShape,
        response_union: &aws_cli_model::shape::StructureShape,
        wake_rx: &std::sync::mpsc::Receiver<Wake>,
        frame_tx: std::sync::mpsc::Sender<Vec<u8>>,
        seed: &str,
        on_event: &mut dyn FnMut(eventstream::Event) -> Result<(), Failure>,
    ) -> Result<(), Failure> {
        let mut frames = Some(frame_tx);
        let mut prior = seed.to_string();
        let mut decoder = eventstream::Decoder::new();

        while let Ok(wake) = wake_rx.recv() {
            match wake {
                Wake::Line(line) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let inner = self.encode_request_event(request_union, &line)?;
                    let signed = self.sign_event_frame(&mut prior, &inner);
                    if let Some(tx) = &frames {
                        if tx.send(signed).is_err() {
                            frames = None;
                        }
                    }
                }
                Wake::InputDone => {
                    // An empty signed frame is how the protocol says "no more input".
                    // Without it the service waits for more rather than finishing.
                    let end = self.sign_event_frame(&mut prior, &[]);
                    if let Some(tx) = &frames {
                        let _ = tx.send(end);
                    }
                    frames = None;
                }
                // Dropping the sender here closes the request body, so the service
                // sees the stream end rather than waiting for frames that never come.
                Wake::InputFailed(message) => {
                    drop(frames.take());
                    return Err(Failure::new(exit::GENERAL_ERROR, message));
                }
                Wake::Response(bytes) => {
                    decoder.push(&bytes);
                    loop {
                        let frame = decoder
                            .next_frame()
                            .map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;
                        let Some(frame) = frame else { break };
                        let event = eventstream::interpret(
                            self.model,
                            self.protocol,
                            response_union,
                            &frame,
                        )
                        .map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;
                        on_event(event)?;
                    }
                }
                Wake::Finished(error) => {
                    drop(frames.take());
                    if let Some(error) = error {
                        return Err(self.duplex_failure(operation_wire_name, error));
                    }
                    if !decoder.is_empty() {
                        return Err(Failure::new(
                            exit::GENERAL_ERROR,
                            "event stream ended in the middle of a frame",
                        ));
                    }
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// One line of input: `{"EventName": {...}}`, the same shape an output event prints.
    fn encode_request_event(
        &self,
        request_union: &aws_cli_model::shape::StructureShape,
        line: &str,
    ) -> Result<Vec<u8>, Failure> {
        let document: Value = serde_json::from_str(line).map_err(|e| {
            Failure::new(exit::PARAM_VALIDATION, format!("input event is not JSON: {e}"))
        })?;
        let object = document.as_object().filter(|o| o.len() == 1).ok_or_else(|| {
            Failure::new(
                exit::PARAM_VALIDATION,
                "each input line must be a JSON object with exactly one key, \
                 naming the event: {\"AudioEvent\": {...}}",
            )
        })?;
        let (name, value) = object.iter().next().expect("exactly one key");
        eventstream::encode_event(self.model, self.protocol, request_union, name, value)
            .map_err(|e| Failure::new(exit::PARAM_VALIDATION, e))
    }

    /// Wrap one frame in its signature and advance the chain.
    fn sign_event_frame(&self, prior: &mut String, inner: &[u8]) -> Vec<u8> {
        use aws_cli_protocol::eventstream::HeaderValue;

        let now = crate::now_unix();
        let date = vec![(":date".to_string(), HeaderValue::Timestamp(now * 1000))];
        let date_headers = eventstream::encode_headers(&date);

        let timestamp = sigv4::format_timestamp(now);
        let ctx = sigv4::SigningContext {
            credentials: &self.credentials,
            region: &self.endpoint.signing_region,
            service: &self.endpoint.signing_name,
            timestamp: &timestamp,
        };
        let signature = sigv4::sign_event(&ctx, prior, &date_headers, inner);
        let raw = hex_decode(&signature);
        *prior = signature;

        let mut headers = date;
        headers.push((":chunk-signature".to_string(), HeaderValue::Bytes(raw)));
        eventstream::encode(&headers, inner)
    }

    fn duplex_failure(
        &self,
        operation_wire_name: &str,
        error: aws_cli_runtime::RuntimeError,
    ) -> Failure {
        match error {
            aws_cli_runtime::RuntimeError::HttpStatus { status, body, headers } => {
                let (code, message) = dispatch::parse_error(
                    self.protocol,
                    status,
                    body.as_bytes(),
                    headers
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case("x-amzn-errortype"))
                        .map(|(_, v)| v.as_str()),
                );
                let mut failure = Failure::new(
                    exit::CLIENT_ERROR,
                    format!(
                        "An error occurred ({code}) when calling the \
                         {operation_wire_name} operation: {message}"
                    ),
                );
                failure.service_error_code = Some(code);
                failure
            }
            other => Failure::new(exit::GENERAL_ERROR, other),
        }
    }

    /// As [`Client::call_operation`], but handing back the raw response.
    ///
    /// Operations with a streaming blob output write their body to a file rather than
    /// parsing it, so they need the bytes and headers, not a document.
    pub fn call_operation_raw(
        &self,
        operation_wire_name: &str,
        op: &aws_cli_model::shape::OperationShape,
        input_shape: Option<&aws_cli_model::shape::StructureShape>,
        input: Option<&Value>,
    ) -> Result<http::Response, Failure> {
        let wire = dispatch::serialize(
            &self.model,
            self.protocol,
            operation_wire_name,
            op,
            input_shape,
            input,
        )
        .map_err(|e| Failure::new(exit::GENERAL_ERROR, e))?;

        let response = self.send_raw(
            &wire.method,
            &self.operation_path(&wire.path),
            &wire.query,
            &wire.headers,
            http::Body::from_vec(wire.body),
        )?;

        if response.status >= 400 {
            let (code, message) = dispatch::parse_error(
                self.protocol,
                response.status,
                response.bytes(),
                response.header("x-amzn-errortype").as_deref(),
            );
            let mut failure = Failure::new(
                exit::CLIENT_ERROR,
                format!(
                    "An error occurred ({code}) when calling the \
                     {operation_wire_name} operation: {message}"
                ),
            );
            failure.service_error_code = Some(code);
            return Err(failure);
        }
        Ok(response)
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
        // S3's list operations are always sent with `EncodingType=url`, so a key that
        // cannot be represented in XML survives the round trip. The response is decoded
        // again below.
        let url_encoded = aws_cli_protocol::response_fixups::wants_url_encoding(
            &self.endpoint.signing_name,
            operation_wire_name,
        );
        let injected;
        let input = if url_encoded {
            let mut object = input.cloned().unwrap_or_else(|| Value::Object(Default::default()));
            if let Some(map) = object.as_object_mut() {
                map.entry("EncodingType").or_insert_with(|| Value::String("url".into()));
            }
            injected = object;
            Some(&injected)
        } else {
            input
        };

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
                path: self.operation_path(&wire.path),
                query: wire.query.clone(),
                content_type: wire.content_type.clone(),
                extra_headers,
                body: http::Body::from_vec(wire.body.clone()),
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
                eprintln!("body: {}", String::from_utf8_lossy(request.body.as_bytes().unwrap_or_default()));
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
                                response.status,
                                response.bytes(),
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
                response.status,
                response.bytes(),
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

        // `rest*` responses bind members to headers as well as the body. `head-object`
        // has no body at all — every field it prints comes from a header — so parsing
        // only the body left it with nothing to parse.
        let from_headers = match (self.protocol, output_shape) {
            (Protocol::RestJson1 | Protocol::RestXml, Some(shape)) => Some(
                aws_cli_protocol::http_binding::bind_output_headers(
                    self.model,
                    shape,
                    response.headers(),
                ),
            ),
            _ => None,
        };

        // A body-less response has nothing for the body parser, and some responses carry
        // a body with no root element at all (`get-bucket-location` in us-east-1). Where
        // the headers can still answer, an unparseable body is empty rather than fatal.
        let parsed_body = dispatch::parse_response(
            self.model,
            self.protocol,
            operation_wire_name,
            output_shape,
            response.bytes(),
        );
        let from_body = match parsed_body {
            Ok(value) => value,
            Err(e) if from_headers.is_some() => {
                if response.bytes().iter().any(|b| !b.is_ascii_whitespace()) && self.debug {
                    eprintln!("note: body did not parse ({e}); using headers alone");
                }
                Value::Object(Default::default())
            }
            Err(e) => return Err(Failure::new(exit::GENERAL_ERROR, e)),
        };

        let mut document = match from_headers {
            Some(headers) => merge_in_model_order(output_shape, headers, from_body),
            None => from_body,
        };
        if url_encoded {
            aws_cli_protocol::response_fixups::decode_encoded_keys(&mut document);
        }
        Ok(document)
    }
}

/// Feeds response bytes into the event-stream decoder as they arrive.
///
/// A `Write` implementation because that is what the transport hands a streaming body,
/// which means event streams reuse the download path rather than adding a second one.
struct EventSink<'a> {
    model: &'a aws_cli_model::Model,
    protocol: Protocol,
    union_shape: &'a aws_cli_model::shape::StructureShape,
    decoder: eventstream::Decoder,
    on_event: &'a mut dyn FnMut(eventstream::Event) -> Result<(), Failure>,
    /// The real error, since `Write` can only report an `io::Error`.
    failure: Option<Failure>,
}

impl std::io::Write for EventSink<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.decoder.push(buf);
        loop {
            let frame = match self.decoder.next_frame() {
                Ok(Some(frame)) => frame,
                Ok(None) => return Ok(buf.len()),
                Err(e) => return Err(self.stop(Failure::new(exit::GENERAL_ERROR, e))),
            };
            let event =
                match eventstream::interpret(self.model, self.protocol, self.union_shape, &frame) {
                    Ok(event) => event,
                    Err(e) => return Err(self.stop(Failure::new(exit::GENERAL_ERROR, e))),
                };
            if let Err(failure) = (self.on_event)(event) {
                return Err(self.stop(failure));
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl EventSink<'_> {
    /// Stash the real failure and end the transfer.
    fn stop(&mut self, failure: Failure) -> std::io::Error {
        let message = failure.message().to_string();
        self.failure = Some(failure);
        std::io::Error::other(message)
    }
}

/// The content type of a request whose body is an event stream.
const EVENT_STREAM_CONTENT_TYPE: &str = "application/vnd.amazon.eventstream";

/// What the event loop waits on. Both worker threads report through one channel, because
/// the loop has to interleave them and `std::sync::mpsc` has no select.
enum Wake {
    /// A line of input to turn into a request event.
    Line(String),
    /// Input is exhausted; the stream should be closed.
    InputDone,
    InputFailed(String),
    /// Raw response bytes, in whatever sizes the network produced.
    Response(Vec<u8>),
    /// The HTTP call returned, with its error if it had one.
    Finished(Option<aws_cli_runtime::RuntimeError>),
}

/// Forwards response bytes into the event loop's channel.
struct ChannelSink(std::sync::mpsc::Sender<Wake>);

impl std::io::Write for ChannelSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .send(Wake::Response(buf.to_vec()))
            .map(|()| buf.len())
            .map_err(|_| std::io::Error::other("the event loop stopped reading"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Hex back to bytes, for the chunk signature: the string-to-sign uses hex, the frame
/// header carries the raw 32 bytes.
fn hex_decode(hex: &str) -> Vec<u8> {
    hex.as_bytes()
        .chunks(2)
        .filter_map(|pair| {
            let text = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(text, 16).ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    /// `Client::operation_path`'s logic, exercised without building a client.
    fn strip(bucket: Option<&str>, path: &str) -> String {
        let Some(bucket) = bucket else { return path.to_string() };
        let Some(rest) = path.strip_prefix('/').and_then(|p| p.strip_prefix(bucket)) else {
            return path.to_string();
        };
        if !rest.is_empty() && !rest.starts_with('/') {
            return path.to_string();
        }
        if rest.is_empty() { "/".to_string() } else { rest.to_string() }
    }

    #[test]
    fn drops_the_bucket_the_endpoint_already_carries() {
        assert_eq!(strip(Some("b"), "/b"), "/");
        assert_eq!(strip(Some("b"), "/b/key.txt"), "/key.txt");
        assert_eq!(strip(Some("my.dotted.bucket"), "/my.dotted.bucket/k"), "/k");
    }

    /// A bucket name that merely prefixes the next segment must not be eaten: with
    /// bucket `logs`, the path `/logs-2/k` belongs to a different bucket entirely.
    #[test]
    fn only_a_whole_segment_counts() {
        assert_eq!(strip(Some("logs"), "/logs-2/k"), "/logs-2/k");
        assert_eq!(strip(Some("logs"), "/other/k"), "/other/k");
    }

    /// With no bucket in the endpoint the path is untouched, which is every service
    /// other than S3.
    #[test]
    fn leaves_other_services_alone() {
        assert_eq!(strip(None, "/service/X/operation/Y"), "/service/X/operation/Y");
    }
}
